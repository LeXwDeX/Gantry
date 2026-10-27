//! 本机控制通道 `/run/gantry/control.sock`（deploy.md §6.1、§6.3；OPS-07）。
//!
//! 传输：Unix 流套接字，文件权限 0600；每个连接用 `SO_PEERCRED` / `getpeereid`
//! 取对端 uid，只放行 `ControlAuth::uid`（生产固定为 0，即 root）。
//! 协议（OPS-07 未定，本实现的最小方案，见 #101 评论）：每行一个 JSON 请求
//! `{"cmd": "...", "args": {...}}`，每行一个 JSON 应答
//! `{"ok": true, "result": ...}` 或 `{"ok": false, "error": {"code", "message"}}`。
//!
//! 命令：`status`；`fakes` 装配下另有 `fake.*`，供进程级测试注入与观察，真实装配
//! 里不存在。`pair`、`settings bootstrap-set` 等 CLI 命令随对应模块接入。

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::assembly::Assembly;

/// 单个请求行的上限；超过即断开，避免无界读取。
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlAuth {
    pub uid: u32,
}

impl ControlAuth {
    pub const ROOT: Self = Self { uid: 0 };
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("{0:?} exists and is not a socket")]
    NotASocket(PathBuf),
    #[error("another gantryd is listening on {0:?}")]
    AlreadyRunning(PathBuf),
    #[error("control socket {path:?}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok { ok: bool, result: Value },
    Err { ok: bool, error: ErrorBody },
}

impl Response {
    pub fn ok(result: Value) -> Self {
        Self::Ok { ok: true, result }
    }

    pub fn err(code: &'static str, message: impl Into<String>) -> Self {
        Self::Err {
            ok: false,
            error: ErrorBody {
                code,
                message: message.into(),
            },
        }
    }
}

/// 已绑定的套接字；drop 时删除套接字文件。
pub struct ControlSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl ControlSocket {
    /// 绑定并把文件权限收紧为 0600。残留的套接字文件（无人监听）被替换；
    /// 有人监听或路径不是套接字则拒绝，不删除别人的文件。目录由部署创建
    /// （systemd-tmpfiles，deploy.md §6.3），这里不建目录。
    pub fn bind(path: &Path) -> Result<Self, BindError> {
        let io_err = |source| BindError::Io {
            path: path.to_owned(),
            source,
        };
        match std::fs::symlink_metadata(path) {
            Ok(meta) if !meta.file_type().is_socket() => {
                return Err(BindError::NotASocket(path.to_owned()))
            }
            Ok(_) => match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => return Err(BindError::AlreadyRunning(path.to_owned())),
                Err(_) => std::fs::remove_file(path).map_err(io_err)?,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_err(error)),
        }
        let listener = UnixListener::bind(path).map_err(io_err)?;
        let socket = Self {
            listener,
            path: path.to_owned(),
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io_err)?;
        Ok(socket)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 接受连接直到 `shutdown` 变为 `true`。
    pub async fn serve(
        &self,
        control: Arc<Control>,
        auth: ControlAuth,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let control = control.clone();
                        tokio::spawn(async move { serve_conn(stream, &control, auth).await });
                    }
                    Err(error) => eprintln!("gantryd: control accept failed: {error}"),
                },
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve_conn(stream: UnixStream, control: &Control, auth: ControlAuth) {
    let peer = stream.peer_cred().map(|cred| cred.uid());
    let (read, mut write) = stream.into_split();
    if peer.as_ref().ok() != Some(&auth.uid) {
        let response = Response::err("forbidden", "control socket is root-only");
        let _ = write_line(&mut write, &response).await;
        return;
    }
    let mut reader = BufReader::new(read);
    loop {
        let line = match read_bounded_line(&mut reader).await {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(response) => {
                let _ = write_line(&mut write, &response).await;
                return;
            }
        };
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => control.handle(request).await,
            Err(error) => Response::err("bad_request", error.to_string()),
        };
        if write_line(&mut write, &response).await.is_err() {
            return;
        }
    }
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<String>, Response> {
    let mut buf = Vec::new();
    let read = reader
        .take(MAX_REQUEST_BYTES + 1)
        .read_until(b'\n', &mut buf)
        .await
        .map_err(|error| Response::err("io", error.to_string()))?;
    if read == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') && read as u64 > MAX_REQUEST_BYTES {
        return Err(Response::err("too_large", "request line exceeds 64 KiB"));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| Response::err("bad_request", "request is not UTF-8"))
}

async fn write_line<W: AsyncWriteExt + Unpin>(
    write: &mut W,
    response: &Response,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await
}

/// 命令分派。只持有装配结果，不持有套接字。
pub struct Control {
    assembly: Arc<Assembly>,
}

impl Control {
    pub fn new(assembly: Arc<Assembly>) -> Self {
        Self { assembly }
    }

    pub async fn handle(&self, request: Request) -> Response {
        match request.cmd.as_str() {
            "status" => self.status().await,
            #[cfg(feature = "fakes")]
            cmd if cmd.starts_with("fake.") && self.assembly.fakes.is_some() => {
                crate::fake_cmds::handle(&self.assembly, cmd, request.args).await
            }
            other => Response::err("unknown_command", format!("unknown command {other:?}")),
        }
    }

    async fn status(&self) -> Response {
        let head = match self.assembly.store.events().head().await {
            Ok(head) => head.as_u64(),
            Err(error) => return Response::err("store", error.to_string()),
        };
        Response::ok(json!({
            "profile": self.assembly.profile,
            "head_seq": head,
            "modules": self.assembly.modules,
        }))
    }
}
