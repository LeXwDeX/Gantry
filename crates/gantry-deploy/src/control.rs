//! 本机控制通道的客户端侧（deploy.md §6.1；OPS-07）。
//!
//! 协议与 `gantryd` 的服务端一致（#101 的最小方案）：每行一个 JSON 请求
//! `{"cmd": "...", "args": {...}}`，每行一个 JSON 应答
//! `{"ok": true, "result": ...}` 或 `{"ok": false, "error": {"code", "message"}}`。

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use serde_json::json;
use serde_json::Value;

pub const DEFAULT_SOCKET: &str = "/run/gantry/control.sock";
/// 单个应答行的上限，与服务端的请求上限同量级。
pub const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ControlError {
    /// 套接字连不上：`gantryd` 没在运行，或当前用户不是 root。
    #[error("无法连接控制通道 {path}：{message}")]
    Unavailable { path: String, message: String },
    #[error("控制通道应答无法解析：{0}")]
    Protocol(String),
    /// 服务端拒绝：`code` 是机器可读的错误码。
    #[error("{code}：{message}")]
    Rejected { code: String, message: String },
}

pub trait ControlClient {
    fn call(&self, cmd: &str, args: Value) -> Result<Value, ControlError>;
}

/// 每次调用新开一个连接（服务端只在 Linux 上运行；Windows 构建里没有这个类型），调用之间不保留状态。
#[cfg(unix)]
pub struct UnixControlClient {
    pub path: PathBuf,
    pub timeout: Duration,
}

#[cfg(unix)]
impl UnixControlClient {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            timeout: Duration::from_secs(30),
        }
    }
}

#[cfg(unix)]
impl ControlClient for UnixControlClient {
    fn call(&self, cmd: &str, args: Value) -> Result<Value, ControlError> {
        let unavailable = |error: std::io::Error| ControlError::Unavailable {
            path: self.path.display().to_string(),
            message: error.to_string(),
        };
        let mut stream = UnixStream::connect(&self.path).map_err(unavailable)?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(unavailable)?;
        let mut line = json!({ "cmd": cmd, "args": args }).to_string();
        line.push('\n');
        // 对端 uid 不符时，服务端在连接建立后先写一行 `forbidden` 再断开；此时写请求可能
        // 失败（EPIPE），仍先读那一行，读不到再报写错误。
        let written = stream.write_all(line.as_bytes());
        let mut reply = String::new();
        let read =
            BufReader::new(std::io::Read::take(&stream, MAX_RESPONSE_BYTES)).read_line(&mut reply);
        match (written, read) {
            (_, Ok(n)) if n > 0 => parse_response(&reply),
            (Err(error), _) => Err(unavailable(error)),
            (Ok(()), Ok(_)) => Err(ControlError::Protocol("连接在应答前关闭".into())),
            (Ok(()), Err(error)) => Err(ControlError::Protocol(error.to_string())),
        }
    }
}

pub fn parse_response(line: &str) -> Result<Value, ControlError> {
    let value: Value =
        serde_json::from_str(line.trim_end()).map_err(|e| ControlError::Protocol(e.to_string()))?;
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        Some(false) => {
            let error = value.get("error");
            let field = |name: &str| {
                error
                    .and_then(|e| e.get(name))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Err(ControlError::Rejected {
                code: field("code"),
                message: field("message"),
            })
        }
        None => Err(ControlError::Protocol("缺少 ok 字段".into())),
    }
}
