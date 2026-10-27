//! `CONNECT` 白名单测试：`docs/modules/egress.md` §12.D（隧道、M1 退出标准拒绝侧、IP 字面量与
//! 端口、DNS 重绑定与钉住、通配、有效分组、方法、上限、从不连 GitHub）、§12.A（无令牌 / 用户名
//! 不对 / 已吊销）与 egress-control §8 的 `CONNECT` 条目。

mod support;

use std::sync::Arc;
use std::time::Duration;

use gantry_egress::connect::{ConnectProxy, TunnelLimits};
use gantry_egress::netdeny::{Cidr, DenyNets};
use gantry_egress::upstream::Connector;
use serde_json::Value;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Tunnels {
    egress: Egress,
    addr: std::net::SocketAddr,
    resolver: Arc<FakeResolver>,
    dialer: Arc<MapDialer>,
}

/// 回显服务，扮演白名单内的包仓库（隧道不解 TLS，所以明文足够）。
async fn echo() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

async fn start(limits: TunnelLimits, extra_deny: Vec<Cidr>) -> Tunnels {
    let egress = Egress::start(Options::default()).await;
    let resolver = Arc::new(FakeResolver::default());
    for host in [
        "registry.npmjs.org",
        "crates.io",
        "index.crates.io",
        "a.b.crates.io",
        "pypi.org",
    ] {
        resolver.set(host, vec![vec![PUBLIC_IP]]);
    }
    let dialer = Arc::new(MapDialer::new(echo().await));
    let connector = Connector::new(
        resolver.clone(),
        dialer.clone(),
        DenyNets::new(extra_deny),
        Duration::from_secs(5),
    );
    let proxy = ConnectProxy::new(
        egress.registry.clone(),
        egress.audit.clone(),
        connector,
        limits,
        None,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(proxy.serve(listener));
    Tunnels {
        egress,
        addr,
        resolver,
        dialer,
    }
}

fn basic(user: &str, token: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{token}");
    let mut out = String::new();
    for chunk in raw.as_bytes().chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

/// 发 CONNECT，返回状态行、整个响应头与（成功时）连接本身。
async fn connect(
    addr: std::net::SocketAddr,
    target: &str,
    auth: Option<&str>,
) -> (String, String, TcpStream) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = auth {
        head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        if n == 0 {
            break;
        }
        response.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text.lines().next().unwrap_or("").to_owned();
    (status, text, stream)
}

fn last_audit(egress: &Egress) -> Value {
    egress.audit.events().last().cloned().unwrap()
}

/// 最近一条拒绝记录。拒绝审计在回应之前写下，所以收到拒绝时它一定已在；而先前放行的
/// 隧道会在关闭时补写 allow 记录，可能与之并发，因此不能取"最后一条"。
fn last_denial(egress: &Egress) -> Value {
    egress
        .audit
        .events()
        .into_iter()
        .rev()
        .find(|event| event["decision"] == "deny")
        .expect("a deny record")
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_hosts_get_a_tunnel_and_an_audit_record() {
    let t = start(TunnelLimits::default(), Vec::new()).await;
    let token = t.egress.issue(1, 1_000_000);
    let (status, _, mut stream) = connect(
        t.addr,
        "registry.npmjs.org:443",
        Some(&basic("job", &token)),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    // 关闭上行并读到 EOF：Egress 在关闭到调用方的连接之前写好审计。
    stream.shutdown().await.unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    let event = last_audit(&t.egress);
    assert_eq!(event["kind"], "connect");
    assert_eq!(event["decision"], "allow");
    assert_eq!(event["caller"]["type"], "job");
    assert_eq!(
        event["caller"]["job_id"],
        gantry_core::ids::AttemptId::from_parts(T0 as u64, 1)
            .unwrap()
            .to_string()
    );
    assert_eq!(event["host"], "registry.npmjs.org");
    assert_eq!(event["port"], 443);
    assert_eq!(event["bytes_in"], 4);
    assert_eq!(event["bytes_out"], 4);
    assert_eq!(event["resolved_ip"], PUBLIC_IP);
    assert!(!t.egress.audit.lines().concat().contains(&token[5..]));
}

#[tokio::test(flavor = "multi_thread")]
async fn github_is_hard_denied_with_an_audited_job_id() {
    let t = start(TunnelLimits::default(), Vec::new()).await;
    let token = t.egress.issue(1, 1_000_000);
    let auth = basic("job", &token);
    for target in [
        "github.com:443",
        "GitHub.COM:443",
        "github.com.:443",
        "api.github.com:443",
        "x.y.github.com:443",
        "raw.githubusercontent.com:443",
        "objects.githubusercontent.com:443",
        "ghcr.io:443",
        "foo.github.io:443",
        "ｇｉｔｈｕｂ.com:443",
        "a@github.com:443",
    ] {
        let (status, _, _) = connect(t.addr, target, Some(&auth)).await;
        assert!(status.starts_with("HTTP/1.1 403"), "{target}: {status}");
        let event = last_denial(&t.egress);
        assert_eq!(event["rule"], "hard.github", "{target}");
        assert_eq!(event["caller"]["type"], "job", "{target}");
    }
    // 从不为 GitHub 发起解析或连接。
    let queries = t.resolver.queries.lock().unwrap().clone();
    assert!(queries.iter().all(|q| !q.contains("github")), "{queries:?}");
    assert!(t.dialer.dialed.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn ip_literals_and_other_ports_are_denied() {
    let t = start(TunnelLimits::default(), Vec::new()).await;
    let token = t.egress.issue(1, 1_000_000);
    let auth = basic("job", &token);
    for (target, rule) in [
        ("127.0.0.1:443", "hard.ip_literal"),
        ("2130706433:443", "hard.ip_literal"),
        ("0x7f.0.0.1:443", "hard.ip_literal"),
        ("[::1]:443", "hard.ip_literal"),
        ("[::ffff:10.0.0.1]:443", "hard.ip_literal"),
        ("registry.npmjs.org:22", "hard.port"),
        ("registry.npmjs.org:80", "hard.port"),
        ("registry.npmjs.org:8443", "hard.port"),
    ] {
        let (status, _, _) = connect(t.addr, target, Some(&auth)).await;
        assert!(status.starts_with("HTTP/1.1 403"), "{target}: {status}");
        assert_eq!(last_denial(&t.egress)["rule"], rule, "{target}");
    }
    assert!(t.dialer.dialed.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_credentials_are_required_and_checked() {
    let t = start(TunnelLimits::default(), Vec::new()).await;
    let token = t.egress.issue(1, 1_000_000);
    let (status, head, _) = connect(t.addr, "registry.npmjs.org:443", None).await;
    assert!(status.starts_with("HTTP/1.1 407"), "{status}");
    assert!(head.contains("Proxy-Authenticate: Basic realm=\"gantry-egress\""));
    for auth in [
        basic("other", &token),
        format!("Bearer {token}"),
        basic("job", &format!("gjt1_{}", "B".repeat(43))),
    ] {
        let (status, _, _) = connect(t.addr, "registry.npmjs.org:443", Some(&auth)).await;
        assert!(status.starts_with("HTTP/1.1 403"), "{auth}: {status}");
    }
    t.egress
        .registry
        .revoke_job(&gantry_core::ids::AttemptId::from_parts(T0 as u64, 1).unwrap())
        .await
        .unwrap();
    let (status, _, _) = connect(
        t.addr,
        "registry.npmjs.org:443",
        Some(&basic("job", &token)),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 403"), "revoked: {status}");
    assert_eq!(last_denial(&t.egress)["rule"], "token.revoked");
    assert!(t.dialer.dialed.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_proxy_requests_are_rejected() {
    let t = start(TunnelLimits::default(), Vec::new()).await;
    let mut stream = TcpStream::connect(t.addr).await.unwrap();
    stream
        .write_all(b"GET http://registry.npmjs.org/ HTTP/1.1\r\nHost: registry.npmjs.org\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 256];
    let n = stream.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 405"));
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_outside_the_effective_groups_are_denied() {
    let t = start(TunnelLimits::default(), Vec::new()).await;
    let token = t.egress.issue(1, 1_000_000);
    let auth = basic("job", &token);
    let (status, _, _) = connect(t.addr, "pypi.org:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 403"));
    assert_eq!(last_denial(&t.egress)["rule"], "profile.not_in_groups");
    // 一层通配：index.crates.io 放行，a.b.crates.io 不放行（OQ-E8 取更严）。
    let (status, _, _) = connect(t.addr, "index.crates.io:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let (status, _, _) = connect(t.addr, "a.b.crates.io:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 403"), "{status}");
    // 收紧档案对存量 job 立即生效。
    let mut narrowed = serde_json::to_value(view(8, "localhost")).unwrap();
    narrowed["profiles"]["default"]["groups"] = serde_json::json!(["model_api", "crates"]);
    t.egress
        .registry
        .apply_config(serde_json::from_value(narrowed).unwrap())
        .unwrap();
    let (status, _, _) = connect(t.addr, "registry.npmjs.org:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 403"), "{status}");
    let usage = t
        .egress
        .registry
        .job_usage(&gantry_core::ids::AttemptId::from_parts(T0 as u64, 1).unwrap())
        .unwrap();
    assert_eq!(usage.denied.not_allowed_host, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn dns_answers_are_checked_and_pinned() {
    let t = start(
        TunnelLimits::default(),
        vec!["203.0.113.200/32".parse().unwrap()],
    )
    .await;
    let token = t.egress.issue(1, 1_000_000);
    let auth = basic("job", &token);
    // 重绑定：第一次公网，之后私网。第一次连上的是校验过的公网落点。
    t.resolver.set(
        "registry.npmjs.org",
        vec![vec![PUBLIC_IP], vec!["127.0.0.1"]],
    );
    let (status, _, _) = connect(t.addr, "registry.npmjs.org:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 200"));
    assert_eq!(
        t.dialer.dialed.lock().unwrap()[0],
        format!("{PUBLIC_IP}:443").parse().unwrap()
    );
    let (status, _, _) = connect(t.addr, "registry.npmjs.org:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 403"), "{status}");
    assert_eq!(last_denial(&t.egress)["rule"], "hard.private_ip");

    for answers in [
        vec![PUBLIC_IP, "10.1.2.3"],
        vec!["169.254.169.254"],
        vec!["::ffff:192.168.1.1"],
        vec!["64:ff9b::a00:1"],
        vec!["fd00::1"],
        vec!["203.0.113.200"], // egress 自身 / 沙盒网段（追加的拒绝网段）
    ] {
        t.resolver.set("crates.io", vec![answers.clone()]);
        let (status, _, _) = connect(t.addr, "crates.io:443", Some(&auth)).await;
        assert!(status.starts_with("HTTP/1.1 403"), "{answers:?}: {status}");
    }
    assert_eq!(t.dialer.dialed.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_limits_are_enforced() {
    let limits = TunnelLimits {
        idle: Duration::from_millis(200),
        tunnel_bytes: 8,
        per_token_connections: 1,
        ..TunnelLimits::default()
    };
    let t = start(limits, Vec::new()).await;
    let token = t.egress.issue(1, 1_000_000);
    let auth = basic("job", &token);

    // 单 job 连接数上限。
    let (status, _, held) = connect(t.addr, "registry.npmjs.org:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 200"));
    let (status, _, _) = connect(t.addr, "registry.npmjs.org:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 429"), "{status}");
    // 另一个 job 不受影响。
    let other = t.egress.issue(2, 1_000_000);
    let (status, _, _) = connect(
        t.addr,
        "registry.npmjs.org:443",
        Some(&basic("job", &other)),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");

    // 空闲超时：不发数据，隧道被关闭。
    let mut held = held;
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(2), held.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "idle tunnel must close"
    );

    // 字节上限：回显 4 字节两次之后超过 8 字节。
    let (status, _, mut stream) = connect(t.addr, "registry.npmjs.org:443", Some(&auth)).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    stream.write_all(b"abcd").await.unwrap();
    let mut echo = [0u8; 4];
    stream.read_exact(&mut echo).await.unwrap();
    stream.write_all(b"efgh").await.unwrap();
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut rest)).await;
    assert!(rest.len() < 4, "the tunnel is cut at the byte limit");
}
