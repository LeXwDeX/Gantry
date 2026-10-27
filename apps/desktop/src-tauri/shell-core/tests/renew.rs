//! 证书自动续期（desktop-shell.md §12"mTLS 与固定 CA"的自动续期一条，P-64）。
//!
//! 时钟可推进；替身服务端用 gantry-pki 按 CSR 里的公钥签发新叶子，并记录每次握手出示的客户端证书。
#![cfg(feature = "test-software-keys")]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_desktop_shell::events::{EventsConfig, FrameSink};
use gantry_desktop_shell::http::HttpPairTransport;
use gantry_desktop_shell::keys::software::{PresenceDouble, TestSoftwareKeyStore};
use gantry_desktop_shell::keys::KeyStore;
use gantry_desktop_shell::session::{system_clock, Clock, Shell};
use gantry_desktop_shell::storage::{AppStore, IdentityFile};
use serde_json::{json, Value};
use support::{serve_any, AnyHandler, Pki, Reply, Seen};

const DEV: &str = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const DAY: i64 = 86_400_000;

#[derive(Default)]
struct S {
    renews: usize,
    /// 注入：续期返回另一套 CA 签发的链。
    foreign: bool,
    /// 注入：续期一直失败。
    fail: bool,
    peers: Vec<Vec<u8>>,
    stream_conns: usize,
}

fn handler(state: Arc<Mutex<S>>, pki: Arc<Pki>, other: Arc<Pki>) -> AnyHandler {
    Arc::new(move |seen: Seen| {
        let json_resp = |status: u16, v: Value| {
            Reply::Full(
                status,
                vec![("content-type".into(), "application/json".into())],
                serde_json::to_vec(&v).unwrap(),
            )
        };
        let mut st = state.lock().unwrap();
        if let Some(p) = &seen.peer {
            st.peers.push(p.clone());
        }
        match (seen.method.as_str(), seen.path.as_str()) {
            ("GET", "/v1/events") => {
                st.stream_conns += 1;
                Reply::Stream(vec![
                    "event: heartbeat\nid: 1\ndata: {\"seq\":1,\"log_epoch\":1}\n\n".into(),
                ])
            }
            ("GET", "/v1/me") => json_resp(
                200,
                json!({"device": {"id": DEV, "role": "approver"}, "server": {"log_epoch": 1}}),
            ),
            ("POST", "/v1/devices/self/renew") => {
                st.renews += 1;
                if st.fail {
                    return json_resp(
                        503,
                        json!({"code": "unavailable", "title": "", "request_id": "r"}),
                    );
                }
                let body: Value = serde_json::from_slice(&seen.body).unwrap();
                let public =
                    gantry_pki::csr::validate_csr(body["csr"].as_str().unwrap(), 16 * 1024)
                        .unwrap();
                let issuer = if st.foreign { &other } else { &pki };
                let [inter, root] = issuer.chain_tail();
                json_resp(
                    200,
                    json!({"certificate_chain": [issuer.issue_device(&public), inter, root], "not_after": "x"}),
                )
            }
            _ => json_resp(
                404,
                json!({"code": "not_found", "title": "", "request_id": "r"}),
            ),
        }
    })
}

struct Nop;
impl FrameSink for Nop {
    fn send(&self, _: Value) -> bool {
        true
    }
}

struct Env {
    shell: Arc<Shell>,
    state: Arc<Mutex<S>>,
    offset: Arc<Mutex<i64>>,
    approval_fpr: String,
}

