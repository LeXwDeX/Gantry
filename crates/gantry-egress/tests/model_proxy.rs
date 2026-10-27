//! 模型反向代理测试：`docs/modules/egress.md` §12.A（归属、监听器、网段）、§12.B（key 注入与
//! 哨兵、路径 / 方法白名单、限量、流式、响应头清洗、上游证书与重定向、凭据缺失、上游错误、
//! 限速）、§12.C（计量与封顶、`svc_model` 回环监听与调用级关闭）、§12.H（每个请求一条审计）。

mod support;

use std::time::{Duration, Instant};

use gantry_core::entities::TokenUsage;
use gantry_egress::model_proxy::Limits;
use gantry_egress::netdeny::Cidr;
use support::*;

fn job_path(token: &str, rest: &str) -> String {
    format!("/j/{token}/anthropic{rest}")
}

async fn post_messages(egress: &Egress, token: &str, headers: &[(&str, &str)]) -> Reply {
    let mut all = vec![
        ("content-type", "application/json"),
        ("anthropic-version", "2023-06-01"),
    ];
    all.extend_from_slice(headers);
    send(
        egress.sandbox,
        "POST",
        &job_path(token, "/v1/messages"),
        &all,
        MESSAGES_BODY,
    )
    .await
}

fn usage_of(egress: &Egress, n: u128) -> gantry_core::egress::FinalUsage {
    egress
        .registry
        .job_usage(&gantry_core::ids::AttemptId::from_parts(T0 as u64, n).unwrap())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn key_is_injected_caller_credentials_dropped_and_usage_metered() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    egress.mock.push(Script::usage(3201, 410, 2800, 12));
    let reply = post_messages(
        &egress,
        &token,
        &[
            ("authorization", "Bearer caller-supplied"),
            ("x-api-key", "caller-key"),
            ("cookie", "session=1"),
            ("x-gantry-caller", "attempt-of-someone-else"),
            ("x-gantry-job", "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD"),
            ("x-gantry-call-id", "01JBX5D2F4H6K8M0P2R4T6V8XZ"),
            ("x-gantry-usage", "0"),
            ("anthropic-beta", "tools-2024"),
            ("user-agent", "claude-cli"),
        ],
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["id"], "msg_mock");

    let recorded = egress.mock.recorded();
    assert_eq!(recorded.len(), 1);
    let upstream = &recorded[0];
    assert_eq!(upstream.method, "POST");
    assert_eq!(upstream.path, "/v1/messages");
    assert_eq!(upstream.header("x-api-key"), Some(SENTINEL));
    assert_eq!(upstream.header("user-agent"), Some("gantry-egress/1"));
    assert_eq!(upstream.header("host"), Some("localhost"));
    assert_eq!(
        upstream.header_names(),
        vec![
            "anthropic-beta",
            "anthropic-version",
            "content-length",
            "content-type",
            "host",
            "user-agent",
            "x-api-key"
        ],
        "white-listed headers plus the injected key only"
    );
    assert_eq!(upstream.body, MESSAGES_BODY.as_bytes());

    let usage = usage_of(&egress, 1);
    assert_eq!(usage.usage, TokenUsage::new(3201, 410, 2800, 12, 1));
    assert_eq!(usage.weighted_tokens, 3201 + 410 + 2800 + 12);
    let records = egress.registry.usage_since(None, None).unwrap().items;
    assert_eq!(records[0].model, "example-worker-model");

    // 每个请求恰好一条审计；不含令牌、key、头值。
    let events = egress.audit.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["decision"], "allow");
    assert_eq!(event["caller"]["type"], "job");
    assert_eq!(event["host"], "localhost");
    assert_eq!(event["path"], "/v1/messages");
    assert_eq!(event["status"], 200);
    assert_eq!(event["usage"]["input_tokens"], 3201);
    assert_eq!(event["resolved_ip"], PUBLIC_IP);
    let audit = egress.audit.lines().concat();
    for needle in [SENTINEL, &token[5..], "caller-key", "session=1"] {
        assert!(!audit.contains(needle), "audit leaks {needle}");
    }
    assert!(!reply.body.contains(SENTINEL));
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_is_streamed_before_the_upstream_finishes() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    egress
        .mock
        .push(Script::sse(10, &[3, 9], Duration::from_millis(400), false));
    let started = Instant::now();
    let stream = tokio::net::TcpStream::connect(egress.sandbox)
        .await
        .unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let request = hyper::Request::post(job_path(&token, "/v1/messages?beta=true"))
        .header("host", "egress")
        .body(http_body_util::Full::new(bytes::Bytes::from(MESSAGES_BODY)))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let mut body = response.into_body();
    use http_body_util::BodyExt;
    let first = body.frame().await.unwrap().unwrap();
    let first_at = started.elapsed();
    assert!(String::from_utf8_lossy(first.data_ref().unwrap()).contains("message_start"));
    let mut rest = Vec::new();
    while let Some(frame) = body.frame().await {
        rest.extend_from_slice(frame.unwrap().data_ref().unwrap());
    }
    let total = started.elapsed();
    assert!(
        first_at < Duration::from_millis(350),
        "first byte took {first_at:?}"
    );
    assert!(
        total >= Duration::from_millis(1100),
        "stream finished after {total:?}"
    );
    assert!(String::from_utf8_lossy(&rest).contains("message_stop"));

    let usage = usage_of(&egress, 1);
    assert_eq!(usage.usage, TokenUsage::new(10, 9, 0, 0, 1));
    assert!(!usage.incomplete);
    // 查询串转发给上游，但不进审计。
    assert_eq!(egress.mock.recorded()[0].path, "/v1/messages?beta=true");
    assert_eq!(egress.audit.events()[0]["path"], "/v1/messages");
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_upstream_streams_are_truncated_and_estimated() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    egress
        .mock
        .push(Script::sse(100, &[4], Duration::from_millis(10), true));
    let reply = post_messages(&egress, &token, &[]).await;
    assert_eq!(reply.status, 200);
    assert!(reply.truncated, "the caller sees a truncated stream");
    let usage = usage_of(&egress, 1);
    assert!(usage.incomplete);
    assert!(usage.usage.input_tokens >= 100, "known lower bound kept");
    assert_eq!(usage.upstream_errors.total_conn_errors, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_disconnect_cancels_the_upstream_stream() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    egress.mock.push(Script::sse(
        10,
        &[1, 2, 3, 4, 5, 6],
        Duration::from_millis(200),
        false,
    ));
    let stream = tokio::net::TcpStream::connect(egress.sandbox)
        .await
        .unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    let conn = tokio::spawn(conn);
    let request = hyper::Request::post(job_path(&token, "/v1/messages"))
        .header("host", "egress")
        .body(http_body_util::Full::new(bytes::Bytes::from(MESSAGES_BODY)))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let mut body = response.into_body();
    use http_body_util::BodyExt;
    body.frame().await.unwrap().unwrap();
    drop(body);
    drop(sender);
    conn.abort();
    // 调用方断开后它看不到任何结束信号：按条件等待，而不是假定某个时长内完成。
    wait_until("upstream stream is cancelled", || {
        egress
            .mock
            .aborted
            .load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;
    wait_until("the interrupted request is metered", || {
        usage_of(&egress, 1).incomplete
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_white_listed_paths_and_methods_are_forwarded() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    let cases: &[(&str, String, u16, &str)] = &[
        (
            "POST",
            job_path(&token, "/v1/files"),
            403,
            "path_not_allowed",
        ),
        (
            "GET",
            job_path(&token, "/v1/organizations/users"),
            403,
            "path_not_allowed",
        ),
        (
            "POST",
            job_path(&token, "/v1/../v1/messages"),
            403,
            "path_not_allowed",
        ),
        (
            "POST",
            job_path(&token, "/v1//messages"),
            403,
            "path_not_allowed",
        ),
        (
            "POST",
            job_path(&token, "/v1/%2e%2e/messages"),
            403,
            "path_not_allowed",
        ),
        (
            "POST",
            job_path(&token, "/v1%2fmessages"),
            403,
            "path_not_allowed",
        ),
        (
            "GET",
            job_path(&token, "/v1/messages"),
            403,
            "method_not_allowed",
        ),
        (
            "DELETE",
            job_path(&token, "/v1/models"),
            403,
            "method_not_allowed",
        ),
        (
            "POST",
            format!("/x/{token}/anthropic/v1/messages"),
            404,
            "not_found",
        ),
        ("POST", "/control/v1/status".to_owned(), 404, "not_found"),
        ("POST", format!("/r/{token}/npm/x"), 404, "not_found"),
        (
            "POST",
            format!("/j/{token}/other/v1/messages"),
            403,
            "provider_not_allowed",
        ),
    ];
    for (method, path, status, error_type) in cases {
        let reply = send(egress.sandbox, method, path, &[], MESSAGES_BODY).await;
        assert_eq!(reply.status, *status, "{method} {path}: {}", reply.body);
        assert_eq!(reply.error_type(), *error_type, "{method} {path}");
    }
    // 绝对形式 URI。
    let reply = send(
        egress.sandbox,
        "POST",
        &format!("http://evil.example{}", job_path(&token, "/v1/messages")),
        &[],
        MESSAGES_BODY,
    )
    .await;
    assert_eq!(reply.status, 403);
    assert_eq!(reply.error_type(), "path_not_allowed");
    assert!(egress.mock.recorded().is_empty(), "nothing is forwarded");
    assert!(usage_of(&egress, 1).denied.path_not_allowed >= 6);
    assert_eq!(egress.audit.events().len(), cases.len() + 1);
    assert!(
        !egress.audit.lines().concat().contains(&token[5..]),
        "rejected paths are audited without the token"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn request_limits_are_enforced() {
    let limits = Limits {
        request_body_bytes: 1024,
        header_read_timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    let egress = Egress::start(Options {
        limits,
        ..Options::default()
    })
    .await;
    let token = egress.issue(1, 1_000_000);
    let big = "x".repeat(2048);
    let reply = send(
        egress.sandbox,
        "POST",
        &job_path(&token, "/v1/messages"),
        &[],
        &big,
    )
    .await;
    assert_eq!(reply.status, 413);
    assert_eq!(reply.error_type(), "request_too_large");

    let huge_header = "y".repeat(20 * 1024);
    let raw = format!(
        "POST {} HTTP/1.1\r\nhost: egress\r\nx-big: {huge_header}\r\ncontent-length: 2\r\n\r\n{{}}",
        job_path(&token, "/v1/messages")
    );
    let status = raw_status(egress.sandbox, &raw).await;
    assert!(status.starts_with("HTTP/1.1 431"), "{status}");

    let raw = format!(
        "POST {} HTTP/1.1\r\nhost: egress\r\ncontent-length: 2\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n",
        job_path(&token, "/v1/messages")
    );
    let status = raw_status(egress.sandbox, &raw).await;
    assert!(status.starts_with("HTTP/1.1 400"), "{status}");
    let raw = format!(
        "POST {} HTTP/1.1\r\nhost: egress\r\ncontent-length: 2\r\ncontent-length: 3\r\n\r\n{{}}",
        job_path(&token, "/v1/messages")
    );
    let status = raw_status(egress.sandbox, &raw).await;
    assert!(status.starts_with("HTTP/1.1 400"), "{status}");

    // slowloris：请求头迟迟不发完 → 连接被关闭。
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut slow = tokio::net::TcpStream::connect(egress.sandbox)
        .await
        .unwrap();
    slow.write_all(b"POST /j/x/anthropic/v1/messages HTTP/1.1\r\nhost: e")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(3), slow.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "slow header sender must be disconnected");
    assert!(egress.mock.recorded().is_empty());
}

async fn raw_status(addr: std::net::SocketAddr, raw: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or("")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn response_headers_are_cleaned() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    egress.mock.push(Script::Raw {
        status: 200,
        headers: vec![
            ("content-type", "application/json".into()),
            ("set-cookie", "a=b".into()),
            ("connection", "x-hop".into()),
            ("x-hop", "1".into()),
            ("keep-alive", "timeout=5".into()),
            ("x-echo", format!("key={SENTINEL}")),
            ("request-id", "req_1".into()),
        ],
        body: r#"{"usage":{"input_tokens":1,"output_tokens":1}}"#.into(),
    });
    let reply = post_messages(&egress, &token, &[]).await;
    assert_eq!(reply.status, 200);
    for name in ["set-cookie", "x-hop", "keep-alive", "x-echo"] {
        assert!(reply.header(name).is_none(), "{name} must be stripped");
    }
    assert_eq!(reply.header("request-id"), Some("req_1"));
    assert!(!format!("{:?}", reply.headers).contains(SENTINEL));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_upstream_certificates_fail_closed() {
    // 证书只对 localhost 有效；provider 主机不符 → 502，不降级。
    let egress = Egress::start(Options {
        provider_host: "api.mock.test",
        ..Options::default()
    })
    .await;
    let token = egress.issue(1, 1_000_000);
    let reply = post_messages(&egress, &token, &[]).await;
    assert_eq!(reply.status, 502);
    assert_eq!(reply.error_type(), "upstream_unavailable");
    assert!(egress.mock.recorded().is_empty());

    // 不受信任的 CA。
    let untrusted = Egress::start(Options {
        trust_test_ca: false,
        ..Options::default()
    })
    .await;
    let token = untrusted.issue(1, 1_000_000);
    assert_eq!(post_messages(&untrusted, &token, &[]).await.status, 502);
    assert!(untrusted.mock.recorded().is_empty());
    assert_eq!(usage_of(&untrusted, 1).upstream_errors.total_conn_errors, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_are_returned_not_followed() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    egress.mock.push(Script::Raw {
        status: 302,
        headers: vec![("location", "https://elsewhere.example/v1/messages".into())],
        body: String::new(),
    });
    let reply = post_messages(&egress, &token, &[]).await;
    assert_eq!(reply.status, 302);
    assert_eq!(
        reply.header("location"),
        Some("https://elsewhere.example/v1/messages")
    );
    assert_eq!(egress.mock.recorded().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_credentials_only_affect_their_provider() {
    let egress = Egress::start(Options::default()).await;
    let (svc, _) = egress.svc();
    let headers = [
        ("x-gantry-ledger", "unattributed"),
        ("x-gantry-tier", "utility"),
        ("x-gantry-call-id", "01JBX5D2F4H6K8M0P2R4T6V8XZ"),
    ];
    let reply = send(
        egress.loopback,
        "POST",
        &format!("/s/{svc}/backup/v1/messages"),
        &headers,
        MESSAGES_BODY,
    )
    .await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.error_type(), "credential_missing");
    let status = egress.registry.status().unwrap();
    assert_eq!(
        status.unresolved_secret_refs[0].to_string(),
        "model.backup.main"
    );
    let ok = send(
        egress.loopback,
        "POST",
        &format!("/s/{svc}/anthropic/v1/messages"),
        &headers,
        MESSAGES_BODY,
    )
    .await;
    assert_eq!(ok.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_errors_pass_through_and_are_summarised() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    for status in [429, 529, 500] {
        egress.mock.push(Script::status(status));
    }
    egress.mock.push(Script::usage(1, 1, 0, 0));
    egress.mock.push(Script::status(503));
    for expected in [429, 529, 500, 200, 503] {
        let reply = post_messages(&egress, &token, &[]).await;
        assert_eq!(reply.status, expected);
    }
    let usage = usage_of(&egress, 1);
    assert_eq!(usage.upstream_errors.total_429, 1);
    assert_eq!(usage.upstream_errors.total_5xx, 3);
    assert_eq!(usage.upstream_errors.max_consecutive, 3);
    assert_eq!(usage.usage.requests, 1, "failed requests carry no usage");
}

#[tokio::test(flavor = "multi_thread")]
async fn per_job_rate_limits_do_not_affect_other_jobs() {
    let egress = Egress::start(Options {
        limits: Limits {
            job_requests_per_min: 2,
            ..Limits::default()
        },
        ..Options::default()
    })
    .await;
    let a = egress.issue(1, 1_000_000);
    let b = egress.issue(2, 1_000_000);
    assert_eq!(post_messages(&egress, &a, &[]).await.status, 200);
    assert_eq!(post_messages(&egress, &a, &[]).await.status, 200);
    let limited = post_messages(&egress, &a, &[]).await;
    assert_eq!(limited.status, 429);
    assert_eq!(limited.error_type(), "rate_limited");
    assert_eq!(post_messages(&egress, &b, &[]).await.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_cap_cuts_off_further_requests() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 100);
    egress.mock.push(Script::usage(60, 39, 0, 0)); // 99
    egress.mock.push(Script::usage(1, 0, 0, 0)); // 100
    assert_eq!(post_messages(&egress, &token, &[]).await.status, 200);
    assert_eq!(post_messages(&egress, &token, &[]).await.status, 200);
    let capped = post_messages(&egress, &token, &[("x-gantry-usage", "0")]).await;
    assert_eq!(capped.status, 403);
    assert_eq!(capped.error_type(), "budget_exhausted");
    assert_eq!(egress.mock.recorded().len(), 2);
    let usage = usage_of(&egress, 1);
    assert!(usage.capped);
    assert_eq!(usage.weighted_tokens, 100);
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_are_rejected_identically_on_the_wrong_listener_or_after_revoke() {
    let egress = Egress::start(Options::default()).await;
    let job = egress.issue(1, 1_000_000);
    let (svc, _) = egress.svc();
    let forged = format!("gjt1_{}", "A".repeat(43));
    let attempts = [
        (egress.sandbox, format!("/j/{forged}/anthropic/v1/messages")),
        (egress.sandbox, format!("/j/{svc}/anthropic/v1/messages")),
        (egress.sandbox, format!("/s/{svc}/anthropic/v1/messages")),
        (egress.loopback, format!("/s/{job}/anthropic/v1/messages")),
        (egress.loopback, format!("/j/{job}/anthropic/v1/messages")),
    ];
    let mut bodies = Vec::new();
    for (addr, path) in &attempts {
        let reply = send(*addr, "POST", path, &[], MESSAGES_BODY).await;
        assert_eq!(reply.status, 403, "{path}");
        assert_eq!(reply.error_type(), "token_invalid");
        bodies.push(reply.body);
    }
    egress
        .registry
        .revoke_job(&gantry_core::ids::AttemptId::from_parts(T0 as u64, 1).unwrap())
        .await
        .unwrap();
    let revoked = post_messages(&egress, &job, &[]).await;
    assert_eq!(revoked.status, 403);
    bodies.push(revoked.body);
    bodies.dedup();
    assert_eq!(bodies.len(), 1, "all token rejections look the same");
    assert!(egress.mock.recorded().is_empty());
    let rules: Vec<String> = egress
        .audit
        .events()
        .iter()
        .map(|e| e["rule"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        rules,
        vec![
            "token.unknown",
            "token.wrong_listener",
            "token.wrong_listener",
            "token.wrong_listener",
            "token.wrong_listener",
            "token.revoked"
        ]
    );
    for line in egress.audit.lines() {
        assert!(!line.contains(&job[5..]) && !line.contains(&svc[5..]));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_from_outside_the_sandbox_networks_are_rejected_when_enforceable() {
    let egress = Egress::start(Options {
        sandbox_cidrs: Some(vec!["10.89.0.0/16".parse::<Cidr>().unwrap()]),
        ..Options::default()
    })
    .await;
    let token = egress.issue(1, 1_000_000);
    let reply = post_messages(&egress, &token, &[]).await;
    assert_eq!(reply.status, 403);
    assert_eq!(reply.error_type(), "token_invalid");
    assert_eq!(egress.audit.events()[0]["rule"], "token.source_not_allowed");
}

#[tokio::test(flavor = "multi_thread")]
async fn svc_model_requires_ledger_headers_and_honours_call_closure() {
    let egress = Egress::start(Options::default()).await;
    let (svc, svc_id) = egress.svc();
    let path = format!("/s/{svc}/anthropic/v1/messages");
    let call = "01JBX5D2F4H6K8M0P2R4T6V8XZ";
    for headers in [
        vec![("x-gantry-tier", "utility"), ("x-gantry-call-id", call)],
        vec![
            ("x-gantry-ledger", "dlv_bad"),
            ("x-gantry-tier", "utility"),
            ("x-gantry-call-id", call),
        ],
        vec![
            ("x-gantry-ledger", "unattributed"),
            ("x-gantry-tier", "worker"),
            ("x-gantry-call-id", call),
        ],
        vec![
            ("x-gantry-ledger", "unattributed"),
            ("x-gantry-tier", "utility"),
            ("x-gantry-call-id", "nope"),
        ],
    ] {
        let reply = send(egress.loopback, "POST", &path, &headers, MESSAGES_BODY).await;
        assert_eq!(reply.status, 400);
        assert_eq!(reply.error_type(), "ledger_header_invalid");
    }
    assert!(egress.mock.recorded().is_empty());
    assert!(egress
        .registry
        .usage_since(None, None)
        .unwrap()
        .items
        .is_empty());

    egress.mock.push(Script::usage(3201, 410, 2800, 0));
    let headers = [
        (
            "x-gantry-ledger",
            "cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ:2026-10-24",
        ),
        ("x-gantry-tier", "planner"),
        ("x-gantry-call-id", call),
        ("content-type", "application/json"),
    ];
    let reply = send(egress.loopback, "POST", &path, &headers, MESSAGES_BODY).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-gantry-call-id"), Some(call));
    let upstream = &egress.mock.recorded()[0];
    assert!(upstream
        .headers
        .iter()
        .all(|(n, _)| !n.starts_with("x-gantry")));

    let record = &egress.registry.usage_since(None, None).unwrap().items[0];
    assert_eq!(record.tier, gantry_core::entities::Tier::Planner);
    assert_eq!(
        serde_json::to_value(&record.ledger).unwrap(),
        serde_json::json!({"subject": "conversation", "id": "cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ", "day": "2026-10-24"})
    );
    assert_eq!(record.weighted_tokens, 3201 + 410 * 5 + 280);

    let closed = egress
        .registry
        .close_call(&svc_id, call.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(closed.terminal.record_count, 1);
    let after = send(egress.loopback, "POST", &path, &headers, MESSAGES_BODY).await;
    assert_eq!(after.status, 403);
    assert_eq!(after.error_type(), "token_invalid");
    assert_eq!(egress.mock.recorded().len(), 1);
    let statement = egress
        .registry
        .call_statement(call.parse().unwrap())
        .unwrap();
    closed
        .terminal
        .verify(call.parse().unwrap(), &statement.items)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_failures_fail_closed() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    egress.audit.fail_writes(true);
    // 这一条照常完成，但审计写失败 → 之后的请求一律拒绝。
    assert_eq!(post_messages(&egress, &token, &[]).await.status, 200);
    let reply = post_messages(&egress, &token, &[]).await;
    assert_eq!(reply.status, 503);
    let status = egress.registry.status().unwrap();
    assert_eq!(status.health, gantry_core::egress::EgressHealth::Unhealthy);
    assert_eq!(egress.mock.recorded().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sentinel_key_never_reaches_callers_or_the_audit_log() {
    let egress = Egress::start(Options::default()).await;
    let token = egress.issue(1, 1_000_000);
    let (svc, _) = egress.svc();
    egress.mock.push(Script::Raw {
        status: 401,
        headers: vec![("content-type", "application/json".into()), ("www-authenticate", format!("x-api-key {SENTINEL}"))],
        body: r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.into(),
    });
    let mut replies = vec![post_messages(&egress, &token, &[]).await];
    replies.push(
        send(
            egress.sandbox,
            "POST",
            &job_path(&token, "/v1/files"),
            &[],
            "",
        )
        .await,
    );
    replies.push(
        send(
            egress.loopback,
            "POST",
            &format!("/s/{svc}/backup/v1/messages"),
            &[
                ("x-gantry-ledger", "unattributed"),
                ("x-gantry-tier", "utility"),
                ("x-gantry-call-id", "01JBX5D2F4H6K8M0P2R4T6V8XZ"),
            ],
            MESSAGES_BODY,
        )
        .await,
    );
    for reply in &replies {
        assert!(!reply.body.contains(SENTINEL));
        assert!(!format!("{:?}", reply.headers).contains(SENTINEL));
    }
    assert!(!egress.audit.lines().concat().contains(SENTINEL));
    assert!(!format!("{:?}", egress.registry.status().unwrap()).contains(SENTINEL));
}
