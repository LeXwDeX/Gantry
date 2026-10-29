//! 审批签名（desktop-shell.md §7.4.1、§8 S-3 / S-4 / S-9 / S-12，api.md §6.2–§6.5，P-47、C27）。
//!
//! - [`self_check`]：展示前自检（§7.4.1 第 4 步）。签名字节与展示模型出自**同一个已解析的 payload**：
//!   用 core 的 `ApprovalPayload` 严格解析并生成 `"gantry/approval/v1\n" ‖ JCS(payload)`，同时要求
//!   原始 payload 的 JCS 与之逐字节相同（不能有被类型化丢掉的字段）。
//! - [`Signer`]：保存 [`PreparedSigning`]（随机、不含语义的 `prepared_id`），一次只允许一个确认框，
//!   每个 `prepared_id` 只签一次；WebView 只能传句柄，没有任何"待签内容"参数。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gantry_core::canon::{jcs, jcs_digest, ApprovalPayload, APPROVAL_SIGNING_PREFIX};
use serde_json::{json, Value};

use crate::ipc::{ShellError, ShellErrorCode};
use crate::keys::{DeviceKeys, KeyError, P1363Sig, PresencePrompt};

/// §10 `signing.min_remaining_ms`：challenge 剩余不足此值不弹确认。
pub const MIN_REMAINING_MS: i64 = 10_000;
/// §10 `signing.max_prepared`。
pub const MAX_PREPARED: usize = 8;

/// P-54 的已知动作。不认识的动作拒绝签名并提示升级客户端（api.md §6.5）。
const KNOWN_ACTIONS: [&str; 8] = [
    "merge_pr",
    "force_push",
    "push_outside_prefix",
    "resolve_review_thread_human",
    "patch_apply_protected",
    "settings_update",
    "device_role_change",
    "device_revoke",
];

fn self_check_failed(reason: &str) -> ShellError {
    ShellError {
        reason: Some(reason.into()),
        ..ShellError::new(
            ShellErrorCode::SelfCheckFailed,
            "服务端数据不一致，无法签名",
        )
    }
}

/// IPC 参数必须恰好是这些键（Tauri 默认忽略多余参数；`approval_sign` 等命令要拒绝夹带的
/// `payload` / `signature` / `nonce` / `subject_digest`，§12）。
pub fn exact_args(args: &Value, allowed: &[&str]) -> Result<(), ShellError> {
    let object = args
        .as_object()
        .ok_or_else(|| ShellError::bad_argument("args"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ShellError::bad_argument(key.clone()));
        }
    }
    Ok(())
}

/// 自检通过的结果：签名字节与已解析的 payload。
#[derive(Clone, Debug)]
pub struct Checked {
    pub payload: ApprovalPayload,
    /// 原始 payload（展示模型据此生成，与 `payload` 等价）。
    pub raw: Value,
    pub bytes: Vec<u8>,
}

