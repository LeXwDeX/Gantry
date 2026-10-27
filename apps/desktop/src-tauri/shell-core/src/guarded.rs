//! 受保护写入（desktop-shell.md §7.4.2、§8 S-2 / S-3，api.md §6.6，config.md §2.5）。
//!
//! 外壳持有请求体（含秘密 `value`），UI 只拿到 `pending_id` 与预览：
//!
//! 1. `prepare`：settings 先 `dry_run` 取 `requires_signature` / `signing`，并读当前值生成差异预览
//!    （秘密字段不含值）；设备动作在本地组装 `signing`。
//! 2. `submit`：不需要签名 → 直接写；需要签名 → 核对 `signing` 与本机待发请求体一致 →
//!    `POST /v1/approvals` → 本机是 approver 则 challenge → 自检 → 用户在场 → `grant` →
//!    原样重发原请求并加 `Gantry-Approval`（settings 同时带 `If-Match`）。
//! 3. 提交（成功或失败）与丢弃后请求体立即清零；[`live_secret_bodies`] 是供测试检查的内存钩子。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use gantry_core::canon::jcs_digest;
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::approval;
use crate::events::Phase;
use crate::ipc::{ShellError, ShellErrorCode};
use crate::routes::{self, PreparedRequest};
use crate::session::{Connected, Shell};

/// §6.3 `GuardedWritePreview.pending_id` 的有效期 [建议值]。
pub const PENDING_TTL_MS: i64 = 5 * 60 * 1000;

const SETTINGS_RESOURCES: [&str; 8] = [
    "models", "levels", "policies", "guardian", "sandbox", "egress", "notify", "github",
];

static LIVE_SECRET_BODIES: AtomicUsize = AtomicUsize::new(0);

/// 当前仍在内存里的待发请求体数（测试用的内存检查钩子，§12"`SecretString` 已清零"）。
pub fn live_secret_bodies() -> usize {
    LIVE_SECRET_BODIES.load(Ordering::SeqCst)
}

/// 待发请求体：只在外壳内存中短暂存在，释放时清零。`Debug` 不打印内容。
struct SecretBody(Zeroizing<Vec<u8>>);

impl SecretBody {
    fn new(bytes: Vec<u8>) -> Self {
        LIVE_SECRET_BODIES.fetch_add(1, Ordering::SeqCst);
        Self(Zeroizing::new(bytes))
    }
}

impl Drop for SecretBody {
    fn drop(&mut self) {
        // `Zeroizing` 在随后的字段析构里清零；计数在这里减。
        LIVE_SECRET_BODIES.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for SecretBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBody(..)")
    }
}

#[derive(Debug)]
struct PendingWrite {
    method: &'static str,
    path: String,
    body: Option<SecretBody>,
    if_match: Option<String>,
    /// 外壳对本机待发请求体自算的 `sha256:` + JCS 摘要。
    digest: String,
    /// 本地推出的签名目标（核对服务端 `signing.target` 用）。
    expected_target: Value,
    action: &'static str,
    requires_signature: bool,
    signing: Option<Value>,
    /// 已发起、尚未用掉的审批（他人签名时复用）。
    approval_id: Option<String>,
    created_at_ms: i64,
}

#[derive(Default)]
pub struct GuardedWrites {
    pending: Mutex<HashMap<String, PendingWrite>>,
}

fn quoted_rev(if_match: &str) -> Option<u64> {
    if_match.strip_prefix('"')?.strip_suffix('"')?.parse().ok()
}

/// 只接受这些键；多余或缺失的必需键都拒绝。
fn take_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ShellError> {
    args[key]
        .as_str()
        .ok_or_else(|| ShellError::bad_argument(key))
}

