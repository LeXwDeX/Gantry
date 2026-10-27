//! 测试用的回环 HTTP 模型桩。
//!
//! `EgressStub`（gantry-fakes）实现控制面并起一个假数据面：校验 `svc_token`、
//! 三个 `x-gantry-*` 头并逐请求计量。它不记录请求内容、也不能注入失败，所以
//! 在它前面放一个记录用的回环代理：
//!
//! ```text
//! ModelRouter ──HTTP──► RecordingProxy ──转发──► EgressStub 数据面（计量）
//!                            └─ 按脚本：替换响应体为提供方形状 / 直接返回 5xx、429… / 延迟
//! ```
//!
//! `ProxiedEgress` 把控制面调用原样委托给 `EgressStub`，只把 `model_base_urls`
//! 的端口改成代理端口。它不实现 Egress 的任何逻辑。
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::egress::{ModelBaseUrls, SvcTokenId, UsagePage};
use gantry_core::ids::AttemptId;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::{
    AppliedConfig, CallClosed, CallStatement, CallUsage, EgressConfigView, EgressControl,
    EgressEndpoints, EgressError, EgressStatus, FinalUsage, IssueJobToken, JobTokenGrant, JobUsage,
    StageSecret, StagedInfo, StagedSecret, SvcTokenGrant, UsageCursor,
};
use gantry_core::ports::models::CallId;
use gantry_core::ports::secrets::SecretVersion;
use gantry_core::secret::SecretName;
use gantry_fakes::egress::EgressStub;
use serde_json::{json, Value};

/// 代理收到的一次请求。
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// 一次请求的脚本动作。
#[derive(Clone, Debug)]
pub enum Action {
    /// 转发给 EgressStub（计量）；上游 200 时把响应体替换为 `body`。
    Forward { body: String, delay: Duration },
    /// 不转发，直接返回（模拟上游 5xx / 429 / 404 / 4xx）。
    Respond {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
        delay: Duration,
    },
}

/// 提供方（Anthropic Messages API）形状的成功响应体。
pub fn anthropic_body(text: &str, stop: &str, usage: (u64, u64, u64, u64)) -> String {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "stop_reason": stop,
        "usage": {
            "input_tokens": usage.0,
            "output_tokens": usage.1,
            "cache_read_input_tokens": usage.2,
            "cache_creation_input_tokens": usage.3
        }
    })
    .to_string()
}

pub fn ok(text: &str) -> Action {
    Action::Forward {
        body: anthropic_body(text, "end_turn", (100, 20, 0, 0)),
        delay: Duration::ZERO,
    }
}

pub fn status(code: u16) -> Action {
    Action::Respond {
        status: code,
        body: json!({"type": "error", "error": {"type": "api_error"}}).to_string(),
        headers: Vec::new(),
        delay: Duration::ZERO,
    }
}

struct ProxyState {
    script: VecDeque<Action>,
    default: Action,
    recorded: Vec<Recorded>,
}

pub struct RecordingProxy {
    pub addr: SocketAddr,
    upstream: SocketAddr,
    state: Arc<Mutex<ProxyState>>,
    current: Arc<AtomicUsize>,
    max_seen: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl RecordingProxy {
    pub fn start(upstream: SocketAddr) -> Arc<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = Arc::new(Self {
            addr: listener.local_addr().unwrap(),
            upstream,
            state: Arc::new(Mutex::new(ProxyState {
                script: VecDeque::new(),
                default: ok(r#"{"kind":"classify","target":"event_line","text":"ok"}"#),
                recorded: Vec::new(),
            })),
            current: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
        });
        let worker = proxy.clone();
        thread::spawn(move || {
            while !worker.stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let worker = worker.clone();
                        thread::spawn(move || worker.handle(stream));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        proxy
    }

    pub fn push(&self, action: Action) {
        self.state.lock().unwrap().script.push_back(action);
    }

    pub fn set_default(&self, action: Action) {
        self.state.lock().unwrap().default = action;
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().recorded.clone()
    }

    pub fn max_concurrency(&self) -> usize {
        self.max_seen.load(Ordering::SeqCst)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn handle(&self, mut stream: TcpStream) {
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let Some(request) = read_request(&mut stream) else {
            return;
        };
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(now, Ordering::SeqCst);
        let action = {
            let mut state = self.state.lock().unwrap();
            state.recorded.push(request.clone());
            state
                .script
                .pop_front()
                .unwrap_or_else(|| state.default.clone())
        };
        let (status, body, headers) = match action {
            Action::Forward { body, delay } => {
                let (status, upstream_body) = forward(self.upstream, &request);
                thread::sleep(delay);
                if status == 200 {
                    (200, body, Vec::new())
                } else {
                    (status, upstream_body, Vec::new())
                }
            }
            Action::Respond {
                status,
                body,
                headers,
                delay,
            } => {
                thread::sleep(delay);
                (status, body, headers)
            }
        };
        self.current.fetch_sub(1, Ordering::SeqCst);
        let mut head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        let read = stream.read(&mut buf).ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..read]);
        if let Some(index) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break index;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buf).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buf[..read]);
    }
    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}