/// 展示前自检（§7.4.1 第 4 步的表，按表中顺序）。
///
/// - `requested_id`：UI 请求的审批 ID；`approval`：刚 `GET /v1/approvals/{id}` 得到的 `Approval`；
/// - `now_ms`：已用 `/v1/me` 的 `server.time` 校正过的当前时间；
/// - `internal_digest`：内部动作时外壳对本机待发请求体自算的 `subject_digest`（§7.4.2）。
pub fn self_check(
    requested_id: &str,
    challenge: &Value,
    approval: &Value,
    device_id: &str,
    now_ms: i64,
    internal_digest: Option<&str>,
) -> Result<Checked, ShellError> {
    if challenge["approval_id"] != requested_id {
        return Err(self_check_failed("approval_id_mismatch"));
    }
    let raw = challenge["payload"].clone();
    if raw["approval_id"] != requested_id {
        return Err(self_check_failed("approval_id_mismatch"));
    }
    if raw["device_id"] != device_id {
        return Err(self_check_failed("device_id_mismatch"));
    }
    if raw["v"] != 1 {
        return Err(self_check_failed("unknown_version"));
    }
    let signing = &challenge["signing"];
    if signing["alg"] != "ES256"
        || signing["canonicalization"] != "JCS"
        || signing["prefix"].as_str().map(str::as_bytes) != Some(APPROVAL_SIGNING_PREFIX)
    {
        return Err(self_check_failed("unsupported_signing"));
    }
    let action = raw["action"].as_str().unwrap_or_default();
    if !KNOWN_ACTIONS.contains(&action) {
        return Err(self_check_failed("unknown_action"));
    }
    // 严格解析；形状不对（target 与动作不符、ID 前缀错、时间格式错）也算数据不一致。
    let payload: ApprovalPayload = serde_json::from_value(raw.clone())
        .map_err(|_| self_check_failed("approval_data_mismatch"))?;
    let bytes = payload
        .signing_bytes()
        .map_err(|_| self_check_failed("approval_data_mismatch"))?;
    // 原始 payload 与类型化后的规范化字节必须完全相同：多出的、被丢掉的字段都不允许。
    let raw_canonical = jcs(&raw).map_err(|_| self_check_failed("approval_data_mismatch"))?;
    if raw_canonical != bytes[APPROVAL_SIGNING_PREFIX.len()..] {
        return Err(self_check_failed("approval_data_mismatch"));
    }
    let expires_ms = payload.expires_at.as_millis();
    if expires_ms - now_ms < MIN_REMAINING_MS {
        return Err(self_check_failed("challenge_expired"));
    }
    for key in ["action", "target", "subject_digest"] {
        if raw[key] != approval[key] {
            return Err(self_check_failed("approval_data_mismatch"));
        }
    }
    // 内部动作没有 delivery_id：两边都缺省（null）才算一致。
    if raw.get("delivery_id").unwrap_or(&Value::Null)
        != approval.get("delivery_id").unwrap_or(&Value::Null)
    {
        return Err(self_check_failed("approval_data_mismatch"));
    }
    let internal = matches!(
        action,
        "settings_update" | "device_role_change" | "device_revoke"
    );
    if internal && internal_digest != raw["subject_digest"].as_str() {
        return Err(self_check_failed("internal_digest_mismatch"));
    }
    Ok(Checked {
        payload,
        raw,
        bytes,
    })
}

// ---- 展示 ----

fn action_label(action: &str) -> (&'static str, &'static str) {
    // （本地化名，提示里的简称）
    match action {
        "merge_pr" => ("合并 PR", "merge"),
        "force_push" => ("强制推送", "force-push"),
        "push_outside_prefix" => ("推送到前缀外分支", "push"),
        "resolve_review_thread_human" => ("解决人工评审线程", "resolve-thread"),
        "patch_apply_protected" => ("应用受保护路径补丁", "apply-patch"),
        "settings_update" => ("修改设置", "settings"),
        "device_role_change" => ("修改设备角色", "set-role"),
        "device_revoke" => ("吊销设备", "revoke"),
        _ => ("未知动作", "unknown"),
    }
}

/// （展示用完整标签，提示里的简短目标）。只用 payload 里的字段，不含任何不可信文本。
fn target_labels(target: &Value) -> (String, String) {
    let s = |k: &str| target[k].as_str().unwrap_or_default().to_string();
    let n = target["number"].as_u64().unwrap_or_default();
    match target["kind"].as_str().unwrap_or_default() {
        "pr" => (
            format!("{} · PR #{n}", s("repo")),
            format!("{}#{n}", s("repo")),
        ),
        "branch" => (
            format!("{} · 分支 {}", s("repo"), s("branch")),
            format!("{}:{}", s("repo"), s("branch")),
        ),
        "review_thread" => (
            format!("{} · PR #{n} · 评审线程 {}", s("repo"), s("thread_node_id")),
            format!("{}#{n}", s("repo")),
        ),
        "patch" => (
            format!("{} · 补丁（尝试 {}）", s("repo"), s("attempt_id")),
            format!("{}:patch", s("repo")),
        ),
        "settings" => {
            let scope = match target["project_id"].as_str() {
                Some(p) => format!("项目 {p}"),
                None => "全局".into(),
            };
            (
                format!(
                    "设置 · {scope} · {} · rev {}",
                    s("resource"),
                    target["base_rev"]
                ),
                format!("settings/{}", s("resource")),
            )
        }
        "device" => (format!("设备 {}", s("id")), s("id")),
        _ => (String::new(), String::new()),
    }
}

/// `subject_digest` 前 12 位（C27）：git SHA 取前 12 位十六进制；`sha256:` 形式跳过前缀。
pub fn digest_head12(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    hex.chars().take(12).collect()
}

/// 系统用户在场提示（§7.4.1 `approval_sign` 第 4 步），如 `merge acme/web#12 @ 9f2c4e7a1b3d`。
pub fn presence_prompt(raw: &Value) -> PresencePrompt {
    let action = raw["action"].as_str().unwrap_or_default();
    let (label, short) = action_label(action);
    let (full, target_short) = target_labels(&raw["target"]);
    let digest = raw["subject_digest"].as_str().unwrap_or_default();
    PresencePrompt {
        title: format!("Gantry：{label}"),
        summary: format!("{short} {target_short} @ {}", digest_head12(digest)),
        detail: vec![
            ("目标".into(), full),
            ("subject_digest".into(), digest.to_string()),
        ],
    }
}

