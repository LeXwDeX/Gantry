//! 数据面测试的公共替身：`MockUpstream`（本地 TLS 上游，脚本化响应，带外记录请求）、
//! `FakeResolver`（可注入 DNS 应答）、`MapDialer`（把任意落点拨到本机模拟服务并记录拨号）、
//! 以及装好登记表、密钥库与审计的 `Egress` 测试实例（`docs/modules/egress.md` §12 测试替身）。

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use gantry_core::egress::{ConfigView, IssueJobTokenRequest, IssueSvcTokenRequest, IssuedTo};
use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::Ulid;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::secrets::{SecretSink, SecretValue};
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_egress::audit::MemoryAudit;
use gantry_egress::db::Db;
use gantry_egress::host::Host;
use gantry_egress::model_proxy::{Limits, ModelProxy};
use gantry_egress::netdeny::{Cidr, DenyNets};
use gantry_egress::registry::{Endpoints, Listener, Registry, RegistryOptions};
use gantry_egress::upstream::{roots_from_pem, tls_config, Connector, Dialer, Resolver};
use gantry_fakes::secrets::MemorySecretStore;
use gantry_fakes::test_ca::TestCA;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

pub const T0: i64 = 1_793_000_000_000;
pub const SENTINEL: &str = "sk-gantry-sentinel-0123456789abcdef";
/// `FakeResolver` 默认返回的"公网"地址（TEST-NET-3，不在拒绝网段内）。
pub const PUBLIC_IP: &str = "203.0.113.10";

// ---------------------------------------------------------------------------
// MockUpstream
// ---------------------------------------------------------------------------

/// 一条脚本化响应。
#[derive(Clone, Debug)]
pub enum Script {
    Json {
        status: u16,
        body: Value,
        headers: Vec<(&'static str, String)>,
    },
    /// SSE：逐块发送，块之间等待 `gap`；`cut` 时最后不正常结束而是断开。
    Sse {
        chunks: Vec<String>,
        gap: Duration,
        cut: bool,
    },
    Raw {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
}

impl Script {
    pub fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Self {
        Self::Json {
            status: 200,
            body: json!({
                "id": "msg_mock", "type": "message", "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {
                    "input_tokens": input, "output_tokens": output,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation_input_tokens": cache_write
                }
            }),
            headers: Vec::new(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self::Json {
            status,
            body: json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
            headers: Vec::new(),
        }
    }

    pub fn sse(input: u64, outputs: &[u64], gap: Duration, cut: bool) -> Self {
        let mut chunks = vec![format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":{input},\"output_tokens\":1,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n\n"
        )];
        for output in outputs {
            chunks.push(format!(
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":{output}}}}}\n\n"
            ));
        }
        if !cut {
            chunks.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into());
        }
        Self::Sse { chunks, gap, cut }
    }
}

/// 上游带外记录到的请求（绝不在响应里回显）。
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn header_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.headers.iter().map(|(n, _)| n.clone()).collect();
        names.sort();
        names.dedup();
        names
    }
}

#[derive(Default)]
struct MockState {
    scripts: VecDeque<Script>,
    recorded: Vec<Recorded>,
}

pub struct MockUpstream {
    pub addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    /// 下游（Egress）中途放弃了流。
    pub aborted: Arc<AtomicBool>,
}

