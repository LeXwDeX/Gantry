//! 包仓库 `CONNECT` 白名单隧道（`docs/modules/egress.md` §7.5；`docs/interfaces/egress-control.md`
//! §5；P-72）。
//!
//! 不注入凭据、不解 TLS。流水线：读请求头（≤ 8 KiB、10 s）→ 方法必须是 `CONNECT` →
//! `Proxy-Authorization`（Basic，用户名 `job`）里的 `job_token` → 严格解析 authority 与硬性
//! 拒绝表 → 有效分组 → 自己解析并校验全部落点、钉住第一个建连 → `200` 后双向拼接，受空闲 /
//! 总时长 / 字节上限约束。允许与拒绝都恰好一条审计。

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::entities::Tier;
use gantry_core::ids::AttemptId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crate::audit::{AuditCaller, AuditEvent, AuditKind, AuditSink, Decision};
use crate::host::{parse_connect_authority, Host, CONNECT_PORT};
use crate::netdeny::Cidr;
use crate::registry::{Caller, Denial, DeniedKind, Listener, Registry};
use crate::upstream::{ConnectError, Connector};

/// 隧道上限（§10 [建议值，待调优]，OQ-E6）。
#[derive(Clone, Debug)]
pub struct TunnelLimits {
    pub head_bytes: usize,
    pub head_timeout: Duration,
    pub idle: Duration,
    pub max_duration: Duration,
    pub tunnel_bytes: u64,
    pub job_tunnel_bytes: u64,
    pub per_token_connections: usize,
}

impl Default for TunnelLimits {
    fn default() -> Self {
        Self {
            head_bytes: 8 * 1024,
            head_timeout: Duration::from_secs(10),
            idle: Duration::from_secs(60),
            max_duration: Duration::from_secs(1800),
            tunnel_bytes: 1 << 30,
            job_tunnel_bytes: 4 << 30,
            per_token_connections: 64,
        }
    }
}

#[derive(Default)]
struct JobTunnels {
    open: usize,
    bytes: u64,
}

pub struct ConnectProxy {
    registry: Registry,
    audit: Arc<dyn AuditSink>,
    connector: Connector,
    limits: TunnelLimits,
    sandbox_cidrs: Option<Vec<Cidr>>,
    jobs: Mutex<HashMap<AttemptId, JobTunnels>>,
}

struct Refusal {
    status: u16,
    reason: &'static str,
    error_type: &'static str,
    rule: &'static str,
}

impl Refusal {
    fn new(
        status: u16,
        reason: &'static str,
        error_type: &'static str,
        rule: &'static str,
    ) -> Self {
        Self {
            status,
            reason,
            error_type,
            rule,
        }
    }

    fn denial(denial: Denial) -> Self {
        Self::new(
            denial.status(),
            "Forbidden",
            denial.error_type(),
            denial.rule(),
        )
    }
}

/// 一次 `CONNECT` 的审计上下文。
struct Trace {
    src: SocketAddr,
    started: Instant,
    caller: Option<AuditCaller>,
    method: Option<String>,
    host: Option<String>,
    port: Option<u16>,
}

