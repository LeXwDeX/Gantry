//! 设备路由（api.md §4.10）：列表 #37、改名 #38、证书续期 #47（P-64）。
//!
//! 业务在 `pki`（`DeviceDirectory` / `DeviceAdmin`）；本模块只做 HTTP 外壳与错误映射
//! （本文 §9）。改角色 #39 与吊销 #40 需要审批签名，随审批切片接入。

use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::DeviceId;
use gantry_pki::admin::{DeviceError, RenewError, RenewRequest};
use gantry_pki::pairing_service::FieldError as PkiFieldError;
use gantry_pki::CertSerial;
use gantry_store::{Order, Page, PageCursor};
use http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::consts::PAGE_DEFAULT_LIMIT;
use crate::handlers::device_view;
use crate::problem::{FieldError, Problem, ProblemCode};
use crate::reads::{unwrap_cursor, wrap_cursor};
use crate::server::{ApiCore, HandlerRequest, Reply};
use crate::validate;

const TAG: &str = "devices";

fn unavailable() -> Problem {
    Problem::retry_after(ProblemCode::Unavailable, "store unavailable", 1)
}

/// pki 的字段错误码 → 契约 `errors[].code`（api.md §3.1 只允许 `required` / `type` /
/// `enum` / `range` / `unknown_field` 与 settings 语义码）：格式不对是 `type`，
/// 取值不被允许（如复用旧密钥 `duplicate_key`）是 `enum`。
pub fn field_errors(errors: Vec<PkiFieldError>) -> Problem {
    Problem::validation(
        errors
            .into_iter()
            .map(|e| {
                let code = match e.code {
                    "required" | "type" | "enum" | "range" | "unknown_field" => e.code,
                    "invalid" => "type",
                    _ => "enum",
                };
                FieldError::new(e.pointer, code, "value was rejected")
            })
            .collect(),
    )
}

pub fn device_error(error: DeviceError) -> Problem {
    match error {
        DeviceError::NotFound => Problem::not_found(),
        DeviceError::SelfRoleChange => Problem::conflict("self_role_change"),
        DeviceError::MobileCannotApprove => Problem::conflict("mobile_cannot_approve"),
        DeviceError::Revoked => Problem::conflict("invalid_state"),
        DeviceError::Validation(errors) => field_errors(errors),
        // 吊销日志写失败：吊销不提交，审批保持 granted（本文 §9）
        DeviceError::Journal(_) => unavailable(),
        DeviceError::Internal(_) => Problem::internal(),
    }
}

fn actor(req: &HandlerRequest<'_>) -> Actor {
    Actor::new(ActorKind::Device, req.auth.device.id.to_string())
}

async fn head(core: &ApiCore) -> Result<gantry_core::entities::Seq, Problem> {
    core.services()
        .store
        .events()
        .head()
        .await
        .map_err(|_| unavailable())
}

/// #37 `GET /v1/devices`：含已吊销，按设备 ID 键集分页；不返回审批公钥原文。
pub async fn list(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let q = req.query;
    let page = Page {
        limit: q
            .int("limit")
            .map(|v| v as u32)
            .unwrap_or(PAGE_DEFAULT_LIMIT),
        cursor: q
            .str("cursor")
            .map(|c| unwrap_cursor(TAG, c).map(PageCursor::new))
            .transpose()?,
        order: match q.str("order") {
            Some("asc") => Order::Asc,
            _ => Order::Desc,
        },
    };
    let seq = head(core).await?;
    let paged = core
        .services()
        .devices
        .list(&page)
        .await
        .map_err(|e| match e {
            // pki 把 store 错误转成文本；游标解析失败是唯一的客户端错误。
            gantry_pki::PkiError::Store(msg) if msg.contains("cursor") => {
                Problem::new(ProblemCode::InvalidCursor, "cursor cannot be parsed")
            }
            _ => unavailable(),
        })?;
    let items: Vec<Value> = paged.items.iter().map(device_view).collect();
    let next = paged.next_cursor.map(|c| wrap_cursor(TAG, c.as_str()));
    Ok(Reply::json(
        StatusCode::OK,
        json!({ "items": items, "next_cursor": next }),
        seq,
    ))
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

/// #38 `PATCH /v1/devices/{device_id}`：改名，事件 `device.updated` 由 `pki` 追加。
pub async fn rename(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let body = req.body.clone().unwrap_or(Value::Null);
    validate::body("RenameDeviceRequest", &body)?;
    let Rename { name } = serde_json::from_value(body).map_err(|_| Problem::internal())?;
    let id: DeviceId = req
        .param("device_id")
        .parse()
        .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "device_id is invalid"))?;
    let device = core
        .services()
        .admin
        .rename(&id, &name, &actor(req))
        .await
        .map_err(device_error)?;
    // device.updated 由 pki 在自己的事务里追加：取该设备最后一条事件，不取全局 head
    let seq = crate::approvals::subject_seq(core, &id.to_string()).await?;
    Ok(Reply::json(StatusCode::OK, device_view(&device), seq))
}

#[derive(Deserialize)]
struct Renew {
    csr: String,
}

/// #47 `POST /v1/devices/self/renew`：把本次握手出示的序列号一并交给 `pki`（P-64）。
pub async fn renew(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let body = req.body.clone().unwrap_or(Value::Null);
    validate::body("DeviceRenewRequest", &body)?;
    let Renew { csr } = serde_json::from_value(body).map_err(|_| Problem::internal())?;
    let presented: CertSerial = req
        .auth
        .cert
        .serial
        .parse()
        .map_err(|_| Problem::new(ProblemCode::DeviceRevoked, "device is not active"))?;
    let renewed = core
        .services()
        .admin
        .renew(
            &req.auth.device.id,
            &presented,
            RenewRequest { csr_pem: csr },
        )
        .await
        .map_err(|e| match e {
            RenewError::Validation(errors) => field_errors(errors),
            RenewError::Revoked => Problem::new(ProblemCode::DeviceRevoked, "device is not active"),
            RenewError::Internal(_) => Problem::internal(),
        })?;
    let seq = head(core).await?;
    let body = json!({
        "certificate_chain": renewed.certificate_chain,
        "not_after": renewed.not_after.to_canonical_string(),
    });
    Ok(Reply::json(StatusCode::OK, body, seq))
}
