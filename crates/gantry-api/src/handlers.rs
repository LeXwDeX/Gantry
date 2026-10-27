//! 步骤 13 的内置处理程序。按切片逐步接入；尚未接入的 M1 路由暂按未启用处理
//! （`404 not_found`，与本文 §1.1 的未启用路由一致，不泄露"后续才有"）。

use async_trait::async_trait;
use gantry_core::entities::Device;
use gantry_core::time::Timestamp;
use http::StatusCode;
use serde_json::{json, Value};

use crate::approvals;
use crate::auth::profile_str;
use crate::consts::{API_VERSION, RENEW_AFTER_DEN, RENEW_AFTER_NUM};
use crate::control;
use crate::devices;
use crate::problem::Problem;
use crate::reads;
use crate::server::{ApiCore, HandlerRequest, Reply, RouteHandler};
use crate::settings;
use crate::sse;

pub struct Builtin;

#[async_trait]
impl RouteHandler for Builtin {
    async fn handle(&self, core: &ApiCore, req: HandlerRequest<'_>) -> Result<Reply, Problem> {
        match req.route.operation_id {
            "getMe" => get_me(core, &req).await,
            "pauseDelivery" | "resumeDelivery" | "abandonDelivery" => {
                control::delivery_control(core, &req).await
            }
            "retryNode" => control::retry_node(core, &req).await,
            "postMessage" => control::post_message(core, &req).await,
            "listMessages" => reads::list_messages(core, &req).await,
            "listInbox" => reads::list_inbox(core, &req).await,
            "getInboxItem" => reads::get_inbox(core, &req).await,
            "getInboxSummary" => reads::inbox_summary(core, &req).await,
            "streamEvents" => sse::stream_events(core, &req).await,
            "listDevices" => devices::list(core, &req).await,
            "getSettings" => settings::get(core, &req).await,
            "putSettings" => settings::put(core, &req).await,
            "getSettingsCatalog" => settings::catalog(core, &req).await,
            "getSettingsStatus" => settings::status(core, &req).await,
            "listApprovals" => approvals::list(core, &req).await,
            "getApproval" => approvals::get(core, &req).await,
            "createApproval" => approvals::create(core, &req).await,
            "getApprovalChallenge" => approvals::challenge(core, &req).await,
            "grantApproval" => approvals::grant(core, &req).await,
            "denyApproval" => approvals::deny(core, &req).await,
            "setDeviceRole" => approvals::set_role(core, &req).await,
            "revokeDevice" => approvals::revoke(core, &req).await,
            "renameDevice" => devices::rename(core, &req).await,
            "renewDeviceCertificate" => devices::renew(core, &req).await,
            _ => Err(Problem::not_found()),
        }
    }
}

/// 设备的线上表示：不含审批签名公钥原文（只给指纹，api.md §4.10）。
pub fn device_view(device: &Device) -> Value {
    let mut value = serde_json::to_value(device).expect("device serializes");
    if let Value::Object(map) = &mut value {
        map.remove("approval_pubkey");
    }
    value
}

/// `renew_after = not_before + 2/3 · 寿命`（剩余 1/3 时续期，P-64）。
pub fn renew_after(not_before: Timestamp, not_after: Timestamp) -> Timestamp {
    let life = not_after.as_millis() - not_before.as_millis();
    Timestamp::from_millis(not_before.as_millis() + life * RENEW_AFTER_NUM / RENEW_AFTER_DEN)
}

/// #2 `GET /v1/me`（api.md §4.2）。
async fn get_me(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let svc = core.services();
    let seq = svc.store.events().head().await.map_err(|_| {
        Problem::retry_after(
            crate::problem::ProblemCode::Unavailable,
            "store unavailable",
            1,
        )
    })?;
    let cert = &req.auth.cert;
    let body = json!({
        "device": device_view(&req.auth.device),
        "profile": profile_str(req.auth.profile),
        "certificate": {
            "not_after": cert.not_after.to_canonical_string(),
            "renew_after": renew_after(cert.not_before, cert.not_after).to_canonical_string(),
        },
        "server": {
            "api_version": API_VERSION,
            "version": core.config().server_version,
            "time": svc.clock.now().to_canonical_string(),
            "log_epoch": svc.store.log_identity().log_epoch,
        },
    });
    Ok(Reply::json(StatusCode::OK, body, seq))
}
