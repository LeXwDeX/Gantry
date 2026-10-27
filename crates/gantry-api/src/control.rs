//! 写类路由的 facade 调用：交付单元控制 #8 #43–#45（`DeliveryControl`，P-56）与
//! 发消息 #12（`ConversationService`）。
//!
//! 写上下文一律是 `core::ports::ActorCtx`（P-78），由本模块在鉴权后构造；
//! `dedup_key = "api:<device_id>:<Idempotency-Key>"`，所属模块用它作所追加业务事件
//! 的 `dedup_key`，使同键重试在 `store` 层也至多追加一次（本文 §7.6）。
//! 写响应的 `X-Event-Seq` 取该 `dedup_key` 对应事件的 `seq`；目标状态已达成、
//! 没有追加事件时取调用后的 head（本文 §7.7）。

use gantry_core::entities::{Actor, ActorKind, Ref, Seq};
use gantry_core::ids::{ConversationId, DeliveryId, NodeId, Ulid};
use gantry_core::level::Level;
use gantry_core::ports::context::{ActorCtx, ModuleError, StoreError};
use gantry_core::ports::conversation::{ConvError, NewUserMessage};
use gantry_core::ports::ClientProfile;
use http::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use crate::problem::{FieldError, Problem, ProblemCode};
use crate::server::{ApiCore, HandlerRequest, Reply};
use crate::validate;

/// 由鉴权结果构造写上下文（本文 §6.3）。
pub fn actor_ctx(req: &HandlerRequest<'_>) -> ActorCtx {
    let device = &req.auth.device;
    ActorCtx {
        actor: Actor::new(ActorKind::Device, device.id.to_string()),
        device: Some(device.id),
        role: Some(device.role),
        client_profile: Some(req.auth.profile),
        request_id: req
            .request_id
            .parse::<Ulid>()
            .unwrap_or_else(|_| Ulid::from_parts(0, 0).expect("zero ulid is valid")),
        idempotency_key: req.idempotency_key.map(str::to_owned),
        dedup_key: req.idempotency_key.map(|key| dedup_key(req, key)),
        causation_id: None,
    }
}

pub fn dedup_key(req: &HandlerRequest<'_>, key: &str) -> String {
    format!("api:{}:{key}", req.auth.device.id)
}

fn parse_id<T: std::str::FromStr>(req: &HandlerRequest<'_>, name: &str) -> Result<T, Problem> {
    req.param(name)
        .parse()
        .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "path parameter is invalid"))
}

fn body_of<'a>(req: &'a HandlerRequest<'_>) -> &'a Value {
    static EMPTY: Value = Value::Null;
    req.body.as_ref().unwrap_or(&EMPTY)
}

/// 写响应的 `X-Event-Seq`。
async fn written_seq(core: &ApiCore, ctx: &ActorCtx) -> Result<Seq, Problem> {
    let events = core.services().store.events();
    if let Some(key) = &ctx.dedup_key {
        if let Some(seq) = events.by_dedup_key(key).await.map_err(|_| unavailable())? {
            return Ok(seq);
        }
    }
    events.head().await.map_err(|_| unavailable())
}

fn unavailable() -> Problem {
    Problem::retry_after(ProblemCode::Unavailable, "store unavailable", 1)
}

/// 下游 `ModuleError` → problem（本文 §9）。
pub fn module_error(error: ModuleError) -> Problem {
    match error {
        ModuleError::NotFound => Problem::not_found(),
        ModuleError::InvalidState { .. } | ModuleError::Conflict { .. } => {
            Problem::conflict("invalid_state")
        }
        ModuleError::Validation { pointer, .. } => {
            Problem::validation(vec![FieldError::new(pointer, "type", "value was rejected")])
        }
        ModuleError::Forbidden => {
            Problem::forbidden_role(gantry_core::entities::DeviceRole::Operator)
        }
        ModuleError::Unavailable { .. } | ModuleError::Store(StoreError::Unavailable { .. }) => {
            unavailable()
        }
        ModuleError::Store(_) => Problem::internal(),
    }
}

