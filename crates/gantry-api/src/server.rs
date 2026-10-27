//! 请求处理管线（本文 §7.1，顺序固定，前一步失败立即返回）。
//!
//! 管线分两段，让传输层在读请求体之前就能拒绝请求（"限流先于 body 解析"）：
//!
//! - [`ApiCore::admit`]：步骤 1–7（只看请求头）；
//! - [`ApiCore::complete`]：步骤 8–10、13、14（拿到请求体之后）。
//!
//! 传输层（TLS 监听器，后续切片）按 [`Admitted::body_limit`] 读至多 `limit + 1`
//! 字节交给 `complete`。步骤 12（请求体 schema）由各处理程序按路由的请求体组件
//! 执行（[`crate::validate`]）。

use std::sync::Arc;

use crate::settings::SettingsContext;
use async_trait::async_trait;
use bytes::Bytes;
use gantry_core::entities::Seq;
use gantry_core::ids::{Clock, IdGen};
use gantry_core::ports::conversation::ConversationService;
use gantry_core::ports::egress::EgressControl;
use gantry_core::ports::engine::DeliveryControl;
use gantry_core::ports::forge::PrRefresher;
use gantry_policy::ApprovalService;
use gantry_store::Store;
use http::header::{
    HeaderName, HeaderValue, ALLOW, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
    TRANSFER_ENCODING, X_CONTENT_TYPE_OPTIONS,
};
use http::{HeaderMap, Method, Response, StatusCode, Uri};
use serde_json::Value;
use tokio::sync::watch;

use crate::auth::{self, AuthContext, PeerCert};
use crate::consts::{IDEMPOTENCY_KEY_MAX, IDEMPOTENCY_KEY_MIN};
use crate::handlers;
use crate::idempotency::{self, Begin, IdempotencyStore, StoredResponse};
use crate::problem::{Problem, ProblemCode};
use crate::query::{self, Query};
use crate::rate::{RateLimiter, RateState};
use crate::registry::{self, IdemPolicy, Milestone, RateClass, Registry, RouteMatch, RouteSpec};
use crate::sse::{EventStream, StreamRegistry};
use gantry_pki::admin::DeviceAdmin;
use gantry_pki::directory::DeviceDirectory;
use gantry_pki::CertSerial;

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
pub const X_LOG_EPOCH: HeaderName = HeaderName::from_static("x-log-epoch");
pub const X_EVENT_SEQ: HeaderName = HeaderName::from_static("x-event-seq");
pub const RATELIMIT_LIMIT: HeaderName = HeaderName::from_static("ratelimit-limit");
pub const RATELIMIT_REMAINING: HeaderName = HeaderName::from_static("ratelimit-remaining");
pub const RATELIMIT_RESET: HeaderName = HeaderName::from_static("ratelimit-reset");
pub const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
pub const IDEMPOTENT_REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");

const PROBLEM_JSON: &str = "application/problem+json; charset=utf-8";
const JSON: &str = "application/json";
/// 响应的媒体类型（`openapi.yaml` 的响应 content 键）。
const JSON_UTF8: &str = "application/json; charset=utf-8";

/// 本模块的静态配置（本文 §10；限流等常量在 [`crate::consts`]）。
#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// 已上线到哪个里程碑；更晚的路由按未注册处理（`404`，本文 §1.1）。
    pub enabled: Milestone,
    /// 服务端构建版本（`/v1/me` 的 `server.version`）。
    pub server_version: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: Milestone::M1,
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// 装配时注入的依赖（本文 §6.1；随切片逐步补齐）。
#[derive(Clone)]
pub struct Services {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<IdGen>,
    pub store: Store,
    /// `pki` 的设备目录（鉴权热路径、`GET /v1/devices`、心跳内吊销检查）。
    pub devices: Arc<dyn DeviceDirectory>,
    /// `pki` 的设备管理（改名、改角色、吊销、续期）。
    pub admin: Arc<dyn DeviceAdmin>,
    /// 审批服务（policy，同泳道接口）：#24–#29 与内部动作的消费。
    pub approvals: Arc<dyn ApprovalService>,
    /// grant 前对 PR 强制对账一次（forge）。
    pub forge: Arc<dyn PrRefresher>,
    /// settings 的校验上下文、catalog 与 status 聚合（本文 §6.3）。
    pub settings_ctx: Arc<dyn SettingsContext>,
    /// Forge 使用方的密钥存储（只读 `present`，供 settings 读回）。
    pub forge_secrets: Arc<dyn gantry_core::ports::secrets::SecretSource>,
    /// 模型凭据的转交通道（Egress 控制面，只写不读，P-50）。
    pub egress: Arc<dyn EgressControl>,
    /// 交付单元控制 #8 #43–#45（P-56）。
    pub engine: Arc<dyn DeliveryControl>,
    /// 发消息 #12。
    pub conversation: Arc<dyn ConversationService>,
    /// `api_idempotency`（本文 §7.6）。
    pub idempotency: Arc<dyn IdempotencyStore>,
}

