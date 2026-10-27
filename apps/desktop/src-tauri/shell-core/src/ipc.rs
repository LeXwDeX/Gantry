//! IPC 边界（desktop-shell.md §6.1）：命令白名单与本地错误。
//!
//! [`COMMANDS`] 是唯一的白名单来源：Tauri 应用的 `build.rs` 用它生成 `allow-<命令>` 权限，
//! `capabilities/main.json` 与命令注册表必须与它逐项一致（S-5，由 `tests/ipc_whitelist.rs` 检查）。

use std::fmt;

use serde::Serialize;

use crate::keys::KeyError;

/// §6.1 命令白名单，顺序与规格表一致。白名单之外的命令一律不存在。
pub const COMMANDS: &[&str] = &[
    "identity_status",
    "pairing_begin",
    "pairing_confirm",
    "identity_reset",
    "connection_state",
    "api_request",
    "events_open",
    "events_ack",
    "events_close",
    "approval_prepare",
    "approval_sign",
    "approval_discard",
    "guarded_write_prepare",
    "guarded_write_submit",
    "guarded_write_discard",
    "prefs_get",
    "prefs_set",
    "ui_state_get",
    "ui_state_set",
    "ui_state_list",
    "open_external",
    "open_github",
    "diagnostics_snapshot",
];

/// 本地产生的错误码（§6.1 末段，前缀 `shell.`）。服务端 `problem` 不走这里，经 `ApiResult` 透传。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ShellErrorCode {
    #[serde(rename = "shell.not_paired")]
    NotPaired,
    #[serde(rename = "shell.not_connected")]
    NotConnected,
    #[serde(rename = "shell.bad_argument")]
    BadArgument,
    #[serde(rename = "shell.unknown_route")]
    UnknownRoute,
    #[serde(rename = "shell.too_large")]
    TooLarge,
    #[serde(rename = "shell.user_cancelled")]
    UserCancelled,
    #[serde(rename = "shell.presence_failed")]
    PresenceFailed,
    #[serde(rename = "shell.signing_busy")]
    SigningBusy,
    #[serde(rename = "shell.prepared_expired")]
    PreparedExpired,
    #[serde(rename = "shell.self_check_failed")]
    SelfCheckFailed,
    #[serde(rename = "shell.key_unavailable")]
    KeyUnavailable,
    /// 外壳尚未实现该能力（与前端 `shell.unavailable` 对齐），不是规格里的永久错误。
    #[serde(rename = "shell.unavailable")]
    Unavailable,
}

/// 过 IPC 的错误体：`{ code, message, field?, reason? }`。`message` 是本地固定文案，
/// 不含服务端文本、秘密值或签名（S-7、§11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ShellError {
    pub code: ShellErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ShellError {
    pub fn new(code: ShellErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
            reason: None,
        }
    }

    pub fn bad_argument(field: impl Into<String>) -> Self {
        let field = field.into();
        Self {
            field: Some(field.clone()),
            ..Self::new(ShellErrorCode::BadArgument, format!("参数 {field} 不合法"))
        }
    }

    pub fn not_paired() -> Self {
        Self::new(ShellErrorCode::NotPaired, "设备尚未配对")
    }

    pub fn unavailable(what: &str) -> Self {
        Self::new(ShellErrorCode::Unavailable, format!("外壳尚未实现：{what}"))
    }
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ShellError {}

impl From<KeyError> for ShellError {
    fn from(e: KeyError) -> Self {
        // 后端细节（OSStatus、LAError 文案）只写日志，不过 IPC。
        match e {
            KeyError::UserCancelled => Self::new(ShellErrorCode::UserCancelled, "用户取消"),
            KeyError::PresenceFailed(_) => {
                Self::new(ShellErrorCode::PresenceFailed, "用户在场验证未通过")
            }
            KeyError::KeyUnavailable | KeyError::HardwareUnavailable(_) | KeyError::Backend(_) => {
                Self::new(ShellErrorCode::KeyUnavailable, "设备密钥不可用")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_unique_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for c in COMMANDS {
            assert!(
                c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{c}"
            );
            assert!(seen.insert(*c), "重复命令 {c}");
        }
        assert_eq!(COMMANDS.len(), 23);
    }

    #[test]
    fn error_serializes_with_shell_prefix_and_hides_backend_detail() {
        let e = ShellError::from(KeyError::Backend("OSStatus -34018 secret detail".into()));
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["code"], "shell.key_unavailable");
        assert!(!json.to_string().contains("34018"));
        let e = ShellError::bad_argument("route");
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["code"], "shell.bad_argument");
        assert_eq!(json["field"], "route");
        assert!(json.get("reason").is_none());
    }
}
