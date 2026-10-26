//! `FakeGitHub`：进程内 HTTP 级 GitHub 替身。
//!
//! 只覆盖被测路径需要的最小子集：REST（仓库 / PR / Issue）、GraphQL、webhook
//! 投递与 `429` / `5xx` 脚本。状态保存在内存；webhook 目标由测试注册。所有
//! 请求（含被脚本拦截的请求）都可从 `recorded_calls()` 读取。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

/// 一次 HTTP 调用的记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubCall {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    pub status: u16,
}

/// 注入到下一条请求的失败响应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptedFailure {
    pub status: u16,
    pub body: String,
    pub retry_after_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookTarget {
    pub url: String,
    pub secret: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookDelivery {
    pub url: String,
    pub event: String,
    pub delivery_id: String,
    pub status: u16,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubError {
    message: String,
}

impl GithubError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GithubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GithubError {}

struct GithubState {
    calls: Vec<GithubCall>,
    failures: VecDeque<ScriptedFailure>,
    pulls: HashMap<(String, u64), Value>,
    issues: HashMap<(String, u64), Value>,
    next_number: u64,
    webhooks: Vec<WebhookTarget>,
    deliveries: Vec<WebhookDelivery>,
    next_delivery: u64,
}

/// M0 的 GitHub 替身。
pub struct FakeGitHub {
    addr: SocketAddr,
    state: Arc<Mutex<GithubState>>,
    server: Option<GithubServer>,
}

impl FakeGitHub {
    pub fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake github");
        listener
            .set_nonblocking(true)
            .expect("set fake github listener nonblocking");
        let addr = listener.local_addr().expect("fake github local addr");
        let state = Arc::new(Mutex::new(GithubState {
            calls: Vec::new(),
            failures: VecDeque::new(),
            pulls: HashMap::new(),
            issues: HashMap::new(),
            next_number: 1,
            webhooks: Vec::new(),
            deliveries: Vec::new(),
            next_delivery: 1,
        }));
        let server = GithubServer::start(listener, state.clone());
        Self {
            addr,
            state,
            server: Some(server),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    pub fn record(&self) -> Vec<GithubCall> {
        self.recorded_calls()
    }

    pub fn recorded_calls(&self) -> Vec<GithubCall> {
        self.lock().calls.clone()
    }

    /// 把一次响应排到队首；下一条 HTTP 请求消费它。
    pub fn push_failure(&self, failure: ScriptedFailure) {
        self.lock().failures.push_back(failure);
    }

    pub fn queue_failure(&self, status: u16) {
        self.push_failure(ScriptedFailure {
            status,
            body: format!(r#"{{"message":"scripted {status}"}}"#),
            retry_after_s: (status == 429).then_some(1),
        });
    }

    pub fn register_webhook(&self, url: impl Into<String>) {
        self.lock().webhooks.push(WebhookTarget {
            url: url.into(),
            secret: None,
        });
    }

    pub fn register_webhook_with_secret(&self, url: impl Into<String>, secret: impl Into<String>) {
        self.lock().webhooks.push(WebhookTarget {
            url: url.into(),
            secret: Some(secret.into()),
        });
    }

    /// 向所有已注册目标投递一条 webhook；有 secret 时附 HMAC-SHA256 签名。
    pub fn deliver_webhook(
        &self,
        event: &str,
        payload: &Value,
    ) -> Result<Vec<WebhookDelivery>, GithubError> {
        let body = serde_json::to_string(payload)
            .map_err(|error| GithubError::new(format!("serialize webhook: {error}")))?;
        let (targets, delivery_id) = {
            let mut state = self.lock();
            let id = format!("{:032x}", state.next_delivery);
            state.next_delivery = state.next_delivery.saturating_add(1);
            (state.webhooks.clone(), id)
        };
        let mut delivered = Vec::new();
        for target in targets {
            let status = send_webhook(&target, event, &delivery_id, &body)?;
            delivered.push(WebhookDelivery {
                url: target.url,
                event: event.to_owned(),
                delivery_id: delivery_id.clone(),
                status,
                body: body.clone(),
            });
        }
        self.lock().deliveries.extend(delivered.clone());
        Ok(delivered)
    }

    pub fn webhook_deliveries(&self) -> Vec<WebhookDelivery> {
        self.lock().deliveries.clone()
    }

    pub fn shutdown(&mut self) {
        if let Some(server) = self.server.take() {
            server.stop();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GithubState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct GithubServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl GithubServer {
    fn start(listener: TcpListener, state: Arc<Mutex<GithubState>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_connection(stream, state.clone());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GithubServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 已解析的 HTTP/1 请求；`github` 与 `egress` 两个回环替身共用同一读取器。
pub(crate) struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub(crate) enum ReadOutcome {
    Request(HttpRequest),
    /// 分帧非法（冲突 / 非法 `Content-Length`、不支持的编码、缺 body 等）。
    Malformed,
    /// 对端未发送任何请求就关闭了连接。
    Closed,
}

const MAX_HEAD_BYTES: usize = 1024 * 1024;
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

struct Response {
    status: u16,
    body: String,
    retry_after_s: Option<u64>,
}

fn handle_connection(mut stream: TcpStream, state: Arc<Mutex<GithubState>>) -> std::io::Result<()> {
    // 监听 socket 是非阻塞的；macOS 上 accept 出的连接会继承 O_NONBLOCK，
    // 首个 read 在请求尚未到达时返回 WouldBlock 并丢弃连接。这里恢复阻塞模式。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    match read_http_request(&mut stream)? {
        ReadOutcome::Closed => Ok(()),
        ReadOutcome::Malformed => {
            write_response(&mut stream, &bad_request())?;
            discard_pending_input(&mut stream);
            Ok(())
        }
        ReadOutcome::Request(request) => {
            let response = route(&request, &state);
            write_response(&mut stream, &response)
        }
    }
}

fn bad_request() -> Response {
    Response {
        status: 400,
        body: json!({"message":"Bad Request"}).to_string(),
        retry_after_s: None,
    }
}

/// 读取一条 HTTP/1 请求。只有分帧完整、`Content-Length` 合法且 body 已全部到达
/// 时才返回 `Request`；否则返回 `Malformed`，调用方应回 `400` 而不是当作请求处理。
pub(crate) fn read_http_request(stream: &mut TcpStream) -> std::io::Result<ReadOutcome> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            if position > MAX_HEAD_BYTES {
                return Ok(ReadOutcome::Malformed);
            }
            break position;
        }
        match stream.read(&mut chunk) {
            Ok(0) if buffer.is_empty() => return Ok(ReadOutcome::Closed),
            Ok(0) => return Ok(ReadOutcome::Malformed),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Ok(ReadOutcome::Malformed);
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(ReadOutcome::Malformed);
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(ReadOutcome::Malformed);
    };

    let mut headers = BTreeMap::new();
    let mut content_length = 0u64;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Ok(ReadOutcome::Malformed);
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            // 重复或冲突的 Content-Length 一律拒绝，避免请求走私式的歧义。
            if headers.contains_key("content-length") {
                return Ok(ReadOutcome::Malformed);
            }
            let Some(length) = parse_content_length(value) else {
                return Ok(ReadOutcome::Malformed);
            };
            content_length = length;
        }
        if name == "transfer-encoding" && !value.eq_ignore_ascii_case("identity") {
            return Ok(ReadOutcome::Malformed);
        }
        headers.insert(name, value.to_owned());
    }

    let body_start = head_end + 4;
    let body_end = body_start as u64 + content_length;
    while (buffer.len() as u64) < body_end {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(ReadOutcome::Malformed),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(ReadOutcome::Malformed);
            }
            Err(error) => return Err(error),
        }
    }

    Ok(ReadOutcome::Request(HttpRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body: buffer[body_start..body_end as usize].to_vec(),
    }))
}

fn parse_content_length(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let length = value.parse::<u64>().ok()?;
    (length <= MAX_BODY_BYTES).then_some(length)
}

/// 写完错误响应后先半关闭，再读掉对端剩余输入，避免关闭带未读数据的连接时
/// 内核发 RST 而丢弃刚写出的响应。
///
/// shortcut: 最多排空 64 KiB / 200 ms；再大的流式 body 会直接关闭连接。
pub(crate) fn discard_pending_input(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < 64 * 1024 {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(read) => drained += read,
        }
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    );
    if let Some(seconds) = response.retry_after_s {
        head.push_str(&format!("retry-after: {seconds}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()
}

fn route(request: &HttpRequest, shared: &Arc<Mutex<GithubState>>) -> Response {
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or(&request.path)
        .to_owned();
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut response = if let Some(failure) = state.failures.pop_front() {
        Response {
            status: failure.status,
            body: failure.body,
            retry_after_s: failure.retry_after_s,
        }
    } else {
        dispatch(request, &path, &mut state)
    };
    if response.status == 204 {
        response.body.clear();
    }
    state.calls.push(GithubCall {
        method: request.method.clone(),
        path: request.path.clone(),
        headers: request.headers.clone(),
        body: String::from_utf8_lossy(&request.body).to_string(),
        status: response.status,
    });
    response
}

fn dispatch(request: &HttpRequest, path: &str, state: &mut GithubState) -> Response {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["health"]) => Response {
            status: 200,
            body: json!({"status":"ok"}).to_string(),
            retry_after_s: None,
        },
        ("GET", ["repos", owner, repo]) => {
            let full = format!("{owner}/{repo}");
            Response {
                status: 200,
                body: repo_json(&full).to_string(),
                retry_after_s: None,
            }
        }
        ("GET", ["repos", owner, repo, "pulls", number]) => {
            let full = format!("{owner}/{repo}");
            let Some(number) = number.parse::<u64>().ok() else {
                return not_found();
            };
            match state.pulls.get(&(full, number)) {
                Some(pull) => Response {
                    status: 200,
                    body: pull.to_string(),
                    retry_after_s: None,
                },
                None => not_found(),
            }
        }
        ("POST", ["repos", owner, repo, "pulls"]) => {
            let full = format!("{owner}/{repo}");
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let number = state.next_number;
            state.next_number = state.next_number.saturating_add(1);
            let head_ref = string_field(&body, "head").unwrap_or_default();
            let base_ref = string_field(&body, "base").unwrap_or_else(|| "main".into());
            let title = string_field(&body, "title").unwrap_or_default();
            let body_text = string_field(&body, "body").unwrap_or_default();
            let draft = body.get("draft").and_then(Value::as_bool).unwrap_or(false);
            let pull = json!({
                "id": number,
                "number": number,
                "node_id": format!("PR_{number}"),
                "state": "open",
                "title": title,
                "body": body_text,
                "draft": draft,
                "head": {"ref": head_ref, "sha": format!("{:040x}", number)},
                "base": {"ref": base_ref},
                "user": {"login": "gantry-test-bot"},
                "mergeable_state": "clean"
            });
            state.pulls.insert((full, number), pull.clone());
            Response {
                status: 201,
                body: pull.to_string(),
                retry_after_s: None,
            }
        }
        ("POST", ["repos", owner, repo, "issues"]) => {
            let full = format!("{owner}/{repo}");
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let number = state.next_number;
            state.next_number = state.next_number.saturating_add(1);
            let issue = json!({
                "id": number,
                "number": number,
                "state": "open",
                "title": string_field(&body, "title").unwrap_or_default(),
                "body": string_field(&body, "body").unwrap_or_default(),
                "labels": body.get("labels").cloned().unwrap_or_else(|| json!([])),
                "user": {"login": "gantry-test-bot"}
            });
            state.issues.insert((full, number), issue.clone());
            Response {
                status: 201,
                body: issue.to_string(),
                retry_after_s: None,
            }
        }
        ("GET", ["repos", owner, repo, "issues", number]) => {
            let full = format!("{owner}/{repo}");
            let Some(number) = number.parse::<u64>().ok() else {
                return not_found();
            };
            match state.issues.get(&(full, number)) {
                Some(issue) => Response {
                    status: 200,
                    body: issue.to_string(),
                    retry_after_s: None,
                },
                None => not_found(),
            }
        }
        ("POST", ["graphql"]) => {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let query = body
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut data = serde_json::Map::new();
            if query.contains("viewer") {
                data.insert("viewer".into(), json!({"login": "gantry-test-bot"}));
            }
            if query.contains("repository") {
                let full = body
                    .get("variables")
                    .and_then(|variables| {
                        let owner = variables.get("owner").and_then(Value::as_str)?;
                        let name = variables.get("name").and_then(Value::as_str)?;
                        Some(format!("{owner}/{name}"))
                    })
                    .unwrap_or_else(|| "acme/widgets".into());
                data.insert("repository".into(), repo_json(&full));
            }
            Response {
                status: 200,
                body: json!({"data": data}).to_string(),
                retry_after_s: None,
            }
        }
        _ => not_found(),
    }
}

fn repo_json(full_name: &str) -> Value {
    let (owner, name) = full_name.split_once('/').unwrap_or((full_name, ""));
    json!({
        "id": 1,
        "name": name,
        "full_name": full_name,
        "owner": {"login": owner},
        "private": false,
        "default_branch": "main"
    })
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn not_found() -> Response {
    Response {
        status: 404,
        body: json!({"message":"Not Found"}).to_string(),
        retry_after_s: None,
    }
}

fn send_webhook(
    target: &WebhookTarget,
    event: &str,
    delivery_id: &str,
    body: &str,
) -> Result<u16, GithubError> {
    let (host, path) = split_url(&target.url)?;
    let mut stream = TcpStream::connect(&host)
        .map_err(|error| GithubError::new(format!("connect webhook target: {error}")))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\ncontent-type: application/json\r\nX-GitHub-Event: {event}\r\nX-GitHub-Delivery: {delivery_id}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    if let Some(secret) = &target.secret {
        let signature = hmac_sha256(secret, body)?;
        head.push_str(&format!("X-Hub-Signature-256: sha256={signature}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body.as_bytes()))
        .and_then(|_| stream.flush())
        .map_err(|error| GithubError::new(format!("send webhook: {error}")))?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    Ok(status)
}

fn split_url(url: &str) -> Result<(String, String), GithubError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| GithubError::new("only http webhook targets are supported"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    Ok((authority.to_owned(), format!("/{path}")))
}

fn hmac_sha256(secret: &str, body: &str) -> Result<String, GithubError> {
    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-hmac", secret, "-binary"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GithubError::new(format!("spawn openssl: {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(body.as_bytes())
            .map_err(|error| GithubError::new(format!("write openssl stdin: {error}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| GithubError::new(format!("wait openssl: {error}")))?;
    if !output.status.success() {
        return Err(GithubError::new(format!(
            "openssl hmac failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(hex(&output.stdout))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}
