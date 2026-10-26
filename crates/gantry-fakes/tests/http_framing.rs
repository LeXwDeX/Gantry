//! HTTP 请求分帧的确定性回归：请求可能在 `accept` 之后才到达，也可能被分片、
//! 缺 body 或带非法 / 冲突的 `Content-Length`。
//!
//! 重复运行（旧实现在延迟或分片请求上确定性失败）：
//!
//! ```text
//! for i in $(seq 1 50); do
//!   cargo test -p gantry-fakes --test http_framing --test runtime_fakes -- --test-threads=1 || break
//! done
//! ```
//!
//! `--test-threads=1` 让失败可归因；旧实现的 `runtime_fakes` 在并行负载下约
//! 15% 的运行会丢失一次响应（连接被空关闭），单测试循环则不会复现。

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use gantry_fakes::egress::EgressStub;
use gantry_fakes::github::FakeGitHub;

struct Response {
    status: u16,
    body: String,
}

fn connect(url: &str) -> TcpStream {
    let rest = url.strip_prefix("http://").expect("http url");
    let (authority, _) = rest.split_once('/').expect("url path");
    let stream = TcpStream::connect(authority).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
}

fn read_response(stream: &mut TcpStream) -> Response {
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response bytes");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "http response has no head/body separator ({} bytes: {:?})",
                raw.len(),
                String::from_utf8_lossy(&raw)
            )
        });
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("status line");
    Response {
        status,
        body: String::from_utf8_lossy(&raw[split + 4..]).to_string(),
    }
}

/// 连接、可选延迟、一次写入完整请求、半关闭写端，然后读取响应。
fn exchange(url: &str, delay: Duration, request: &[u8]) -> Response {
    let mut stream = connect(url);
    std::thread::sleep(delay);
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush request");
    stream
        .shutdown(Shutdown::Write)
        .expect("shutdown request write");
    read_response(&mut stream)
}

fn request(method: &str, path: &str, extra: &str, body: &str) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: fake\r\nConnection: close\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn github_serves_request_sent_after_connect_delay() {
    // 根因回归：监听 socket 是非阻塞的，macOS 上 accept 出来的连接继承该标志，
    // 旧实现第一次 read 得到 WouldBlock 后直接关闭连接，客户端只能读到 EOF。
    let mut github = FakeGitHub::start();
    let url = format!("{}/health", github.base_url());
    let response = exchange(
        &url,
        Duration::from_millis(150),
        request("GET", "/health", "", "").as_bytes(),
    );
    assert_eq!(response.status, 200);
    assert!(response.body.contains("ok"));
    assert_eq!(github.recorded_calls().len(), 1);
    github.shutdown();
}

#[test]
fn github_rejects_conflicting_content_length() {
    let mut github = FakeGitHub::start();
    let url = format!("{}/graphql", github.base_url());
    let raw = concat!(
        "POST /graphql HTTP/1.1\r\nHost: fake\r\nConnection: close\r\n",
        "Content-Length: 5\r\nContent-Length: 7\r\n\r\n",
        r#"{"a":1}"#
    );
    let response = exchange(&url, Duration::ZERO, raw.as_bytes());
    assert_eq!(response.status, 400, "body: {}", response.body);
    assert!(
        github.recorded_calls().is_empty(),
        "a rejected request must not be recorded"
    );
    github.shutdown();
}

#[test]
fn github_rejects_invalid_content_length() {
    let mut github = FakeGitHub::start();
    let url = format!("{}/graphql", github.base_url());
    for value in ["abc", "+5", "-1", "5x", ""] {
        let raw = format!(
            "POST /graphql HTTP/1.1\r\nHost: fake\r\nConnection: close\r\nContent-Length: {value}\r\n\r\n{{}}"
        );
        let response = exchange(&url, Duration::ZERO, raw.as_bytes());
        assert_eq!(response.status, 400, "Content-Length: {value:?}");
    }
    assert!(github.recorded_calls().is_empty());
    github.shutdown();
}

#[test]
fn github_rejects_oversized_content_length_without_overflowing() {
    let mut github = FakeGitHub::start();
    let url = format!("{}/graphql", github.base_url());
    // u64 溢出与超过 16 MiB 上限都必须拒绝，且不能触发 usize 加法溢出。
    for value in ["18446744073709551616", "999999999999", "16777217"] {
        let raw = format!(
            "POST /graphql HTTP/1.1\r\nHost: fake\r\nConnection: close\r\nContent-Length: {value}\r\n\r\n{{}}"
        );
        let response = exchange(&url, Duration::ZERO, raw.as_bytes());
        assert_eq!(response.status, 400, "Content-Length: {value}");
    }
    assert!(github.recorded_calls().is_empty());
    github.shutdown();
}