/// 传输层交来的请求头部分。
#[derive(Clone, Debug)]
pub struct RequestHead {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    /// mTLS 握手出示的叶子证书；API 监听器上必有。
    pub peer: Option<PeerCert>,
}

impl RequestHead {
    pub fn new(method: Method, uri: &str, peer: Option<PeerCert>) -> Self {
        Self {
            method,
            uri: uri.parse().expect("valid request uri"),
            headers: HeaderMap::new(),
            peer,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.append(
            HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
            HeaderValue::from_str(value).expect("valid header value"),
        );
        self
    }
}

pub type ApiResponse = Response<Body>;

/// 响应体：普通 JSON / problem，或事件流（#41）。
pub enum Body {
    Full(Bytes),
    Events(EventStream),
}

impl Body {
    /// 普通响应的字节；事件流为空切片（帧从 [`Body::into_events`] 读取）。
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Body::Full(bytes) => bytes,
            Body::Events(_) => &[],
        }
    }

    pub fn into_events(self) -> Option<EventStream> {
        match self {
            Body::Events(stream) => Some(stream),
            Body::Full(_) => None,
        }
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Full(bytes) => f.debug_tuple("Full").field(bytes).finish(),
            Body::Events(_) => f.write_str("Events"),
        }
    }
}

/// 步骤 1–7 通过后的请求。
pub struct Admitted {
    request_id: String,
    instance: String,
    route: &'static RouteSpec,
    params: Vec<(&'static str, String)>,
    query: Result<Query, query::QueryError>,
    auth: AuthContext,
    rate: RateState,
    log_epoch: u64,
    headers: HeaderMap,
}

impl Admitted {
    /// 传输层读请求体的上限（读至多 `limit + 1` 字节）。
    pub fn body_limit(&self) -> usize {
        self.route.body_limit
    }

    pub fn route(&self) -> &'static RouteSpec {
        self.route
    }
}

/// `admit` 的结果。
pub enum Admission {
    Respond(ApiResponse),
    Proceed(Box<Admitted>),
}

/// 处理程序看到的请求（步骤 13 的输入）。
pub struct HandlerRequest<'a> {
    pub route: &'static RouteSpec,
    pub params: &'a [(&'static str, String)],
    pub query: &'a Query,
    pub auth: &'a AuthContext,
    pub request_id: &'a str,
    pub headers: &'a HeaderMap,
    pub idempotency_key: Option<&'a str>,
    /// 已解析的 JSON 请求体（`EmptyRequest` 的空正文为 `{}`）；无请求体的路由为 `None`。
    pub body: Option<Value>,
}

