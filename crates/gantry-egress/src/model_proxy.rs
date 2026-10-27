//! 模型 API 反向代理（`docs/modules/egress.md` §6.1、§6.3、§6.4、§7.3；
//! `docs/interfaces/egress-control.md` §5）。
//!
//! 两个监听器共用一条流水线：沙盒侧 `sandbox_model`（路径 `/j/<job_token>/<provider>/…`）与
//! 回环 `svc_model`（`/s/<svc_token>/<provider>/…`，另带 `X-Gantry-Ledger` 等三个头）。
//! 上游主机永远来自配置；调用方的认证头一律丢弃，出站时注入 provider 的 key；响应流式
//! 透传并旁路解析用量；每个请求恰好一条审计。拒绝体与模型 API 错误同形：
//! `{"type":"error","error":{"type":"<code>","message":"…"}}`。

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use gantry_core::egress::LedgerRef;
use gantry_core::entities::Tier;
use gantry_core::ids::{AttemptId, Ulid};
use gantry_core::ports::secrets::SecretSource;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{HeaderMap, HeaderName, HeaderValue, CONNECTION, CONTENT_TYPE};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::audit::{AuditCaller, AuditEvent, AuditKind, AuditSink, Decision};
use crate::netdeny::Cidr;
use crate::registry::{
    Admitted, Caller, Denial, DeniedKind, Listener, Metered, Registry, SvcCallMeta, UpstreamClass,
};
use crate::review::ProviderUpstream;
use crate::upstream::{connect_tls, ConnectError, Connector};
use crate::usage::{BodyKind, UsageParser};

/// 固定的出站 User-Agent。
pub const USER_AGENT: &str = "gantry-egress/1";

/// 只复制这些请求头到上游（§7.3 步 6；头名需核实，OQ-E9）。
const FORWARD_HEADERS: &[&str] = &[
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
];

/// 响应里一律去掉的头（逐跳头、`set-cookie`、可能回显凭据的头）。
const STRIP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "set-cookie",
    "set-cookie2",
    "authorization",
    "x-api-key",
];

/// 限量与超时（§7.3 步 4–5、§10 [建议值，待调优]）。
#[derive(Clone, Debug)]
pub struct Limits {
    pub request_header_bytes: usize,
    pub request_body_bytes: usize,
    pub header_read_timeout: Duration,
    pub body_read_timeout: Duration,
    pub job_inflight: usize,
    pub job_requests_per_min: usize,
    /// 上游首字节（响应头）超时。
    pub upstream_first_byte: Duration,
    /// 上游两帧之间的空闲超时。
    pub upstream_idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_header_bytes: 16 * 1024,
            request_body_bytes: 32 * 1024 * 1024,
            header_read_timeout: Duration::from_secs(10),
            body_read_timeout: Duration::from_secs(30),
            job_inflight: 8,
            job_requests_per_min: 120,
            upstream_first_byte: Duration::from_secs(300),
            upstream_idle: Duration::from_secs(300),
        }
    }
}

pub type ProxyBody = http_body_util::combinators::BoxBody<Bytes, io::Error>;

/// 模型反向代理。
pub struct ModelProxy {
    registry: Registry,
    secrets: Arc<dyn SecretSource>,
    audit: Arc<dyn AuditSink>,
    connector: Connector,
    tls: Arc<rustls::ClientConfig>,
    limits: Limits,
    /// `Some` = 网段检查可强制（P-73）：沙盒侧令牌只接受这些来源。
    sandbox_cidrs: Option<Vec<Cidr>>,
    rate: Mutex<HashMap<AttemptId, RateState>>,
}

#[derive(Default)]
struct RateState {
    inflight: usize,
    recent: VecDeque<Instant>,
}

/// 一次请求在审计里需要的上下文。
struct Trace {
    listener: Listener,
    src: SocketAddr,
    method: String,
    path: String,
    started: Instant,
    caller: Option<AuditCaller>,
    provider: Option<ProviderUpstream>,
    provider_id: Option<String>,
}

/// 拒绝：状态码、对外错误类型、审计规则。
struct Reject {
    status: StatusCode,
    error_type: &'static str,
    rule: &'static str,
    message: &'static str,
}