/// `ApprovalSheetModel` 的已签名区与链接（desktop-shell.md §6.3）。未签名区由调用方另行填入。
pub fn sheet_model(
    prepared_id: &str,
    checked: &Checked,
    device_name: &str,
    can_sign: Value,
    server_time_offset_ms: i64,
    unsigned: Value,
) -> Value {
    let raw = &checked.raw;
    let action = raw["action"].as_str().unwrap_or_default();
    let digest = raw["subject_digest"].as_str().unwrap_or_default();
    let (target_label, _) = target_labels(&raw["target"]);
    let kind = raw["target"]["kind"].as_str().unwrap_or_default();
    let is_git_sha = digest.len() == 40 && !digest.starts_with("sha256:");
    let mut links = serde_json::Map::new();
    if is_git_sha && raw["target"].get("repo").is_some() {
        links.insert("commit".into(), json!(true));
    }
    if matches!(kind, "pr" | "review_thread") {
        links.insert("pr".into(), json!(true));
    }
    json!({
        "prepared_id": prepared_id,
        "signed": {
            "action": action,
            "action_label": action_label(action).0,
            "target": raw["target"],
            "target_label": target_label,
            "subject_digest": digest,
            "digest_head12": digest_head12(digest),
            "expires_at": raw["expires_at"],
            "device_id": raw["device_id"],
            "device_name": device_name,
            "delivery_id": raw.get("delivery_id").cloned().unwrap_or(Value::Null),
            "tech": { "approval_id": raw["approval_id"], "nonce": raw["nonce"], "v": raw["v"] },
        },
        "unsigned": unsigned,
        "links": links,
        "can_sign": can_sign,
        "challenge_expires_at": raw["expires_at"],
        "server_time_offset_ms": server_time_offset_ms,
    })
}

/// 不能签名时的只读展示（非 `pending`、非 approver）：只用 `Approval` 本身的字段，不取 challenge。
pub fn readonly_model(
    approval: &Value,
    device_id: &str,
    device_name: &str,
    reason: &str,
    server_time_offset_ms: i64,
    unsigned: Value,
) -> Value {
    let action = approval["action"].as_str().unwrap_or_default();
    let digest = approval["subject_digest"].as_str().unwrap_or_default();
    let (target_label, _) = target_labels(&approval["target"]);
    json!({
        "prepared_id": "",
        "signed": {
            "action": action,
            "action_label": action_label(action).0,
            "target": approval["target"],
            "target_label": target_label,
            "subject_digest": digest,
            "digest_head12": digest_head12(digest),
            "expires_at": approval["expires_at"],
            "device_id": device_id,
            "device_name": device_name,
            "delivery_id": approval.get("delivery_id").cloned().unwrap_or(Value::Null),
            "tech": { "approval_id": approval["id"], "nonce": "", "v": 1 },
        },
        "unsigned": unsigned,
        "links": {},
        "can_sign": { "ok": false, "reason": reason },
        "challenge_expires_at": approval["expires_at"],
        "server_time_offset_ms": server_time_offset_ms,
    })
}

/// 跨设备签内部动作（OQ-S7）：本机没有待发请求体时，外壳能自行核对的摘要。
/// `device_revoke` 的请求体是常量 `{}`；`device_role_change` 对三个候选角色各算一次；
/// `settings_update` 无法核对内容，返回 `None`（自检随之以 `internal_digest_mismatch` 拒签）。
pub fn cross_device_internal_digest(approval: &Value) -> Option<String> {
    let digest = |v: Value| jcs_digest(&v).ok().map(|d| d.to_string());
    match approval["action"].as_str()? {
        "device_revoke" => digest(json!({})),
        "device_role_change" => ["viewer", "operator", "approver"]
            .into_iter()
            .filter_map(|role| digest(json!({ "role": role })))
            .find(|d| approval["subject_digest"].as_str() == Some(d.as_str())),
        _ => None,
    }
}

// ---- 签名 ----

/// 外壳内存里保存的待签内容（§7.4.1 第 5 步）。
#[derive(Clone, Debug)]
pub struct PreparedSigning {
    pub checked: Checked,
    pub created_at_ms: i64,
}