impl HandlerRequest<'_> {
    pub fn param(&self, name: &str) -> &str {
        self.params
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

/// 处理程序的成功结果。
pub struct Reply {
    pub status: StatusCode,
    pub body: Option<Value>,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// `X-Event-Seq`：读 = 快照反映到的最大 `seq`；写 = 本次写入的最后一个事件。
    pub event_seq: Seq,
    /// 事件流（#41）；有值时忽略 `body`。
    pub stream: Option<EventStream>,
}

impl Reply {
    pub fn json(status: StatusCode, body: Value, event_seq: Seq) -> Self {
        Self {
            status,
            body: Some(body),
            headers: Vec::new(),
            event_seq,
            stream: None,
        }
    }

    pub fn events(stream: EventStream, event_seq: Seq) -> Self {
        Self {
            status: StatusCode::OK,
            body: None,
            headers: Vec::new(),
            event_seq,
            stream: Some(stream),
        }
    }
}

/// 步骤 13。默认实现是 [`handlers::Builtin`]；测试可注入替身以单独检验管线。
#[async_trait]
pub trait RouteHandler: Send + Sync {
    async fn handle(&self, core: &ApiCore, req: HandlerRequest<'_>) -> Result<Reply, Problem>;
}

pub struct ApiCore {
    cfg: ApiConfig,
    svc: Services,
    registry: Registry,
    limiter: RateLimiter,
    handler: Arc<dyn RouteHandler>,
    streams: Arc<StreamRegistry>,
    shutdown: watch::Sender<bool>,
}

impl ApiCore {
    pub fn new(cfg: ApiConfig, svc: Services) -> Self {
        Self::with_handler(cfg, svc, Arc::new(handlers::Builtin))
    }

    pub fn with_handler(cfg: ApiConfig, svc: Services, handler: Arc<dyn RouteHandler>) -> Self {
        Self {
            registry: Registry::api(cfg.enabled),
            limiter: RateLimiter::new(svc.clock.clone()),
            cfg,
            svc,
            handler,
            streams: Arc::default(),
            shutdown: watch::channel(false).0,
        }
    }

    /// 优雅关停：所有事件流尽力收到 `close(server_shutdown)` 后结束。
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub(crate) fn streams(&self) -> &Arc<StreamRegistry> {
        &self.streams
    }

    pub fn config(&self) -> &ApiConfig {
        &self.cfg
    }

    pub fn services(&self) -> &Services {
        &self.svc
    }

    /// 便捷入口：`admit` + `complete`（请求体已在内存）。
    pub async fn handle(&self, head: RequestHead, body: Bytes) -> ApiResponse {
        match self.admit(head).await {
            Admission::Respond(resp) => resp,
            Admission::Proceed(admitted) => self.complete(*admitted, body).await,
        }
    }

    /// 步骤 1–7。
    pub async fn admit(&self, head: RequestHead) -> Admission {
        // 1. request_id
        let request_id = match self.svc.ids.next_ulid() {
            Ok(id) => id.to_string(),
            Err(_) => "00000000000000000000000000".to_owned(),
        };
        let instance = head.uri.path().to_owned();
        let log_epoch = self.svc.store.log_identity().log_epoch;
        let early = |problem: Problem, rate: Option<RateState>| {
            Admission::Respond(problem_response(
                &problem,
                &instance,
                &request_id,
                log_epoch,
                rate,
            ))
        };

        // 2. 映射设备（实时读库）
        let Some(peer) = head.peer.clone() else {
            return early(
                Problem::new(ProblemCode::DeviceRevoked, "no client certificate"),
                None,
            );
        };
        // 序列号格式不合法的证书不可能由本 CA 签发：视同不在册。
        let serial = peer.serial.parse::<CertSerial>().ok();
        let lookup = match &serial {
            Some(serial) => self.svc.devices.by_cert_serial(serial).await,
            None => Ok(None),
        };
        let device = match lookup {
            Ok(device) => device,
            Err(_) => {
                return early(
                    Problem::retry_after(
                        ProblemCode::Unavailable,
                        "device directory unavailable",
                        1,
                    ),
                    None,
                )
            }
        };
        let Some(device) = auth::active(device) else {
            return early(
                Problem::new(ProblemCode::DeviceRevoked, "device is not active"),
                None,
            );
        };
        self.svc.devices.touch_last_seen(&device.id).await;
        let subject = device.id.to_string();
        let read_state = || Some(self.limiter.peek(&subject, RateClass::Read));

        // 3. 路由
        let (route, params) = match self.registry.find(&head.method, head.uri.path()) {
            RouteMatch::Found { route, params } => (route, params),
            RouteMatch::NoPath => return early(Problem::not_found(), read_state()),
            RouteMatch::MethodNotAllowed { allow } => {
                let problem = Problem::new(
                    ProblemCode::MethodNotAllowed,
                    "method is not allowed for this path",
                )
                .with_header(
                    ALLOW,
                    HeaderValue::from_str(&allow.join(", ")).expect("method names are ASCII"),
                );
                return early(problem, read_state());
            }
        };

        // 4. 角色（先角色后档案）
        if let Some(floor) = route.min_role.floor() {
            if !auth::role_satisfies(device.role, floor) {
                return early(Problem::forbidden_role(floor), read_state());
            }
        }
        // 5. 档案
        let profile = auth::profile_of(&device.platform);
        if profile == gantry_core::ports::ClientProfile::Mobile && !route.mobile.allowed() {
            return early(
                Problem::new(
                    ProblemCode::ForbiddenProfile,
                    "route is not available for this client profile",
                ),
                read_state(),
            );
        }
        // 6. 限流
        let rate = self.limiter.take(&subject, route.rate);
        if let Some(secs) = rate.retry_after {
            return early(
                Problem::retry_after(ProblemCode::RateLimited, "rate limit exceeded", secs),
                Some(rate),
            );
        }
        // 7. Content-Type
        if route.has_body() && !content_type_ok(&head.headers, route) {
            return early(
                Problem::new(
                    ProblemCode::UnsupportedMediaType,
                    "body must be application/json",
                ),
                Some(rate),
            );
        }
        // 9. 路径与查询参数：此处只解析，错误在步骤 8（体积）之后才报告，保持契约顺序。
        let query = if registry::check_path_params(&params).is_err() {
            Err("path parameter is invalid")
        } else {
            query::parse(head.uri.query(), query::params_for(route.operation_id))
        };

        Admission::Proceed(Box::new(Admitted {
            request_id,
            instance,
            route,
            params,
            query,
            auth: AuthContext {
                device,
                profile,
                cert: peer,
            },
            rate,
            log_epoch,
            headers: head.headers,
        }))
    }

    /// 步骤 8–10、13、14。
    pub async fn complete(&self, admitted: Admitted, body: Bytes) -> ApiResponse {
        let a = &admitted;
        let fail = |problem: Problem| {
            problem_response(
                &problem,
                &a.instance,
                &a.request_id,
                a.log_epoch,
                Some(a.rate),
            )
        };

        // 8. 体积
        if body.len() > a.route.body_limit {
            return fail(Problem::new(
                ProblemCode::PayloadTooLarge,
                "request body exceeds the contract limit",
            ));
        }
        // 9. 路径与查询参数
        let query = match &a.query {
            Ok(query) => query,
            Err(detail) => return fail(Problem::new(ProblemCode::InvalidRequest, *detail)),
        };
        // 10. JSON 语法与 Idempotency-Key
        let parsed = if a.route.has_body() {
            match parse_body(a.route, &body) {
                Ok(value) => Some(value),
                Err(problem) => return fail(problem),
            }
        } else {
            None
        };
        let key = match idempotency_key(&a.headers, a.route.idem) {
            Ok(key) => key,
            Err(problem) => return fail(problem),
        };

        // 11. 幂等登记：重放 / 冲突 / 进行中
        let device_id = a.auth.device.id;
        let mut lease = None;
        if let Some(key) = key {
            let print = idempotency::fingerprint(a.route.method, &a.instance, parsed.as_ref());
            match self.svc.idempotency.begin(&device_id, key, print) {
                Begin::Proceed(held) => lease = Some(held),
                Begin::Replay(stored) => return replay_response(stored, a.log_epoch, a.rate),
                Begin::InProgress => {
                    return fail(Problem::new(
                        ProblemCode::IdempotencyInProgress,
                        "a request with this Idempotency-Key is still in progress",
                    ))
                }
                Begin::Reuse => {
                    return fail(Problem::new(
                        ProblemCode::IdempotencyKeyReuse,
                        "Idempotency-Key was used with a different request",
                    ))
                }
            }
        }

        // 12–13. 请求体 schema 校验（在处理程序里，按路由的请求体组件）与处理
        let req = HandlerRequest {
            route: a.route,
            params: &a.params,
            query,
            auth: &a.auth,
            request_id: &a.request_id,
            headers: &a.headers,
            idempotency_key: key,
            body: parsed,
        };
        let resp = match self.handler.handle(self, req).await {
            Ok(reply) => success_response(reply, &a.request_id, a.log_epoch, a.rate),
            Err(problem) => fail(problem),
        };
        if let (Some(key), Some(lease)) = (key, lease) {
            // 首次响应是 5xx 不登记，允许同键重试；4xx 登记并重放。
            if resp.status().is_server_error() {
                self.svc.idempotency.abandon(&device_id, key, lease);
            } else {
                self.svc
                    .idempotency
                    .finish(&device_id, key, lease, stored(&resp));
            }
        }
        resp
    }
}

/// 重放时原样保留的头；`X-Log-Epoch`、`RateLimit-*` 按重放时刻重新计算。
const REPLAYED_HEADERS: [HeaderName; 4] = [CONTENT_TYPE, X_REQUEST_ID, X_EVENT_SEQ, ETAG];

fn stored(resp: &ApiResponse) -> StoredResponse {
    StoredResponse {
        status: resp.status(),
        headers: REPLAYED_HEADERS
            .iter()
            .filter_map(|name| resp.headers().get(name).map(|v| (name.clone(), v.clone())))
            .collect(),
        body: resp.body().as_bytes().to_vec(),
    }
}

fn replay_response(stored: StoredResponse, log_epoch: u64, rate: RateState) -> ApiResponse {
    let mut resp = Response::new(Body::Full(Bytes::from(stored.body)));
    *resp.status_mut() = stored.status;
    let request_id = stored
        .headers
        .iter()
        .find(|(name, _)| name == X_REQUEST_ID)
        .and_then(|(_, v)| v.to_str().ok().map(str::to_owned))
        .unwrap_or_default();
    let headers = resp.headers_mut();
    for (name, value) in stored.headers {
        headers.insert(name, value);
    }
    headers.insert(IDEMPOTENT_REPLAYED, HeaderValue::from_static("true"));
    common_headers(headers, &request_id, log_epoch, Some(rate));
    resp
}

fn content_type_ok(headers: &HeaderMap, route: &RouteSpec) -> bool {
    match headers.get(CONTENT_TYPE) {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|v| v.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case(JSON)),
        // 没有 Content-Type：只有声明了空正文、且路由的请求体是 EmptyRequest 时放行。
        None => {
            let declared_empty = headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .is_none_or(|v| v.trim() == "0")
                && !headers.contains_key(TRANSFER_ENCODING);
            route.empty_body() && declared_empty
        }
    }
}

fn parse_body(route: &RouteSpec, body: &[u8]) -> Result<Value, Problem> {
    if body.is_empty() {
        return if route.empty_body() {
            Ok(Value::Object(Default::default()))
        } else {
            Err(Problem::new(
                ProblemCode::InvalidRequest,
                "request body is required",
            ))
        };
    }
    serde_json::from_slice(body).map_err(|_| {
        Problem::new(
            ProblemCode::InvalidRequest,
            "request body is not valid JSON",
        )
    })
}

fn idempotency_key(headers: &HeaderMap, policy: IdemPolicy) -> Result<Option<&str>, Problem> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY).iter();
    let Some(value) = values.next() else {
        return match policy {
            IdemPolicy::Required => Err(Problem::new(
                ProblemCode::IdempotencyKeyRequired,
                "Idempotency-Key is required",
            )),
            _ => Ok(None),
        };
    };
    let invalid = || {
        Problem::new(
            ProblemCode::InvalidRequest,
            "Idempotency-Key must be 16-64 characters of [A-Za-z0-9_-]",
        )
    };
    if values.next().is_some() {
        return Err(invalid());
    }
    let key = value.to_str().map_err(|_| invalid())?;
    let valid = (IDEMPOTENCY_KEY_MIN..=IDEMPOTENCY_KEY_MAX).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid {
        return Err(invalid());
    }
    // 不适用的路由上带了头：忽略（契约只规定"必带"与"可带"）。
    Ok(match policy {
        IdemPolicy::NotApplicable => None,
        _ => Some(key),
    })
}

