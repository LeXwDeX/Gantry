//! 受保护写入（desktop-shell.md §12"受保护写入"的 A 级部分）。
//!
//! 替身服务端实现 api.md §6.6：`dry_run` 判定是否需要签名并给出 `signing`；未带 `Gantry-Approval` 的
//! 秘密写入返回 `403 signature_required`；带审批时核对 `granted`、目标与 `SHA-256(JCS(请求体))`。
#![cfg(feature = "test-software-keys")]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::canon::{jcs, jcs_digest, APPROVAL_SIGNING_PREFIX};
use gantry_desktop_shell::events::{EventsConfig, FrameSink};
use gantry_desktop_shell::guarded::live_secret_bodies;
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

const DEV: &str = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const OTHER: &str = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZA";
const APR: &str = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
const SENTINEL: &str = "sk-ant-SENTINEL-0123456789";

#[derive(Default)]
struct S {
    role: String,
    approval: Option<Value>,
    payload: Option<Value>,
    approvals_created: usize,
    /// 注入：dry_run 返回的 subject_digest 与请求体不符。
    tamper_digest: bool,
    writes: Vec<(String, String, Vec<u8>, Option<String>)>,
    all_bodies: Vec<String>,
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

fn has_secret(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            (m.contains_key("secret_ref") && m.contains_key("value")) || m.values().any(has_secret)
        }
        Value::Array(a) => a.iter().any(has_secret),
        _ => false,
    }
}

