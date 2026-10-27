//! HTTPS 客户端与配对传输（desktop-shell.md §12"配对"、"mTLS 与固定 CA"，§7.1 重试 / 并发 / 超时）。
//!
//! 本进程内起 hyper + tokio-rustls 服务端，证书由 TestCA 与 gantry-pki 签发。
#![cfg(feature = "test-software-keys")]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gantry_core::ids::DeviceId;
use gantry_core::testing::SeqEntropy;
use gantry_core::time::Timestamp;
use gantry_desktop_shell::http::{HttpClient, HttpPairTransport, NetError, Origin};
use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::keys::software::{PresenceDouble, TestSoftwareKeyStore};
use gantry_desktop_shell::pairing;
use gantry_desktop_shell::routes::prepare;
use gantry_desktop_shell::tls::{chain_from_pem, client_config, ClientIdentity, RootFingerprint};
use gantry_fakes::test_ca::TestCA;
use gantry_pki::{Authority, CertPolicy, CertSerial};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

fn now() -> Timestamp {
    Timestamp::from_millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    )
}

fn der(pem_text: &str) -> CertificateDer<'static> {
    CertificateDer::from(pem::parse(pem_text).unwrap().into_contents())
}

struct Pki {
    ca: TestCA,
    inter: Authority,
}

impl Pki {
    fn new() -> Self {
        let ca = TestCA::new().unwrap();
        let read = |n: &str| std::fs::read_to_string(ca.root_dir().join(n)).unwrap();
        let inter = Authority::load(&read("inter.crt"), &read("inter.key")).unwrap();
        Self { ca, inter }
    }

    fn fp(&self) -> String {
        self.ca.root_fingerprint().to_string()
    }

    fn chain_tail(&self) -> [String; 2] {
        [
            self.ca.intermediate_cert_pem().to_string(),
            self.ca.root_cert_pem().to_string(),
        ]
    }

