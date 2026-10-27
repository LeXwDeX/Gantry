//! 日志与诊断（desktop-shell.md §11）。
//!
//! - **日志**：JSON 行，写应用数据目录 `logs/shell.log`，轮转 5 × 5 MiB [建议值]。日志项只能由
//!   [`LogEntry`] 的固定字段构成（路由、状态码、耗时、`request_id`、`Idempotent-Replayed`、事件名、
//!   本地错误码），从结构上就写不进请求 / 响应体、秘密值、配对 token、签名、`Gantry-Approval` 的值。
//! - **诊断快照**：版本、连接状态、游标、计数器、最近 20 个错误码；不含任何服务端业务内容。

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_FILES: usize = 5;
const RECENT_ERRORS: usize = 20;

/// 一条日志。字段集合是固定的：没有任何"任意字符串"或"任意 JSON"字段。
#[derive(Debug, Default)]
pub struct LogEntry {
    /// 事件名，取自本模块固定的常量（如 `api_request`、`sse_reconnect`）。
    pub event: &'static str,
    /// 路由 ID 或 `operationId`（枚举值，不含路径参数）。
    pub route: Option<&'static str>,
    pub status: Option<u16>,
    pub duration_ms: Option<u64>,
    /// 服务端 `X-Request-Id`，只接受形如 `[A-Za-z0-9_-]{1,64}` 的值。
    pub request_id: Option<String>,
    pub idempotent_replayed: Option<bool>,
    /// 本地错误码或网络错误类别（枚举值）。
    pub code: Option<&'static str>,
    /// 原因（枚举值，如 `ca_fingerprint_mismatch`）。
    pub reason: Option<&'static str>,
}

fn safe_request_id(id: &str) -> Option<String> {
    (!id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then(|| id.to_string())
}

impl LogEntry {
    pub fn with_request_id(mut self, id: Option<&str>) -> Self {
        self.request_id = id.and_then(safe_request_id);
        self
    }

    fn to_json(&self, ts_ms: i64) -> Value {
        let mut v = json!({ "ts_ms": ts_ms, "event": self.event });
        let o = v.as_object_mut().expect("对象");
        if let Some(r) = self.route {
            o.insert("route".into(), json!(r));
        }
        if let Some(s) = self.status {
            o.insert("status".into(), json!(s));
        }
        if let Some(d) = self.duration_ms {
            o.insert("duration_ms".into(), json!(d));
        }
        if let Some(r) = &self.request_id {
            o.insert("request_id".into(), json!(r));
        }
        if let Some(r) = self.idempotent_replayed {
            o.insert("idempotent_replayed".into(), json!(r));
        }
        if let Some(c) = self.code {
            o.insert("code".into(), json!(c));
        }
        if let Some(r) = self.reason {
            o.insert("reason".into(), json!(r));
        }
        v
    }
}

/// 把运行时字符串收敛成固定集合里的 `&'static str`；不在集合里的记为 `other`。
pub fn intern(value: &str, known: &'static [&'static str]) -> &'static str {
    known
        .iter()
        .find(|k| **k == value)
        .copied()
        .unwrap_or("other")
}

#[derive(Default)]
struct State {
    counters: BTreeMap<String, u64>,
    recent_errors: VecDeque<String>,
}

pub struct Diagnostics {
    dir: Option<PathBuf>,
    state: Mutex<State>,
    file: Mutex<()>,
}

impl Diagnostics {
    /// `dir` 为 `None` 时只计数、不写文件（测试与无数据目录时）。
    pub fn new(dir: Option<&Path>) -> Self {
        if let Some(d) = dir {
            let _ = fs::create_dir_all(d);
        }
        Self {
            dir: dir.map(Path::to_path_buf),
            state: Mutex::new(State::default()),
            file: Mutex::new(()),
        }
    }

