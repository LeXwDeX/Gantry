//! 只连回环地址的最小 HTTP/1.1 客户端。
//!
//! 直接调用只发往 Egress 的回环反向代理（P-50）；TLS 与上游转发由 Egress 负责。
//! 因此这里只支持 `http://` + 回环主机，拒绝任何其他目标：即使配置或令牌 URL 被
//! 篡改，本模块也不会把请求直接发往提供方。

use std::net::{IpAddr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 响应体上限；超出视为协议错误。
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("target is not a loopback http URL")]
    NotLoopback,
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("io failed: {0}")]
    Io(String),
    #[error("malformed response: {0}")]
    Malformed(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 解析后的回环目标。`Debug` 不打印路径（路径里有 `svc_token`）。
#[derive(Clone, PartialEq, Eq)]
pub struct LoopbackUrl {
    addr: SocketAddr,
    host: String,
    path: String,
}

impl std::fmt::Debug for LoopbackUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackUrl")
            .field("addr", &self.addr)
            .field("path", &"***")
            .finish()
    }
}

impl LoopbackUrl {
    /// 只接受 `http://127.0.0.1:<port>/…`、`http://[::1]:<port>/…` 或 `http://localhost:<port>/…`。
    pub fn parse(url: &str) -> Result<Self, HttpError> {
        let rest = url.strip_prefix("http://").ok_or(HttpError::NotLoopback)?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err(HttpError::NotLoopback);
        }
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, tail) = inner.split_once(']').ok_or(HttpError::NotLoopback)?;
            (host, tail.strip_prefix(':').ok_or(HttpError::NotLoopback)?)
        } else {
            authority.rsplit_once(':').ok_or(HttpError::NotLoopback)?
        };
        let port: u16 = port.parse().map_err(|_| HttpError::NotLoopback)?;
        let ip: IpAddr = match host {
            "localhost" => IpAddr::from([127, 0, 0, 1]),
            other => other.parse().map_err(|_| HttpError::NotLoopback)?,
        };
        if !ip.is_loopback() || port == 0 {
            return Err(HttpError::NotLoopback);
        }
        if path.chars().any(|ch| ch.is_control() || ch == ' ') {
            return Err(HttpError::NotLoopback);
        }
        Ok(Self {
            addr: SocketAddr::new(ip, port),
            host: authority.to_owned(),
            path: path.to_owned(),
        })
    }

    /// 在路径后追加一段（如 `/v1/messages`）。
    pub fn join(&self, suffix: &str) -> Self {
        let mut path = self.path.trim_end_matches('/').to_owned();
        path.push_str(suffix);
        Self {
            addr: self.addr,
            host: self.host.clone(),
            path,
        }
    }
}

/// 发一个 `POST`，读到连接关闭为止。超时由调用方用 `tokio::time::timeout` 包住。
pub async fn post(
    url: &LoopbackUrl,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<HttpResponse, HttpError> {
    let mut stream = TcpStream::connect(url.addr)
        .await
        .map_err(|error| HttpError::Connect(error.kind().to_string()))?;
    let mut head = format!(
        "POST {} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        url.path,
        url.host,
        body.len()
    );
    for (name, value) in headers {
        if value.contains(['\r', '\n']) || name.contains(['\r', '\n', ':']) {
            return Err(HttpError::Malformed("header contains a line break"));
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let io = |error: std::io::Error| HttpError::Io(error.kind().to_string());
    stream.write_all(head.as_bytes()).await.map_err(io)?;
    stream.write_all(body).await.map_err(io)?;
    stream.flush().await.map_err(io)?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = stream.read(&mut buf).await.map_err(io)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(HttpError::Malformed("response too large"));
        }
        if response_complete(&raw) {
            break;
        }
    }
    parse_response(&raw)
}

fn split_head(raw: &[u8]) -> Option<(usize, usize)> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, index + 4))
}

/// 有 `content-length` 时读够即可返回，不必等对端关闭。
fn response_complete(raw: &[u8]) -> bool {
    let Some((head_end, body_start)) = split_head(raw) else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..head_end]);
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .is_some_and(|length| raw.len() >= body_start + length)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, HttpError> {
    let (head_end, body_start) = split_head(raw).ok_or(HttpError::Malformed("no header end"))?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| HttpError::Malformed("head"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(HttpError::Malformed("status line"))?;
    let mut parts = status_line.split(' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Malformed("version"));
    }
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or(HttpError::Malformed("status code"))?;
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(HttpError::Malformed("header"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let rest = &raw[body_start..];
    let response = HttpResponse {
        status,
        headers,
        body: Vec::new(),
    };
    let body = if response
        .header("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        dechunk(rest)?
    } else if let Some(length) = response.header("content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| HttpError::Malformed("content-length"))?;
        if rest.len() < length {
            return Err(HttpError::Malformed("body shorter than content-length"));
        }
        rest[..length].to_vec()
    } else {
        rest.to_vec()
    };
    Ok(HttpResponse { body, ..response })
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or(HttpError::Malformed("chunk size"))?;
        let size_text = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| HttpError::Malformed("chunk size"))?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size =
            usize::from_str_radix(size_text, 16).map_err(|_| HttpError::Malformed("chunk size"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err(HttpError::Malformed("chunk body"));
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_http_is_accepted() {
        for ok in [
            "http://127.0.0.1:8080/s/tok/anthropic",
            "http://localhost:1/x",
            "http://[::1]:9/x",
        ] {
            LoopbackUrl::parse(ok).unwrap();
        }
        for bad in [
            "https://127.0.0.1:8080/x",
            "http://api.anthropic.com:443/v1",
            "http://10.0.0.1:80/x",
            "http://127.0.0.1/x",
            "http://user@127.0.0.1:80/x",
            "http://127.0.0.1:0/x",
            "http://127.0.0.1:80/a b",
        ] {
            assert_eq!(
                LoopbackUrl::parse(bad),
                Err(HttpError::NotLoopback),
                "{bad}"
            );
        }
    }

    #[test]
    fn debug_hides_path() {
        let url = LoopbackUrl::parse("http://127.0.0.1:8080/s/gst1_secret/anthropic").unwrap();
        assert!(!format!("{url:?}").contains("gst1_secret"));
    }

    #[test]
    fn parses_content_length_and_chunked_bodies() {
        let fixed = parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhiEXTRA").unwrap();
        assert_eq!(fixed.status, 200);
        assert_eq!(fixed.body, b"hi");
        let chunked = parse_response(
            b"HTTP/1.1 429 Too Many\r\nTransfer-Encoding: chunked\r\nretry-after: 3\r\n\r\n2\r\nab\r\n1\r\nc\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(chunked.status, 429);
        assert_eq!(chunked.body, b"abc");
        assert_eq!(chunked.header("Retry-After"), Some("3"));
    }
}