impl PreparedSigning {
    pub fn approval_id(&self) -> String {
        self.checked.payload.approval_id.to_string()
    }

    fn expires_at_ms(&self) -> i64 {
        self.checked.payload.expires_at.as_millis()
    }
}

/// `POST …/grant` 的内容：只有 nonce 与签名，**不带 payload**（§7.4.1 第 6 步）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRequest {
    pub approval_id: String,
    pub body: Value,
    /// 按 `prepared_id` 派生，重试中沿用。
    pub idempotency_key: String,
}

/// `PreparedSigning` 的保管与单飞签名。
#[derive(Default)]
pub struct Signer {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    prepared: HashMap<String, PreparedSigning>,
    order: Vec<String>,
    busy: bool,
}

/// 签名期间持有的占位；释放即解除单飞。
struct BusyGuard<'a>(&'a Mutex<State>);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap().busy = false;
    }
}

impl Signer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 保存待签内容，返回随机句柄。过期的先清掉；超过上限时挤掉最早的。
    pub fn insert(&self, checked: Checked, now_ms: i64) -> Result<String, ShellError> {
        let id = random_id()?;
        let mut state = self.state.lock().unwrap();
        state.evict_expired(now_ms);
        while state.order.len() >= MAX_PREPARED {
            let oldest = state.order.remove(0);
            state.prepared.remove(&oldest);
        }
        state.order.push(id.clone());
        state.prepared.insert(
            id.clone(),
            PreparedSigning {
                checked,
                created_at_ms: now_ms,
            },
        );
        Ok(id)
    }

    pub fn get(&self, prepared_id: &str, now_ms: i64) -> Option<PreparedSigning> {
        let mut state = self.state.lock().unwrap();
        state.evict_expired(now_ms);
        state.prepared.get(prepared_id).cloned()
    }

    pub fn discard(&self, prepared_id: &str) {
        self.state.lock().unwrap().remove(prepared_id);
    }

    /// 吊销、`log_epoch` 变化时丢弃全部（S-10、S-13）。
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.prepared.clear();
        state.order.clear();
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().prepared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `approval_sign` 的第 1–6 步。`recheck` 由调用方做"连接 live、审批仍 pending 且未变"的重检，
    /// 在弹确认框**之前**执行；它失败则不弹框。
    ///
    /// - 用户取消 → `shell.user_cancelled`，`PreparedSigning` 保留到过期，可再次签名；
    /// - 签名成功 → 该 `PreparedSigning` 立即作废（每个句柄只签一次，S-9）。
    pub fn sign(
        &self,
        prepared_id: &str,
        keys: &dyn DeviceKeys,
        now_ms: i64,
        recheck: impl FnOnce(&PreparedSigning) -> Result<(), ShellError>,
    ) -> Result<GrantRequest, ShellError> {
        let (prepared, _busy) = {
            let mut state = self.state.lock().unwrap();
            state.evict_expired(now_ms);
            let prepared = state.prepared.get(prepared_id).cloned().ok_or_else(|| {
                ShellError::new(ShellErrorCode::PreparedExpired, "签名句柄已过期")
            })?;
            if state.busy {
                return Err(ShellError::new(
                    ShellErrorCode::SigningBusy,
                    "另一个确认正在进行",
                ));
            }
            state.busy = true;
            (prepared, BusyGuard(&self.state))
        };
        recheck(&prepared)?;
        let prompt = presence_prompt(&prepared.checked.raw);
        let signature: P1363Sig = keys
            .sign_approval(&prepared.checked.bytes, &prompt)
            .map_err(|e| match e {
                KeyError::UserCancelled => {
                    ShellError::new(ShellErrorCode::UserCancelled, "用户取消")
                }
                other => ShellError::from(other),
            })?;
        self.state.lock().unwrap().remove(prepared_id);
        Ok(GrantRequest {
            approval_id: prepared.approval_id(),
            body: json!({
                "nonce": prepared.checked.raw["nonce"],
                "signature": signature.to_base64url(),
            }),
            idempotency_key: format!("grant_{prepared_id}"),
        })
    }
}

impl State {
    fn evict_expired(&mut self, now_ms: i64) {
        let expired: Vec<String> = self
            .prepared
            .iter()
            .filter(|(_, p)| p.expires_at_ms() <= now_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.remove(&id);
        }
    }

    fn remove(&mut self, id: &str) {
        self.prepared.remove(id);
        self.order.retain(|o| o != id);
    }
}

fn random_id() -> Result<String, ShellError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| ShellError::new(ShellErrorCode::KeyUnavailable, "随机数不可用"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
