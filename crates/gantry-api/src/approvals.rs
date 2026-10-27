//! 统一签名原语的服务端部分（api.md §6，本文 §7.10）：审批路由 #24–#29 与内部动作的
//! 带签名重发（改角色 #39、吊销 #40）。
//!
//! 验签、状态机、challenge / nonce、摘要一致性都在 `policy`（`ApprovalService`）；
//! 本模块负责 HTTP 外壳、前置对账（PR 类审批在 grant 前强制对账一次）、事务编排
//! （`consume_internal` 与 `pki` 的业务写同一 `WriteTx`）与错误映射（本文 §9）。

use gantry_core::canon::SubjectDigest;
use gantry_core::entities::{
    Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, Device, DevicePlatform, DeviceRole,
    DeviceStatus, Seq,
};
use gantry_core::events::{ApprovalDeniedData, EventData, Subject};
use gantry_core::ids::{ApprovalId, DeliveryId, DeviceId};
use gantry_core::ports::{ActorCtx, MismatchReason, PageQuery, PolicyError};
use gantry_pki::admin::RevokeOutcome;
use gantry_policy::service::{
    ApprovalQuery, ChallengeError, ConsumeError, ConsumeInternal, CurrentSubject, DeviceKey,
    GrantError, GrantRequest, InternalAction, InternalApprovalSpec,
};
use gantry_policy::signing::request_digest;
use http::header::HeaderName;
use http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::consts::PAGE_DEFAULT_LIMIT;
use crate::control::actor_ctx;
use crate::devices::device_error;
use crate::handlers::device_view;
use crate::problem::{Problem, ProblemCode};
use crate::reads::{unwrap_cursor, wrap_cursor};
use crate::server::{ApiCore, HandlerRequest, Reply};
use crate::validate;

pub const GANTRY_APPROVAL: HeaderName = HeaderName::from_static("gantry-approval");

fn unavailable() -> Problem {
    Problem::retry_after(ProblemCode::Unavailable, "store unavailable", 1)
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<Value, Problem> {
    serde_json::to_value(value).map_err(|_| Problem::internal())
}

fn status_str(status: &ApprovalStatus) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

fn path_id<T: std::str::FromStr>(req: &HandlerRequest<'_>, name: &str) -> Result<T, Problem> {
    req.param(name)
        .parse()
        .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "path parameter is invalid"))
}

fn body(req: &HandlerRequest<'_>) -> Value {
    req.body.clone().unwrap_or(Value::Null)
}

/// 写响应的 `X-Event-Seq`：所属模块在自己的事务里追加事件，api 拿不到提交结果；
/// 取该主体（审批 / 设备）最后一条事件的 `seq`，而不是提交后的全局 head——
/// 否则并发写入时会指向别的请求的事件（api.md §3.6）。没有事件时退回 head。
pub(crate) async fn subject_seq(core: &ApiCore, subject: &str) -> Result<Seq, Problem> {
    let events = core.services().store.events();
    if let Ok(subject) = Subject::parse(subject) {
        let found = events
            .by_subject(&subject)
            .await
            .map_err(|_| unavailable())?;
        if let Some(last) = found.last() {
            return Ok(last.seq);
        }
    }
    events.head().await.map_err(|_| unavailable())
}

async fn head(core: &ApiCore) -> Result<Seq, Problem> {
    core.services()
        .store
        .events()
        .head()
        .await
        .map_err(|_| unavailable())
}

fn policy_error(error: PolicyError) -> Problem {
    match error {
        PolicyError::NotFound => Problem::not_found(),
        PolicyError::InvalidRequest { pointer, .. } if pointer == "/cursor" => {
            Problem::new(ProblemCode::InvalidCursor, "cursor cannot be parsed")
        }
        PolicyError::InvalidRequest { pointer, .. } => {
            Problem::validation(vec![crate::problem::FieldError::new(
                pointer,
                "type",
                "value was rejected",
            )])
        }
        PolicyError::NotInternalAction { .. } => {
            Problem::validation(vec![crate::problem::FieldError::new(
                "/action",
                "enum",
                "clients may only request internal approvals",
            )])
        }
        PolicyError::Conflict { .. } => Problem::conflict("invalid_state"),
        PolicyError::Store(_) | PolicyError::ApprovalUnavailable { .. } => unavailable(),
        PolicyError::NotConfigured => Problem::internal(),
    }
}

fn not_pending(status: &ApprovalStatus) -> Problem {
    Problem::new(ProblemCode::ApprovalNotPending, "approval is not pending")
        .with_ext("approval_status", status_str(status))
}