async fn env(name: &str) -> Env {
    let pki = Arc::new(Pki::new());
    let other = Arc::new(Pki::new());
    let keys = TestSoftwareKeyStore::new(PresenceDouble::new());
    let device = keys.generate().unwrap();
    let state = Arc::new(Mutex::new(S::default()));
    let addr = serve_any(
        pki.server_config(true),
        handler(state.clone(), pki.clone(), other),
    )
    .await;
    let dir =
        std::env::temp_dir().join(format!("gantry-shell-renew-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let public =
        gantry_pki::P256PublicKey::from_spki_der(&device.tls_public_key().spki_der()).unwrap();
    let [inter, root] = pki.chain_tail();
    let store = AppStore::open(&dir).unwrap();
    store
        .save_identity(&IdentityFile {
            api_base_url: format!("https://localhost:{}", addr.port()),
            ca_fingerprint: pki.fp(),
            device_id: DEV.into(),
            device_name: "mac".into(),
            platform: "macos".into(),
            cert_chain_pem: vec![pki.issue_device(&public), inter, root],
            tls_key_label: device.labels().tls.clone(),
            approval_key_label: device.labels().approval.clone(),
            paired_at: "2026-10-27T00:00:00.000Z".into(),
            renewed_at: None,
            log_epoch: Some(1),
            revoked: false,
        })
        .unwrap();
    let offset = Arc::new(Mutex::new(0i64));
    let clock: Clock = {
        let (offset, base) = (offset.clone(), system_clock());
        Arc::new(move || base() + *offset.lock().unwrap())
    };
    let shell = Arc::new(
        Shell::new(
            store,
            Box::new(keys),
            Box::new(HttpPairTransport::default()),
            clock,
        )
        .with_events_config(EventsConfig {
            idle: Duration::from_secs(5),
            backoff_scale: 0.001,
            handshake_fail_limit: 3,
        }),
    );
    Env {
        shell,
        state,
        offset,
        approval_fpr: device.approval_public_key().fingerprint(),
    }
}

fn leaf(shell: &Shell) -> String {
    shell.store().identity().unwrap().cert_chain_pem[0].clone()
}

#[tokio::test]
async fn renews_at_two_thirds_and_new_handshakes_use_the_new_certificate() {
    let e = env("ok").await;
    // 先建立事件流（在途长连接）。
    e.shell.events_open(None, Arc::new(Nop)).unwrap();
    for _ in 0..300 {
        if e.shell.connection_state()["phase"] == "live" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let old_leaf = leaf(&e.shell);

    // 未到 2/3（设备证书 30 天，第 19 天）：不续期。
    *e.offset.lock().unwrap() = 19 * DAY;
    assert!(!e.shell.renew_if_due().await.unwrap());
    assert_eq!(e.state.lock().unwrap().renews, 0);

    // 到达 renew_after（剩余 1/3 有效期处）：续期，换证。新链从真实"现在"起有效，
    // 校验用的"现在"（约第 20 天）仍在新链有效期内。
    let due = e.shell.renew_after_ms().unwrap();
    *e.offset.lock().unwrap() = due - system_clock()() + 1;
    assert!(e.shell.renew_if_due().await.unwrap());
    let new_leaf = leaf(&e.shell);
    assert_ne!(new_leaf, old_leaf);
    assert!(e.shell.store().identity().unwrap().renewed_at.is_some());

    // 下一次握手出示新证书。
    e.shell
        .api_request("me.get", "/v1/me", None, None, None)
        .await
        .unwrap();
    let new_der = pem::parse(&new_leaf).unwrap().into_contents();
    assert_eq!(e.state.lock().unwrap().peers.last().unwrap(), &new_der);
    // 在途事件流不断：仍只有一条流连接，状态仍 live。
    assert_eq!(e.state.lock().unwrap().stream_conns, 1);
    assert_eq!(e.shell.connection_state()["phase"], "live");
    // 审批签名密钥不变。
    let status = e.shell.identity_status().unwrap();
    assert_eq!(status["device"]["approval_pubkey_fpr"], e.approval_fpr);
}

#[tokio::test]
async fn foreign_root_in_renewed_chain_is_rejected_and_old_cert_kept() {
    let e = env("foreign").await;
    e.state.lock().unwrap().foreign = true;
    let old_leaf = leaf(&e.shell);
    let due = e.shell.renew_after_ms().unwrap();
    *e.offset.lock().unwrap() = due - system_clock()() + 1;
    let err = e.shell.renew_if_due().await.unwrap_err();
    assert_eq!(err.reason.as_deref(), Some("ca_fingerprint_mismatch"));
    assert_eq!(leaf(&e.shell), old_leaf, "拒绝替换，继续用旧证书");
    // 旧证书仍可用。
    assert_eq!(
        e.shell
            .api_request("me.get", "/v1/me", None, None, None)
            .await
            .unwrap()["ok"],
        true
    );
}

#[tokio::test]
async fn failing_renewal_marks_cert_expiring_near_expiry() {
    let e = env("expiring").await;
    e.state.lock().unwrap().fail = true;
    let due = e.shell.renew_after_ms().unwrap();
    // 到期但离 notAfter 还远：失败不告警。
    *e.offset.lock().unwrap() = due - system_clock()() + 1;
    assert!(e.shell.renew_if_due().await.is_err());
    assert_eq!(e.shell.connection_state()["cert_expiring"], false);
    // 距到期不足 7 天：告警。
    *e.offset.lock().unwrap() = 24 * DAY;
    assert!(e.shell.renew_if_due().await.is_err());
    assert_eq!(e.shell.connection_state()["cert_expiring"], true);
    assert!(e.state.lock().unwrap().renews >= 2);
}

#[tokio::test]
async fn background_task_renews_when_due() {
    let e = env("background").await;
    let due = e.shell.renew_after_ms().unwrap();
    *e.offset.lock().unwrap() = due - system_clock()() + 1;
    let old_leaf = leaf(&e.shell);
    let task = e.shell.spawn_renewal(Duration::from_millis(20));
    for _ in 0..300 {
        if leaf(&e.shell) != old_leaf {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    assert_ne!(leaf(&e.shell), old_leaf);
}
