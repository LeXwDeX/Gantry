//! 配对监听器（api.md §1.3，本文 §1 职责 2、§7.2）：独立端口、单向 TLS、只路由
//! `POST /v1/pair`，其余路径一律 `404`；只在 `pki` 给出的配对窗口内接受连接；业务委托
//! `pki::PairingService::redeem`。这是暴露在未认证面上的最小面：按源 IP 限流
//! （IPv6 按 /64 聚合）、请求体 ≤ 16 KiB、令牌无效 / 已用 / 过期 / 作废的 `401`
//! 逐字节相同。

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use gantry_core::entities::DevicePlatform;
use gantry_core::ids::{Clock, IdGen};
use gantry_pki::pairing_service::{NewDevice, PairError, PairRequest, PairingService};
use gantry_store::Store;
use http::header::{HeaderValue, CONTENT_TYPE};
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use crate::consts::PAIR_MAX_BODY;
use crate::devices::field_errors;
use crate::problem::{Problem, ProblemCode};
use crate::rate::RateLimiter;
use crate::registry::RateClass;
use crate::server::{problem_response, ApiResponse, Body};

pub struct PairingListener {
    pub pairing: Arc<PairingService>,
    pub store: Store,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<IdGen>,
    limiter: RateLimiter,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDevice {
    name: String,
    platform: DevicePlatform,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePair {
    token: String,
    csr: String,
    #[serde(default)]
    approval_pubkey: Option<String>,
    device: WireDevice,
}

/// 限流主体：IPv4 按地址，IPv6 按 /64（本文 §7.11 [建议]）。
fn rate_subject(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

impl PairingListener {
    pub fn new(
        pairing: Arc<PairingService>,
        store: Store,
        clock: Arc<dyn Clock>,
        ids: Arc<IdGen>,
    ) -> Self {
        Self {
            limiter: RateLimiter::new(clock.clone()),
            pairing,
            store,
            clock,
            ids,
        }
    }

    /// 处理一个配对端口上的请求（不含 TLS）。`body` 已按 [`PAIR_MAX_BODY`] + 1 截断。
    pub async fn handle(
        &self,
        peer: IpAddr,
        method: &Method,
        path: &str,
        content_type: Option<&str>,
        body: Bytes,
    ) -> ApiResponse {
        let request_id = self
            .ids
            .next_ulid()
            .map(|id| id.to_string())
            .unwrap_or_else(|_| "00000000000000000000000000".to_owned());
        let epoch = self.store.log_identity().log_epoch;
        let fail =
            |problem: Problem, rate| problem_response(&problem, path, &request_id, epoch, rate);
        if path != "/v1/pair" {
            return fail(Problem::not_found(), None);
        }
        if method != Method::POST {
            return fail(
                Problem::new(
                    ProblemCode::MethodNotAllowed,
                    "method is not allowed for this path",
                )
                .with_header(http::header::ALLOW, HeaderValue::from_static("POST")),
                None,
            );
        }
        let rate = self.limiter.take(&rate_subject(peer), RateClass::PairIp);
        if let Some(secs) = rate.retry_after {
            return fail(
                Problem::retry_after(ProblemCode::RateLimited, "rate limit exceeded", secs),
                Some(rate),
            );
        }
        let json = content_type
            .and_then(|v| v.split(';').next())
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
        if !json {
            return fail(
                Problem::new(
                    ProblemCode::UnsupportedMediaType,
                    "body must be application/json",
                ),
                Some(rate),
            );
        }
        if body.len() > PAIR_MAX_BODY {
            return fail(
                Problem::new(
                    ProblemCode::PayloadTooLarge,
                    "request body exceeds the limit",
                ),
                Some(rate),
            );
        }
        let Ok(raw) = serde_json::from_slice::<Value>(&body) else {
            return fail(
                Problem::new(
                    ProblemCode::InvalidRequest,
                    "request body is not valid JSON",
                ),
                Some(rate),
            );
        };
        if let Err(problem) = crate::validate::body("PairRequest", &raw) {
            return fail(problem, Some(rate));
        }
        let Ok(wire) = serde_json::from_value::<WirePair>(raw) else {
            return fail(Problem::internal(), Some(rate));
        };
        let result = self
            .pairing
            .redeem(PairRequest {
                token: wire.token,
                csr_pem: wire.csr,
                approval_pubkey: wire.approval_pubkey,
                device: NewDevice {
                    name: wire.device.name,
                    platform: wire.device.platform,
                },
            })
            .await;
        match result {
            Ok(paired) => {
                let mut device = serde_json::to_value(&paired.device).unwrap_or(Value::Null);
                if let Value::Object(map) = &mut device {
                    map.remove("approval_pubkey");
                }
                let body = json!({
                    "device": device,
                    "certificate_chain": paired.certificate_chain,
                    "api_base_url": paired.api_base_url,
                    "ca_fingerprint": paired.ca_fingerprint,
                    "confirmation_code": paired.confirmation_code,
                });
                let mut resp = ApiResponse::new(Body::Full(Bytes::from(body.to_string())));
                *resp.status_mut() = StatusCode::CREATED;
                let headers = resp.headers_mut();
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/json; charset=utf-8"),
                );
                headers.insert(
                    crate::server::X_REQUEST_ID,
                    HeaderValue::from_str(&request_id).expect("ulid"),
                );
                headers.insert(crate::server::X_LOG_EPOCH, HeaderValue::from(epoch));
                headers.insert(
                    http::header::CACHE_CONTROL,
                    HeaderValue::from_static("no-store"),
                );
                resp
            }
            // 无效 / 已用 / 过期 / 作废对外完全相同（原因只进日志）
            Err(PairError::TokenInvalid(_)) => fail(
                Problem::new(ProblemCode::PairTokenInvalid, "pairing token is invalid"),
                Some(rate),
            ),
            Err(PairError::Validation(errors)) => fail(field_errors(errors), Some(rate)),
            Err(PairError::Internal(_)) => fail(Problem::internal(), Some(rate)),
        }
    }

    /// 配对监听器：只在配对窗口内接受连接；窗口外的连接立即关闭。
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        tls: Arc<rustls::ServerConfig>,
        mut shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let acceptor = TlsAcceptor::from(tls);
        let window = self.pairing.window();
        loop {
            let (tcp, addr): (_, SocketAddr) = tokio::select! {
                accepted = listener.accept() => accepted?,
                _ = shutdown.changed() => return Ok(()),
            };
            if window.borrow().is_none() {
                drop(tcp);
                continue;
            }
            let acceptor = acceptor.clone();
            let me = self.clone();
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |req: hyper::Request<Incoming>| {
                    let me = me.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let mut buf = Vec::new();
                        let mut body = body;
                        while let Some(Ok(frame)) = body.frame().await {
                            if let Some(data) = frame.data_ref() {
                                let room = PAIR_MAX_BODY + 1 - buf.len();
                                buf.extend_from_slice(&data[..data.len().min(room)]);
                                if buf.len() > PAIR_MAX_BODY {
                                    break;
                                }
                            }
                        }
                        let content_type = parts
                            .headers
                            .get(CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok());
                        let resp = me
                            .handle(
                                addr.ip(),
                                &parts.method,
                                parts.uri.path(),
                                content_type,
                                Bytes::from(buf),
                            )
                            .await;
                        Ok::<_, Infallible>(crate::listener::to_wire(resp))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    }
}