fn reason_str<T: std::fmt::Debug>(reason: &T) -> String {
    // 契约的 reason 是 snake_case（expired / used / mismatch / none / not_granted …）
    let debug = format!("{reason:?}");
    let mut out = String::new();
    for (i, ch) in debug.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn challenge_error(error: ChallengeError) -> Problem {
    match error {
        ChallengeError::NotFound => Problem::not_found(),
        ChallengeError::Forbidden => Problem::forbidden_role(DeviceRole::Approver),
        ChallengeError::NotPending { status } => not_pending(&status),
        ChallengeError::Store(_) => unavailable(),
    }
}

fn grant_error(error: GrantError) -> Problem {
    match error {
        GrantError::NotFound => Problem::not_found(),
        GrantError::Forbidden => Problem::forbidden_role(DeviceRole::Approver),
        GrantError::NotPending { status } => not_pending(&status),
        GrantError::ChallengeInvalid { reason } => {
            Problem::new(ProblemCode::ChallengeInvalid, "challenge is invalid")
                .with_ext("reason", Value::String(reason_str(&reason)))
        }
        GrantError::SignatureInvalid => Problem::new(
            ProblemCode::SignatureInvalid,
            "approval signature is invalid",
        ),
        GrantError::StaleHead { current_head_sha } => Problem::new(
            ProblemCode::StaleHead,
            "pull request head moved; approval invalidated",
        )
        .with_ext(
            "current_head_sha",
            Value::String(current_head_sha.to_string()),
        )
        .with_ext("approval_status", json!("invalidated")),
        GrantError::Store(_) => unavailable(),
    }
}

pub(crate) fn consume_problem(error: ConsumeError) -> Problem {
    consume_error(error)
}

fn consume_error(error: ConsumeError) -> Problem {
    match error {
        ConsumeError::NotFound => mismatch(MismatchReason::NotGranted),
        ConsumeError::Mismatch { reason } => mismatch(reason),
        ConsumeError::Store(_) => unavailable(),
    }
}

fn mismatch(reason: MismatchReason) -> Problem {
    Problem::new(
        ProblemCode::ApprovalMismatch,
        "approval cannot be used for this write",
    )
    .with_ext("reason", Value::String(reason_str(&reason)))
}

/// grant / challenge / deny 所需的设备钥匙：角色、档案、状态与审批签名公钥实时读 pki（P-47）。
async fn device_key(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<DeviceKey, Problem> {
    let device = &req.auth.device;
    let verifying_key = core
        .services()
        .devices
        .verifying_key(&device.id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(|| Problem::forbidden_role(DeviceRole::Approver))?;
    let approval_pubkey_fpr = device
        .approval_pubkey_fpr
        .ok_or_else(|| Problem::forbidden_role(DeviceRole::Approver))?;
    Ok(DeviceKey {
        device_id: device.id,
        role: device.role,
        profile: req.auth.profile,
        status: device.status.clone(),
        approval_pubkey_fpr,
        verifying_key,
    })
}

// ---------------------------------------------------------------------------
// #24 #25 读
// ---------------------------------------------------------------------------

/// #24 `GET /v1/approvals`。审批数量很小：从 policy 取齐（其分页只有升序），
/// 在本层按契约的 `order`（默认 `desc`）做键集分页。
pub async fn list(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let q = req.query;
    let desc = q.str("order") != Some("asc");
    let tag = if desc {
        "approvals.desc"
    } else {
        "approvals.asc"
    };
    let after: Option<ApprovalId> = q
        .str("cursor")
        .map(|c| {
            unwrap_cursor(tag, c)?
                .parse()
                .map_err(|_| Problem::new(ProblemCode::InvalidCursor, "cursor cannot be parsed"))
        })
        .transpose()?;
    let filter = ApprovalQuery {
        status: q
            .str("status")
            .map(|s| serde_json::from_value(json!(s)))
            .transpose()
            .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "status is invalid"))?,
        delivery_id: q
            .str("delivery_id")
            .map(str::parse::<DeliveryId>)
            .transpose()
            .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "delivery_id is invalid"))?,
    };
    let seq = head(core).await?;
    let policy = &core.services().approvals;
    let mut all: Vec<Approval> = Vec::new();
    let mut cursor = None;
    loop {
        let page = policy
            .list(filter.clone(), PageQuery { cursor, limit: 200 })
            .await
            .map_err(policy_error)?;
        all.extend(page.items);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    if desc {
        all.reverse();
    }
    let limit = q
        .int("limit")
        .map(|v| v as usize)
        .unwrap_or(PAGE_DEFAULT_LIMIT as usize);
    let start = match after {
        None => 0,
        Some(after) => all
            .iter()
            .position(|a| if desc { a.id < after } else { a.id > after })
            .unwrap_or(all.len()),
    };
    let window: Vec<&Approval> = all[start..].iter().take(limit + 1).collect();
    let next = (window.len() > limit).then(|| wrap_cursor(tag, &window[limit - 1].id.to_string()));
    let items = window
        .iter()
        .take(limit)
        .map(|a| to_json(*a))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Reply::json(
        StatusCode::OK,
        json!({ "items": items, "next_cursor": next }),
        seq,
    ))
}