impl ConnectProxy {
    pub fn new(
        registry: Registry,
        audit: Arc<dyn AuditSink>,
        connector: Connector,
        limits: TunnelLimits,
        sandbox_cidrs: Option<Vec<Cidr>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            audit,
            connector,
            limits,
            sandbox_cidrs,
            jobs: Mutex::new(HashMap::new()),
        })
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        loop {
            let (stream, src) = listener.accept().await?;
            let proxy = self.clone();
            tokio::spawn(async move { proxy.handle(stream, src).await });
        }
    }

    async fn handle(self: Arc<Self>, mut client: TcpStream, src: SocketAddr) {
        let mut trace = Trace {
            src,
            started: Instant::now(),
            caller: None,
            method: None,
            host: None,
            port: None,
        };
        match self.clone().open(&mut client, &mut trace).await {
            Ok(tunnel) => self.run(client, tunnel, trace).await,
            Err(refusal) => {
                // 先写审计再回应：调用方收到拒绝时，审计记录已经落下。
                let mut event = self.event(&trace, Decision::Deny);
                event.rule = Some(refusal.rule);
                event.status = Some(refusal.status);
                self.write_audit(event);
                let _ = respond(&mut client, &refusal).await;
            }
        }
    }

    async fn open(
        self: Arc<Self>,
        client: &mut TcpStream,
        trace: &mut Trace,
    ) -> Result<Tunnel, Refusal> {
        let head = tokio::time::timeout(
            self.limits.head_timeout,
            read_head(client, self.limits.head_bytes),
        )
        .await
        .map_err(|_| Refusal::new(408, "Request Timeout", "request_timeout", "request.timeout"))?
        .map_err(|_| Refusal::new(400, "Bad Request", "invalid_request", "request.malformed"))?;
        let request = Head::parse(&head).ok_or_else(|| {
            Refusal::new(400, "Bad Request", "invalid_request", "request.malformed")
        })?;
        trace.method = Some(request.method.clone());
        if request.method != "CONNECT" {
            // 含绝对 URI 的明文代理请求同样走到这里。
            return Err(Refusal::new(
                405,
                "Method Not Allowed",
                "method_not_allowed",
                "method.not_allowed",
            ));
        }
        if let Some(cidrs) = &self.sandbox_cidrs {
            if !cidrs.iter().any(|cidr| cidr.contains(trace.src.ip())) {
                return Err(Refusal::denial(Denial::TokenInvalid {
                    rule: "token.source_not_allowed",
                }));
            }
        }
        let Some(credentials) = request.header("proxy-authorization") else {
            return Err(Refusal::new(
                407,
                "Proxy Authentication Required",
                "proxy_auth_required",
                "token.missing",
            ));
        };
        let token = basic_job_token(credentials).ok_or_else(|| {
            Refusal::denial(Denial::TokenInvalid {
                rule: "token.unknown",
            })
        })?;
        let caller = self
            .registry
            .authenticate(Listener::Sandbox, &token)
            .map_err(Refusal::denial)?;
        let Caller::Job { job_id } = caller.clone() else {
            return Err(Refusal::denial(Denial::TokenInvalid {
                rule: "token.wrong_listener",
            }));
        };
        trace.caller = Some(AuditCaller::Job {
            job_id,
            tier: self.registry.job_tier(&job_id).unwrap_or(Tier::Worker),
        });

        // authority 的原文（只到主机与端口）进审计，便于按 job_id 查被拒的目标。
        let (raw_host, raw_port) = split_authority(&request.target);
        trace.host = Some(raw_host);
        trace.port = raw_port;
        let host = parse_connect_authority(&request.target)
            .map_err(|denial| Refusal::new(403, "Forbidden", "hard_denied", denial.rule_id()))?;
        trace.host = Some(host.to_string());
        trace.port = Some(CONNECT_PORT);
        if !self.registry.host_allowed(&job_id, &host) {
            let _ = self
                .registry
                .note_denied(&caller, DeniedKind::NotAllowedHost);
            return Err(Refusal::new(
                403,
                "Forbidden",
                "not_allowed_host",
                "profile.not_in_groups",
            ));
        }
        let slot = self.claim(job_id)?;
        let (upstream, ip) = match self.connector.connect(&host, CONNECT_PORT).await {
            Ok(ok) => ok,
            Err(ConnectError::Denied) => {
                return Err(Refusal::new(
                    403,
                    "Forbidden",
                    "hard_denied",
                    "hard.private_ip",
                ))
            }
            Err(ConnectError::Timeout) => {
                return Err(Refusal::new(
                    504,
                    "Gateway Timeout",
                    "upstream_unavailable",
                    "upstream.timeout",
                ))
            }
            Err(_) => {
                return Err(Refusal::new(
                    502,
                    "Bad Gateway",
                    "upstream_unavailable",
                    "upstream.connect",
                ))
            }
        };
        Ok(Tunnel {
            job_id,
            host,
            upstream,
            ip,
            _slot: slot,
        })
    }

    async fn run(self: Arc<Self>, mut client: TcpStream, tunnel: Tunnel, trace: Trace) {
        let mut event = self.event(&trace, Decision::Allow);
        event.host = Some(tunnel.host.to_string());
        event.port = Some(CONNECT_PORT);
        event.resolved_ip = Some(tunnel.ip.to_string());
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            event.error = Some("client went away".into());
            self.write_audit(event);
            return;
        }
        event.status = Some(200);
        let Tunnel {
            job_id,
            upstream,
            _slot: slot,
            ..
        } = tunnel;
        let (outcome, client_r, mut client_w) = self.pump(client, upstream, job_id).await;
        // 先释放连接名额、写审计，再关闭到调用方的连接：调用方看到隧道结束时，
        // 这两件事已经确定发生。
        drop(slot);
        event.bytes_in = outcome.up;
        event.bytes_out = outcome.down;
        event.duration_ms = trace.started.elapsed().as_millis() as u64;
        event.error = outcome.ended_by.map(str::to_owned);
        self.write_audit(event);
        let _ = client_w.shutdown().await;
        drop(client_r);
    }

    /// 双向拼接，结束时关闭上游；返回计数与到调用方的两半连接（由调用方收尾后关闭）。
    async fn pump(
        &self,
        client: TcpStream,
        upstream: TcpStream,
        job_id: AttemptId,
    ) -> (
        Pumped,
        tokio::net::tcp::OwnedReadHalf,
        tokio::net::tcp::OwnedWriteHalf,
    ) {
        let (mut client_r, mut client_w) = client.into_split();
        let (mut up_r, mut up_w) = upstream.into_split();
        let mut up_buf = vec![0u8; 16 * 1024];
        let mut down_buf = vec![0u8; 16 * 1024];
        let deadline = Instant::now() + self.limits.max_duration;
        let mut out = Pumped::default();
        loop {
            let idle = Instant::now() + self.limits.idle;
            let (n, upward) = tokio::select! {
                read = client_r.read(&mut up_buf) => (read, true),
                read = up_r.read(&mut down_buf) => (read, false),
                _ = tokio::time::sleep_until(idle) => { out.ended_by = Some("idle timeout"); break; }
                _ = tokio::time::sleep_until(deadline) => { out.ended_by = Some("max duration"); break; }
            };
            let n = match n {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !self.charge(job_id, n as u64, &mut out) {
                out.ended_by = Some("byte limit");
                break;
            }
            let written = if upward {
                out.up += n as u64;
                up_w.write_all(&up_buf[..n]).await
            } else {
                out.down += n as u64;
                client_w.write_all(&down_buf[..n]).await
            };
            if written.is_err() {
                break;
            }
        }
        let _ = up_w.shutdown().await;
        (out, client_r, client_w)
    }

    /// 记字节；超过单隧道或单 job 上限返回 `false`。
    fn charge(&self, job_id: AttemptId, n: u64, out: &mut Pumped) -> bool {
        if out.up + out.down + n > self.limits.tunnel_bytes {
            return false;
        }
        let mut jobs = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
        let job = jobs.entry(job_id).or_default();
        if job.bytes + n > self.limits.job_tunnel_bytes {
            return false;
        }
        job.bytes += n;
        true
    }

    fn claim(self: &Arc<Self>, job_id: AttemptId) -> Result<TunnelSlot, Refusal> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
        let job = jobs.entry(job_id).or_default();
        if job.open >= self.limits.per_token_connections {
            return Err(Refusal::new(
                429,
                "Too Many Requests",
                "rate_limited",
                "rate.connections",
            ));
        }
        job.open += 1;
        Ok(TunnelSlot {
            proxy: self.clone(),
            job_id,
        })
    }

    fn event(&self, trace: &Trace, decision: Decision) -> AuditEvent {
        let caller = trace.caller.clone().unwrap_or(AuditCaller::Unknown {
            src: trace.src.ip().to_string(),
        });
        let mut event = AuditEvent::new(AuditKind::Connect, caller, decision);
        event.method = trace.method.clone();
        event.host = trace.host.clone();
        event.port = trace.port;
        event.src = Some(trace.src.ip().to_string());
        event.duration_ms = trace.started.elapsed().as_millis() as u64;
        event
    }

    fn write_audit(&self, event: AuditEvent) {
        if self.audit.record(event).is_err() {
            self.registry.set_audit_failed(true);
        }
    }
}

