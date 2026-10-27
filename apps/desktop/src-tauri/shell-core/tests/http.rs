//! HTTPS 客户端与配对传输（desktop-shell.md §12"配对"、"mTLS 与固定 CA"，§7.1 重试 / 并发 / 超时）。
//!
//! 本进程内起 hyper + tokio-rustls 服务端，证书由 TestCA 与 gantry-pki 签发。
#![cfg(feature = "test-software-keys")]

mod support;
use support::{now, origin, serve, serve_simple, Handler, Pki, Seen};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gantry_desktop_shell::http::{HttpClient, HttpPairTransport, NetError, Origin};
use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::keys::software::{PresenceDouble, TestSoftwareKeyStore};
use gantry_desktop_shell::pairing;
use gantry_desktop_shell::routes::prepare;
use gantry_desktop_shell::tls::{chain_from_pem, client_config, ClientIdentity, RootFingerprint};
use serde_json::{json, Value};
use tokio::net::TcpListener;

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