impl Reject {
    fn new(
        status: u16,
        error_type: &'static str,
        rule: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            status: StatusCode::from_u16(status).expect("valid status"),
            error_type,
            rule,
            message,
        }
    }

    fn from_denial(denial: Denial) -> Self {
        Self {
            status: StatusCode::from_u16(denial.status()).expect("valid status"),
            error_type: denial.error_type(),
            rule: denial.rule(),
            message: "request denied by egress",
        }
    }
}

impl ModelProxy {
    pub fn new(
        registry: Registry,
        secrets: Arc<dyn SecretSource>,
        audit: Arc<dyn AuditSink>,
        connector: Connector,
        tls: Arc<rustls::ClientConfig>,
        limits: Limits,
        sandbox_cidrs: Option<Vec<Cidr>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            secrets,
            audit,
            connector,
            tls,
            limits,
            sandbox_cidrs,
            rate: Mutex::new(HashMap::new()),
        })
    }

    /// accept 循环。
    pub async fn serve(self: Arc<Self>, listener: TcpListener, kind: Listener) -> io::Result<()> {
        loop {
            let (stream, src) = listener.accept().await?;
            let proxy = self.clone();
            let limits = self.limits.clone();
            tokio::spawn(async move {
                let mut stream = stream;
                // 先自己读完请求头并校验帧（hyper 遇到 CL 与 TE 并存会静默丢掉 CL）；
                // 关闭 keep-alive，使每个连接只有一个请求、每个请求都过这道校验。
                let head = tokio::time::timeout(
                    limits.header_read_timeout,
                    read_head(&mut stream, limits.request_header_bytes),
                )
                .await;
                let head = match head {
                    Ok(Ok(head)) => head,
                    Ok(Err(HeadError::TooLarge)) => {
                        proxy
                            .refuse_raw(
                                stream,
                                src,
                                431,
                                "request_too_large",
                                "request.header_too_large",
                            )
                            .await;
                        return;
                    }
                    Ok(Err(HeadError::Io)) => return,
                    Err(_) => {
                        proxy
                            .refuse_raw(stream, src, 408, "request_timeout", "request.timeout")
                            .await;
                        return;
                    }
                };
                if !framing_ok(&head) {
                    proxy
                        .refuse_raw(stream, src, 400, "invalid_request", "request.framing")
                        .await;
                    return;
                }
                let service = hyper::service::service_fn(move |req| {
                    let proxy = proxy.clone();
                    async move { Ok::<_, Infallible>(proxy.handle(req, kind, src).await) }
                });
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(TokioTimer::new())
                    .keep_alive(false)
                    .max_buf_size(limits.request_header_bytes.max(MIN_BUF));
                let _ = builder
                    .serve_connection(TokioIo::new(Rewind::new(head, stream)), service)
                    .await;
            });
        }
    }

    /// 在 HTTP 层之前拒绝（头过大 / 超时 / 帧冲突）：固定错误体、断开连接、写审计。
    async fn refuse_raw(
        &self,
        mut stream: TcpStream,
        src: SocketAddr,
        status: u16,
        error_type: &'static str,
        rule: &'static str,
    ) {
        use tokio::io::AsyncWriteExt;
        let body = json!({"type": "error", "error": {"type": error_type, "message": "request rejected by egress"}}).to_string();
        let reason = match status {
            408 => "Request Timeout",
            431 => "Request Header Fields Too Large",
            _ => "Bad Request",
        };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.shutdown().await;
        let mut event = AuditEvent::new(
            AuditKind::Model,
            AuditCaller::Unknown {
                src: src.ip().to_string(),
            },
            Decision::Deny,
        );
        event.rule = Some(rule);
        event.status = Some(status);
        event.src = Some(src.ip().to_string());
        self.write_audit(event);
    }

    pub async fn handle(
        self: Arc<Self>,
        req: Request<Incoming>,
        listener: Listener,
        src: SocketAddr,
    ) -> Response<ProxyBody> {
        let mut trace = Trace {
            listener,
            src,
            method: req.method().to_string(),
            path: redact_tokens(req.uri().path()),
            started: Instant::now(),
            caller: None,
            provider: None,
            provider_id: None,
        };
        match self.clone().pipeline(req, &mut trace).await {
            Ok(response) => response,
            Err(reject) => self.reject(&trace, reject),
        }
    }

    async fn pipeline(
        self: Arc<Self>,
        req: Request<Incoming>,
        trace: &mut Trace,
    ) -> Result<Response<ProxyBody>, Reject> {
        // 绝对形式 URI（`POST http://evil/…`）：上游主机只来自配置。
        if req.uri().scheme().is_some() || req.uri().authority().is_some() {
            return Err(Reject::new(
                403,
                "path_not_allowed",
                "path.absolute_uri",
                "absolute-form request targets are not allowed",
            ));
        }
        let route = Route::parse(req.uri().path(), req.uri().query())
            .ok_or_else(|| Reject::new(404, "not_found", "path.not_found", "unknown route"))?;
        trace.path = format!("/{}", route.rest.trim_start_matches('/'));
        let expected = match trace.listener {
            Listener::Sandbox => "j",
            Listener::Loopback => "s",
        };
        if route.prefix != expected {
            return Err(Reject::from_denial(Denial::TokenInvalid {
                rule: "token.wrong_listener",
            }));
        }
        if trace.listener == Listener::Sandbox {
            if let Some(cidrs) = &self.sandbox_cidrs {
                if !cidrs.iter().any(|cidr| cidr.contains(trace.src.ip())) {
                    return Err(Reject::from_denial(Denial::TokenInvalid {
                        rule: "token.source_not_allowed",
                    }));
                }
            }
        }
        let caller = self
            .registry
            .authenticate(trace.listener, &route.token)
            .map_err(Reject::from_denial)?;
        trace.provider_id = Some(route.provider.clone());
        let svc_meta = match &caller {
            Caller::Svc { svc_token_id } => {
                let meta = ledger_headers(req.headers()).ok_or_else(|| {
                    trace.caller = Some(AuditCaller::Svc {
                        svc_token_id: svc_token_id.clone(),
                        tier: None,
                        call_id: None,
                    });
                    Reject::new(
                        400,
                        "ledger_header_invalid",
                        "ledger.header_invalid",
                        "X-Gantry-Ledger, x-gantry-tier and x-gantry-call-id are required",
                    )
                })?;
                trace.caller = Some(AuditCaller::Svc {
                    svc_token_id: svc_token_id.clone(),
                    tier: Some(meta.tier),
                    call_id: Some(meta.call_id),
                });
                Some(meta)
            }
            Caller::Job { job_id } => {
                trace.caller = Some(AuditCaller::Job {
                    job_id: *job_id,
                    tier: self.registry.job_tier(job_id).unwrap_or(Tier::Worker),
                });
                None
            }
        };

        // 方法 / 路径白名单（规范化之前就拒绝可疑写法）。
        let kind = match check_path(req.method(), &route.rest) {
            Ok(kind) => kind,
            Err(reject) => {
                if reject.error_type == "path_not_allowed" {
                    let _ = self
                        .registry
                        .note_denied(&caller, DeniedKind::PathNotAllowed);
                }
                return Err(reject);
            }
        };

        let slot = match &caller {
            Caller::Job { job_id } => Some(self.rate_slot(*job_id)?),
            Caller::Svc { .. } => None,
        };

        let forward = forward_headers(req.headers());
        let body = tokio::time::timeout(
            self.limits.body_read_timeout,
            Limited::new(req.into_body(), self.limits.request_body_bytes).collect(),
        )
        .await
        .map_err(|_| {
            Reject::new(
                408,
                "request_timeout",
                "request.timeout",
                "request body was not received in time",
            )
        })?
        .map_err(|_| {
            Reject::new(
                413,
                "request_too_large",
                "request.too_large",
                "request body is too large",
            )
        })?
        .to_bytes();
        let model = model_of(&body);

        let config = self
            .registry
            .config()
            .ok_or_else(|| Reject::from_denial(Denial::Unavailable))?;
        let provider_config = config
            .view
            .providers
            .get(&route.provider)
            .ok_or_else(|| Reject::from_denial(Denial::ProviderNotAllowed))?;
        let secret = match self.secrets.get(&provider_config.secret_ref) {
            Ok(Some(secret)) => secret,
            _ => {
                return Err(Reject::new(
                    503,
                    "credential_missing",
                    "credential.missing",
                    "the provider credential is not configured",
                ))
            }
        };

        let admitted = self
            .registry
            .admit(&caller, &route.provider, &model, svc_meta.as_ref())
            .map_err(Reject::from_denial)?;
        trace.provider = Some(admitted.provider.clone());

        let key = HeaderValue::from_bytes(secret.value.expose()).map_err(|_| {
            Reject::new(
                503,
                "credential_missing",
                "credential.invalid",
                "the provider credential cannot be sent",
            )
        });
        let key = match key {
            Ok(mut key) => {
                key.set_sensitive(true);
                key
            }
            Err(reject) => {
                self.registry
                    .finish(admitted, metered_failure(UpstreamClass::ConnError))
                    .ok();
                return Err(reject);
            }
        };
        let upstream_path = format!(
            "{}{}{}",
            admitted.provider.base_path,
            route.rest,
            route.query.map(|q| format!("?{q}")).unwrap_or_default()
        );
        self.forward(
            admitted,
            trace,
            Upstream {
                method: route_method(&kind),
                path: upstream_path,
                headers: forward,
                key,
                body,
                kind,
                call_id: svc_meta.map(|meta| meta.call_id),
                secret: secret.value.expose().to_vec(),
            },
            slot,
        )
        .await
    }

    async fn forward(
        self: Arc<Self>,
        admitted: Admitted,
        trace: &Trace,
        upstream: Upstream,
        slot: Option<RateSlot>,
    ) -> Result<Response<ProxyBody>, Reject> {
        let provider = admitted.provider.clone();
        let request_bytes = upstream.body.len() as u64;
        let connected = connect_tls(&self.connector, &self.tls, &provider.host).await;
        let (tls, ip) = match connected {
            Ok(ok) => ok,
            Err(error) => {
                let (status, rule) = match error {
                    ConnectError::Denied => (403, "hard.private_ip"),
                    ConnectError::Timeout => (504, "upstream.timeout"),
                    _ => (502, "upstream.connect"),
                };
                self.registry
                    .finish(admitted, metered_failure(UpstreamClass::ConnError))
                    .ok();
                return Err(Reject::new(
                    status,
                    "upstream_unavailable",
                    rule,
                    "the upstream provider is unavailable",
                ));
            }
        };
        let (mut sender, conn) =
            match hyper::client::conn::http1::handshake(TokioIo::new(tls)).await {
                Ok(pair) => pair,
                Err(_) => {
                    self.registry
                        .finish(admitted, metered_failure(UpstreamClass::ConnError))
                        .ok();
                    return Err(Reject::new(
                        502,
                        "upstream_unavailable",
                        "upstream.connect",
                        "the upstream provider is unavailable",
                    ));
                }
            };
        let conn_task = tokio::spawn(conn);

        let mut builder = Request::builder()
            .method(upstream.method)
            .uri(&upstream.path)
            .header(hyper::header::HOST, provider.host.as_str())
            .header(hyper::header::USER_AGENT, USER_AGENT);
        for (name, value) in upstream.headers.iter() {
            builder = builder.header(name, value);
        }
        let request = builder
            .header("x-api-key", upstream.key)
            .body(Full::new(upstream.body))
            .expect("valid upstream request");

        let response = tokio::time::timeout(
            self.limits.upstream_first_byte,
            sender.send_request(request),
        )
        .await;
        let response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(_)) | Err(_) => {
                conn_task.abort();
                let timed_out = response.is_err();
                self.registry
                    .finish(admitted, metered_failure(UpstreamClass::ConnError))
                    .ok();
                return Err(if timed_out {
                    Reject::new(
                        504,
                        "upstream_unavailable",
                        "upstream.timeout",
                        "the upstream provider timed out",
                    )
                } else {
                    Reject::new(
                        502,
                        "upstream_unavailable",
                        "upstream.connect",
                        "the upstream provider is unavailable",
                    )
                });
            }
        };

        let status = response.status();
        let class = match status.as_u16() {
            429 => UpstreamClass::Status429,
            500..=599 => UpstreamClass::Status5xx,
            _ => UpstreamClass::Ok,
        };
        let is_sse = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let body_kind = match upstream.kind {
            PathKind::CountTokens => BodyKind::CountTokens,
            _ if is_sse => BodyKind::EventStream,
            _ => BodyKind::Json,
        };
        let (parts, upstream_body) = response.into_parts();
        let mut out = Response::from_parts(parts, ());
        clean_response_headers(out.headers_mut(), &upstream.secret);
        if let Some(call_id) = upstream.call_id {
            if let Ok(value) = HeaderValue::from_str(&call_id.to_string()) {
                out.headers_mut().insert("x-gantry-call-id", value);
            }
        }

        let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, io::Error>>(16);
        let proxy = self.clone();
        let mut event = self.event(trace, Decision::Allow, None);
        event.status = Some(status.as_u16());
        event.resolved_ip = Some(ip.to_string());
        let idle = self.limits.upstream_idle;
        let started = trace.started;
        tokio::spawn(async move {
            let _slot = slot;
            let mut parser = UsageParser::new(body_kind);
            let mut body = upstream_body;
            let mut completed = false;
            let mut caller_gone = false;
            loop {
                match tokio::time::timeout(idle, body.frame()).await {
                    Ok(Some(Ok(frame))) => {
                        if let Some(data) = frame.data_ref() {
                            parser.feed(data);
                            event.bytes_out += data.len() as u64;
                        }
                        // 只转发数据帧；上游的 trailers 不透传。
                        if let Ok(data) = frame.into_data() {
                            if tx.send(Ok(Frame::data(data))).await.is_err() {
                                caller_gone = true; // 调用方断开：放弃上游
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        completed = true;
                        break;
                    }
                    Ok(Some(Err(_))) | Err(_) => {
                        let _ = tx
                            .send(Err(io::Error::other("upstream stream interrupted")))
                            .await;
                        break;
                    }
                }
            }
            drop(body);
            conn_task.abort();
            let parsed = parser.finish(completed, status.is_success(), request_bytes);
            // 调用方自己断开不算上游错误。
            let upstream_class = if completed || caller_gone {
                class
            } else {
                UpstreamClass::ConnError
            };
            let metered = Metered {
                usage: parsed.usage,
                incomplete: parsed.incomplete,
                upstream: upstream_class,
            };
            let usage = parsed.usage;
            let _ = proxy.registry.finish(admitted, metered);
            event.bytes_in = request_bytes;
            event.usage = Some(usage.into());
            event.duration_ms = started.elapsed().as_millis() as u64;
            if !completed {
                event.error = Some("stream interrupted".into());
            }
            proxy.write_audit(event);
        });

        let body = ChannelBody { rx };
        Ok(out.map(|()| body.boxed()))
    }

    fn rate_slot(self: &Arc<Self>, job_id: AttemptId) -> Result<RateSlot, Reject> {
        let now = Instant::now();
        let mut rate = self.rate.lock().unwrap_or_else(|p| p.into_inner());
        let state = rate.entry(job_id).or_default();
        while state
            .recent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            state.recent.pop_front();
        }
        if state.inflight >= self.limits.job_inflight
            || state.recent.len() >= self.limits.job_requests_per_min
        {
            return Err(Reject::new(
                429,
                "rate_limited",
                "rate.limited",
                "too many requests for this job",
            ));
        }
        state.inflight += 1;
        state.recent.push_back(now);
        Ok(RateSlot {
            proxy: self.clone(),
            job_id,
        })
    }

    fn event(&self, trace: &Trace, decision: Decision, rule: Option<&'static str>) -> AuditEvent {
        let caller = trace.caller.clone().unwrap_or(AuditCaller::Unknown {
            src: trace.src.ip().to_string(),
        });
        let mut event = AuditEvent::new(AuditKind::Model, caller, decision);
        event.rule = rule;
        event.method = Some(trace.method.clone());
        event.path = Some(trace.path.clone());
        event.src = Some(trace.src.ip().to_string());
        event.provider = trace.provider_id.clone();
        if let Some(provider) = &trace.provider {
            event.host = Some(provider.host.to_string());
            event.port = Some(443);
        }
        event.duration_ms = trace.started.elapsed().as_millis() as u64;
        event
    }

    fn reject(&self, trace: &Trace, reject: Reject) -> Response<ProxyBody> {
        let mut event = self.event(trace, Decision::Deny, Some(reject.rule));
        event.status = Some(reject.status.as_u16());
        self.write_audit(event);
        let body = json!({
            "type": "error",
            "error": { "type": reject.error_type, "message": reject.message }
        });
        let mut response = Response::new(
            Full::new(Bytes::from(body.to_string()))
                .map_err(|never: Infallible| match never {})
                .boxed(),
        );
        *response.status_mut() = reject.status;
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if matches!(reject.status.as_u16(), 400 | 408 | 413) {
            response
                .headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("close"));
        }
        response
    }

    fn write_audit(&self, event: AuditEvent) {
        if self.audit.record(event).is_err() {
            // 审计写失败：失败闭合（拒绝新请求、unhealthy，OQ-E10）。
            self.registry.set_audit_failed(true);
        }
    }
}