fn common_headers(
    headers: &mut HeaderMap,
    request_id: &str,
    log_epoch: u64,
    rate: Option<RateState>,
) {
    headers.insert(
        X_REQUEST_ID,
        HeaderValue::from_str(request_id).expect("request id is ASCII"),
    );
    headers.insert(X_LOG_EPOCH, HeaderValue::from(log_epoch));
    if let Some(rate) = rate {
        headers.insert(RATELIMIT_LIMIT, HeaderValue::from(rate.limit));
        headers.insert(RATELIMIT_REMAINING, HeaderValue::from(rate.remaining));
        headers.insert(RATELIMIT_RESET, HeaderValue::from(rate.reset_secs));
    }
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
}

/// 渲染 problem 响应（步骤 14 的错误分支）。
pub(crate) fn problem_response(
    problem: &Problem,
    instance: &str,
    request_id: &str,
    log_epoch: u64,
    rate: Option<RateState>,
) -> ApiResponse {
    let body = serde_json::to_vec(&problem.body(instance, request_id)).expect("problem serializes");
    let mut resp = Response::new(Body::Full(Bytes::from(body)));
    *resp.status_mut() = problem.code.status();
    let headers = resp.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    for (name, value) in &problem.headers {
        headers.insert(name.clone(), value.clone());
    }
    common_headers(headers, request_id, log_epoch, rate);
    resp
}

fn success_response(
    reply: Reply,
    request_id: &str,
    log_epoch: u64,
    rate: RateState,
) -> ApiResponse {
    let (body, is_json) = match (reply.stream, reply.body) {
        (Some(stream), _) => (Body::Events(stream), false),
        (None, Some(value)) => (
            Body::Full(Bytes::from(
                serde_json::to_vec(&value).expect("json serializes"),
            )),
            true,
        ),
        (None, None) => (Body::Full(Bytes::new()), false),
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = reply.status;
    let headers = resp.headers_mut();
    if is_json {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_UTF8));
    }
    for (name, value) in reply.headers {
        headers.insert(name, value);
    }
    headers.insert(X_EVENT_SEQ, HeaderValue::from(reply.event_seq.as_u64()));
    common_headers(headers, request_id, log_epoch, Some(rate));
    resp
}