    pub fn log_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("shell.log"))
    }

    /// 计数器 `name{label=value,...}`；标签值必须是枚举值（调用方用 [`intern`] 收敛）。
    pub fn count(&self, name: &str, labels: &[(&str, &str)]) {
        let mut key = name.to_string();
        if !labels.is_empty() {
            let parts: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            key = format!("{name}{{{}}}", parts.join(","));
        }
        *self.state.lock().unwrap().counters.entry(key).or_default() += 1;
    }

    /// 记一个错误码（只记码，不记文案），保留最近 20 个。
    pub fn error_code(&self, code: &str) {
        let mut st = self.state.lock().unwrap();
        st.recent_errors.push_back(code.to_string());
        while st.recent_errors.len() > RECENT_ERRORS {
            st.recent_errors.pop_front();
        }
    }

    pub fn log(&self, entry: LogEntry, ts_ms: i64) {
        let Some(path) = self.log_path() else {
            return;
        };
        let _guard = self.file.lock().unwrap();
        if fs::metadata(&path).is_ok_and(|m| m.len() >= MAX_FILE_BYTES) {
            rotate(&path);
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut f) = options.open(&path) {
            let _ = writeln!(f, "{}", entry.to_json(ts_ms));
        }
    }

    pub fn counters(&self) -> BTreeMap<String, u64> {
        self.state.lock().unwrap().counters.clone()
    }

    pub fn recent_errors(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .recent_errors
            .iter()
            .cloned()
            .collect()
    }
}

fn rotate(path: &Path) {
    for i in (1..MAX_FILES).rev() {
        let from = if i == 1 {
            path.to_path_buf()
        } else {
            path.with_extension(format!("log.{}", i - 1))
        };
        let to = path.with_extension(format!("log.{i}"));
        if from.exists() {
            let _ = fs::rename(&from, &to);
        }
    }
}

/// 已知的本地错误码（`ShellErrorCode` 的线上值），用于把错误收敛进日志与计数器。
pub const SHELL_CODES: &[&str] = &[
    "shell.not_paired",
    "shell.not_connected",
    "shell.bad_argument",
    "shell.unknown_route",
    "shell.too_large",
    "shell.user_cancelled",
    "shell.presence_failed",
    "shell.signing_busy",
    "shell.prepared_expired",
    "shell.self_check_failed",
    "shell.key_unavailable",
    "shell.pairing_failed",
    "pair_token_invalid",
    "shell.unavailable",
];

/// 已知的原因（自检、配对等）。
pub const REASONS: &[&str] = &[
    "approval_id_mismatch",
    "device_id_mismatch",
    "unknown_version",
    "unsupported_signing",
    "unknown_action",
    "challenge_expired",
    "approval_data_mismatch",
    "internal_digest_mismatch",
    "not_pending",
    "ca_fingerprint_mismatch",
    "chain_shape",
    "chain_invalid",
    "chain_expired",
    "leaf_key_mismatch",
    "pubkey_fpr_mismatch",
    "approval_pubkey_fpr_mismatch",
    "response_shape",
    "server_rejected",
    "network",
    "csr",
    "confirm_timeout",
    "renew_rejected",
];

/// 路由 `operationId`（48 条）之外的值记为 `other`。
pub fn route_label(operation_id: &str) -> &'static str {
    crate::routes::ROUTES
        .iter()
        .find(|r| r.operation_id == operation_id)
        .map(|r| r.operation_id)
        .unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_only_carry_whitelisted_fields() {
        let e = LogEntry {
            event: "api_request",
            route: Some("getMe"),
            status: Some(200),
            duration_ms: Some(12),
            ..Default::default()
        }
        .with_request_id(Some("req_01ABC"));
        let v = e.to_json(1);
        assert_eq!(v["request_id"], "req_01ABC");
        // request_id 形态不对的直接丢弃（服务端头值不可信）。
        let e = LogEntry::default().with_request_id(Some("x\"; token=sk-ant-SENTINEL"));
        assert!(e.request_id.is_none());
        assert_eq!(intern("sk-ant-SENTINEL", SHELL_CODES), "other");
        assert_eq!(route_label("evil path /v1/x?token=1"), "other");
    }

    #[test]
    fn rotates_at_five_files() {
        let dir = std::env::temp_dir().join(format!("gantry-diag-rotate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let d = Diagnostics::new(Some(&dir));
        let path = d.log_path().unwrap();
        for i in 0..7 {
            fs::write(&path, vec![b'x'; MAX_FILE_BYTES as usize]).unwrap();
            d.log(
                LogEntry {
                    event: "tick",
                    ..Default::default()
                },
                i,
            );
        }
        let files: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), MAX_FILES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_last_twenty_error_codes() {
        let d = Diagnostics::new(None);
        for i in 0..25 {
            d.error_code(&format!("c{i}"));
        }
        let recent = d.recent_errors();
        assert_eq!(recent.len(), 20);
        assert_eq!(recent[0], "c5");
        d.count("api_requests", &[("route", "getMe"), ("status", "200")]);
        d.count("api_requests", &[("route", "getMe"), ("status", "200")]);
        assert_eq!(d.counters()["api_requests{route=getMe,status=200}"], 2);
    }
}