#[test]
fn github_rejects_incomplete_body() {
    let mut github = FakeGitHub::start();
    let url = format!("{}/graphql", github.base_url());
    let mut stream = connect(&url);
    stream
        .write_all(
            b"POST /graphql HTTP/1.1\r\nHost: fake\r\nConnection: close\r\nContent-Length: 10\r\n\r\nabc",
        )
        .expect("write partial request");
    stream.shutdown(Shutdown::Write).expect("shutdown write");
    let response = read_response(&mut stream);
    assert_eq!(response.status, 400, "body: {}", response.body);
    assert!(
        github.recorded_calls().is_empty(),
        "a truncated body must not be treated as a request"
    );
    github.shutdown();
}

#[test]
fn github_accepts_fragmented_body() {
    let mut github = FakeGitHub::start();
    let url = format!("{}/graphql", github.base_url());
    let body = r#"{"query":"{ viewer { login } }"}"#;
    let head = format!(
        "POST /graphql HTTP/1.1\r\nHost: fake\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut stream = connect(&url);
    stream.write_all(head.as_bytes()).expect("write head");
    for chunk in body.as_bytes().chunks(7) {
        std::thread::sleep(Duration::from_millis(20));
        stream.write_all(chunk).expect("write body chunk");
    }
    stream.shutdown(Shutdown::Write).expect("shutdown write");
    let response = read_response(&mut stream);
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(response.body.contains("gantry-test-bot"));
    let calls = github.recorded_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body, body);
    github.shutdown();
}

#[test]
fn egress_serves_request_sent_after_connect_delay() {
    let mut egress = EgressStub::with_default_config();
    let url = format!("{}/j/gjt1_unknown/anthropic", egress.data_plane_url());
    let response = exchange(
        &url,
        Duration::from_millis(150),
        request("POST", "/j/gjt1_unknown/anthropic", "", "{}").as_bytes(),
    );
    assert_eq!(response.status, 403, "body: {}", response.body);
    assert!(response.body.contains("token_invalid"));
    egress.shutdown();
}

#[test]
fn egress_rejects_conflicting_content_length() {
    let mut egress = EgressStub::with_default_config();
    let url = format!("{}/j/gjt1_unknown/anthropic", egress.data_plane_url());
    let raw = concat!(
        "POST /j/gjt1_unknown/anthropic HTTP/1.1\r\nHost: fake\r\nConnection: close\r\n",
        "Content-Length: 0\r\nContent-Length: 5\r\n\r\n",
        "hello"
    );
    let response = exchange(&url, Duration::ZERO, raw.as_bytes());
    assert_eq!(response.status, 400, "body: {}", response.body);
    assert!(egress.calls().is_empty());
    egress.shutdown();
}

#[test]
fn egress_rejects_incomplete_body() {
    let mut egress = EgressStub::with_default_config();
    let url = format!("{}/j/gjt1_unknown/anthropic", egress.data_plane_url());
    let mut stream = connect(&url);
    stream
        .write_all(
            b"POST /j/gjt1_unknown/anthropic HTTP/1.1\r\nHost: fake\r\nConnection: close\r\nContent-Length: 10\r\n\r\nabc",
        )
        .expect("write partial request");
    stream.shutdown(Shutdown::Write).expect("shutdown write");
    let response = read_response(&mut stream);
    assert_eq!(response.status, 400, "body: {}", response.body);
    assert!(egress.calls().is_empty());
    egress.shutdown();
}

#[test]
fn egress_rejects_oversized_content_length_without_overflowing() {
    let mut egress = EgressStub::with_default_config();
    let url = format!("{}/j/gjt1_unknown/anthropic", egress.data_plane_url());
    for value in ["18446744073709551616", "999999999999"] {
        let raw = format!(
            "POST /j/gjt1_unknown/anthropic HTTP/1.1\r\nHost: fake\r\nConnection: close\r\nContent-Length: {value}\r\n\r\n{{}}"
        );
        let response = exchange(&url, Duration::ZERO, raw.as_bytes());
        assert_eq!(response.status, 400, "Content-Length: {value}");
    }
    assert!(egress.calls().is_empty());
    egress.shutdown();
}