#[derive(Default)]
struct Pumped {
    up: u64,
    down: u64,
    ended_by: Option<&'static str>,
}

struct Tunnel {
    job_id: AttemptId,
    host: Host,
    upstream: TcpStream,
    ip: std::net::IpAddr,
    _slot: TunnelSlot,
}

struct TunnelSlot {
    proxy: Arc<ConnectProxy>,
    job_id: AttemptId,
}

impl Drop for TunnelSlot {
    fn drop(&mut self) {
        let mut jobs = self.proxy.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(job) = jobs.get_mut(&self.job_id) {
            job.open = job.open.saturating_sub(1);
        }
    }
}

struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn parse(raw: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(raw).ok()?;
        let mut lines = text.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?.to_owned();
        let target = request_line.next()?.to_owned();
        let version = request_line.next()?;
        if !version.starts_with("HTTP/1.") || request_line.next().is_some() {
            return None;
        }
        let mut headers = Vec::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':')?;
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
        Some(Self {
            method,
            target,
            headers,
        })
    }

    /// 同名头只允许一个。
    fn header(&self, name: &str) -> Option<&str> {
        let mut values = self.headers.iter().filter(|(n, _)| n == name);
        let first = values.next()?;
        values.next().is_none().then_some(first.1.as_str())
    }
}