/// hyper 要求的最小读缓冲。
const MIN_BUF: usize = 8 * 1024;

struct RateSlot {
    proxy: Arc<ModelProxy>,
    job_id: AttemptId,
}

impl Drop for RateSlot {
    fn drop(&mut self) {
        let mut rate = self.proxy.rate.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(state) = rate.get_mut(&self.job_id) {
            state.inflight = state.inflight.saturating_sub(1);
        }
    }
}

struct Upstream {
    method: Method,
    path: String,
    headers: HeaderMap,
    key: HeaderValue,
    body: Bytes,
    kind: PathKind,
    call_id: Option<Ulid>,
    secret: Vec<u8>,
}

/// `/<prefix>/<token>/<provider>/<rest>`。
struct Route {
    prefix: String,
    token: String,
    provider: String,
    /// 以 `/` 开头的剩余路径（原样，未解码）。
    rest: String,
    query: Option<String>,
}

impl Route {
    fn parse(path: &str, query: Option<&str>) -> Option<Self> {
        let mut parts = path.strip_prefix('/')?.splitn(4, '/');
        let prefix = parts.next()?;
        if prefix != "j" && prefix != "s" {
            return None;
        }
        let token = parts.next().filter(|t| !t.is_empty())?;
        let provider = parts.next().filter(|p| !p.is_empty())?;
        let rest = format!("/{}", parts.next()?);
        Some(Self {
            prefix: prefix.to_owned(),
            token: token.to_owned(),
            provider: provider.to_owned(),
            rest,
            query: query.map(str::to_owned),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKind {
    Messages,
    CountTokens,
    Models,
}

fn route_method(kind: &PathKind) -> Method {
    match kind {
        PathKind::Messages | PathKind::CountTokens => Method::POST,
        PathKind::Models => Method::GET,
    }
}

/// Anthropic 的方法 / 路径白名单（§7.3 步 3 [提议，需核实当前端点]）。
fn check_path(method: &Method, rest: &str) -> Result<PathKind, Reject> {
    let lower = rest.to_ascii_lowercase();
    let suspicious = rest.contains("//")
        || rest.contains('\\')
        || rest
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        || lower.contains("%2e")
        || lower.contains("%2f")
        || lower.contains("%5c")
        || lower.contains("%00");
    let kind = if suspicious {
        None
    } else {
        match rest {
            "/v1/messages" => Some(PathKind::Messages),
            "/v1/messages/count_tokens" => Some(PathKind::CountTokens),
            "/v1/models" => Some(PathKind::Models),
            _ => None,
        }
    };
    let Some(kind) = kind else {
        return Err(Reject::new(
            403,
            "path_not_allowed",
            "path.not_allowed",
            "this path is not allowed",
        ));
    };
    if *method != route_method(&kind) {
        return Err(Reject::new(
            403,
            "method_not_allowed",
            "method.not_allowed",
            "this method is not allowed",
        ));
    }
    Ok(kind)
}

fn forward_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in FORWARD_HEADERS {
        for value in headers.get_all(*name) {
            out.append(HeaderName::from_static(name), value.clone());
        }
    }
    out
}

fn clean_response_headers(headers: &mut HeaderMap, secret: &[u8]) {
    let named_in_connection: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    for name in STRIP_RESPONSE_HEADERS
        .iter()
        .map(|n| n.to_string())
        .chain(named_in_connection)
    {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    let leaking: Vec<HeaderName> = headers
        .iter()
        .filter(|(_, value)| contains(value.as_bytes(), secret))
        .map(|(name, _)| name.clone())
        .collect();
    for name in leaking {
        headers.remove(name);
    }
}

/// 审计里的路径不带令牌原文：形如令牌的路径段替换为 `***`。
fn redact_tokens(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with(gantry_core::egress::JOB_TOKEN_PREFIX)
                || segment.starts_with(gantry_core::egress::SVC_TOKEN_PREFIX)
            {
                "***"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// 回环请求的三个可信头（P-74）。缺任一或格式不符 → `None`。
fn ledger_headers(headers: &HeaderMap) -> Option<SvcCallMeta> {
    let text = |name: &str| -> Option<&str> {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?;
        values.next().is_none().then_some(value)
    };
    let ledger = match text("x-gantry-ledger")? {
        "unattributed" => None,
        raw if raw.starts_with("dlv_") => Some(LedgerRef::Delivery {
            id: raw.parse().ok()?,
        }),
        raw if raw.starts_with("cnv_") => {
            let (id, day) = raw.split_once(':')?;
            Some(LedgerRef::Conversation {
                id: id.parse().ok()?,
                day: day.parse().ok()?,
            })
        }
        _ => return None,
    };
    let tier = match text("x-gantry-tier")? {
        "utility" => Tier::Utility,
        "planner" => Tier::Planner,
        _ => return None,
    };
    let call_id = text("x-gantry-call-id")?.parse::<Ulid>().ok()?;
    Some(SvcCallMeta {
        call_id,
        ledger,
        tier,
    })
}

fn model_of(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|model| model.len() <= 200 && !model.chars().any(char::is_control))
        .unwrap_or_else(|| "unknown".into())
}

fn metered_failure(upstream: UpstreamClass) -> Metered {
    Metered {
        usage: Default::default(),
        incomplete: false,
        upstream,
    }
}

/// 把上游帧转发给调用方的通道体。
struct ChannelBody {
    rx: mpsc::Receiver<Result<Frame<Bytes>, io::Error>>,
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.rx.poll_recv(cx)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

enum HeadError {
    TooLarge,
    Io,
}

/// 读到 `\r\n\r\n`（含）为止；返回已读字节（可能多读一点请求体，交给 `Rewind` 回放）。
async fn read_head(stream: &mut TcpStream, limit: usize) -> Result<Vec<u8>, HeadError> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find_head_end(&buf) {
            if end > limit {
                return Err(HeadError::TooLarge);
            }
            return Ok(buf);
        }
        if buf.len() > limit {
            return Err(HeadError::TooLarge);
        }
        let n = stream.read(&mut chunk).await.map_err(|_| HeadError::Io)?;
        if n == 0 {
            return Err(HeadError::Io);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// 拒绝 `Content-Length` 与 `Transfer-Encoding` 并存、重复的 `Content-Length`、
/// 非 `chunked` 的 `Transfer-Encoding`（请求走私的常见形状）。
fn framing_ok(buf: &[u8]) -> bool {
    let Some(end) = find_head_end(buf) else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&buf[..end]) else {
        return false;
    };
    let mut content_length = 0;
    let mut transfer_encoding = 0;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.ends_with(' ') || name.ends_with('\t') {
            return false;
        }
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length += 1,
            "transfer-encoding" => {
                transfer_encoding += 1;
                if !value.trim().eq_ignore_ascii_case("chunked") {
                    return false;
                }
            }
            _ => {}
        }
    }
    content_length <= 1
        && transfer_encoding <= 1
        && !(content_length == 1 && transfer_encoding == 1)
}

/// 先回放已读的请求头字节，再读底层连接。
struct Rewind {
    prefix: Bytes,
    inner: TcpStream,
}

impl Rewind {
    fn new(prefix: Vec<u8>, inner: TcpStream) -> Self {
        Self {
            prefix: Bytes::from(prefix),
            inner,
        }
    }
}

impl tokio::io::AsyncRead for Rewind {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            let chunk = self.prefix.split_to(n);
            buf.put_slice(&chunk);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Rewind {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