impl MockUpstream {
    pub async fn start(ca: &TestCA) -> Self {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let chain = format!("{}{}", ca.server_cert_pem(), ca.intermediate_cert_pem());
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(ca.server_key_pem().as_bytes()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(MockState::default()));
        let aborted = Arc::new(AtomicBool::new(false));
        let (accept_state, accept_aborted) = (state.clone(), aborted.clone());
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let state = accept_state.clone();
                let aborted = accept_aborted.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |req| {
                        let state = state.clone();
                        let aborted = aborted.clone();
                        async move { Ok::<_, Infallible>(serve(req, state, aborted).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self {
            addr,
            state,
            aborted,
        }
    }

    pub fn push(&self, script: Script) {
        self.state.lock().unwrap().scripts.push_back(script);
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().recorded.clone()
    }
}

async fn serve(
    req: Request<Incoming>,
    state: Arc<Mutex<MockState>>,
    aborted: Arc<AtomicBool>,
) -> Response<MockBody> {
    let method = req.method().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let headers = req
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
    let script = {
        let mut state = state.lock().unwrap();
        state.recorded.push(Recorded {
            method,
            path,
            headers,
            body,
        });
        state
            .scripts
            .pop_front()
            .unwrap_or_else(|| Script::usage(1, 1, 0, 0))
    };
    let (tx, rx) = mpsc::channel(4);
    let mut builder = Response::builder();
    match script {
        Script::Json {
            status,
            body,
            headers,
        } => {
            builder = builder
                .status(status)
                .header("content-type", "application/json");
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            tx.try_send(Ok(Bytes::from(body.to_string()))).unwrap();
        }
        Script::Raw {
            status,
            headers,
            body,
        } => {
            builder = builder.status(status);
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            tx.try_send(Ok(Bytes::from(body))).unwrap();
        }
        Script::Sse { chunks, gap, cut } => {
            builder = builder
                .status(200)
                .header("content-type", "text/event-stream");
            tokio::spawn(async move {
                for (index, chunk) in chunks.into_iter().enumerate() {
                    if index > 0 {
                        tokio::time::sleep(gap).await;
                    }
                    if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                        aborted.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                if cut {
                    let _ = tx.send(Err(io::Error::other("cut"))).await;
                }
            });
        }
    }
    builder.body(MockBody { rx }).unwrap()
}

pub struct MockBody {
    rx: mpsc::Receiver<Result<Bytes, io::Error>>,
}

impl Body for MockBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(data))) => Poll::Ready(Some(Ok(Frame::data(data)))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------
// FakeResolver / MapDialer
// ---------------------------------------------------------------------------

/// 可注入的 DNS：每个主机一个应答序列（用完后重复最后一个），并记录查询。
#[derive(Default)]
pub struct FakeResolver {
    answers: Mutex<HashMap<String, VecDeque<Vec<IpAddr>>>>,
    pub queries: Mutex<Vec<String>>,
}

impl FakeResolver {
    pub fn set(&self, host: &str, answers: Vec<Vec<&str>>) {
        let answers = answers
            .into_iter()
            .map(|ips| ips.into_iter().map(|ip| ip.parse().unwrap()).collect())
            .collect();
        self.answers.lock().unwrap().insert(host.into(), answers);
    }
}

#[async_trait]
impl Resolver for FakeResolver {
    async fn resolve(&self, host: &Host) -> io::Result<Vec<IpAddr>> {
        self.queries.lock().unwrap().push(host.to_string());
        let mut answers = self.answers.lock().unwrap();
        let queue = answers.get_mut(host.as_str()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no answer for {host}"))
        })?;
        if queue.len() > 1 {
            Ok(queue.pop_front().unwrap())
        } else {
            Ok(queue.front().cloned().unwrap_or_default())
        }
    }
}

/// 把任意落点拨到一个本机地址，并记录实际拨的落点（断言"钉住"与"从不连 GitHub"）。
pub struct MapDialer {
    pub target: Mutex<SocketAddr>,
    pub dialed: Mutex<Vec<SocketAddr>>,
}

impl MapDialer {
    pub fn new(target: SocketAddr) -> Self {
        Self {
            target: Mutex::new(target),
            dialed: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Dialer for MapDialer {
    async fn dial(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        self.dialed.lock().unwrap().push(addr);
        let target = *self.target.lock().unwrap();
        TcpStream::connect(target).await
    }
}

// ---------------------------------------------------------------------------
// Egress 测试实例
// ---------------------------------------------------------------------------

pub fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(T0 as u64, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

pub fn view(rev: u64, provider_host: &str) -> ConfigView {
    serde_json::from_value(json!({
        "rev": rev,
        "providers": {
            "anthropic": {
                "kind": "anthropic",
                "base_url": format!("https://{provider_host}"),
                "secret_ref": "model.anthropic.main"
            },
            "backup": {
                "kind": "anthropic",
                "base_url": format!("https://{provider_host}"),
                "secret_ref": "model.backup.main"
            }
        },
        "groups": {
            "model_api": { "hosts": [provider_host] },
            "npm": { "hosts": ["registry.npmjs.org"] },
            "crates": { "hosts": ["crates.io", "*.crates.io"] }
        },
        "profiles": { "default": { "groups": ["model_api", "npm", "crates"] } },
        "default_profile": "default",
        "injection": {},
        "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 }
    }))
    .unwrap()
}

pub fn job_request(n: u128, tokens_total: u64) -> IssueJobTokenRequest {
    serde_json::from_value(json!({
        "job_id": gantry_core::ids::AttemptId::from_parts(T0 as u64, n).unwrap().to_string(),
        "sandbox_id": "sbx_01JBX4R2M6P8Q1S3T5V7W9Y0ZA",
        "project_id": "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA",
        "ledger": { "subject": "delivery", "id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD" },
        "tier": "worker",
        "provider": "anthropic",
        "profile": "default",
        "groups": ["model_api", "npm", "crates"],
        "tokens_total": tokens_total,
        "token_weights": { "input": 1.0, "output": 1.0, "cache_read": 1.0, "cache_write": 1.0 },
        "wall_time_s": 1800,
        "config_rev": 7
    }))
    .unwrap()
}

pub struct Options {
    pub provider_host: &'static str,
    pub limits: Limits,
    pub sandbox_cidrs: Option<Vec<Cidr>>,
    pub trust_test_ca: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            provider_host: "localhost",
            limits: Limits::default(),
            sandbox_cidrs: None,
            trust_test_ca: true,
        }
    }
}

pub struct Egress {
    pub ca: TestCA,
    pub mock: MockUpstream,
    pub registry: Registry,
    pub audit: Arc<MemoryAudit>,
    pub resolver: Arc<FakeResolver>,
    pub dialer: Arc<MapDialer>,
    pub secrets: Arc<dyn SecretSink>,
    pub sandbox: SocketAddr,
    pub loopback: SocketAddr,
    pub clock: Arc<FakeClock>,
}

impl Egress {
    pub async fn start(options: Options) -> Self {
        let ca = TestCA::new().expect("openssl test CA");
        let mock = MockUpstream::start(&ca).await;
        let clock = Arc::new(FakeClock::new(T0));
        let store = MemorySecretStore::new(clock.now_timestamp());
        store.allow(Consumer::Egress);
        let secrets: Arc<dyn SecretSink> = Arc::new(store.scoped(Consumer::Egress).unwrap());
        let registry = Registry::open(
            Db::open_in_memory().unwrap(),
            clock.clone(),
            Arc::new(SeqEntropy::new(11)),
            secrets.clone(),
            RegistryOptions::new(Endpoints {
                sandbox_model_base: "http://10.89.0.1:8081".into(),
                connect_proxy_authority: "10.89.0.1:3128".into(),
                loopback_model_base: "http://127.0.0.1:8082".into(),
            }),
        )
        .unwrap();
        registry
            .apply_config(view(7, options.provider_host))
            .unwrap();
        let staged = registry
            .stage_secret(
                Ulid::from_parts(T0 as u64, 99).unwrap(),
                SecretName::parse("model.anthropic.main").unwrap(),
                SecretValue::from_bytes(SENTINEL.as_bytes().to_vec()),
                &ctx(),
            )
            .unwrap();
        registry.activate_secret(&staged.staged_id, &ctx()).unwrap();

        let resolver = Arc::new(FakeResolver::default());
        resolver.set(options.provider_host, vec![vec![PUBLIC_IP]]);
        let dialer = Arc::new(MapDialer::new(mock.addr));
        let connector = Connector::new(
            resolver.clone(),
            dialer.clone(),
            DenyNets::default(),
            Duration::from_secs(5),
        );
        let roots = if options.trust_test_ca {
            roots_from_pem(ca.root_cert_pem()).unwrap()
        } else {
            rustls::RootCertStore::empty()
        };
        let audit = Arc::new(MemoryAudit::new());
        let proxy = ModelProxy::new(
            registry.clone(),
            secrets.clone(),
            audit.clone(),
            connector,
            tls_config(roots),
            options.limits,
            options.sandbox_cidrs,
        );
        let sandbox_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let loopback_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sandbox = sandbox_listener.local_addr().unwrap();
        let loopback = loopback_listener.local_addr().unwrap();
        tokio::spawn(proxy.clone().serve(sandbox_listener, Listener::Sandbox));
        tokio::spawn(proxy.serve(loopback_listener, Listener::Loopback));
        Self {
            ca,
            mock,
            registry,
            audit,
            resolver,
            dialer,
            secrets,
            sandbox,
            loopback,
            clock,
        }
    }

    pub fn issue(&self, n: u128, tokens_total: u64) -> String {
        self.registry
            .issue_job(job_request(n, tokens_total))
            .unwrap()
            .job_token
            .expose_secret()
            .to_owned()
    }

    pub fn svc(&self) -> (String, gantry_core::egress::SvcTokenId) {
        let grant = self
            .registry
            .issue_svc(IssueSvcTokenRequest {
                issued_to: IssuedTo {
                    process: "gantryd".into(),
                    pid: 1,
                    started_at: self.clock.now_timestamp(),
                },
                ttl_s: None,
            })
            .unwrap();
        (
            grant.svc_token.expose_secret().to_owned(),
            grant.svc_token_id,
        )
    }
}

/// 下游客户端收到的响应。
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// 流在读完之前出错（截断）。
    pub truncated: bool,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    pub fn error_type(&self) -> String {
        self.json()["error"]["type"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

pub async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "egress");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = sender
        .send_request(
            builder
                .body(Full::new(Bytes::from(body.to_owned())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let mut body = response.into_body();
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(frame) => {
                if let Ok(data) = frame.into_data() {
                    out.extend_from_slice(&data);
                }
            }
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&out).into_owned(),
        truncated,
    }
}

pub const MESSAGES_BODY: &str = r#"{"model":"example-worker-model","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