fn forward(upstream: SocketAddr, request: &Recorded) -> (u16, String) {
    let mut stream = TcpStream::connect(upstream).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut head = format!("{} {} HTTP/1.1\r\n", request.method, request.path);
    for (name, value) in &request.headers {
        if name == "connection" {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&request.body).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(502);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

/// 控制面原样委托给 `EgressStub`，只改写 `model_base_urls` 的端口。
pub struct ProxiedEgress {
    pub stub: Arc<EgressStub>,
    stub_port: u16,
    proxy_port: u16,
    /// 置位后 `call_terminal` 返回 `Unavailable`，模拟控制面取不到计量。
    pub fail_terminal: AtomicBool,
}

impl ProxiedEgress {
    pub fn new(stub: Arc<EgressStub>, proxy: &RecordingProxy) -> Self {
        let stub_port = stub
            .data_plane_url()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        Self {
            stub,
            stub_port,
            proxy_port: proxy.addr.port(),
            fail_terminal: AtomicBool::new(false),
        }
    }

    fn rewrite(&self, mut grant: SvcTokenGrant) -> SvcTokenGrant {
        let from = format!("127.0.0.1:{}", self.stub_port);
        let to = format!("127.0.0.1:{}", self.proxy_port);
        let urls = grant
            .model_base_urls
            .iter()
            .map(|(provider, url)| (provider.clone(), url.replace(&from, &to)))
            .collect();
        grant.model_base_urls = ModelBaseUrls::new(urls);
        grant
    }
}

#[async_trait]
impl EgressControl for ProxiedEgress {
    async fn endpoints(&self) -> Result<EgressEndpoints, EgressError> {
        self.stub.endpoints().await
    }
    async fn issue_job_token(
        &self,
        req: IssueJobToken,
        ctx: &ActorCtx,
    ) -> Result<JobTokenGrant, EgressError> {
        self.stub.issue_job_token(req, ctx).await
    }
    async fn job_usage(&self, job_id: &AttemptId) -> Result<JobUsage, EgressError> {
        self.stub.job_usage(job_id).await
    }
    async fn revoke_job_token(
        &self,
        job_id: &AttemptId,
        ctx: &ActorCtx,
    ) -> Result<FinalUsage, EgressError> {
        self.stub.revoke_job_token(job_id, ctx).await
    }
    async fn forget_job(&self, job_id: &AttemptId, ctx: &ActorCtx) -> Result<(), EgressError> {
        self.stub.forget_job(job_id, ctx).await
    }
    async fn service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        self.stub
            .service_token(ctx)
            .await
            .map(|grant| self.rewrite(grant))
    }
    async fn rotate_service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        self.stub
            .rotate_service_token(ctx)
            .await
            .map(|grant| self.rewrite(grant))
    }
    async fn apply_config(
        &self,
        view: EgressConfigView,
        ctx: &ActorCtx,
    ) -> Result<AppliedConfig, EgressError> {
        self.stub.apply_config(view, ctx).await
    }
    async fn secret_present(
        &self,
        refs: &[SecretName],
    ) -> Result<BTreeMap<SecretName, bool>, EgressError> {
        self.stub.secret_present(refs).await
    }
    async fn stage_secret(
        &self,
        req: StageSecret,
        ctx: &ActorCtx,
    ) -> Result<StagedSecret, EgressError> {
        self.stub.stage_secret(req, ctx).await
    }
    async fn activate_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<SecretVersion, EgressError> {
        self.stub.activate_secret(staged, ctx).await
    }
    async fn discard_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<(), EgressError> {
        self.stub.discard_secret(staged, ctx).await
    }
    async fn staged_secrets(&self) -> Result<Vec<StagedInfo>, EgressError> {
        self.stub.staged_secrets().await
    }
    async fn call_usage(&self, call_id: &CallId) -> Result<CallUsage, EgressError> {
        self.stub.call_usage(call_id).await
    }
    async fn usage_since(&self, cursor: UsageCursor, limit: u32) -> Result<UsagePage, EgressError> {
        self.stub.usage_since(cursor, limit).await
    }
    async fn svc_token_usage(
        &self,
        id: &SvcTokenId,
        cursor: Option<UsageCursor>,
    ) -> Result<UsagePage, EgressError> {
        self.stub.svc_token_usage(id, cursor).await
    }
    async fn close_call(
        &self,
        svc_token_id: &SvcTokenId,
        call_id: &CallId,
        ctx: &ActorCtx,
    ) -> Result<CallClosed, EgressError> {
        self.stub.close_call(svc_token_id, call_id, ctx).await
    }
    async fn call_terminal(&self, call_id: &CallId) -> Result<CallStatement, EgressError> {
        if self.fail_terminal.load(Ordering::SeqCst) {
            return Err(EgressError::Unavailable {
                message: "control plane down".into(),
            });
        }
        self.stub.call_terminal(call_id).await
    }
    async fn status(&self) -> Result<EgressStatus, EgressError> {
        self.stub.status().await
    }
}
