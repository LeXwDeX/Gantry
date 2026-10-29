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
    /// 无法从用户数据库解析期望 uid；失败闭合，不连接。
    #[error("无法解析用户 {user} 的 uid，拒绝连接控制通道")]
    UnknownUser { user: String },
    /// 对端 uid 与期望不符：请求可能发给了 `gantry-egress` 或冒名套接字。
    #[error("控制通道对端 uid 不符：期望 {expected}，实际 {actual}")]
    PeerUid { expected: u32, actual: u32 },
    #[error("无法读取控制通道对端凭据：{0}")]
    PeerCredentials(String),
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
    /// 期望的对端用户名；生产是 `gantry`（deploy.md §6.3）。
    pub user: String,
    /// 期望 uid 的显式覆盖（测试注入）；`None` 时用 `getpwnam` 解析 `user`。
    pub expected_uid: Option<u32>,
}

#[cfg(unix)]
impl UnixControlClient {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            timeout: Duration::from_secs(30),
            user: "gantry".into(),
            expected_uid: None,
        }
    }

    fn exchange(
        &self,
        mut stream: UnixStream,
        cmd: &str,
        args: Value,
    ) -> Result<Value, ControlError> {
        let unavailable = |error: std::io::Error| ControlError::Unavailable {
            path: self.path.display().to_string(),
            message: error.to_string(),
        };
        if let Err(error) = stream.set_read_timeout(Some(self.timeout)) {
            let recover =
                !self.timeout.is_zero() && error.kind() == std::io::ErrorKind::InvalidInput;
            let original = unavailable(error);
            return if recover {
                Self::early_denial(&stream, original)
            } else {
                Err(original)
            };
        }
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

    fn early_denial(stream: &UnixStream, original: ControlError) -> Result<Value, ControlError> {
        if stream.set_nonblocking(true).is_ok() {
            let mut reply = String::new();
            let read = BufReader::new(std::io::Read::take(stream, MAX_RESPONSE_BYTES))
                .read_line(&mut reply);
            // take 的上限或非阻塞读取都可能在换行前结束，不能把前缀当作完整应答。
            if read.is_ok() && reply.ends_with('\n') {
                if let Err(error) = parse_response(&reply) {
                    if matches!(&error, ControlError::Rejected { code, .. } if code == "forbidden")
                    {
                        return Err(error);
                    }
                }
            }
        }
        Err(original)
    }
}

#[cfg(unix)]
impl ControlClient for UnixControlClient {
    fn call(&self, cmd: &str, args: Value) -> Result<Value, ControlError> {
        let expected = match self.expected_uid {
            Some(uid) => uid,
            None => resolve_uid(&self.user)?,
        };
        let stream =
            UnixStream::connect(&self.path).map_err(|error| ControlError::Unavailable {
                path: self.path.display().to_string(),
                message: error.to_string(),
            })?;
        // #368：连接后、发送任何请求前校验对端 uid，防止请求发给 gantry-egress 或冒名套接字。
        let actual =
            peer_uid(&stream).map_err(|error| ControlError::PeerCredentials(error.to_string()))?;
        if actual != expected {
            return Err(ControlError::PeerUid { expected, actual });
        }
        self.exchange(stream, cmd, args)
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    stream.peer_cred().map(|cred| cred.uid())
}

#[cfg(target_vendor = "apple")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    nix::unistd::getpeereid(stream)
        .map(|(uid, _)| uid.as_raw())
        .map_err(std::io::Error::from)
}

#[cfg(all(unix, not(any(target_os = "linux", target_vendor = "apple"))))]
fn peer_uid(_: &UnixStream) -> std::io::Result<u32> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "对端凭据读取未在此平台实现",
    ))
}

/// `getpwnam` 解析 `user` 的 uid；解析失败或出错都按拒绝处理（失败闭合）。
fn resolve_uid(user: &str) -> Result<u32, ControlError> {
    nix::unistd::User::from_name(user)
        .ok()
        .flatten()
        .map(|user| user.uid.as_raw())
        .ok_or_else(|| ControlError::UnknownUser {
            user: user.to_owned(),
        })
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const FORBIDDEN: &[u8] =
        b"{\"ok\":false,\"error\":{\"code\":\"forbidden\",\"message\":\"root only\"}}\n";

    fn original() -> ControlError {
        ControlError::Unavailable {
            path: "fixture.sock".into(),
            message: "original timeout error".into(),
        }
    }

    #[test]
    fn closed_peer_with_forbidden_reply_is_reported() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(FORBIDDEN).unwrap();
        drop(peer);
        let result = UnixControlClient::new("fixture.sock").exchange(stream, "status", Value::Null);
        assert_eq!(
            result,
            Err(ControlError::Rejected {
                code: "forbidden".into(),
                message: "root only".into()
            })
        );
    }

    #[test]
    fn zero_timeout_keeps_error_even_with_forbidden_reply() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(FORBIDDEN).unwrap();
        let mut client = UnixControlClient::new("fixture.sock");
        client.timeout = Duration::ZERO;
        assert!(matches!(
            client.exchange(stream, "status", Value::Null),
            Err(ControlError::Unavailable { .. })
        ));
        peer.set_nonblocking(true).unwrap();
        let mut request = [0; 1];
        assert_eq!(std::io::Read::read(&mut peer, &mut request).unwrap(), 0);
    }

    #[test]
    fn early_denial_only_accepts_a_complete_forbidden_line() {
        let cases: &[&[u8]] = &[
            b"",
            b"{\"ok\":false",
            &FORBIDDEN[..FORBIDDEN.len() - 1],
            b"not json\n",
            b"\xff\n",
            b"{\"ok\":true}\n",
            b"{\"ok\":false,\"error\":{\"code\":\"other\"}}\n",
        ];
        for input in cases {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.write_all(input).unwrap();
            drop(peer);
            assert_eq!(
                UnixControlClient::early_denial(&stream, original()),
                Err(original()),
                "fixture: {input:?}"
            );
        }
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(FORBIDDEN).unwrap();
        assert_eq!(
            UnixControlClient::early_denial(&stream, original()),
            Err(ControlError::Rejected {
                code: "forbidden".into(),
                message: "root only".into()
            })
        );
        peer.set_nonblocking(true).unwrap();
        let mut request = [0; 1];
        assert_eq!(
            std::io::Read::read(&mut peer, &mut request)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn early_denial_does_not_wait_for_an_open_peer() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        assert_eq!(
            UnixControlClient::early_denial(&stream, original()),
            Err(original())
        );
    }

    #[test]
    fn early_denial_rejects_an_over_limit_line() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (ready, received) = std::sync::mpsc::sync_channel(0);
        let writer = std::thread::spawn(move || {
            let mut input = FORBIDDEN[..FORBIDDEN.len() - 1].to_vec();
            input.resize(MAX_RESPONSE_BYTES as usize, b' ');
            input.push(b'\n');
            peer.write_all(&input[..FORBIDDEN.len() - 1]).unwrap();
            ready.send(()).unwrap();
            let _ = peer.write_all(&input[FORBIDDEN.len() - 1..]);
        });
        received.recv().unwrap();
        assert_eq!(
            UnixControlClient::early_denial(&stream, original()),
            Err(original())
        );
        drop(stream);
        writer.join().unwrap();
    }
}