/// #25 `GET /v1/approvals/{approval_id}`。
pub async fn get(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let id: ApprovalId = path_id(req, "approval_id")?;
    let seq = head(core).await?;
    let approval = core
        .services()
        .approvals
        .get(&id)
        .await
        .map_err(policy_error)?
        .ok_or_else(Problem::not_found)?;
    Ok(Reply::json(StatusCode::OK, to_json(&approval)?, seq))
}

// ---------------------------------------------------------------------------
// #26 发起内部动作审批
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Create {
    action: ApprovalAction,
    target: ApprovalTarget,
    subject_digest: SubjectDigest,
}

/// #26 `POST /v1/approvals`：只接受三种内部动作（Forge 动作、条件伪动作、补丁闸门
/// 由契约 schema 拒绝为 `422`，P-54）；设备目标必须存在。
pub async fn create(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let raw = body(req);
    validate::body("ApprovalCreateRequest", &raw)?;
    let Create {
        action,
        target,
        subject_digest,
    } = serde_json::from_value(raw).map_err(|_| Problem::internal())?;
    if let ApprovalTarget::Device { id } = &target {
        let exists = core
            .services()
            .devices
            .get(id)
            .await
            .map_err(|_| unavailable())?
            .is_some();
        if !exists {
            return Err(Problem::not_found());
        }
    }
    let ctx = actor_ctx(req);
    // 业务已提交、幂等完成记录丢失后的同键重试：按 dedup_key 找回首次创建的审批
    // （本文 §7.6），而不是依赖"同主体仍 pending 才复用"。
    if let Some(existing) = recover_by_dedup_key(core, &ctx).await? {
        let seq = subject_seq(core, &existing.id.to_string()).await?;
        return Ok(Reply::json(StatusCode::CREATED, to_json(&existing)?, seq));
    }
    let approval = core
        .services()
        .approvals
        .create_internal(
            InternalApprovalSpec {
                action,
                target,
                subject_digest,
            },
            &ctx,
        )
        .await
        .map_err(policy_error)?;
    let seq = subject_seq(core, &approval.id.to_string()).await?;
    Ok(Reply::json(StatusCode::CREATED, to_json(&approval)?, seq))
}

async fn recover_by_dedup_key(core: &ApiCore, ctx: &ActorCtx) -> Result<Option<Approval>, Problem> {
    let Some(key) = &ctx.dedup_key else {
        return Ok(None);
    };
    let events = core.services().store.events();
    let Some(seq) = events.by_dedup_key(key).await.map_err(|_| unavailable())? else {
        return Ok(None);
    };
    let Some(event) = events.get_by_seq(seq).await.map_err(|_| unavailable())? else {
        return Ok(None);
    };
    let Ok(id) = event.subject.as_str().parse::<ApprovalId>() else {
        return Ok(None);
    };
    core.services()
        .approvals
        .get(&id)
        .await
        .map_err(policy_error)
}

// ---------------------------------------------------------------------------
// #27 #28 #29 challenge / grant / deny
// ---------------------------------------------------------------------------

/// #27 `GET /v1/approvals/{approval_id}/challenge`：同一设备有效期内重复请求得到同一份。
pub async fn challenge(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let id: ApprovalId = path_id(req, "approval_id")?;
    let key = device_key(core, req).await?;
    let challenge = core
        .services()
        .approvals
        .challenge(&id, &key)
        .await
        .map_err(challenge_error)?;
    let seq = head(core).await?;
    Ok(Reply::json(StatusCode::OK, to_json(&challenge)?, seq))
}

#[derive(Deserialize)]
struct Grant {
    nonce: String,
    signature: String,
}

