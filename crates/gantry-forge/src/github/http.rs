//! 最小 HTTP/1.1 客户端：只支持明文 `http://`，供回环替身使用。
//!
//! shortcut: 接真实 `https://api.github.com` 需要 TLS 依赖（issue #92 评论"出站 HTTPS"），
//! 在 M1.4 的 B / C 级取证前补；`https://` 地址在这里显式返回 `Unsupported`，不静默降级。

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    #[error("unsupported url scheme: {0}")]
    Unsupported(String),
    #[error("timeout")]
    Timeout,
    #[error("io: {0}")]
    Io(String),
    #[error("malformed response")]
    Malformed,
}

pub(crate) struct HttpRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: Vec<(&'a str, String)>,
    pub body: Option<Vec<u8>>,
}

pub(crate) async fn send(
    base: &str,
    request: HttpRequest<'_>,
    timeout: Duration,
) -> Result<HttpResponse, TransportError> {
    match tokio::time::timeout(timeout, send_inner(base, request)).await {
        Ok(result) => result,
        Err(_) => Err(TransportError::Timeout),
    }
}

async fn send_inner(base: &str, request: HttpRequest<'_>) -> Result<HttpResponse, TransportError> {
    let authority = base
        .strip_prefix("http://")
        .ok_or_else(|| TransportError::Unsupported(scheme_of(base)))?
        .trim_end_matches('/');
    let mut stream = TcpStream::connect(authority)
        .await
        .map_err(|error| TransportError::Io(error.kind().to_string()))?;
    let body = request.body.unwrap_or_default();
    let mut head = format!(
        "{} {} HTTP/1.1\r\nhost: {authority}\r\nconnection: close\r\ncontent-length: {}\r\n",
        request.method,
        request.path,
        body.len()
    );
    for (name, value) in &request.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let io = |error: std::io::Error| TransportError::Io(error.kind().to_string());
    stream.write_all(head.as_bytes()).await.map_err(io)?;
    stream.write_all(&body).await.map_err(io)?;
    stream.flush().await.map_err(io)?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await.map_err(io)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(TransportError::Malformed);
        }
    }
    parse_response(&raw)
}

fn scheme_of(base: &str) -> String {
    base.split_once("://")
        .map(|(scheme, _)| scheme.to_owned())
        .unwrap_or_default()
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, TransportError> {
    // 连接被对端直接关闭（没有任何响应字节）视为结果未知的传输错误。
    if raw.is_empty() {
        return Err(TransportError::Io(
            "connection closed without response".into(),
        ));
    }
    let head_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(TransportError::Malformed)?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| TransportError::Malformed)?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(TransportError::Malformed)?;
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let mut body = raw[head_end + 4..].to_vec();
    if let Some(length) = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        if body.len() < length {
            return Err(TransportError::Malformed);
        }
        body.truncate(length);
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}
