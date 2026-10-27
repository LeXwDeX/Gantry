//! Webhook 接收（forge.md §7.4、FA §5.1–§5.3）。
//!
//! 独立监听，只绑回环地址、明文 HTTP（P-65），服务 `POST /webhook`，以及 App 创建期间的
//! `GET /setup/github/callback`（交给 [`crate::setup::SetupService`]）。每个请求按 FA §5.1
//! 的顺序处理：体积 → 必需头 → HMAC（原始字节、常量时间、当前 secret 失败再试上一个）→
//! App ID → `ping` → 订阅清单 → 路由提示 → 单事务落盘（`delivery_id` 去重）→ 202 / 200。
//! 落盘提交后才回应，随后唤醒分发器；响应前不做任何回读。
//!
//! shortcut: 超过 `inline_max_bytes` 的载荷先进 CAS 的路径（forge.md §7.3）要等 store 的
//! CAS（#88）；现在整包内联进 `forge_webhook_inbox`。`push` 只保留默认分支，活跃 PR 的
//! 非默认 base 分支要等快照切片（FA §9.3）。

pub mod subscriptions;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gantry_core::ids::Clock;
use gantry_core::ports::SecretSource;
use gantry_core::secret::SecretName;
use gantry_core::time::Timestamp;
use gantry_store::Store;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::config::ForgeConfig;

/// `forge` 的模块私有表注册项（store.md §7.10）。
pub const MODULE: &str = "forge";
pub const INBOX_TABLE: &str = "forge_webhook_inbox";

/// 头部上限：GitHub 的请求头远小于此。
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// `X-GitHub-Delivery` 是 GUID；放宽到 128 字符，仍拒绝控制字符。
const MAX_DELIVERY_ID: usize = 128;
/// App Manifest 创建回调（P-65；forge.md §7.14）。
pub const CALLBACK_PATH: &str = "/setup/github/callback";
/// 关闭前读掉剩余输入的时间与字节上限。
const LINGER: Duration = Duration::from_millis(500);
const LINGER_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookOptions {
    /// `forge.webhook.listen`；只允许回环地址。
    pub listen: SocketAddr,
    /// `forge.webhook.max_body_bytes`。
    pub max_body_bytes: usize,
    /// 单个请求读完头与 body 的上限（GitHub 10 秒超时，FA §5.1）。
    pub read_timeout: Duration,
}

impl Default for WebhookOptions {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8787)),
            max_body_bytes: 26_214_400,
            read_timeout: Duration::from_secs(5),
        }
    }
}