fn handler(state: Arc<Mutex<S>>, approval_pubkey: P256PublicKey) -> AnyHandler {
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
        st.all_bodies
            .push(String::from_utf8_lossy(&seen.body).to_string());
        let header = |name: &str| {
            seen.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let (method, path) = (seen.method.as_str(), seen.path.as_str());
        match (method, path) {
            ("GET", "/v1/events") => Reply::Stream(vec![
                "event: heartbeat\nid: 1\ndata: {\"seq\":1,\"log_epoch\":1}\n\n".into(),
            ]),
            ("GET", "/v1/me") => json_resp(
                200,
                json!({"device": {"id": DEV, "name": "mac", "role": st.role}, "profile": "desktop", "server": {"time": ts(now_ms()), "log_epoch": 1}}),
            ),
            ("GET", "/v1/settings/models") => json_resp(
                200,
                json!({"resource": "models", "rev": 7, "value": {"worker": {"model": "old"}}}),
            ),
            ("PUT", p) if p.starts_with("/v1/settings/models") => {
                let body: Value = serde_json::from_slice(&seen.body).unwrap();
                let digest = jcs_digest(&body).unwrap().to_string();
                let secret = has_secret(&body);
                if p.ends_with("?dry_run=true") {
                    let mut resp = json!({"valid": true, "errors": [], "warnings": [], "relaxes": [], "required_role": if secret {"approver"} else {"operator"}, "requires_signature": secret});
                    if secret {
                        resp["signing"] = json!({
                            "action": "settings_update",
                            "target": {"kind": "settings", "scope": "global", "resource": "models", "base_rev": 7},
                            "subject_digest": if st.tamper_digest { "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string() } else { digest },
                        });
                    }
                    return json_resp(200, resp);
                }
                assert_eq!(header("if-match").as_deref(), Some("\"7\""));
                let approval = header("gantry-approval");
                if secret && approval.is_none() {
                    return json_resp(
                        403,
                        json!({"code": "signature_required", "title": "x", "request_id": "r"}),
                    );
                }
                if let Some(a) = &approval {
                    let ap = st.approval.clone().unwrap();
                    if ap["id"] != *a || ap["status"] != "granted" || ap["subject_digest"] != digest
                    {
                        return json_resp(
                            409,
                            json!({"code": "approval_mismatch", "title": "x", "request_id": "r"}),
                        );
                    }
                    st.approval.as_mut().unwrap()["status"] = json!("consumed");
                }
                st.writes
                    .push((method.into(), path.into(), seen.body.clone(), approval));
                json_resp(
                    200,
                    json!({"resource": "models", "rev": 8, "value": {"worker": {"model": "new"}}}),
                )
            }
            ("POST", "/v1/approvals") => {
                assert!(header("idempotency-key").is_some());
                st.approvals_created += 1;
                let signing: Value = serde_json::from_slice(&seen.body).unwrap();
                let approval = json!({"id": APR, "action": signing["action"], "target": signing["target"], "subject_digest": signing["subject_digest"], "status": "pending", "expires_at": ts(now_ms() + 3_600_000)});
                st.approval = Some(approval.clone());
                json_resp(201, approval)
            }
            ("GET", p) if p == format!("/v1/approvals/{APR}") => {
                json_resp(200, st.approval.clone().unwrap())
            }
            ("GET", p) if p == format!("/v1/approvals/{APR}/challenge") => {
                let a = st.approval.clone().unwrap();
                let payload = json!({"action": a["action"], "approval_id": APR, "device_id": DEV, "expires_at": ts((now_ms() / 1000 + 120) * 1000), "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js", "subject_digest": a["subject_digest"], "target": a["target"], "v": 1});
                st.payload = Some(payload.clone());
                json_resp(
                    200,
                    json!({"approval_id": APR, "payload": payload, "signing": {"alg": "ES256", "canonicalization": "JCS", "prefix": "gantry/approval/v1\n"}}),
                )
            }
            ("POST", p) if p == format!("/v1/approvals/{APR}/grant") => {
                let body: Value = serde_json::from_slice(&seen.body).unwrap();
                let mut bytes = APPROVAL_SIGNING_PREFIX.to_vec();
                bytes.extend(jcs(st.payload.as_ref().unwrap()).unwrap());
                let sig = Signature::from_slice(
                    &URL_SAFE_NO_PAD
                        .decode(body["signature"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap();
                assert!(VerifyingKey::from_sec1_bytes(approval_pubkey.as_bytes())
                    .unwrap()
                    .verify(&bytes, &sig)
                    .is_ok());
                st.approval.as_mut().unwrap()["status"] = json!("granted");
                json_resp(200, st.approval.clone().unwrap())
            }
            (m, p)
                if p == format!("/v1/devices/{OTHER}/revoke")
                    || (m == "PUT" && p == format!("/v1/devices/{OTHER}/role")) =>
            {
                let body: Value = if seen.body.is_empty() {
                    json!({})
                } else {
                    serde_json::from_slice(&seen.body).unwrap()
                };
                let ap = st.approval.clone().unwrap();
                if header("gantry-approval").as_deref() != Some(APR)
                    || ap["status"] != "granted"
                    || ap["subject_digest"] != jcs_digest(&body).unwrap().to_string()
                {
                    return json_resp(
                        409,
                        json!({"code": "approval_mismatch", "title": "x", "request_id": "r"}),
                    );
                }
                st.writes.push((
                    m.into(),
                    p.into(),
                    seen.body.clone(),
                    header("gantry-approval"),
                ));
                json_resp(
                    200,
                    json!({"id": OTHER, "status": if p.ends_with("revoke") {"revoked"} else {"active"}}),
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
    presence: PresenceDouble,
    state: Arc<Mutex<S>>,
    dir: std::path::PathBuf,
}

async fn env(name: &str, role: &str) -> Env {
    let pki = Pki::new();
    let presence = PresenceDouble::new();
    let keys = TestSoftwareKeyStore::new(presence.clone());
    let device = keys.generate().unwrap();
    let state = Arc::new(Mutex::new(S {
        role: role.into(),
        ..Default::default()
    }));
    let addr = serve_any(
        pki.server_config(true),
        handler(state.clone(), device.approval_public_key()),
    )
    .await;
    let dir = std::env::temp_dir().join(format!(
        "gantry-shell-guarded-{name}-{}",
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
    shell.events_open(None, Arc::new(Nop)).unwrap();
    for _ in 0..300 {
        if shell.connection_state()["phase"] == "live" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Env {
        shell,
        presence,
        state,
        dir,
    }
}

fn secret_put() -> Value {
    json!({
        "kind": "settings_put", "scope": "global", "resource": "models", "if_match": "\"7\"",
        "value": {"providers": {"anthropic": {"api_key": {"secret_ref": "anthropic_key", "value": SENTINEL}}}, "worker": {"model": "new"}}
    })
}

// 存活秘密块计数是进程级的，并行测试会互相影响；需要检查它的用例串行执行。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn model_credential_write_goes_through_the_signing_chain() {
    let _g = SERIAL.lock().await;
    let e = env("secret", "approver").await;
    let preview = e.shell.guarded_write_prepare(&secret_put()).await.unwrap();
    assert_eq!(preview["requires_signature"], true);
    assert_eq!(preview["required_role"], "approver");
    assert_eq!(preview["can_submit"], json!({"ok": true}));
    assert_eq!(
        preview["preview"]["secret_paths"][0]["pointer"],
        "/providers/anthropic/api_key"
    );
    assert!(preview["preview"]["changed_paths"]
        .as_array()
        .unwrap()
        .contains(&json!("/worker/model")));
    assert!(
        !preview.to_string().contains(SENTINEL),
        "秘密值不出现在预览"
    );
    assert_eq!(live_secret_bodies(), 1);

    e.presence.push(PresenceOutcome::Approve);
    let result = e
        .shell
        .guarded_write_submit(preview["pending_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert!(
        !result.to_string().contains(SENTINEL),
        "秘密值不出现在 ApiResult"
    );
    let st = e.state.lock().unwrap();
    assert_eq!(st.approvals_created, 1);
    let (_, _, body, approval) = st.writes.last().unwrap();
    assert_eq!(approval.as_deref(), Some(APR), "带 Gantry-Approval 重发");
    // 发出的请求体与 prepare 时一致（服务端 digest 校验通过即证明）。
    assert!(String::from_utf8_lossy(body).contains(SENTINEL));
    drop(st);
    assert_eq!(live_secret_bodies(), 0, "提交后请求体已清零");
    assert!(e.presence.prompts()[0]
        .summary
        .starts_with("settings settings/models @ "));
    // 日志 / 诊断以外的本机文件里也没有秘密值。
    for entry in walk(&e.dir) {
        assert!(
            !std::fs::read_to_string(&entry)
                .unwrap_or_default()
                .contains(SENTINEL),
            "{entry:?}"
        );
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn neutral_write_is_a_direct_put_without_approval() {
    let _g = SERIAL.lock().await;
    let e = env("neutral", "operator").await;
    let preview = e
        .shell
        .guarded_write_prepare(&json!({"kind": "settings_put", "scope": "global", "resource": "models", "if_match": "\"7\"", "value": {"worker": {"model": "new"}}}))
        .await
        .unwrap();
    assert_eq!(preview["requires_signature"], false);
    let result = e
        .shell
        .guarded_write_submit(preview["pending_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(e.state.lock().unwrap().approvals_created, 0, "不发起审批");
    assert!(e.presence.prompts().is_empty());
    assert_eq!(live_secret_bodies(), 0);
}

#[tokio::test]
async fn digest_mismatch_from_server_is_rejected_before_any_prompt() {
    let _g = SERIAL.lock().await;
    let e = env("tamper", "approver").await;
    e.state.lock().unwrap().tamper_digest = true;
    let preview = e.shell.guarded_write_prepare(&secret_put()).await.unwrap();
    let err = e
        .shell
        .guarded_write_submit(preview["pending_id"].as_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        (err.code, err.reason.as_deref()),
        (
            ShellErrorCode::SelfCheckFailed,
            Some("internal_digest_mismatch")
        )
    );
    assert_eq!(e.state.lock().unwrap().approvals_created, 0);
    assert!(e.presence.prompts().is_empty());
    assert_eq!(live_secret_bodies(), 0, "失败后也清零");
}

#[tokio::test]
async fn device_actions_assemble_signing_locally() {
    let _g = SERIAL.lock().await;
    let e = env("device", "approver").await;
    // 吊销另一台设备：摘要是 {} 的常量。
    let preview = e
        .shell
        .guarded_write_prepare(&json!({"kind": "device_revoke", "device_id": OTHER}))
        .await
        .unwrap();
    assert_eq!(preview["requires_signature"], true);
    e.presence.push(PresenceOutcome::Approve);
    let result = e
        .shell
        .guarded_write_submit(preview["pending_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(result["body"]["status"], "revoked", "{result}");
    assert_eq!(
        e.state.lock().unwrap().approval.as_ref().unwrap()["subject_digest"],
        jcs_digest(&json!({})).unwrap().to_string()
    );

    // 改自己的角色：本地校验不通过。
    let preview = e
        .shell
        .guarded_write_prepare(&json!({"kind": "device_role", "device_id": DEV, "role": "viewer"}))
        .await
        .unwrap();
    assert_eq!(preview["valid"], false);
    assert_eq!(preview["errors"][0]["code"], "self_role_change");
    assert_eq!(preview["can_submit"]["reason"], "invalid");
    e.shell
        .guarded_write_discard(preview["pending_id"].as_str().unwrap());
    assert_eq!(live_secret_bodies(), 0, "丢弃后清零");
}

#[tokio::test]
async fn operator_initiates_and_waits_for_another_approver() {
    let _g = SERIAL.lock().await;
    let e = env("operator", "operator").await;
    let preview = e.shell.guarded_write_prepare(&secret_put()).await.unwrap();
    assert_eq!(preview["can_submit"]["reason"], "not_approver");
    let id = preview["pending_id"].as_str().unwrap().to_string();
    let result = e.shell.guarded_write_submit(&id).await.unwrap();
    assert_eq!(
        result["body"]["status"], "pending",
        "已发起审批，等待 approver 设备"
    );
    assert!(e.presence.prompts().is_empty());
    // 另一台 approver 批准后（这里直接改服务端状态），再次提交即带 Gantry-Approval 写入。
    e.state.lock().unwrap().approval.as_mut().unwrap()["status"] = json!("granted");
    let result = e.shell.guarded_write_submit(&id).await.unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(e.state.lock().unwrap().approvals_created, 1, "复用同一审批");
    assert_eq!(live_secret_bodies(), 0);
}

#[tokio::test]
async fn bad_arguments_are_rejected() {
    let _g = SERIAL.lock().await;
    let e = env("args", "approver").await;
    for (args, field) in [
        (
            json!({"kind": "settings_put", "scope": "global", "resource": "nope", "if_match": "\"7\"", "value": {}}),
            "resource",
        ),
        (
            json!({"kind": "settings_put", "scope": "global", "resource": "models", "if_match": "7", "value": {}}),
            "if_match",
        ),
        (
            json!({"kind": "settings_put", "scope": "project", "resource": "models", "if_match": "\"7\"", "value": {}}),
            "project_id",
        ),
        (
            json!({"kind": "settings_put", "scope": "global", "resource": "models", "if_match": "\"7\"", "value": {}, "signing": {}}),
            "signing",
        ),
        (
            json!({"kind": "device_role", "device_id": OTHER, "role": "root"}),
            "role",
        ),
        (
            json!({"kind": "device_revoke", "device_id": "dev_x"}),
            "device_id",
        ),
        (json!({"kind": "wipe"}), "kind"),
    ] {
        let err = e.shell.guarded_write_prepare(&args).await.unwrap_err();
        assert_eq!(
            (err.code, err.field.as_deref()),
            (ShellErrorCode::BadArgument, Some(field)),
            "{args}"
        );
    }
    assert_eq!(
        e.shell
            .guarded_write_submit("forged")
            .await
            .unwrap_err()
            .code,
        ShellErrorCode::PreparedExpired
    );
    assert_eq!(live_secret_bodies(), 0);
}
