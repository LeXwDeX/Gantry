//! TLS 与监听器（本文 §12.2「TLS 与监听器」的 M1 条目）：真实握手、`ServerConfig`
//! 加固属性、握手失败零 HTTP 字节、吊销后握手被拒、事件流经真实连接。

mod support;

use std::sync::Arc;

use bytes::Bytes;
use gantry_api::tls::api_server_config;
use gantry_core::entities::{Actor, ActorKind, DevicePlatform};
use gantry_core::ids::Clock;
use gantry_pki::admin::DeviceAdmin;
use gantry_pki::verifier::{p256_only_provider, ClockTimeProvider, DeviceCertVerifier};
use http_body_util::{BodyExt, Empty};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use support::pki_env::{Paired, PkiEnv};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;

const SERVER_NAME: &str = "gantry.example.test";

fn der(pem_text: &str) -> Vec<CertificateDer<'static>> {
    pem::parse_many(pem_text)
        .unwrap()
        .into_iter()
        .map(|p| CertificateDer::from(p.into_contents()))
        .collect()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(p256_only_provider())
}

fn time(env: &PkiEnv) -> Arc<dyn rustls::time_provider::TimeProvider> {
    Arc::new(ClockTimeProvider::new(env.clock.clone() as Arc<dyn Clock>))
}

fn server_config(env: &PkiEnv) -> rustls::ServerConfig {
    let root = der(&env.root_pem).remove(0);
    let intermediate = der(&env.intermediate_pem).remove(0);
    let verifier =
        DeviceCertVerifier::new(&root, &[intermediate.to_vec()], env.directory.clone()).unwrap();
    let mut chain = der(&env.server_crt_pem);
    chain.push(intermediate);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        pem::parse(&env.server_key_pem).unwrap().into_contents(),
    ));
    let signing_key = provider().key_provider.load_private_key(key).unwrap();
    let certified = Arc::new(rustls::sign::CertifiedKey::new(chain, signing_key));
    struct One(Arc<rustls::sign::CertifiedKey>);
    impl std::fmt::Debug for One {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("One")
        }
    }
    impl rustls::server::ResolvesServerCert for One {
        fn resolve(
            &self,
            _: rustls::server::ClientHello<'_>,
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            Some(self.0.clone())
        }
    }
    api_server_config(
        provider(),
        time(env),
        Arc::new(verifier),
        Arc::new(One(certified)),
    )
    .unwrap()
}

/// 起一个真实的 mTLS 监听器，返回地址。
async fn start(env: &PkiEnv) -> (std::net::SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let core = env.core.clone();
    tokio::spawn(gantry_api::listener::serve_api(
        core,
        listener,
        Arc::new(server_config(env)),
        rx,
    ));
    (addr, tx)
}

fn client_config(env: &PkiEnv, device: Option<&Paired>) -> ClientConfig {
    let mut roots = RootCertStore::empty();
    roots.add(der(&env.root_pem).remove(0)).unwrap();
    let builder = ClientConfig::builder_with_details(provider(), time(env))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots);
    let mut config = match device {
        Some(d) => {
            let chain: Vec<CertificateDer<'static>> = d.response.certificate_chain[..2]
                .iter()
                .flat_map(|p| der(p))
                .collect();
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(d.keys.client.serialize_der()));
            builder.with_client_auth_cert(chain, key).unwrap()
        }
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

async fn connect(
    env: &PkiEnv,
    addr: std::net::SocketAddr,
    device: Option<&Paired>,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    TlsConnector::from(Arc::new(client_config(env, device)))
        .connect(ServerName::try_from(SERVER_NAME).unwrap(), tcp)
        .await
}

/// 经真实连接发一个 HTTP/1.1 请求，返回状态与正文。
async fn get(
    stream: tokio_rustls::client::TlsStream<TcpStream>,
    path: &str,
) -> (u16, http::HeaderMap, Bytes) {
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = http::Request::get(path)
        .header("host", SERVER_NAME)
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

#[test]
fn server_config_is_hardened() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let env = PkiEnv::new().await;
        let c = server_config(&env);
        assert_eq!(c.send_tls13_tickets, 0);
        assert_eq!(c.max_early_data_size, 0);
        assert!(!c.session_storage.can_cache());
        assert_eq!(c.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    });
}

#[tokio::test]
async fn paired_device_reaches_the_pipeline_over_real_tls() {
    let env = PkiEnv::new().await;
    let d = env.pair(DevicePlatform::Macos, "a").await;
    let (addr, _stop) = start(&env).await;
    let (status, headers, body) = get(connect(&env, addr, Some(&d)).await.unwrap(), "/v1/me").await;
    assert_eq!(status, 200, "{body:?}");
    assert!(headers.get("x-log-epoch").is_some());
    let me: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(me["device"]["id"], d.response.device.id.to_string());
    // /v1/pair 与 /webhook 在 API 端口上都是 404
    for path in ["/v1/pair", "/webhook"] {
        let (status, headers, _) = get(connect(&env, addr, Some(&d)).await.unwrap(), path).await;
        assert_eq!(status, 404);
        assert!(headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/problem+json"));
    }
}

/// 无证书 / 已吊销：握手失败，零 HTTP 响应字节。
#[tokio::test]
async fn handshake_failures_produce_no_http_bytes() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "a").await;
    let victim = env.pair(DevicePlatform::Macos, "b").await;
    let (addr, _stop) = start(&env).await;

    async fn no_bytes(result: std::io::Result<tokio_rustls::client::TlsStream<TcpStream>>) {
        // TLS 1.3 的客户端证书错误可能在第一次读写时才暴露
        if let Ok(mut s) = result {
            let _ = s.write_all(b"GET /v1/me HTTP/1.1\r\nhost: x\r\n\r\n").await;
            let mut buf = Vec::new();
            let read = s.read_to_end(&mut buf).await;
            assert!(
                read.is_err() || buf.is_empty(),
                "server sent HTTP bytes: {:?}",
                String::from_utf8_lossy(&buf)
            );
        }
    }
    no_bytes(connect(&env, addr, None).await).await;

    let mut tx = env.store.begin_write().await.unwrap();
    env.admin
        .revoke(
            &mut tx,
            &victim.response.device.id,
            &Actor::new(ActorKind::System, "host"),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    no_bytes(connect(&env, addr, Some(&victim)).await).await;
    // 其他设备照常
    let (status, _, _) = get(
        connect(&env, addr, Some(&approver)).await.unwrap(),
        "/v1/me",
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn event_stream_flows_over_a_real_connection_and_closes_on_shutdown() {
    let env = PkiEnv::new().await;
    let d = env.pair(DevicePlatform::Macos, "a").await;
    let (addr, stop) = start(&env).await;
    let stream = connect(&env, addr, Some(&d)).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = http::Request::get("/v1/events")
        .header("host", SERVER_NAME)
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let mut body = resp.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&first[..], b"retry: 3000\n\n");
    let heartbeat = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(String::from_utf8_lossy(&heartbeat).starts_with("event: heartbeat"));
    stop.send(true).unwrap();
    let mut rest = String::new();
    while let Some(Ok(frame)) = body.frame().await {
        if let Some(data) = frame.data_ref() {
            rest.push_str(&String::from_utf8_lossy(data));
        }
    }
    assert!(rest.contains(r#"{"reason":"server_shutdown"}"#), "{rest}");
}