/// #28 `POST /v1/approvals/{approval_id}/grant`：请求体只有 `{nonce, signature}`，
/// 客户端不能提交待签载荷；PR 类审批先强制对账一次。
pub async fn grant(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let raw = body(req);
    validate::body("ApprovalGrantRequest", &raw)?;
    let Grant { nonce, signature } =
        serde_json::from_value(raw).map_err(|_| Problem::internal())?;
    let id: ApprovalId = path_id(req, "approval_id")?;
    let key = device_key(core, req).await?;
    let ctx = actor_ctx(req);
    let svc = core.services();
    let current_subject = match svc.approvals.get(&id).await.map_err(policy_error)? {
        Some(Approval {
            target: ApprovalTarget::Pr { repo, number },
            ..
        }) => refreshed_head(core, &repo, number, &ctx).await,
        Some(_) => None,
        None => return Err(Problem::not_found()),
    };
    let approval = svc
        .approvals
        .grant(
            GrantRequest {
                approval_id: id,
                device: key,
                nonce,
                signature,
                current_subject,
            },
            &ctx,
        )
        .await
        .map_err(grant_error)?;
    let seq = subject_seq(core, &approval.id.to_string()).await?;
    Ok(Reply::json(StatusCode::OK, to_json(&approval)?, seq))
}

/// forge 不可用 / 被限流则不传，policy 退回投影快照（权威检查在消费点，本文 §9）。
async fn refreshed_head(
    core: &ApiCore,
    repo: &gantry_core::canon::RepoSlug,
    number: u64,
    ctx: &ActorCtx,
) -> Option<CurrentSubject> {
    let head = core
        .services()
        .forge
        .refresh_pr(repo, number, ctx)
        .await
        .ok()?;
    Some(CurrentSubject::PrHead {
        repo: repo.clone(),
        number,
        head_sha: head.sha,
        observed_at: core.services().clock.now(),
    })
}

#[derive(Deserialize)]
struct Deny {
    #[serde(default)]
    reason: Option<String>,
}

/// #29 `POST /v1/approvals/{approval_id}/deny`：approver，不需要签名。
pub async fn deny(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let raw = body(req);
    validate::body("ApprovalDenyRequest", &raw)?;
    let Deny { reason } = serde_json::from_value(raw).map_err(|_| Problem::internal())?;
    // 备注进 approval.denied 事件，按纯文本约束（无控制字符、无双向覆盖符）在边界校验，
    // 确定性的输入错误是 422，不能落成存储层的 503。
    let data = ApprovalDeniedData {
        reason: reason.clone(),
    };
    if data.validate().is_err() {
        return Err(Problem::validation(vec![crate::problem::FieldError::new(
            "/reason",
            "type",
            "text contains characters that are not allowed",
        )]));
    }
    let id: ApprovalId = path_id(req, "approval_id")?;
    let key = device_key(core, req).await?;
    let approval = core
        .services()
        .approvals
        .deny(&id, &key, reason, &actor_ctx(req))
        .await
        .map_err(grant_error)?;
    let seq = subject_seq(core, &approval.id.to_string()).await?;
    Ok(Reply::json(StatusCode::OK, to_json(&approval)?, seq))
}

// ---------------------------------------------------------------------------
// #39 #40 带签名的内部动作
// ---------------------------------------------------------------------------

/// 缺 `Gantry-Approval` → `403 signature_required` 并附 `signing`，供客户端发起审批。
fn signature_required(action: ApprovalAction, target: &ApprovalTarget, body: &Value) -> Problem {
    let digest = request_digest(body)
        .ok()
        .and_then(|d| serde_json::to_value(d).ok())
        .unwrap_or(Value::Null);
    Problem::new(
        ProblemCode::SignatureRequired,
        "this write requires a signed approval",
    )
    .with_ext(
        "signing",
        json!({
            "action": serde_json::to_value(action).unwrap_or(Value::Null),
            "target": serde_json::to_value(target).unwrap_or(Value::Null),
            "subject_digest": digest,
        }),
    )
}

fn approval_header(req: &HandlerRequest<'_>) -> Result<Option<ApprovalId>, Problem> {
    match req.headers.get(GANTRY_APPROVAL) {
        None => Ok(None),
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .map(Some)
            .ok_or_else(|| {
                Problem::new(
                    ProblemCode::InvalidRequest,
                    "Gantry-Approval is not an approval id",
                )
            }),
    }
}