    fn server_config(&self, mtls: bool) -> Arc<ServerConfig> {
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

    fn issue_device(&self, public: &gantry_pki::P256PublicKey) -> String {
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
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

type Handler = Arc<dyn Fn(Seen) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync>;

/// 起一个 HTTPS 服务端，返回地址。`delay` 让每个请求先睡一会儿（测并发与超时）。
async fn serve(
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

async fn serve_simple(config: Arc<ServerConfig>, handler: Handler) -> SocketAddr {
    serve(
        config,
        handler,
        Duration::ZERO,
        Arc::default(),
        Arc::default(),
    )
    .await
}

fn origin(addr: SocketAddr) -> Origin {
    Origin {
        host: "localhost".into(),
        port: addr.port(),
    }
}

#[tokio::test]
async fn pairing_over_real_tls_then_mtls_me() {
    let pki = Arc::new(Pki::new());
    let token = "gp1_4Ww9rX0b2sQ7hYk1";
    let handler: Handler = {
        let pki = pki.clone();
        Arc::new(move |seen: Seen| {
            assert_eq!(
                (seen.method.as_str(), seen.path.as_str()),
                ("POST", "/v1/pair")
            );
            let body: Value = serde_json::from_slice(&seen.body).unwrap();
            if body["token"] != token {
                return (
                    401,
                    vec![],
                    br#"{"code":"pair_token_invalid","title":"x","request_id":"r"}"#.to_vec(),
                );
            }
            let tls =
                gantry_pki::csr::validate_csr(body["csr"].as_str().unwrap(), 16 * 1024).unwrap();
            let approval =
                gantry_pki::csr::parse_approval_pubkey(body["approval_pubkey"].as_str().unwrap())
                    .unwrap();
            let [inter, root] = pki.chain_tail();
            let resp = json!({
                "device": {
                    "id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM", "name": body["device"]["name"], "platform": "macos",
                    "pubkey_fpr": tls.fingerprint().to_string(),
                    "approval_pubkey_fpr": approval.fingerprint().to_string(),
                    "role": "approver", "status": "active"
                },
                "certificate_chain": [pki.issue_device(&tls), inter, root],
                "api_base_url": "https://localhost:1",
                "ca_fingerprint": pki.fp(),
                "confirmation_code": "482913",
                "unknown_future_field": true
            });
            (
                201,
                vec![("content-type".into(), "application/json".into())],
                serde_json::to_vec(&resp).unwrap(),
            )
        })
    };
    let pair_addr = serve_simple(pki.server_config(false), handler).await;
    let pairing_string = format!(
        "gantry+pair://localhost:{}/?v=1&fp={}&t={token}",
        pair_addr.port(),
        pki.fp()
    );

    let store = TestSoftwareKeyStore::new(PresenceDouble::new());
    let pending = pairing::begin(
        &pairing_string,
        "mac",
        &store,
        &HttpPairTransport::default(),
        now().as_millis() / 1000,
    )
    .await
    .unwrap();
    assert_eq!(pending.confirmation_code(), "482913");
    let keys = pending.keys().clone();
    let identity = pending.into_identity("2026-10-27T00:00:00.000Z".into());

    // 用新证书走 mTLS：服务端要求并验证客户端证书。
    let me: Handler = Arc::new(|seen: Seen| {
        assert_eq!(seen.path, "/v1/me");
        (
            200,
            vec![
                ("x-log-epoch".into(), "7".into()),
                ("content-type".into(), "application/json".into()),
            ],
            br#"{"server":{"log_epoch":7}}"#.to_vec(),
        )
    });
    let api_addr = serve_simple(pki.server_config(true), me).await;
    let client = HttpClient::new(
        origin(api_addr),
        client_config(
            RootFingerprint::parse(&identity.ca_fingerprint).unwrap(),
            Some(ClientIdentity {
                chain: chain_from_pem(&identity.cert_chain_pem).unwrap(),
                keys,
            }),
        ),
    );
    let resp = client.send("GET", "/v1/me", None, &[]).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.log_epoch(), Some(7));

    // 没有客户端证书 → TLS 层失败。
    let anon = HttpClient::new(
        origin(api_addr),
        client_config(RootFingerprint::parse(&pki.fp()).unwrap(), None),
    );
    assert!(matches!(
        anon.send("GET", "/v1/me", None, &[]).await,
        Err(NetError::Tls | NetError::Aborted)
    ));
}

#[tokio::test]
async fn pairing_against_wrong_ca_or_bad_token_deletes_keys() {
    let pki = Pki::new();
    let other = Pki::new();
    let reject: Handler = Arc::new(|_| (401, vec![], br#"{"code":"pair_token_invalid"}"#.to_vec()));
    let addr = serve_simple(other.server_config(false), reject.clone()).await;
    let store = TestSoftwareKeyStore::new(PresenceDouble::new());
    let s = format!(
        "gantry+pair://localhost:{}/?v=1&fp={}&t=gp1_x",
        addr.port(),
        pki.fp()
    );
    let err = pairing::begin(&s, "mac", &store, &HttpPairTransport::default(), 0)
        .await
        .err()
        .unwrap();
    assert_eq!(
        (err.code, err.reason.as_deref()),
        (
            ShellErrorCode::PairingFailed,
            Some("ca_fingerprint_mismatch")
        )
    );
    assert_eq!(store.key_count(), 0);

    let addr = serve_simple(pki.server_config(false), reject).await;
    let s = format!(
        "gantry+pair://localhost:{}/?v=1&fp={}&t=gp1_x",
        addr.port(),
        pki.fp()
    );
    let err = pairing::begin(&s, "mac", &store, &HttpPairTransport::default(), 0)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ShellErrorCode::PairTokenInvalid);
    assert_eq!(store.key_count(), 0);
}

fn counting(statuses: Vec<(u16, Option<&'static str>)>, hits: Arc<AtomicUsize>) -> Handler {
    Arc::new(move |_| {
        let i = hits.fetch_add(1, Ordering::SeqCst).min(statuses.len() - 1);
        let (status, retry_after) = statuses[i];
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if let Some(r) = retry_after {
            headers.push(("retry-after".into(), r.into()));
        }
        (status, headers, b"{}".to_vec())
    })
}

async fn client_for(pki: &Pki, handler: Handler) -> HttpClient {
    let addr = serve_simple(pki.server_config(false), handler).await;
    HttpClient::new(
        origin(addr),
        client_config(RootFingerprint::parse(&pki.fp()).unwrap(), None),
    )
    .with_timeouts(
        Duration::from_secs(2),
        Duration::from_secs(2),
        Duration::from_millis(5),
    )
}

#[tokio::test]
async fn retry_rules() {
    let pki = Pki::new();
    const CNV: &str = "cnv_01JB7K2M9Q4X8V3N5R6T1W0YZA";
    let read = prepare("me.get", "/v1/me", None, None, None).unwrap();
    let write_keyed = prepare(
        "messages.post",
        &format!("/v1/conversations/{CNV}/messages"),
        None,
        Some(&json!({"text": "x"})),
        Some("idem_0123456789abcdef"),
    )
    .unwrap();
    let write_unkeyed = prepare(
        "devices.rename",
        "/v1/devices/dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
        None,
        Some(&json!({"name": "x"})),
        None,
    )
    .unwrap();

    // 503 + Retry-After 后成功：重试一次。
    let hits = Arc::new(AtomicUsize::new(0));
    let c = client_for(
        &pki,
        counting(vec![(503, Some("0")), (200, None)], hits.clone()),
    )
    .await;
    assert_eq!(c.send_prepared(&read).await.unwrap().status, 200);
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // 读路由 500：重试到上限 3 次。
    let hits = Arc::new(AtomicUsize::new(0));
    let c = client_for(&pki, counting(vec![(500, None)], hits.clone())).await;
    assert_eq!(c.send_prepared(&read).await.unwrap().status, 500);
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    // 带幂等键的写路由 500：重试，且每次带同一个键。
    let hits = Arc::new(AtomicUsize::new(0));
    let keys_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handler: Handler = {
        let (hits, keys_seen) = (hits.clone(), keys_seen.clone());
        Arc::new(move |seen: Seen| {
            hits.fetch_add(1, Ordering::SeqCst);
            keys_seen.lock().unwrap().push(
                seen.headers
                    .iter()
                    .find(|(k, _)| k == "idempotency-key")
                    .map(|(_, v)| v.clone()),
            );
            (502, vec![], vec![])
        })
    };
    let c = client_for(&pki, handler).await;
    c.send_prepared(&write_keyed).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    assert!(keys_seen
        .lock()
        .unwrap()
        .iter()
        .all(|k| k.as_deref() == Some("idem_0123456789abcdef")));

    // 不带幂等键的写路由 500：不重试。其余 4xx 也不重试。
    for (status, req) in [(500, &write_unkeyed), (404, &read), (409, &write_keyed)] {
        let hits = Arc::new(AtomicUsize::new(0));
        let c = client_for(&pki, counting(vec![(status, None)], hits.clone())).await;
        assert_eq!(c.send_prepared(req).await.unwrap().status, status);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "{status}");
    }
}

#[tokio::test]
async fn network_failures_are_classified() {
    let pki = Pki::new();
    let fp = RootFingerprint::parse(&pki.fp()).unwrap();
    // 关闭的端口 → offline。
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let c = HttpClient::new(origin(closed), client_config(fp, None));
    assert_eq!(
        c.send("GET", "/v1/me", None, &[]).await.unwrap_err(),
        NetError::Offline
    );

    // 证书不受信 → tls。
    let other = Pki::new();
    let addr = serve_simple(
        other.server_config(false),
        counting(vec![(200, None)], Arc::default()),
    )
    .await;
    let c = HttpClient::new(origin(addr), client_config(fp, None));
    assert_eq!(
        c.send("GET", "/v1/me", None, &[]).await.unwrap_err(),
        NetError::Tls
    );

    // 服务端迟迟不回 → timeout。
    let addr = serve(
        pki.server_config(false),
        counting(vec![(200, None)], Arc::default()),
        Duration::from_secs(5),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let c = HttpClient::new(origin(addr), client_config(fp, None)).with_timeouts(
        Duration::from_secs(2),
        Duration::from_millis(300),
        Duration::ZERO,
    );
    assert_eq!(
        c.send("GET", "/v1/me", None, &[]).await.unwrap_err(),
        NetError::Timeout
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_eight_requests_in_flight() {
    let pki = Pki::new();
    let (live, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let addr = serve(
        pki.server_config(false),
        counting(vec![(200, None)], Arc::default()),
        Duration::from_millis(150),
        live,
        peak.clone(),
    )
    .await;
    let client = Arc::new(HttpClient::new(
        origin(addr),
        client_config(RootFingerprint::parse(&pki.fp()).unwrap(), None),
    ));
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let c = client.clone();
            tokio::spawn(async move { c.send("GET", "/v1/me", None, &[]).await.unwrap().status })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let p = peak.load(Ordering::SeqCst);
    assert!((2..=8).contains(&p), "峰值并发 {p}");
}

#[test]
fn origin_parsing() {
    assert_eq!(
        Origin::parse("https://gantry.example.com:8443"),
        Some(Origin {
            host: "gantry.example.com".into(),
            port: 8443
        })
    );
    assert_eq!(
        Origin::parse("https://gantry.example.com/"),
        Some(Origin {
            host: "gantry.example.com".into(),
            port: 443
        })
    );
    for bad in [
        "http://x:1",
        "https://u:p@x:1",
        "https://x:1/path",
        "https://x:1/?q",
        "not a url",
    ] {
        assert_eq!(Origin::parse(bad), None, "{bad}");
    }
}