/// #43 #44 #8：暂停 / 继续 / 放弃。
pub async fn delivery_control(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let op = req.route.operation_id;
    validate::body(
        if op == "abandonDelivery" {
            "AbandonRequest"
        } else {
            "EmptyRequest"
        },
        body_of(req),
    )?;
    let id: DeliveryId = parse_id(req, "delivery_id")?;
    let ctx = actor_ctx(req);
    let engine = &core.services().engine;
    let result = match op {
        "pauseDelivery" => engine.pause(ctx.clone(), id).await,
        "resumeDelivery" => engine.resume(ctx.clone(), id).await,
        "abandonDelivery" => {
            #[derive(Deserialize)]
            struct Abandon {
                reason: Option<String>,
            }
            let body: Abandon = serde_json::from_value(body_of(req).clone())
                .map_err(|_| Problem::validation(Vec::new()))?;
            engine.abandon(ctx.clone(), id, body.reason).await
        }
        _ => return Err(Problem::not_found()),
    };
    let delivery = result.map_err(module_error)?;
    let seq = written_seq(core, &ctx).await?;
    let body = serde_json::to_value(delivery).map_err(|_| Problem::internal())?;
    Ok(Reply::json(StatusCode::OK, body, seq))
}

/// #45：重试失败节点 → `202 Node`（节点状态仍为当前值）。
pub async fn retry_node(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    validate::body("EmptyRequest", body_of(req))?;
    let id: NodeId = parse_id(req, "node_id")?;
    let ctx = actor_ctx(req);
    let node = core
        .services()
        .engine
        .retry_node(ctx.clone(), id)
        .await
        .map_err(module_error)?;
    let seq = written_seq(core, &ctx).await?;
    let body = serde_json::to_value(node).map_err(|_| Problem::internal())?;
    Ok(Reply::json(StatusCode::ACCEPTED, body, seq))
}

#[derive(Deserialize)]
struct PostMessage {
    text: String,
    #[serde(default)]
    level_cap: Option<Level>,
    #[serde(default)]
    refs: Vec<Ref>,
}

/// #12：发消息 → `201 Message`，`X-Event-Seq` = `message.posted` 的 `seq`。
pub async fn post_message(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    validate::body("MessagePostRequest", body_of(req))?;
    let cnv: ConversationId = parse_id(req, "conversation_id")?;
    let body: PostMessage = serde_json::from_value(body_of(req).clone())
        .map_err(|_| Problem::validation(Vec::new()))?;
    let refs = body.refs.clone();
    let msg = NewUserMessage {
        text: body.text,
        // 手机档案带的 level_cap 被接受但忽略：由 conversation 使用项目默认上限（P-35）。
        level_cap: match req.auth.profile {
            ClientProfile::Mobile => None,
            ClientProfile::Desktop => body.level_cap,
        },
        refs: body.refs,
    };
    let ctx = actor_ctx(req);
    let message = core
        .services()
        .conversation
        .post_user_message(ctx.clone(), cnv, msg)
        .await
        .map_err(|error| conv_error(error, &refs))?;
    let seq = written_seq(core, &ctx).await?;
    let body = serde_json::to_value(message).map_err(|_| Problem::internal())?;
    Ok(Reply::json(StatusCode::CREATED, body, seq))
}

fn conv_error(error: ConvError, refs: &[Ref]) -> Problem {
    match error {
        ConvError::NotFound => Problem::not_found(),
        ConvError::UnresolvedRef { reference } => {
            let index = refs.iter().position(|r| *r == reference).unwrap_or(0);
            Problem::validation(vec![FieldError::new(
                format!("/refs/{index}"),
                // 契约的 errors[].code 枚举里没有专门的"解析不到"；取 enum（不在可解析集合内）。
                "enum",
                "reference does not resolve",
            )])
        }
        ConvError::InvalidRequest { pointer, .. } => {
            Problem::validation(vec![FieldError::new(pointer, "type", "value was rejected")])
        }
        ConvError::Forbidden => {
            Problem::forbidden_role(gantry_core::entities::DeviceRole::Operator)
        }
        ConvError::Store(StoreError::Unavailable { .. }) => unavailable(),
        ConvError::Store(_) => Problem::internal(),
    }
}
