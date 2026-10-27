//! 测试共用：TestCA + gantry-pki 签发的证书，与进程内 hyper + tokio-rustls HTTPS 服务端。
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gantry_core::ids::DeviceId;
use gantry_core::testing::SeqEntropy;
use gantry_core::time::Timestamp;
use gantry_desktop_shell::http::Origin;
use gantry_fakes::test_ca::TestCA;
use gantry_pki::{Authority, CertPolicy, CertSerial};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

pub fn now() -> Timestamp {
    Timestamp::from_millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    )
}

pub fn der(pem_text: &str) -> CertificateDer<'static> {
    CertificateDer::from(pem::parse(pem_text).unwrap().into_contents())
}

pub struct Pki {
    pub ca: TestCA,
    pub inter: Authority,
}

impl Pki {
    pub fn new() -> Self {
        let ca = TestCA::new().unwrap();
        let read = |n: &str| std::fs::read_to_string(ca.root_dir().join(n)).unwrap();
        let inter = Authority::load(&read("inter.crt"), &read("inter.key")).unwrap();
        Self { ca, inter }
    }

    pub fn fp(&self) -> String {
        self.ca.root_fingerprint().to_string()
    }

    pub fn chain_tail(&self) -> [String; 2] {
        [
            self.ca.intermediate_cert_pem().to_string(),
            self.ca.root_cert_pem().to_string(),
        ]
    }

    pub fn server_config(&self, mtls: bool) -> Arc<ServerConfig> {
        let (leaf, key) = self
            .inter
            .issue_server_leaf(
                &["localhost".to_string()],
                now(),
                &CertPolicy::default(),
                &SeqEntropy::new(3),
            )
            .unwrap();
        let chain = vec![
            der(&leaf.pem),
            der(self.ca.intermediate_cert_pem()),
            der(self.ca.root_cert_pem()),
        ];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            pem::parse(key.as_str()).unwrap().into_contents(),
        ));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap();
        let builder = if mtls {
            let mut roots = RootCertStore::empty();
            roots.add(der(self.ca.root_cert_pem())).unwrap();
            builder.with_client_cert_verifier(
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                    .build()
                    .unwrap(),
            )
        } else {
            builder.with_no_client_auth()
        };
        Arc::new(builder.with_single_cert(chain, key).unwrap())
    }

    pub fn issue_device(&self, public: &gantry_pki::P256PublicKey) -> String {
        let id: DeviceId = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().unwrap();
        self.inter
            .issue_device_leaf(
                &id,
                public,
                &CertSerial::generate(&SeqEntropy::new(9)),
                now(),
                &CertPolicy::default(),
            )
            .unwrap()
            .pem
    }
}

#[derive(Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub type Handler = Arc<dyn Fn(Seen) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync>;

/// 起一个 HTTPS 服务端，返回地址。`delay` 让每个请求先睡一会儿（测并发与超时）。
pub async fn serve(
    config: Arc<ServerConfig>,
    handler: Handler,
    delay: Duration,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(config);
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let handler = handler.clone();
            let (live, peak) = (live.clone(), peak.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let handler = handler.clone();
                    let (live, peak) = (live.clone(), peak.clone());
                    async move {
                        let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(n, Ordering::SeqCst);
                        tokio::time::sleep(delay).await;
                        let seen = Seen {
                            method: req.method().to_string(),
                            path: req.uri().to_string(),
                            headers: req
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (k.to_string(), v.to_str().unwrap_or_default().to_string())
                                })
                                .collect(),
                            body: req.into_body().collect().await.unwrap().to_bytes().to_vec(),
                        };
                        let (status, headers, body) = handler(seen);
                        live.fetch_sub(1, Ordering::SeqCst);
                        let mut resp = Response::builder().status(status);
                        for (k, v) in headers {
                            resp = resp.header(k, v);
                        }
                        Ok::<_, Infallible>(resp.body(Full::new(Bytes::from(body))).unwrap())
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    addr
}

pub async fn serve_simple(config: Arc<ServerConfig>, handler: Handler) -> SocketAddr {
    serve(
        config,
        handler,
        Duration::ZERO,
        Arc::default(),
        Arc::default(),
    )
    .await
}

pub fn origin(addr: SocketAddr) -> Origin {
    Origin {
        host: "localhost".into(),
        port: addr.port(),
    }
}

/// 通用替身服务端的回复：普通响应或流式 SSE（写完后保持连接）。
pub enum Reply {
    Full(u16, Vec<(String, String)>, Vec<u8>),
    Stream(Vec<String>),
}

pub type AnyHandler = Arc<dyn Fn(Seen) -> Reply + Send + Sync>;

/// 起一个既能回普通 JSON、也能推 SSE 的 HTTPS 服务端。
pub async fn serve_any(config: Arc<ServerConfig>, handler: AnyHandler) -> SocketAddr {
    use http_body_util::{combinators::BoxBody, StreamBody};
    use hyper::body::Frame;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(config);
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, handler) = (acceptor.clone(), handler.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let handler = handler.clone();
                    async move {
                        let seen = Seen {
                            method: req.method().to_string(),
                            path: req.uri().to_string(),
                            headers: req
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (k.to_string(), v.to_str().unwrap_or_default().to_string())
                                })
                                .collect(),
                            body: req.into_body().collect().await.unwrap().to_bytes().to_vec(),
                        };
                        let resp: Response<BoxBody<Bytes, Infallible>> = match handler(seen) {
                            Reply::Full(status, headers, body) => {
                                let mut b = Response::builder().status(status);
                                for (k, v) in headers {
                                    b = b.header(k, v);
                                }
                                b.body(BoxBody::new(Full::new(Bytes::from(body)))).unwrap()
                            }
                            Reply::Stream(chunks) => {
                                let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
                                tokio::spawn(async move {
                                    for c in chunks {
                                        if tx.send(Bytes::from(c)).await.is_err() {
                                            return;
                                        }
                                    }
                                    tokio::time::sleep(Duration::from_secs(30)).await;
                                });
                                let body = StreamBody::new(futures_util::stream::unfold(
                                    rx,
                                    |mut rx| async {
                                        rx.recv()
                                            .await
                                            .map(|b| (Ok::<_, Infallible>(Frame::data(b)), rx))
                                    },
                                ));
                                Response::builder()
                                    .status(200)
                                    .header("content-type", "text/event-stream")
                                    .header("x-log-epoch", "1")
                                    .body(BoxBody::new(body))
                                    .unwrap()
                            }
                        };
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    addr
}