/// 列出 `value` 里形如 `SecretRef { secret_ref, value }` 的位置（JSON Pointer）。预览只标"将写入新值"。
fn secret_paths(value: &Value, base: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.contains_key("secret_ref") && map.contains_key("value") {
                out.push(base.to_string());
                return;
            }
            for (k, v) in map {
                secret_paths(v, &format!("{base}/{k}"), out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                secret_paths(v, &format!("{base}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// 新旧值不同的叶子位置；秘密子树整体算一处，且不展开。
fn changed_paths(old: &Value, new: &Value, base: &str, secrets: &[String], out: &mut Vec<String>) {
    if secrets.iter().any(|s| s == base) {
        out.push(base.to_string());
        return;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                changed_paths(
                    a.get(k).unwrap_or(&Value::Null),
                    b.get(k).unwrap_or(&Value::Null),
                    &format!("{base}/{k}"),
                    secrets,
                    out,
                );
            }
        }
        _ if old != new => out.push(base.to_string()),
        _ => {}
    }
}

fn digest_of(body: &Value) -> String {
    jcs_digest(body).expect("JSON 可规范化").to_string()
}

impl GuardedWrites {
    fn insert(&self, write: PendingWrite) -> Result<String, ShellError> {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes)
            .map_err(|_| ShellError::new(ShellErrorCode::KeyUnavailable, "随机数不可用"))?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, w| write.created_at_ms - w.created_at_ms <= PENDING_TTL_MS);
        pending.insert(id.clone(), write);
        Ok(id)
    }

    fn take(&self, id: &str, now_ms: i64) -> Result<PendingWrite, ShellError> {
        let w = self.pending.lock().unwrap().remove(id);
        match w {
            Some(w) if now_ms - w.created_at_ms <= PENDING_TTL_MS => Ok(w),
            _ => Err(ShellError::new(
                ShellErrorCode::PreparedExpired,
                "待定写入已过期",
            )),
        }
    }

    pub fn discard(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
    }

    pub fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
}

impl Shell {
    /// `guarded_write_prepare`（§7.4.2）。`args` 为 IPC 的扁平参数，按 `kind` 严格解析。
    pub async fn guarded_write_prepare(&self, args: &Value) -> Result<Value, ShellError> {
        let kind = take_str(args, "kind")?;
        let allowed: &[&str] = match kind {
            "settings_put" => &[
                "kind",
                "scope",
                "project_id",
                "resource",
                "value",
                "if_match",
            ],
            "settings_delete" => &["kind", "project_id", "resource", "if_match"],
            "device_role" => &["kind", "device_id", "role"],
            "device_revoke" => &["kind", "device_id"],
            _ => return Err(ShellError::bad_argument("kind")),
        };
        approval::exact_args(args, allowed)?;
        let c = self.connected()?;
        let now = self.server_now_ms_pub();
        let me = self.me_pub(&c).await?;
        let my_role = me["device"]["role"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let (write, mut preview, check) = match kind {
            "settings_put" | "settings_delete" => self.prepare_settings(kind, args, &c).await?,
            _ => prepare_device(kind, args, &c.identity.device_id)?,
        };
        let mut write = write;
        write.created_at_ms = now;
        let valid = check["valid"].as_bool().unwrap_or(false);
        let can_submit = if self.phase() != Phase::Live {
            json!({ "ok": false, "reason": "not_connected" })
        } else if !valid {
            json!({ "ok": false, "reason": "invalid" })
        } else if write.requires_signature && my_role != "approver" {
            // OQ-S7 临时处理：仍可发起审批，由另一台 approver 设备签名后再提交。
            json!({ "ok": false, "reason": "not_approver" })
        } else {
            json!({ "ok": true })
        };
        let requires_signature = write.requires_signature;
        let required_role = check["required_role"]
            .as_str()
            .unwrap_or(if requires_signature {
                "approver"
            } else {
                "operator"
            })
            .to_string();
        let pending_id = self.guarded().insert(write)?;
        preview["pending_id"] = json!(pending_id);
        Ok(json!({
            "pending_id": pending_id,
            "valid": valid,
            "errors": check["errors"].as_array().cloned().unwrap_or_default(),
            "warnings": check["warnings"].as_array().cloned().unwrap_or_default(),
            "relaxes": check["relaxes"].as_array().cloned().unwrap_or_default(),
            "required_role": required_role,
            "requires_signature": requires_signature,
            "preview": preview,
            "can_submit": can_submit,
        }))
    }

    async fn prepare_settings(
        &self,
        kind: &str,
        args: &Value,
        c: &Connected,
    ) -> Result<(PendingWrite, Value, Value), ShellError> {
        let resource = take_str(args, "resource")?;
        if !SETTINGS_RESOURCES.contains(&resource) {
            return Err(ShellError::bad_argument("resource"));
        }
        let if_match = take_str(args, "if_match")?;
        let base_rev = quoted_rev(if_match).ok_or_else(|| ShellError::bad_argument("if_match"))?;
        let scope = if kind == "settings_delete" {
            "project"
        } else {
            take_str(args, "scope")?
        };
        let project_id = match (scope, args.get("project_id")) {
            ("global", None) => None,
            ("project", Some(Value::String(p)))
                if p.parse::<gantry_core::ids::ProjectId>().is_ok() =>
            {
                Some(p.clone())
            }
            ("global" | "project", _) => return Err(ShellError::bad_argument("project_id")),
            _ => return Err(ShellError::bad_argument("scope")),
        };
        let path = match &project_id {
            Some(p) => format!("/v1/projects/{p}/settings/{resource}"),
            None => format!("/v1/settings/{resource}"),
        };
        let mut target = json!({ "kind": "settings", "scope": scope, "resource": resource, "base_rev": base_rev });
        if let Some(p) = &project_id {
            target["project_id"] = json!(p);
        }

        if kind == "settings_delete" {
            // `DELETE` 没有 dry_run（OQ-S11）：先按"无需签名"预览，提交时遇 `403 signature_required` 再转签名链。
            let write = PendingWrite {
                method: "DELETE",
                path,
                body: None,
                if_match: Some(if_match.to_string()),
                digest: digest_of(&json!({})),
                expected_target: target,
                action: "settings_update",
                requires_signature: false,
                signing: None,
                approval_id: None,
                created_at_ms: 0,
            };
            let preview = json!({ "kind": kind, "scope": scope, "project_id": project_id, "resource": resource, "remove_override": true });
            return Ok((write, preview, json!({ "valid": true })));
        }

        let value = args
            .get("value")
            .ok_or_else(|| ShellError::bad_argument("value"))?;
        let body = json!({ "value": value });
        let bytes = serde_json::to_vec(&body).expect("可序列化");
        if bytes.len() > routes::MAX_BODY_BYTES {
            return Err(ShellError::new(
                ShellErrorCode::TooLarge,
                "请求体超过 64 KiB",
            ));
        }
        let digest = digest_of(&body);
        let resp = c
            .client
            .send(
                "PUT",
                &format!("{path}?dry_run=true"),
                Some(&bytes),
                &[("if-match", if_match)],
            )
            .await
            .map_err(|_| ShellError::new(ShellErrorCode::NotConnected, "连接中断"))?;
        let check: Value = match resp.status {
            200 => serde_json::from_slice(&resp.body).unwrap_or(Value::Null),
            // 校验失败按 `valid = false` 展示；`detail` 不过 IPC。
            _ => {
                let p: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                json!({ "valid": false, "errors": p.get("errors").cloned().unwrap_or_else(|| json!([{ "pointer": "", "code": p["code"] }])) })
            }
        };
        let current = self
            .get_json_pub(c, &path)
            .await
            .ok()
            .map(|(_, v)| v["value"].clone())
            .unwrap_or(Value::Null);
        let mut secrets = Vec::new();
        secret_paths(value, "", &mut secrets);
        let mut changed = Vec::new();
        changed_paths(&current, value, "", &secrets, &mut changed);
        let requires_signature = check["requires_signature"].as_bool().unwrap_or(false);
        let write = PendingWrite {
            method: "PUT",
            path,
            body: Some(SecretBody::new(bytes)),
            if_match: Some(if_match.to_string()),
            digest,
            expected_target: target,
            action: "settings_update",
            requires_signature,
            signing: check.get("signing").cloned(),
            approval_id: None,
            created_at_ms: 0,
        };
        let preview = json!({
            "kind": kind, "scope": scope, "project_id": project_id, "resource": resource, "base_rev": base_rev,
            "changed_paths": changed,
            // 秘密字段只告知"将写入新值"，不含任何值（S-2）。
            "secret_paths": secrets.iter().map(|p| json!({ "pointer": p, "note": "将写入新值（不显示）" })).collect::<Vec<_>>(),
        });
        Ok((write, preview, check))
    }

    /// `guarded_write_submit`（api.md §6.6）。无论结果如何，请求体都随之清零。
    pub async fn guarded_write_submit(&self, pending_id: &str) -> Result<Value, ShellError> {
        let c = self.connected()?;
        let mut write = self.guarded().take(pending_id, self.server_now_ms_pub())?;
        if self.phase() != Phase::Live {
            return Err(ShellError::new(ShellErrorCode::NotConnected, "连接中断"));
        }
        if !write.requires_signature && write.approval_id.is_none() {
            let resp = self.send_write(&c, &write, None).await?;
            // DELETE 没有 dry_run：服务端要求签名时才转签名链（OQ-S11）。
            if resp.status == 403 {
                let p: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                if p["code"] == "signature_required" && p.get("signing").is_some() {
                    write.signing = p.get("signing").cloned();
                    write.requires_signature = true;
                    return self.signing_chain(&c, write, pending_id).await;
                }
            }
            return Ok(routes::shape_response(
                resp.status,
                &resp.headers,
                &resp.body,
            ));
        }
        self.signing_chain(&c, write, pending_id).await
    }

    async fn signing_chain(
        &self,
        c: &Connected,
        mut write: PendingWrite,
        pending_id: &str,
    ) -> Result<Value, ShellError> {
        let signing = write
            .signing
            .clone()
            .ok_or_else(|| approval_self_check("internal_digest_mismatch"))?;
        // 内部动作的额外自检（§7.4.2）：动作、目标、摘要都与本机待发请求一致。
        if signing["action"] != write.action
            || signing["target"] != write.expected_target
            || signing["subject_digest"].as_str() != Some(write.digest.as_str())
        {
            return Err(approval_self_check("internal_digest_mismatch"));
        }
        let approval = match &write.approval_id {
            Some(id) => {
                self.get_json_pub(c, &format!("/v1/approvals/{id}"))
                    .await?
                    .1
            }
            None => {
                let body = serde_json::to_vec(&json!({
                    "action": signing["action"], "target": signing["target"], "subject_digest": signing["subject_digest"],
                }))
                .expect("可序列化");
                let req = PreparedRequest {
                    operation_id: "createApproval",
                    method: "POST",
                    path_and_query: "/v1/approvals".into(),
                    body: Some(body),
                    idempotency_key: Some(format!("gw_{pending_id}")),
                };
                let resp = c
                    .client
                    .send_prepared(&req)
                    .await
                    .map_err(|_| ShellError::new(ShellErrorCode::NotConnected, "连接中断"))?;
                if resp.status != 201 && resp.status != 200 {
                    return Ok(routes::shape_response(
                        resp.status,
                        &resp.headers,
                        &resp.body,
                    ));
                }
                serde_json::from_slice(&resp.body).unwrap_or(Value::Null)
            }
        };
        let approval_id = approval["id"].as_str().unwrap_or_default().to_string();
        write.approval_id = Some(approval_id.clone());

        if approval["status"] == "pending" {
            let me = self.me_pub(c).await?;
            if me["device"]["role"] != "approver" {
                // 本机不能签：保留待定写入，等另一台 approver 设备批准后再提交（OQ-S7）。
                let pending = json!({ "ok": true, "status": 201, "headers": {}, "body": approval });
                self.guarded()
                    .pending
                    .lock()
                    .unwrap()
                    .insert(pending_id.to_string(), write);
                return Ok(pending);
            }
            let (status, challenge) = self
                .get_json_pub(c, &format!("/v1/approvals/{approval_id}/challenge"))
                .await?;
            if status != 200 {
                return Err(ShellError::new(
                    ShellErrorCode::NotConnected,
                    "取不到签名 challenge",
                ));
            }
            let checked = approval::self_check(
                &approval_id,
                &challenge,
                &approval,
                &c.identity.device_id,
                self.server_now_ms_pub(),
                Some(&write.digest),
            )?;
            let prepared_id = self.signer().insert(checked, self.server_now_ms_pub())?;
            let grant = self.approval_sign(&prepared_id).await?;
            if grant["ok"] != true || grant["body"]["status"] != "granted" {
                return Ok(grant);
            }
        } else if approval["status"] != "granted" {
            return Ok(json!({ "ok": true, "status": 200, "headers": {}, "body": approval }));
        }
        let resp = self.send_write(c, &write, Some(&approval_id)).await?;
        Ok(routes::shape_response(
            resp.status,
            &resp.headers,
            &resp.body,
        ))
    }

    async fn send_write(
        &self,
        c: &Connected,
        write: &PendingWrite,
        approval_id: Option<&str>,
    ) -> Result<crate::http::HttpResponse, ShellError> {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(m) = &write.if_match {
            headers.push(("if-match", m));
        }
        if let Some(a) = approval_id {
            headers.push(("gantry-approval", a));
        }
        let empty = b"{}".to_vec();
        let body: &[u8] = match &write.body {
            Some(b) => &b.0,
            None if write.method == "DELETE" => &[],
            None => &empty,
        };
        let body = (!body.is_empty()).then_some(body);
        let resp = c
            .client
            .send(write.method, &write.path, body, &headers)
            .await
            .map_err(|_| ShellError::new(ShellErrorCode::NotConnected, "连接中断"))?;
        if let Some(epoch) = resp.log_epoch() {
            self.observe_log_epoch(epoch)?;
        }
        Ok(resp)
    }

    pub fn guarded_write_discard(&self, pending_id: &str) {
        self.guarded().discard(pending_id);
    }
}

fn approval_self_check(reason: &str) -> ShellError {
    ShellError {
        reason: Some(reason.into()),
        ..ShellError::new(
            ShellErrorCode::SelfCheckFailed,
            "服务端数据不一致，无法签名",
        )
    }
}

/// 设备动作：本地组装 `signing`（`subject_digest` = JCS 请求体摘要，§6.6）。
fn prepare_device(
    kind: &str,
    args: &Value,
    my_device: &str,
) -> Result<(PendingWrite, Value, Value), ShellError> {
    let device_id = take_str(args, "device_id")?;
    device_id
        .parse::<gantry_core::ids::DeviceId>()
        .map_err(|_| ShellError::bad_argument("device_id"))?;
    let target = json!({ "kind": "device", "id": device_id });
    let (body, action, path, method) = if kind == "device_role" {
        let role = take_str(args, "role")?;
        if !["viewer", "operator", "approver"].contains(&role) {
            return Err(ShellError::bad_argument("role"));
        }
        (
            json!({ "role": role }),
            "device_role_change",
            format!("/v1/devices/{device_id}/role"),
            "PUT",
        )
    } else {
        (
            json!({}),
            "device_revoke",
            format!("/v1/devices/{device_id}/revoke"),
            "POST",
        )
    };
    let digest = digest_of(&body);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if device_id == my_device {
        if kind == "device_role" {
            errors.push(json!({ "pointer": "/device_id", "code": "self_role_change" }));
        } else {
            warnings.push(json!({ "pointer": "/device_id", "code": "self_revoke" }));
        }
    }
    let signing = json!({ "action": action, "target": target, "subject_digest": digest });
    let preview = json!({ "kind": kind, "device_id": device_id, "role": body.get("role") });
    let write = PendingWrite {
        method,
        path,
        body: Some(SecretBody::new(
            serde_json::to_vec(&body).expect("可序列化"),
        )),
        if_match: None,
        digest,
        expected_target: target,
        action,
        requires_signature: true,
        signing: Some(signing),
        approval_id: None,
        created_at_ms: 0,
    };
    let check = json!({
        "valid": errors.is_empty(), "errors": errors, "warnings": warnings,
        "required_role": "approver", "requires_signature": true,
    });
    Ok((write, preview, check))
}