pub struct WebhookDeps {
    pub store: Store,
    pub config: Arc<ForgeConfig>,
    /// `forge` 使用方视图；每个请求都重新读取，轮换即时生效（P-29）。
    pub secrets: Arc<dyn SecretSource>,
    pub secret: SecretName,
    pub previous_secret: Option<SecretName>,
    /// `github.app.app_id`：`X-GitHub-Hook-Installation-Target-ID` 必须等于它。
    pub app_id: u64,
    pub clock: Arc<dyn Clock>,
    pub options: WebhookOptions,
    /// App 创建期间的 setup 服务；`None` 时回调路径一律 404。
    pub setup: Option<Arc<crate::setup::SetupService>>,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    /// P-65：webhook 只在回环地址上以明文 HTTP 提供。
    #[error("webhook listener must bind a loopback address, got {0}")]
    NonLoopback(SocketAddr),
    #[error("bind webhook listener: {0}")]
    Bind(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxStatus {
    Received,
    Processing,
    Parked,
    Done,
    Failed,
    Dropped,
}

/// `forge_webhook_inbox` 的一行（forge.md §7.3），键为 `delivery_id`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxRow {
    pub delivery_id: String,
    pub event: String,
    pub action: Option<String>,
    pub installation_id: Option<i64>,
    /// 载荷里的 `owner/repo`，只作路由提示。
    pub repo: Option<String>,
    pub received_at: Timestamp,
    /// 原始 body（GitHub 载荷是 UTF-8 JSON）；`payload_retention` 后置空，保留本行。
    pub body: Option<String>,
    pub status: InboxStatus,
    pub attempts: u32,
    pub retry_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

/// 计数（forge.md §11）。只计数，不记载荷。
#[derive(Debug, Default)]
pub struct WebhookStats {
    pub accepted: AtomicU64,
    pub duplicates: AtomicU64,
    pub bad_signature: AtomicU64,
    pub bad_request: AtomicU64,
    pub too_large: AtomicU64,
    pub foreign_target: AtomicU64,
    pub unsubscribed: AtomicU64,
    pub unbound_installation: AtomicU64,
    pub pings: AtomicU64,
    pub unavailable: AtomicU64,
}

impl WebhookStats {
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// 运行中的监听。drop 即停止接收。
pub struct WebhookReceiver {
    local_addr: SocketAddr,
    stats: Arc<WebhookStats>,
    wake: Arc<Notify>,
    task: JoinHandle<()>,
}

impl WebhookReceiver {
    pub async fn bind(deps: WebhookDeps) -> Result<Self, WebhookError> {
        let listen = deps.options.listen;
        if !listen.ip().is_loopback() {
            return Err(WebhookError::NonLoopback(listen));
        }
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|error| WebhookError::Bind(error.kind().to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|error| WebhookError::Bind(error.kind().to_string()))?;
        let stats = Arc::new(WebhookStats::default());
        let wake = Arc::new(Notify::new());
        let handler = Arc::new(Handler {
            deps,
            stats: stats.clone(),
            wake: wake.clone(),
        });
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move { handler.serve(stream).await });
            }
        });
        Ok(Self {
            local_addr,
            stats,
            wake,
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// `http://127.0.0.1:<port>/webhook`。
    pub fn url(&self) -> String {
        format!("http://{}/webhook", self.local_addr)
    }

    pub fn stats(&self) -> &WebhookStats {
        &self.stats
    }

    /// 每次有新行落盘后唤醒（分发器用）。
    pub fn wake(&self) -> Arc<Notify> {
        self.wake.clone()
    }
}

impl Drop for WebhookReceiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Handler {
    deps: WebhookDeps,
    stats: Arc<WebhookStats>,
    wake: Arc<Notify>,
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

enum ReadOutcome {
    Request(Request),
    Status(u16),
}

impl Handler {
    async fn serve(&self, mut stream: TcpStream) {
        let status = match tokio::time::timeout(
            self.deps.options.read_timeout,
            read_request(&mut stream, self.deps.options.max_body_bytes),
        )
        .await
        {
            Ok(Ok(ReadOutcome::Request(request)))
                if request.method == "GET" && request.path.starts_with(CALLBACK_PATH) =>
            {
                let (status, body) = self.setup_callback(&request.path).await;
                let response = format!(
                    "HTTP/1.1 {status} {}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
                    reason(status),
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
                return;
            }
            Ok(Ok(ReadOutcome::Request(request))) => self.handle(request).await,
            Ok(Ok(ReadOutcome::Status(status))) => {
                bump(if status == 413 {
                    &self.stats.too_large
                } else {
                    &self.stats.bad_request
                });
                status
            }
            Ok(Err(_)) => return,
            Err(_) => 408,
        };
        let response = format!(
            "HTTP/1.1 {status} {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            reason(status)
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        // 提前拒绝（413 等）时对端还在发 body；直接关闭会因未读数据触发 RST，把响应冲掉。
        // 有界地读掉剩余输入再关（lingering close），不缓冲内容。
        let mut sink = [0u8; 8192];
        let mut drained = 0usize;
        let _ = tokio::time::timeout(LINGER, async {
            while drained < LINGER_MAX_BYTES {
                match stream.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => drained += read,
                }
            }
        })
        .await;
    }

    /// `GET /setup/github/callback`：只在有未完成的 setup 会话时响应，否则 404（forge.md §7.14）。
    async fn setup_callback(&self, path: &str) -> (u16, String) {
        let Some(setup) = &self.deps.setup else {
            return (404, String::new());
        };
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        if route != CALLBACK_PATH || !setup.has_pending() {
            return (404, String::new());
        }
        let response = setup.callback(query).await;
        (response.status, response.body)
    }

    async fn handle(&self, mut request: Request) -> u16 {
        let raw = std::mem::take(&mut request.body);
        if request.method != "POST" || request.path != "/webhook" {
            return 404;
        }
        // 步 2：三个必需头。
        let (Some(event), Some(delivery), Some(signature)) = (
            request.header("x-github-event"),
            request.header("x-github-delivery"),
            request.header("x-hub-signature-256"),
        ) else {
            bump(&self.stats.bad_request);
            return 400;
        };
        if !valid_delivery_id(delivery) {
            bump(&self.stats.bad_request);
            return 400;
        }
        // 步 3：HMAC。secret 缺失 → 验签关闭（forge.md §9），不落盘。
        match self.verify(&raw, signature) {
            Verify::Ok => {}
            Verify::Mismatch => {
                bump(&self.stats.bad_signature);
                return 401;
            }
            Verify::Unavailable => {
                bump(&self.stats.unavailable);
                return 503;
            }
        }
        // 防串线：投递目标必须是本 App；不给探测者信号，回 200。
        if request.header("x-github-hook-installation-target-id")
            != Some(self.deps.app_id.to_string().as_str())
        {
            bump(&self.stats.foreign_target);
            return 200;
        }
        if event == "ping" {
            bump(&self.stats.pings);
            return 200;
        }
        let Ok(body) = String::from_utf8(raw) else {
            bump(&self.stats.bad_request);
            return 400;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&body) else {
            bump(&self.stats.bad_request);
            return 400;
        };
        let action = payload.get("action").and_then(Value::as_str);
        if !subscriptions::accepts(event, action) {
            bump(&self.stats.unsubscribed);
            return 200;
        }
        let installation_id = payload.pointer("/installation/id").and_then(Value::as_i64);
        let repo = payload
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // 路由提示：安装必须属于某个 Project（`installation*` 事件除外）。
        let project = installation_id.and_then(|installation| {
            self.deps
                .config
                .projects
                .values()
                .find(|binding| binding.project.installation_id == installation)
        });
        if project.is_none() && !event.starts_with("installation") {
            bump(&self.stats.unbound_installation);
            return 200;
        }
        if event == "push" {
            let branch = payload.get("ref").and_then(Value::as_str);
            let default = project.map(|binding| binding.project.default_branch.as_str());
            if branch.and_then(|full| full.strip_prefix("refs/heads/")) != default {
                // `gantry/` 分支等非默认分支：落盘前丢弃（FA §9.3）。
                bump(&self.stats.unsubscribed);
                return 200;
            }
        }
        let row = InboxRow {
            delivery_id: delivery.to_owned(),
            event: event.to_owned(),
            action: action.map(str::to_owned),
            installation_id,
            repo,
            received_at: self.deps.clock.now(),
            body: Some(body),
            status: InboxStatus::Received,
            attempts: 0,
            retry_at: None,
            last_error: None,
        };
        // 步 6–7：单事务、只做一次插入。
        match self.persist(&row).await {
            Ok(true) => {
                bump(&self.stats.accepted);
                self.wake.notify_one();
                202
            }
            Ok(false) => {
                bump(&self.stats.duplicates);
                200
            }
            Err(()) => {
                bump(&self.stats.unavailable);
                503
            }
        }
    }

    fn verify(&self, body: &[u8], header: &str) -> Verify {
        let Some(expected) = header.strip_prefix("sha256=").and_then(decode_hex) else {
            return Verify::Mismatch;
        };
        let names = std::iter::once(Some(&self.deps.secret))
            .chain(std::iter::once(self.deps.previous_secret.as_ref()))
            .flatten();
        let mut any = false;
        for name in names {
            let Ok(Some(secret)) = self.deps.secrets.get(name) else {
                continue;
            };
            any = true;
            let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.value.expose()) else {
                continue;
            };
            mac.update(body);
            if mac.verify_slice(&expected).is_ok() {
                return Verify::Ok;
            }
        }
        if any {
            Verify::Mismatch
        } else {
            Verify::Unavailable
        }
    }

    async fn persist(&self, row: &InboxRow) -> Result<bool, ()> {
        let mut tx = self.deps.store.begin_write().await.map_err(|_| ())?;
        let inserted = tx
            .module_conn(MODULE)
            .insert(INBOX_TABLE, &row.delivery_id, row)
            .map_err(|_| ())?;
        if inserted {
            tx.commit().await.map_err(|_| ())?;
        } else {
            tx.rollback().await.map_err(|_| ())?;
        }
        Ok(inserted)
    }
}

enum Verify {
    Ok,
    Mismatch,
    Unavailable,
}

async fn read_request(stream: &mut TcpStream, max_body: usize) -> std::io::Result<ReadOutcome> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Ok(ReadOutcome::Status(431));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(ReadOutcome::Status(400));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let Ok(head) = std::str::from_utf8(&buffer[..head_end]) else {
        return Ok(ReadOutcome::Status(400));
    };
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(ReadOutcome::Status(400));
    };
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        // GitHub 带 Content-Length；不接受分块，避免无上限缓冲。
        return Ok(ReadOutcome::Status(400));
    }
    let length = match headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse::<usize>())
    {
        Some(Ok(length)) => length,
        // 浏览器的 GET（setup 回调）不带 body。
        None if method == "GET" => 0,
        _ => return Ok(ReadOutcome::Status(400)),
    };
    // 步 1：先看 Content-Length，超限不读 body。
    if length > max_body {
        return Ok(ReadOutcome::Status(413));
    }
    let mut body = buffer[head_end + 4..].to_vec();
    if body.len() > length {
        return Ok(ReadOutcome::Status(400));
    }
    body.reserve(length - body.len());
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(ReadOutcome::Status(400));
        }
        if body.len() + read > length {
            return Ok(ReadOutcome::Status(400));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(ReadOutcome::Request(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body,
    }))
}

fn valid_delivery_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DELIVERY_ID
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(value.get(index..index + 2)?, 16).ok())
        .collect()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    }
}
