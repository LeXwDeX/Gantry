//! `FakeGitHub`：进程内 HTTP 级 GitHub 替身。
//!
//! 只覆盖被测路径需要的最小子集：REST（仓库 / PR / Issue / 分支 ref / 合并）、
//! GraphQL、App 安装令牌与安装权限、webhook 投递与 `429` / `5xx` 脚本。状态保存在内存；webhook 目标由测试
//! 注册。所有请求（含被脚本拦截的请求）都可从 `recorded_calls()` 读取。
//!
//! 创建类调用以 App 机器人身份（`bot()`）落对象；`lose_next_response()` 注入
//! "变更已生效但响应丢失"（forge.md §12 的响应丢失场景）。

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
    /// `(repo, branch) -> sha`。
    refs: HashMap<(String, String), String>,
    bot_login: String,
    bot_id: i64,
    /// 还要丢弃多少条变更类请求的响应（请求照常处理，连接直接关闭）。
    lose_responses: u32,
    /// App 安装：`installation id -> permissions`。
    installations: HashMap<i64, Value>,
    /// 安装令牌响应里的 `expires_at`。
    token_expires_at: String,
    issued_tokens: u64,
    /// `(repo, issue / PR number) -> 评论`（Issue 与 PR 对话区共用，同 GitHub）。
    comments: HashMap<(String, u64), Vec<Value>>,
    next_comment: u64,
    /// 设置后，投递带 `X-GitHub-Hook-Installation-Target-ID`（= App ID）。
    app_id: Option<u64>,
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
            refs: HashMap::new(),
            bot_login: DEFAULT_BOT_LOGIN.to_owned(),
            bot_id: DEFAULT_BOT_ID,
            lose_responses: 0,
            installations: HashMap::new(),
            token_expires_at: DEFAULT_TOKEN_EXPIRES_AT.to_owned(),
            issued_tokens: 0,
            comments: HashMap::new(),
            next_comment: 1,
            app_id: None,
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

    /// 下一条**变更类**请求（非 GET）照常处理（含状态变更），但不写回响应、直接关闭连接。
    pub fn lose_next_write_response(&self) {
        self.lock().lose_responses += 1;
    }

    /// 创建类调用使用的 App 机器人身份 `(login, user id)`。
    pub fn set_bot(&self, login: impl Into<String>, id: i64) {
        let mut state = self.lock();
        state.bot_login = login.into();
        state.bot_id = id;
    }

    pub fn bot(&self) -> (String, i64) {
        let state = self.lock();
        (state.bot_login.clone(), state.bot_id)
    }

    /// 登记（或替换）一个 App 安装及其获批权限（`{"contents":"write",…}`）。
    pub fn set_installation(&self, id: i64, permissions: Value) {
        self.lock().installations.insert(id, permissions);
    }

    pub fn remove_installation(&self, id: i64) {
        self.lock().installations.remove(&id);
    }

    /// 之后签发的安装令牌的 `expires_at`（RFC 3339）。
    pub fn set_token_expires_at(&self, expires_at: &str) {
        self.lock().token_expires_at = expires_at.to_owned();
    }

    /// 已签发的安装令牌数。
    pub fn issued_tokens(&self) -> u64 {
        self.lock().issued_tokens
    }

    /// 设置远端分支 head（模拟推送）。
    pub fn set_branch(&self, repo: &str, branch: &str, sha: &str) {
        self.lock()
            .refs
            .insert((repo.to_owned(), branch.to_owned()), sha.to_owned());
    }

    pub fn branch(&self, repo: &str, branch: &str) -> Option<String> {
        self.lock()
            .refs
            .get(&(repo.to_owned(), branch.to_owned()))
            .cloned()
    }

    /// 以给定作者创建一个 PR（模拟人手开的 PR）。返回 PR 号。
    pub fn insert_pull(
        &self,
        repo: &str,
        head: &str,
        base: &str,
        author: (&str, i64),
        body: &str,
    ) -> u64 {
        let mut state = self.lock();
        let number = state.next_number;
        state.next_number += 1;
        let sha = state
            .refs
            .get(&(repo.to_owned(), head.to_owned()))
            .cloned()
            .unwrap_or_else(|| format!("{number:040x}"));
        let pull = pull_json(number, "", body, false, head, &sha, base, author);
        state.pulls.insert((repo.to_owned(), number), pull);
        number
    }

    pub fn insert_issue(&self, repo: &str, title: &str) -> u64 {
        let mut state = self.lock();
        let number = state.next_number;
        state.next_number += 1;
        let issue = json!({
            "id": number,
            "number": number,
            "state": "open",
            "state_reason": null,
            "title": title,
            "body": "",
            "labels": [],
            "user": {"login": "octocat", "id": 1}
        });
        state.issues.insert((repo.to_owned(), number), issue);
        number
    }

    /// 设置 Issue 或 PR 的标题与正文。
    pub fn set_title_body(&self, repo: &str, number: u64, title: &str, body: &str) {
        let mut state = self.lock();
        let state = &mut *state;
        let key = (repo.to_owned(), number);
        for object in [state.pulls.get_mut(&key), state.issues.get_mut(&key)]
            .into_iter()
            .flatten()
        {
            object["title"] = json!(title);
            object["body"] = json!(body);
        }
    }

    /// 在 Issue / PR 对话区追加一条评论，返回评论 ID。`created_at` 为 RFC 3339。
    pub fn add_comment(
        &self,
        repo: &str,
        number: u64,
        author: (&str, &str),
        created_at: &str,
        body: &str,
    ) -> u64 {
        let mut state = self.lock();
        let id = state.next_comment;
        state.next_comment += 1;
        let (login, association) = author;
        state
            .comments
            .entry((repo.to_owned(), number))
            .or_default()
            .push(json!({
                "id": id,
                "user": {"login": login},
                "author_association": association,
                "created_at": created_at,
                "updated_at": created_at,
                "body": body,
            }));
        id
    }

    pub fn pull(&self, repo: &str, number: u64) -> Option<Value> {
        self.lock().pulls.get(&(repo.to_owned(), number)).cloned()
    }

    pub fn pulls(&self, repo: &str) -> Vec<Value> {
        let state = self.lock();
        let mut pulls: Vec<(u64, Value)> = state
            .pulls
            .iter()
            .filter(|((name, _), _)| name == repo)
            .map(|((_, number), pull)| (*number, pull.clone()))
            .collect();
        pulls.sort_by_key(|(number, _)| *number);
        pulls.into_iter().map(|(_, pull)| pull).collect()
    }

    pub fn issue(&self, repo: &str, number: u64) -> Option<Value> {
        self.lock().issues.get(&(repo.to_owned(), number)).cloned()
    }

    /// 改 PR 的 head（模拟有人推送了新提交）。分支 ref 同步移动。
    pub fn set_pull_head(&self, repo: &str, number: u64, sha: &str) {
        let mut state = self.lock();
        let Some(pull) = state.pulls.get_mut(&(repo.to_owned(), number)) else {
            return;
        };
        pull["head"]["sha"] = json!(sha);
        let head_ref = pull["head"]["ref"].as_str().unwrap_or_default().to_owned();
        state
            .refs
            .insert((repo.to_owned(), head_ref), sha.to_owned());
    }

    /// 改 PR 的 `state`（`open` / `closed`），不动合并标记。
    pub fn set_pull_state(&self, repo: &str, number: u64, pr_state: &str) {
        if let Some(pull) = self.lock().pulls.get_mut(&(repo.to_owned(), number)) {
            pull["state"] = json!(pr_state);
        }
    }

    /// 把 PR 置为已合并（模拟在 GitHub 上手动合并）。
    pub fn set_pull_merged(&self, repo: &str, number: u64) {
        let mut state = self.lock();
        if let Some(pull) = state.pulls.get_mut(&(repo.to_owned(), number)) {
            merge_in_place(pull);
        }
    }

    /// 仓库变更类调用（POST / PATCH / PUT / DELETE）的次数；`/app/*`（换令牌）不算。
    pub fn write_count(&self) -> usize {
        self.lock()
            .calls
            .iter()
            .filter(|call| is_repo_write(&call.method, &call.path))
            .count()
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
        let delivery_id = {
            let mut state = self.lock();
            let id = format!("{:032x}", state.next_delivery);
            state.next_delivery = state.next_delivery.saturating_add(1);
            id
        };
        self.send_delivery(event, &delivery_id, body)
    }

    /// 以原 `X-GitHub-Delivery` 重投一次已投递过的载荷（官方的手动 / 程序重投语义）。
    pub fn redeliver(&self, delivery_id: &str) -> Result<Vec<WebhookDelivery>, GithubError> {
        let original = self
            .lock()
            .deliveries
            .iter()
            .find(|delivery| delivery.delivery_id == delivery_id)
            .cloned()
            .ok_or_else(|| GithubError::new("unknown delivery"))?;
        self.send_delivery(&original.event, delivery_id, original.body)
    }

    /// 投递时附带的 App ID（`X-GitHub-Hook-Installation-Target-ID`）。
    pub fn set_app_id(&self, app_id: u64) {
        self.lock().app_id = Some(app_id);
    }

    fn send_delivery(
        &self,
        event: &str,
        delivery_id: &str,
        body: String,
    ) -> Result<Vec<WebhookDelivery>, GithubError> {
        let (targets, app_id) = {
            let state = self.lock();
            (state.webhooks.clone(), state.app_id)
        };
        let mut delivered = Vec::new();
        for target in targets {
            let status = send_webhook(&target, event, delivery_id, &body, app_id)?;
            delivered.push(WebhookDelivery {
                url: target.url,
                event: event.to_owned(),
                delivery_id: delivery_id.to_owned(),
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
            let (response, lose) = route(&request, &state);
            if lose {
                // 变更已生效，响应丢失：直接关闭连接。
                return Ok(());
            }
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
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Entity",
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

fn route(request: &HttpRequest, shared: &Arc<Mutex<GithubState>>) -> (Response, bool) {
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
    let lose = state.lose_responses > 0 && is_repo_write(&request.method, &path);
    if lose {
        state.lose_responses -= 1;
    }
    (response, lose)
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
        ("GET", ["repos", owner, repo, "pulls"]) => {
            let full = format!("{owner}/{repo}");
            let query = parse_query(&request.path);
            let head = query.get("head").cloned();
            let base = query.get("base").cloned();
            let wanted_state = query.get("state").cloned().unwrap_or_else(|| "open".into());
            let mut pulls: Vec<(u64, Value)> = state
                .pulls
                .iter()
                .filter(|((name, _), _)| *name == full)
                .filter(|(_, pull)| {
                    let head_ok = head.as_ref().is_none_or(|head| {
                        let branch = head.split_once(':').map(|(_, b)| b).unwrap_or(head);
                        pull["head"]["ref"] == json!(branch)
                    });
                    let base_ok = base
                        .as_ref()
                        .is_none_or(|base| pull["base"]["ref"] == json!(base));
                    let state_ok = wanted_state == "all" || pull["state"] == json!(wanted_state);
                    head_ok && base_ok && state_ok
                })
                .map(|((_, number), pull)| (*number, pull.clone()))
                .collect();
            pulls.sort_by_key(|(number, _)| *number);
            let list: Vec<Value> = pulls.into_iter().map(|(_, pull)| pull).collect();
            Response {
                status: 200,
                body: Value::Array(list).to_string(),
                retry_after_s: None,
            }
        }
        ("POST", ["repos", owner, repo, "pulls"]) => {
            let full = format!("{owner}/{repo}");
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let head_ref = string_field(&body, "head").unwrap_or_default();
            let base_ref = string_field(&body, "base").unwrap_or_else(|| "main".into());
            // 分支未经 `set_branch` 登记时沿用旧行为：生成确定性的 head sha。
            let sha = state
                .refs
                .get(&(full.clone(), head_ref.clone()))
                .cloned()
                .unwrap_or_else(|| format!("{:040x}", state.next_number));
            let duplicate = state.pulls.iter().any(|((name, _), pull)| {
                *name == full
                    && pull["state"] == json!("open")
                    && pull["head"]["ref"] == json!(head_ref)
                    && pull["base"]["ref"] == json!(base_ref)
            });
            if duplicate {
                return unprocessable("A pull request already exists");
            }
            let number = state.next_number;
            state.next_number = state.next_number.saturating_add(1);
            let title = string_field(&body, "title").unwrap_or_default();
            let body_text = string_field(&body, "body").unwrap_or_default();
            let draft = body.get("draft").and_then(Value::as_bool).unwrap_or(false);
            let author = (state.bot_login.clone(), state.bot_id);
            let pull = pull_json(
                number,
                &title,
                &body_text,
                draft,
                &head_ref,
                &sha,
                &base_ref,
                (&author.0, author.1),
            );
            state.pulls.insert((full, number), pull.clone());
            Response {
                status: 201,
                body: pull.to_string(),
                retry_after_s: None,
            }
        }
        ("PATCH", ["repos", owner, repo, "pulls", number]) => {
            let full = format!("{owner}/{repo}");
            let Some(number) = number.parse::<u64>().ok() else {
                return not_found();
            };
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let Some(pull) = state.pulls.get_mut(&(full, number)) else {
                return not_found();
            };
            for field in ["title", "body", "state"] {
                if let Some(value) = body.get(field) {
                    pull[field] = value.clone();
                }
            }
            if let Some(base) = body.get("base") {
                pull["base"]["ref"] = base.clone();
            }
            Response {
                status: 200,
                body: pull.to_string(),
                retry_after_s: None,
            }
        }
        ("PUT", ["repos", owner, repo, "pulls", number, "merge"]) => {
            let full = format!("{owner}/{repo}");
            let Some(number) = number.parse::<u64>().ok() else {
                return not_found();
            };
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let Some(pull) = state.pulls.get_mut(&(full, number)) else {
                return not_found();
            };
            if pull["merged"] == json!(true) || pull["state"] != json!("open") {
                return Response {
                    status: 405,
                    body: json!({"message": "Pull Request is not mergeable"}).to_string(),
                    retry_after_s: None,
                };
            }
            if let Some(sha) = body.get("sha") {
                if pull["head"]["sha"] != *sha {
                    return Response {
                        status: 409,
                        body: json!({"message": "Head branch was modified"}).to_string(),
                        retry_after_s: None,
                    };
                }
            }
            merge_in_place(pull);
            Response {
                status: 200,
                body: json!({
                    "sha": pull["merge_commit_sha"],
                    "merged": true,
                    "message": "Pull Request successfully merged"
                })
                .to_string(),
                retry_after_s: None,
            }
        }
        ("PATCH", ["repos", owner, repo, "issues", number]) => {
            let full = format!("{owner}/{repo}");
            let Some(number) = number.parse::<u64>().ok() else {
                return not_found();
            };
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let Some(issue) = state.issues.get_mut(&(full, number)) else {
                return not_found();
            };
            for field in ["title", "body", "state", "state_reason", "labels"] {
                if let Some(value) = body.get(field) {
                    issue[field] = value.clone();
                }
            }
            Response {
                status: 200,
                body: issue.to_string(),
                retry_after_s: None,
            }
        }
        ("GET", ["repos", owner, repo, "git", "ref", "heads", branch @ ..]) => {
            let full = format!("{owner}/{repo}");
            let branch = branch.join("/");
            match state.refs.get(&(full, branch.clone())) {
                Some(sha) => Response {
                    status: 200,
                    body: json!({
                        "ref": format!("refs/heads/{branch}"),
                        "object": {"type": "commit", "sha": sha}
                    })
                    .to_string(),
                    retry_after_s: None,
                },
                None => not_found(),
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
                "user": {"login": state.bot_login.clone(), "id": state.bot_id}
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
        ("POST", ["app", "installations", id, "access_tokens"]) => {
            if !has_app_jwt(request) {
                return unauthorized();
            }
            let Some(granted) = id
                .parse::<i64>()
                .ok()
                .and_then(|id| state.installations.get(&id).cloned())
            else {
                return not_found();
            };
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            state.issued_tokens += 1;
            Response {
                status: 201,
                body: json!({
                    "token": format!("ghs_fake{:06}", state.issued_tokens),
                    "expires_at": state.token_expires_at,
                    "permissions": body.get("permissions").cloned().unwrap_or(granted),
                    "repository_selection": "selected",
                })
                .to_string(),
                retry_after_s: None,
            }
        }
        ("GET", ["app", "installations", id]) => {
            if !has_app_jwt(request) {
                return unauthorized();
            }
            match id
                .parse::<i64>()
                .ok()
                .and_then(|id| state.installations.get(&id).map(|p| (id, p.clone())))
            {
                Some((id, permissions)) => Response {
                    status: 200,
                    body: json!({"id": id, "permissions": permissions}).to_string(),
                    retry_after_s: None,
                },
                None => not_found(),
            }
        }
        ("GET", ["repos", owner, repo, "issues", number, "comments"]) => {
            let full = format!("{owner}/{repo}");
            let Some(number) = number.parse::<u64>().ok() else {
                return not_found();
            };
            let key = (full, number);
            if !state.issues.contains_key(&key) && !state.pulls.contains_key(&key) {
                return not_found();
            }
            let query = parse_query(&request.path);
            let per_page = query
                .get("per_page")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(30)
                .clamp(1, 100);
            let page = query
                .get("page")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1);
            // RFC 3339 的 `Z` 形式按字节序即时间序；替身只接受这种形式。
            let since = query.get("since").cloned();
            let comments: Vec<Value> = state
                .comments
                .get(&key)
                .into_iter()
                .flatten()
                .filter(|comment| {
                    since.as_deref().is_none_or(|since| {
                        comment["updated_at"].as_str().unwrap_or_default() >= since
                    })
                })
                .skip((page - 1) * per_page)
                .take(per_page)
                .cloned()
                .collect();
            Response {
                status: 200,
                body: Value::Array(comments).to_string(),
                retry_after_s: None,
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

const DEFAULT_BOT_LOGIN: &str = "gantry-test-bot[bot]";
const DEFAULT_BOT_ID: i64 = 9;
const DEFAULT_TOKEN_EXPIRES_AT: &str = "2099-01-01T00:00:00Z";

#[allow(clippy::too_many_arguments)]
fn pull_json(
    number: u64,
    title: &str,
    body: &str,
    draft: bool,
    head_ref: &str,
    head_sha: &str,
    base_ref: &str,
    author: (&str, i64),
) -> Value {
    json!({
        "id": number,
        "number": number,
        "node_id": format!("PR_{number}"),
        "state": "open",
        "merged": false,
        "merge_commit_sha": null,
        "title": title,
        "body": body,
        "draft": draft,
        "head": {"ref": head_ref, "sha": head_sha},
        "base": {"ref": base_ref},
        "user": {"login": author.0, "id": author.1},
        "mergeable_state": "clean"
    })
}

fn merge_in_place(pull: &mut Value) {
    let number = pull["number"].as_u64().unwrap_or_default();
    pull["merged"] = json!(true);
    pull["state"] = json!("closed");
    pull["merge_commit_sha"] = json!(format!("{:040x}", 0xfeed_0000_u64 + number));
}

fn unprocessable(message: &str) -> Response {
    Response {
        status: 422,
        body: json!({ "message": message }).to_string(),
        retry_after_s: None,
    }
}

/// 解析查询串（只做 `%XX` 解码，足够替身使用）。
fn parse_query(path: &str) -> BTreeMap<String, String> {
    let Some((_, query)) = path.split_once('?') else {
        return BTreeMap::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

fn is_repo_write(method: &str, path: &str) -> bool {
    method != "GET" && path != "/graphql" && !path.starts_with("/app/")
}

/// App JWT 的形状检查：`Bearer <header>.<claims>.<signature>`。验签由调用方测试自己做
/// （替身不依赖 JWT 库）。
fn has_app_jwt(request: &HttpRequest) -> bool {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|jwt| jwt.split('.').count() == 3 && !jwt.starts_with("ghs_"))
}

fn unauthorized() -> Response {
    Response {
        status: 401,
        body: json!({"message":"A JSON web token could not be decoded"}).to_string(),
        retry_after_s: None,
    }
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
    app_id: Option<u64>,
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
    if let Some(app_id) = app_id {
        head.push_str(&format!(
            "X-GitHub-Hook-Installation-Target-ID: {app_id}\r\nX-GitHub-Hook-Installation-Target-Type: integration\r\n"
        ));
    }
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
