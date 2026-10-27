//! 审批签名接线（desktop-shell.md §12"签名"的 A 级部分）：prepare → 重检 → 用户在场 → grant。
//!
//! 替身服务端按登记的 `approval_pubkey` 真实验签（JCS + ES256），可切换设备角色、注入新推送（`stale_head`）。
#![cfg(feature = "test-software-keys")]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::canon::{jcs, APPROVAL_SIGNING_PREFIX};
use gantry_desktop_shell::approval::exact_args;
use gantry_desktop_shell::events::{EventsConfig, FrameSink};
use gantry_desktop_shell::http::HttpPairTransport;
use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::keys::software::{PresenceDouble, PresenceOutcome, TestSoftwareKeyStore};
use gantry_desktop_shell::keys::{KeyStore, P256PublicKey};
use gantry_desktop_shell::session::{system_clock, Shell};
use gantry_desktop_shell::storage::{AppStore, IdentityFile};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::{json, Value};
use support::{serve_any, AnyHandler, Pki, Reply, Seen};

const APR: &str = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
const DLV: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const DEV: &str = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const SHA: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";

struct ServerState {
    role: String,
    status: String,
    stale: bool,
    challenges: usize,
    payload: Option<Value>,
    grant_verified: Option<bool>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn ts(ms: i64) -> String {
    gantry_core::time::Timestamp::from_millis(ms).to_string()
}

fn approval_json(st: &ServerState) -> Value {
    json!({
        "id": APR, "delivery_id": DLV, "action": "merge_pr",
        "target": {"kind": "pr", "repo": "acme/web", "number": 12},
        "subject_digest": SHA, "device_id": null, "signature": null,
        "status": st.status, "expires_at": ts(now_ms() + 3_600_000)
    })
}

fn handler(state: Arc<Mutex<ServerState>>, approval_pubkey: P256PublicKey) -> AnyHandler {
    Arc::new(move |seen: Seen| {
        let json_resp = |status: u16, v: Value| {
            Reply::Full(
                status,
                vec![
                    ("content-type".into(), "application/json".into()),
                    ("x-log-epoch".into(), "1".into()),
                ],
                serde_json::to_vec(&v).unwrap(),
            )
        };
        let mut st = state.lock().unwrap();
        match (seen.method.as_str(), seen.path.as_str()) {
            ("GET", "/v1/events") => Reply::Stream(vec![
                "event: heartbeat\nid: 1\ndata: {\"seq\":1,\"log_epoch\":1}\n\n".into(),
            ]),
            ("GET", "/v1/me") => json_resp(
                200,
                json!({
                    "device": {"id": DEV, "name": "Susan 的 MacBook", "role": st.role},
                    "profile": "desktop",
                    "server": {"time": ts(now_ms() - 1_500), "log_epoch": 1}
                }),
            ),
            ("GET", p) if p == format!("/v1/deliveries/{DLV}") => {
                json_resp(200, json!({"id": DLV, "title": "修复登录 <script>"}))
            }
            ("GET", p) if p == format!("/v1/approvals/{APR}") => json_resp(200, approval_json(&st)),
            ("GET", p) if p == format!("/v1/approvals/{APR}/challenge") => {
                st.challenges += 1;
                let payload = json!({
                    "action": "merge_pr", "approval_id": APR, "delivery_id": DLV, "device_id": DEV,
                    "expires_at": ts((now_ms() / 1000 + 120) * 1000),
                    "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
                    "subject_digest": SHA, "target": {"kind": "pr", "number": 12, "repo": "acme/web"}, "v": 1
                });
                st.payload = Some(payload.clone());
                json_resp(
                    200,
                    json!({
                        "approval_id": APR, "payload": payload,
                        "signing": {"alg": "ES256", "canonicalization": "JCS", "prefix": "gantry/approval/v1\n"}
                    }),
                )
            }
            ("POST", p) if p == format!("/v1/approvals/{APR}/grant") => {
                let body: Value = serde_json::from_slice(&seen.body).unwrap();
                assert_eq!(
                    body.as_object().unwrap().len(),
                    2,
                    "grant 体只有 nonce 与签名"
                );
                assert!(seen.headers.iter().any(|(k, _)| k == "idempotency-key"));
                // 按登记的 approval_pubkey 验签。
                let mut bytes = APPROVAL_SIGNING_PREFIX.to_vec();
                bytes.extend(jcs(st.payload.as_ref().unwrap()).unwrap());
                let sig = Signature::from_slice(
                    &URL_SAFE_NO_PAD
                        .decode(body["signature"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap();
                let ok = VerifyingKey::from_sec1_bytes(approval_pubkey.as_bytes())
                    .unwrap()
                    .verify(&bytes, &sig)
                    .is_ok();
                st.grant_verified = Some(ok);
                if !ok {
                    return json_resp(
                        403,
                        json!({"code": "signature_invalid", "title": "x", "request_id": "r"}),
                    );
                }
                if st.stale {
                    return json_resp(
                        409,
                        json!({"code": "stale_head", "title": "x", "request_id": "r", "current_head_sha": "0000000000000000000000000000000000000000"}),
                    );
                }
                st.status = "granted".into();
                json_resp(200, approval_json(&st))
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
    presence: PresenceDouble,
    state: Arc<Mutex<ServerState>>,
}

async fn env(name: &str, role: &str) -> Env {
    let pki = Pki::new();
    let presence = PresenceDouble::new();
    let keys = TestSoftwareKeyStore::new(presence.clone());
    let device = keys.generate().unwrap();
    let state = Arc::new(Mutex::new(ServerState {
        role: role.into(),
        status: "pending".into(),
        stale: false,
        challenges: 0,
        payload: None,
        grant_verified: None,
    }));
    let addr = serve_any(
        pki.server_config(true),
        handler(state.clone(), device.approval_public_key()),
    )
    .await;
    let dir = std::env::temp_dir().join(format!(
        "gantry-shell-approval-{name}-{}",
        std::process::id()
    ));
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
            paired_at: ts(now_ms()),
            renewed_at: None,
            log_epoch: Some(1),
            revoked: false,
        })
        .unwrap();
    let shell = Arc::new(
        Shell::new(
            store,
            Box::new(keys),
            Box::new(HttpPairTransport::default()),
            system_clock(),
        )
        .with_events_config(EventsConfig {
            idle: Duration::from_secs(5),
            backoff_scale: 0.001,
            handshake_fail_limit: 3,
        }),
    );
    Env {
        shell,
        presence,
        state,
    }
}

async fn go_live(shell: &Arc<Shell>) {
    shell.events_open(None, Arc::new(Nop)).unwrap();
    for _ in 0..300 {
        if shell.connection_state()["phase"] == "live" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("事件流没有进入 live");
}

#[tokio::test]
async fn approver_signs_and_server_verifies_with_registered_key() {
    let e = env("ok", "approver").await;
    go_live(&e.shell).await;
    let model = e.shell.approval_prepare(APR).await.unwrap();
    assert_eq!(model["can_sign"], json!({"ok": true}));
    assert_eq!(model["signed"]["digest_head12"], "9f2c4e7a1b3d");
    assert_eq!(model["signed"]["device_name"], "Susan 的 MacBook");
    assert_eq!(
        model["unsigned"]["delivery_title"]["text"],
        "修复登录 <script>"
    );
    // 时钟偏差已用 /v1/me 校正（服务端慢约 1.5 s）。
    let skew = e.shell.connection_state()["clock_skew_ms"]
        .as_i64()
        .unwrap();
    assert!((1_000..5_000).contains(&skew), "{skew}");

    e.presence.push(PresenceOutcome::Approve);
    let result = e
        .shell
        .approval_sign(model["prepared_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["body"]["status"], "granted");
    assert_eq!(e.state.lock().unwrap().grant_verified, Some(true));
    assert_eq!(
        e.presence.prompts()[0].summary,
        "merge acme/web#12 @ 9f2c4e7a1b3d"
    );
    // 同一句柄不能再用。
    let again = e
        .shell
        .approval_sign(model["prepared_id"].as_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(again.code, ShellErrorCode::PreparedExpired);
}

#[tokio::test]
async fn operator_gets_readonly_sheet_without_challenge() {
    let e = env("operator", "operator").await;
    go_live(&e.shell).await;
    let model = e.shell.approval_prepare(APR).await.unwrap();
    assert_eq!(
        model["can_sign"],
        json!({"ok": false, "reason": "not_approver"})
    );
    assert_eq!(
        e.state.lock().unwrap().challenges,
        0,
        "非 approver 不取 challenge"
    );
    let err = e
        .shell
        .approval_sign("0123456789abcdef0123456789abcdef")
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        ShellErrorCode::PreparedExpired,
        "伪造的 prepared_id"
    );
    assert!(e.presence.prompts().is_empty());
}

#[tokio::test]
async fn not_live_means_no_prompt() {
    let e = env("offline", "approver").await;
    let model = e.shell.approval_prepare(APR).await.unwrap();
    let err = e
        .shell
        .approval_sign(model["prepared_id"].as_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(err.code, ShellErrorCode::NotConnected);
    assert!(e.presence.prompts().is_empty(), "连接中断时不弹确认框");
}

#[tokio::test]
async fn new_push_after_signing_gives_stale_head_and_discards() {
    let e = env("stale", "approver").await;
    go_live(&e.shell).await;
    let model = e.shell.approval_prepare(APR).await.unwrap();
    e.state.lock().unwrap().stale = true;
    e.presence.push(PresenceOutcome::Approve);
    let id = model["prepared_id"].as_str().unwrap();
    let result = e.shell.approval_sign(id).await.unwrap();
    assert_eq!(result["status"], 409);
    assert_eq!(result["problem"]["code"], "stale_head");
    assert!(
        e.shell.signer().get(id, 0).is_none(),
        "丢弃 PreparedSigning，不重试"
    );
}

#[tokio::test]
async fn approval_changed_before_confirmation_aborts_without_prompt() {
    let e = env("changed", "approver").await;
    go_live(&e.shell).await;
    let model = e.shell.approval_prepare(APR).await.unwrap();
    e.state.lock().unwrap().status = "denied".into();
    let err = e
        .shell
        .approval_sign(model["prepared_id"].as_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        (err.code, err.reason.as_deref()),
        (ShellErrorCode::SelfCheckFailed, Some("not_pending"))
    );
    assert!(e.presence.prompts().is_empty());
    // 非 pending 的审批：只读展示。
    let model = e.shell.approval_prepare(APR).await.unwrap();
    assert_eq!(model["can_sign"]["reason"], "not_pending");
}

#[tokio::test]
async fn cancel_keeps_the_handle_and_bad_ids_are_rejected() {
    let e = env("cancel", "approver").await;
    go_live(&e.shell).await;
    let model = e.shell.approval_prepare(APR).await.unwrap();
    let id = model["prepared_id"].as_str().unwrap();
    e.presence.push(PresenceOutcome::Cancel);
    assert_eq!(
        e.shell.approval_sign(id).await.unwrap_err().code,
        ShellErrorCode::UserCancelled
    );
    e.presence.push(PresenceOutcome::Approve);
    assert_eq!(e.shell.approval_sign(id).await.unwrap()["ok"], true);
    assert_eq!(
        e.shell
            .approval_prepare("apr_bad")
            .await
            .unwrap_err()
            .field
            .as_deref(),
        Some("approval_id")
    );
}

#[test]
fn sign_command_rejects_smuggled_arguments() {
    exact_args(&json!({"prepared_id": "x"}), &["prepared_id"]).unwrap();
    for extra in ["payload", "signature", "nonce", "subject_digest"] {
        let mut args = json!({"prepared_id": "x"});
        args[extra] = json!("y");
        let err = exact_args(&args, &["prepared_id"]).unwrap_err();
        assert_eq!(
            (err.code, err.field.as_deref()),
            (ShellErrorCode::BadArgument, Some(extra))
        );
    }
}
