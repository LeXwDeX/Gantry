//! 网络面（本文 §6.1 `serve`）：mTLS 监听器把握手通过的连接接到请求管线。
//!
//! 每连接一个任务：TLS 握手（失败不产生任何 HTTP 字节）→ 取客户端叶子证书的序列号与
//! 有效期 → hyper（`h2` / `http/1.1` 按 ALPN 自动）→ [`ApiCore::admit`] →
//! 按路由上限读请求体（至多 `limit + 1` 字节，超限即停读）→ [`ApiCore::complete`]。
//! 事件流响应按帧流式写出。

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use gantry_core::time::Timestamp;
use gantry_pki::CertSerial;
use http_body_util::BodyExt;
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use x509_parser::prelude::FromDer;

use crate::auth::PeerCert;
use crate::server::{Admission, ApiCore, ApiResponse, Body, RequestHead};
use crate::sse::EventStream;

/// 写给 hyper 的响应体。
pub enum WireBody {
    Full(Option<Bytes>),
    Events(EventStream),
}

impl hyper::body::Body for WireBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            WireBody::Full(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            WireBody::Events(stream) => stream
                .poll_frame(cx)
                .map(|frame| frame.map(|b| Ok(Frame::data(b)))),
        }
    }
}

pub(crate) fn to_wire(resp: ApiResponse) -> hyper::Response<WireBody> {
    let (parts, body) = resp.into_parts();
    let body = match body {
        Body::Full(bytes) if bytes.is_empty() => WireBody::Full(None),
        Body::Full(bytes) => WireBody::Full(Some(bytes)),
        Body::Events(stream) => WireBody::Events(stream),
    };
    hyper::Response::from_parts(parts, body)
}

/// 握手出示的叶子证书 → 管线需要的字段。解析失败按"不在册"处理（失败即关闭）。
pub fn peer_cert(leaf_der: &[u8]) -> Option<PeerCert> {
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(leaf_der).ok()?;
    let serial = CertSerial::from_be_bytes(cert.raw_serial())?;
    let ms = |t: x509_parser::time::ASN1Time| Timestamp::from_millis(t.timestamp() * 1000);
    Some(PeerCert {
        serial: serial.as_str().to_owned(),
        not_before: ms(cert.validity().not_before),
        not_after: ms(cert.validity().not_after),
    })
}

/// 读请求体，至多 `limit + 1` 字节；超出即停读（`complete` 据此返回 413）。
async fn read_body(mut body: Incoming, limit: usize) -> Bytes {
    let mut buf = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { break };
        if let Some(data) = frame.data_ref() {
            let room = limit + 1 - buf.len();
            buf.extend_from_slice(&data[..data.len().min(room)]);
            if buf.len() > limit {
                break;
            }
        }
    }
    buf.freeze()
}

async fn handle(
    core: Arc<ApiCore>,
    peer: Option<PeerCert>,
    req: hyper::Request<Incoming>,
) -> Result<hyper::Response<WireBody>, Infallible> {
    let (parts, body) = req.into_parts();
    let head = RequestHead {
        method: parts.method,
        uri: parts.uri,
        headers: parts.headers,
        peer,
    };
    let resp = match core.admit(head).await {
        Admission::Respond(resp) => resp,
        Admission::Proceed(admitted) => {
            let bytes = read_body(body, admitted.body_limit()).await;
            core.complete(*admitted, bytes).await
        }
    };
    Ok(to_wire(resp))
}

/// mTLS API 监听器。`shutdown` 置位后停止接受新连接，并让所有事件流收到
/// `close(server_shutdown)`。
pub async fn serve_api(
    core: Arc<ApiCore>,
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from(tls);
    loop {
        let (tcp, _addr) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = shutdown.changed() => {
                core.shutdown();
                return Ok(());
            }
        };
        let acceptor = acceptor.clone();
        let core = core.clone();
        tokio::spawn(async move {
            // 握手失败：连接直接关闭，不写任何 HTTP 字节（api.md §1.2）
            let Ok(stream) = acceptor.accept(tcp).await else {
                return;
            };
            let peer = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .and_then(|leaf| peer_cert(leaf));
            let service = service_fn(move |req| handle(core.clone(), peer.clone(), req));
            let _ = Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// 仅测试：明文 HTTP 监听器，每个连接都视为出示了 `peer` 证书（本文 §6.1
/// `router_for_test` 的网络版）。用于让同一套 HTTP 行为用例同时跑 Mock API 与真实
/// `api`；生产装配不得使用。
#[cfg(feature = "testing")]
pub async fn serve_plain_for_tests(
    core: Arc<ApiCore>,
    listener: TcpListener,
    peer: PeerCert,
) -> std::io::Result<()> {
    loop {
        let (tcp, _addr) = listener.accept().await?;
        let core = core.clone();
        let peer = peer.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| handle(core.clone(), Some(peer.clone()), req));
            let _ = Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(tcp), service)
                .await;
        });
    }
}