/// 读到 `\r\n\r\n` 为止（不多读，隧道里的字节留给拼接）。
async fn read_head(stream: &mut TcpStream, limit: usize) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }
    head.truncate(head.len() - 4);
    Ok(head)
}

async fn respond(client: &mut TcpStream, refusal: &Refusal) -> io::Result<()> {
    let body = format!(
        r#"{{"type":"error","error":{{"type":"{}","message":"CONNECT denied by egress"}}}}"#,
        refusal.error_type
    );
    let challenge = if refusal.status == 407 {
        "Proxy-Authenticate: Basic realm=\"gantry-egress\"\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\n{challenge}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        refusal.status,
        refusal.reason,
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;
    client.shutdown().await
}

/// 审计用：authority 去掉 userinfo 后的主机原文与端口（截断、去控制符由审计层负责）。
fn split_authority(target: &str) -> (String, Option<u16>) {
    let hostport = target.rsplit_once('@').map_or(target, |(_, rest)| rest);
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || hostport.starts_with('[') => {
            (host.to_owned(), port.parse().ok())
        }
        _ => (hostport.to_owned(), None),
    }
}

/// `Basic base64(job:<token>)`；其他形状返回 `None`。
fn basic_job_token(value: &str) -> Option<String> {
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64_decode(encoded.trim())?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, token) = text.split_once(':')?;
    (user == "job" && !token.is_empty()).then(|| token.to_owned())
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let bytes = input.trim_end_matches('=').as_bytes();
    if input.len() % 4 != 0 || input.len() - bytes.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= value(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_credentials_must_use_the_job_user() {
        // "job:gjt1_x" / "other:gjt1_x"
        assert_eq!(
            basic_job_token("Basic am9iOmdqdDFfeA=="),
            Some("gjt1_x".into())
        );
        assert_eq!(
            basic_job_token("basic am9iOmdqdDFfeA=="),
            Some("gjt1_x".into())
        );
        assert_eq!(basic_job_token("Basic b3RoZXI6Z2p0MV94"), None);
        assert_eq!(basic_job_token("Bearer am9iOmdqdDFfeA=="), None);
        assert_eq!(basic_job_token("Basic !!!"), None);
        assert_eq!(basic_job_token("Basic am9iOg=="), None);
    }

    #[test]
    fn heads_are_parsed_strictly() {
        let head = Head::parse(
            b"CONNECT registry.npmjs.org:443 HTTP/1.1\r\nHost: x\r\nProxy-Authorization: Basic abc",
        )
        .unwrap();
        assert_eq!(head.method, "CONNECT");
        assert_eq!(head.target, "registry.npmjs.org:443");
        assert_eq!(head.header("proxy-authorization"), Some("Basic abc"));
        assert!(Head::parse(b"CONNECT a:443 HTTP/1.1 extra").is_none());
        let dup = Head::parse(
            b"CONNECT a:443 HTTP/1.1\r\nProxy-Authorization: a\r\nproxy-authorization: b",
        )
        .unwrap();
        assert_eq!(dup.header("proxy-authorization"), None);
    }
}
