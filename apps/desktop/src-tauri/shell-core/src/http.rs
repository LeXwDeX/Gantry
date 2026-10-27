//! HTTPS 客户端（desktop-shell.md §7.1"重试""并发""超时"、§1 第 3 条）。
//!
//! 桌面端所有网络请求只由外壳发出（WebView 不联网，S-6）。每次请求新建一条连接：
//! TCP → TLS 1.3（[`crate::tls::client_config`]：只信固定根，mTLS 时出示设备证书）→ HTTP/1.1。
//!
//! - 超时：连接 10 s、非流式请求 30 s [建议值]；
//! - 并发：在途请求 ≤ 8，超出排队（防 UI 刷新风暴触发 `429`）；
//! - 重试：网络错误与 `5xx` 在"读路由，或带幂等键的写路由"上退避重试，沿用同一 `Idempotency-Key`；
//!   `429` / `503` 按 `Retry-After` 加抖动；其余 `4xx` 不重试。

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_rustls::TlsConnector;

use crate::routes::PreparedRequest;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_INFLIGHT: usize = 8;
const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// 响应体上限：服务端列表页最大 200 项，远小于此值。
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// 无 HTTP 响应的失败（`ApiResult.network`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    Timeout,
    /// TLS 层失败：证书不受信、握手被拒（吊销 / 过期的客户端证书也表现为这里）。
    Tls,
    Offline,
    Aborted,
}

impl NetError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::Offline => "offline",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `X-Log-Epoch`（P-67）：外壳自己比对，不转给 UI。
    pub fn log_epoch(&self) -> Option<u64> {
        self.header("x-log-epoch")?.trim().parse().ok()
    }

    fn retry_after(&self) -> Option<Duration> {
        let secs: u64 = self.header("retry-after")?.trim().parse().ok()?;
        Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
    }
}

/// `https://host:port` 形式的服务端地址。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub host: String,
    pub port: u16,
}

impl Origin {
    /// 只接受 `https://host[:port]`，不带路径以外的东西（`api_base_url` 来自配对响应，§7.2）。
    pub fn parse(base_url: &str) -> Option<Self> {
        let url = url::Url::parse(base_url).ok()?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return None;
        }
        Some(Self {
            host: url.host_str()?.trim_matches(['[', ']']).to_string(),
            port: url.port_or_known_default()?,
        })
    }
}

pub struct HttpClient {
    origin: Origin,
    tls: TlsConnector,
    inflight: Arc<Semaphore>,
    connect_timeout: Duration,
    request_timeout: Duration,
    retry_base: Duration,
}