async fn target_device(core: &ApiCore, id: &DeviceId) -> Result<Device, Problem> {
    core.services()
        .devices
        .get(id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(Problem::not_found)
}

/// 同一写事务：`policy.consume_internal` → `write`（pki）→ 提交；任一失败整体回滚，
/// 审批保持 `granted`（api.md §6.6）。返回业务结果与本事务最后一个事件的 `seq`。
async fn consume_and_write<T, F>(
    core: &ApiCore,
    req: &HandlerRequest<'_>,
    approval_id: ApprovalId,
    action: InternalAction,
    target: ApprovalTarget,
    raw: &Value,
    write: F,
) -> Result<(T, Seq), Problem>
where
    F: for<'t> FnOnce(
        &'t mut gantry_store::WriteTx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, Problem>> + Send + 't>,
    >,
{
    let svc = core.services();
    let body_digest = request_digest(raw).map_err(|_| Problem::internal())?;
    let mut tx = svc.store.begin_write().await.map_err(|_| unavailable())?;
    let consumed = svc
        .approvals
        .consume_internal(
            &mut tx,
            ConsumeInternal {
                approval_id,
                expected_action: action,
                expected_target: target,
                body_digest,
                rev_check: None,
                request_id: req.request_id.to_owned(),
                actor: actor_ctx(req).actor,
            },
        )
        .await;
    if let Err(error) = consumed {
        let _ = tx.rollback().await;
        return Err(consume_error(error));
    }
    match write(&mut tx).await {
        Ok(value) => {
            let committed = tx.commit().await.map_err(|_| unavailable())?;
            let seq = Seq::from_wire(committed.last_seq).map_err(|_| Problem::internal())?;
            Ok((value, seq))
        }
        Err(problem) => {
            let _ = tx.rollback().await;
            Err(problem)
        }
    }
}

#[derive(Deserialize)]
struct RoleChange {
    role: DeviceRole,
}

/// #39 `PUT /v1/devices/{device_id}/role`：approver + 签名；改自己、把 ios 设为
/// approver 在消费审批之前就拒绝（P-76）。新角色对该设备的下一个请求立即生效。
pub async fn set_role(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let raw = body(req);
    validate::body("RoleChangeRequest", &raw)?;
    let RoleChange { role } =
        serde_json::from_value(raw.clone()).map_err(|_| Problem::internal())?;
    let id: DeviceId = path_id(req, "device_id")?;
    let target = target_device(core, &id).await?;
    if id == req.auth.device.id {
        return Err(Problem::conflict("self_role_change"));
    }
    if target.platform == DevicePlatform::Ios && role == DeviceRole::Approver {
        return Err(Problem::conflict("mobile_cannot_approve"));
    }
    let approval_target = ApprovalTarget::Device { id };
    let Some(approval_id) = approval_header(req)? else {
        return Err(signature_required(
            ApprovalAction::DeviceRoleChange,
            &approval_target,
            &raw,
        ));
    };
    let admin = core.services().admin.clone();
    let actor = actor_ctx(req).actor;
    let (device, seq) = consume_and_write(
        core,
        req,
        approval_id,
        InternalAction::DeviceRoleChange,
        approval_target,
        &raw,
        move |tx| {
            Box::pin(async move {
                admin
                    .set_role(tx, &id, role, &actor)
                    .await
                    .map_err(device_error)
            })
        },
    )
    .await?;
    Ok(Reply::json(StatusCode::OK, device_view(&device), seq))
}

/// #40 `POST /v1/devices/{device_id}/revoke`：approver + 签名；已吊销则原样 `200`，
/// 先于签名校验、无副作用（本文 OQ-A3）。吊销提交后 M1 由事件流心跳断流。
pub async fn revoke(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let raw = body(req);
    validate::body("EmptyRequest", &raw)?;
    let id: DeviceId = path_id(req, "device_id")?;
    let target = target_device(core, &id).await?;
    if target.status == DeviceStatus::Revoked {
        let seq = head(core).await?;
        return Ok(Reply::json(StatusCode::OK, device_view(&target), seq));
    }
    let approval_target = ApprovalTarget::Device { id };
    let Some(approval_id) = approval_header(req)? else {
        return Err(signature_required(
            ApprovalAction::DeviceRevoke,
            &approval_target,
            &raw,
        ));
    };
    let admin = core.services().admin.clone();
    let actor = actor_ctx(req).actor;
    let (outcome, seq) = consume_and_write(
        core,
        req,
        approval_id,
        InternalAction::DeviceRevoke,
        approval_target,
        &raw,
        move |tx| {
            Box::pin(async move { admin.revoke(tx, &id, &actor).await.map_err(device_error) })
        },
    )
    .await?;
    let device = match outcome {
        RevokeOutcome::Revoked(d) | RevokeOutcome::AlreadyRevoked(d) => d,
    };
    Ok(Reply::json(StatusCode::OK, device_view(&device), seq))
}