impl HttpClient {
    pub fn new(origin: Origin, tls: Arc<ClientConfig>) -> Self {
        Self {
            origin,
            tls: TlsConnector::from(tls),
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT)),
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
            retry_base: Duration::from_millis(250),
        }
    }

    /// 测试用：缩短超时与退避。
    pub fn with_timeouts(
        mut self,
        connect: Duration,
        request: Duration,
        retry_base: Duration,
    ) -> Self {
        self.connect_timeout = connect;
        self.request_timeout = request;
        self.retry_base = retry_base;
        self
    }

    /// 单次请求，不重试。`headers` 由外壳决定（UI 不能传任何头，§7.1"头"）。
    pub async fn send(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, NetError> {
        let _permit = self
            .inflight
            .acquire()
            .await
            .map_err(|_| NetError::Aborted)?;
        tokio::time::timeout(
            self.request_timeout,
            self.send_inner(method, path_and_query, body, headers),
        )
        .await
        .map_err(|_| NetError::Timeout)?
    }

    async fn send_inner(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, NetError> {
        let tcp = tokio::time::timeout(
            self.connect_timeout,
            TcpStream::connect((self.origin.host.as_str(), self.origin.port)),
        )
        .await
        .map_err(|_| NetError::Timeout)?
        .map_err(|_| NetError::Offline)?;
        let name = ServerName::try_from(self.origin.host.clone()).map_err(|_| NetError::Tls)?;
        let tls = self
            .tls
            .connect(name, tcp)
            .await
            .map_err(|_| NetError::Tls)?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|_| NetError::Aborted)?;
        let driver = tokio::spawn(conn);

        let authority = format!("{}:{}", self.origin.host, self.origin.port);
        let mut builder = Request::builder()
            .method(method)
            .uri(path_and_query)
            .header("host", authority)
            .header("accept", "application/json, application/problem+json");
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(Full::new(Bytes::copy_from_slice(body.unwrap_or_default())))
            .map_err(|_| NetError::Aborted)?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| NetError::Aborted)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(n, v)| Some((n.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let limited = http_body_util::Limited::new(response.into_body(), MAX_RESPONSE_BYTES);
        let body = limited
            .collect()
            .await
            .map_err(|_| NetError::Aborted)?
            .to_bytes()
            .to_vec();
        driver.abort();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    /// `api_request` 的发送：按 §7.1 的重试规则。
    pub async fn send_prepared(&self, req: &PreparedRequest) -> Result<HttpResponse, NetError> {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(key) = &req.idempotency_key {
            headers.push(("idempotency-key", key));
        }
        let retriable = req.method == "GET" || req.idempotency_key.is_some();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self
                .send(
                    req.method,
                    &req.path_and_query,
                    req.body.as_deref(),
                    &headers,
                )
                .await;
            let wait = match &result {
                Ok(resp) if matches!(resp.status, 429 | 503) => {
                    resp.retry_after().unwrap_or_else(|| self.backoff(attempt))
                }
                Ok(resp) if resp.status >= 500 && retriable => self.backoff(attempt),
                // TLS 失败不重试：多半是证书问题，交给连接状态机计数（§7.3"握手失败"）。
                Err(NetError::Offline | NetError::Timeout | NetError::Aborted) if retriable => {
                    self.backoff(attempt)
                }
                _ => return result,
            };
            if attempt >= MAX_ATTEMPTS {
                return result;
            }
            tokio::time::sleep(jitter(wait)).await;
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        self.retry_base * 2u32.saturating_pow(attempt - 1)
    }
}

/// 在 `[d, 1.5 d)` 内加抖动，避免多台客户端同时重试。
fn jitter(d: Duration) -> Duration {
    let mut b = [0u8; 2];
    let _ = getrandom::getrandom(&mut b);
    let frac = u16::from_le_bytes(b) as f64 / u16::MAX as f64;
    d + d.mul_f64(frac * 0.5)
}

// ---- 配对传输 ----

/// 配对端点：单向 TLS、只信配对串固定的根（api.md §1.3）。
pub struct HttpPairTransport {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for HttpPairTransport {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

#[async_trait::async_trait]
impl crate::pairing::PairTransport for HttpPairTransport {
    async fn pair(
        &self,
        endpoint: &crate::pairing::PairEndpoint,
        body: &crate::pairing::PairRequestBody,
    ) -> Result<crate::pairing::PairReply, crate::pairing::TransportError> {
        use crate::pairing::{PairReply, TransportError};
        let pinned = crate::tls::RootFingerprint::parse(&endpoint.ca_fingerprint)
            .ok_or(TransportError::Untrusted)?;
        let client = HttpClient::new(
            Origin {
                host: endpoint.host.clone(),
                port: endpoint.port,
            },
            crate::tls::client_config(pinned, None),
        )
        .with_timeouts(self.connect_timeout, self.request_timeout, Duration::ZERO);
        let bytes = serde_json::to_vec(body).map_err(|_| TransportError::Network)?;
        // 不重试：token 一次有效，响应丢失只能重新生成配对串（api.md §1.3）。
        let resp = client
            .send("POST", "/v1/pair", Some(&bytes), &[])
            .await
            .map_err(|e| match e {
                NetError::Tls => TransportError::Untrusted,
                _ => TransportError::Network,
            })?;
        match resp.status {
            201 => serde_json::from_slice(&resp.body)
                .map(|b| PairReply::Created(Box::new(b)))
                .map_err(|_| TransportError::Network),
            401 => Ok(PairReply::TokenInvalid),
            status => Ok(PairReply::Problem {
                status,
                code: serde_json::from_slice::<serde_json::Value>(&resp.body)
                    .ok()
                    .and_then(|v| v["code"].as_str().map(str::to_string))
                    .unwrap_or_default(),
            }),
        }
    }
}
