//! 审批签名（desktop-shell.md §12"签名"的 A 级部分）。
#![cfg(feature = "test-software-keys")]

use std::cell::Cell;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::time::Timestamp;
use gantry_desktop_shell::approval::{
    self, presence_prompt, self_check, sheet_model, Signer, MAX_PREPARED,
};
use gantry_desktop_shell::ipc::{ShellError, ShellErrorCode};
use gantry_desktop_shell::keys::software::{PresenceDouble, PresenceOutcome, TestSoftwareKeyStore};
use gantry_desktop_shell::keys::{DeviceKeys, KeyStore};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const APR: &str = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
const DEV: &str = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const SHA: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";

/// api.md §6.2 的示例 challenge。
fn challenge() -> Value {
    json!({
        "approval_id": APR,
        "payload": {
            "action": "merge_pr",
            "approval_id": APR,
            "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
            "device_id": DEV,
            "expires_at": "2026-10-24T12:52:00.000Z",
            "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
            "subject_digest": SHA,
            "target": { "kind": "pr", "number": 12, "repo": "acme/web" },
            "v": 1
        },
        "signing": { "alg": "ES256", "canonicalization": "JCS", "prefix": "gantry/approval/v1\n" }
    })
}

/// api.md §6.1 的 `Approval`。
fn approval() -> Value {
    json!({
        "id": APR,
        "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "action": "merge_pr",
        "target": { "kind": "pr", "repo": "acme/web", "number": 12 },
        "subject_digest": SHA,
        "device_id": null,
        "signature": null,
        "status": "pending",
        "expires_at": "2026-10-24T14:41:00.000Z"
    })
}

fn expires_ms() -> i64 {
    "2026-10-24T12:52:00.000Z"
        .parse::<Timestamp>()
        .unwrap()
        .as_millis()
}

/// challenge 到期前 60 秒。
fn now() -> i64 {
    expires_ms() - 60_000
}

#[test]
fn api_test_vector_matches_byte_for_byte() {
    let checked = self_check(APR, &challenge(), &approval(), DEV, now(), None).unwrap();
    let digest = Sha256::digest(&checked.bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "be72c31cfcdc6b5eed579c8d77def70ea64afa994f89205904cd05ba07fd1d7a"
    );

    // 向量里的"有效签名"用向量公钥验证通过。
    let pubkey = URL_SAFE_NO_PAD
        .decode("BHL4aSHzHatNyrq8TX4bJNAxH7no8LM4qb5TVlHTT7ZaASo8h6hMNHwYgitHzpH-S9kLN8ImOTHNk0q-vuB-sTk")
        .unwrap();
    let sig = URL_SAFE_NO_PAD
        .decode("QAzQTiBfxzxWOP2G_q8M4i405Cr8GOZvAFZlbZGCC96ICBp04L9nG2vAWEbWF-jQAkPYVrwZjbBeJGf8nUdDLg")
        .unwrap();
    VerifyingKey::from_sec1_bytes(&pubkey)
        .unwrap()
        .verify(&checked.bytes, &Signature::from_slice(&sig).unwrap())
        .unwrap();
}

fn keys(presence: PresenceDouble) -> Box<dyn DeviceKeys> {
    TestSoftwareKeyStore::new(presence).generate().unwrap()
}

fn ok(_: &approval::PreparedSigning) -> Result<(), ShellError> {
    Ok(())
}

#[test]
fn shell_signature_verifies_with_approval_key_only() {
    let presence = PresenceDouble::with_default(PresenceOutcome::Approve);
    let keys = keys(presence.clone());
    let signer = Signer::new();
    let checked = self_check(APR, &challenge(), &approval(), DEV, now(), None).unwrap();
    let bytes = checked.bytes.clone();
    let id = signer.insert(checked, now()).unwrap();
    let grant = signer.sign(&id, keys.as_ref(), now(), ok).unwrap();

    assert_eq!(grant.approval_id, APR);
    // grant 体只有 nonce 与签名，不带 payload。
    let body = grant.body.as_object().unwrap();
    assert_eq!(body.keys().collect::<Vec<_>>(), ["nonce", "signature"]);
    let sig_b64 = body["signature"].as_str().unwrap();
    assert_eq!(sig_b64.len(), 86);
    assert!(!sig_b64.contains('='));
    let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(sig_b64).unwrap()).unwrap();

    let approval_vk = VerifyingKey::from_sec1_bytes(keys.approval_public_key().as_bytes()).unwrap();
    approval_vk.verify(&bytes, &sig).unwrap();
    // 用 mTLS 公钥验不过（P-47：后台连接用的密钥签不出审批）。
    let tls_vk = VerifyingKey::from_sec1_bytes(keys.tls_public_key().as_bytes()).unwrap();
    assert!(tls_vk.verify(&bytes, &sig).is_err());

    // 提示文案由 payload 生成，含 subject_digest 前 12 位（C27）。
    let prompts = presence.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].summary, "merge acme/web#12 @ 9f2c4e7a1b3d");
}

#[test]
fn every_self_check_failure_is_reported_before_any_prompt() {
    let with = |f: &dyn Fn(&mut Value, &mut Value)| {
        let (mut c, mut a) = (challenge(), approval());
        f(&mut c, &mut a);
        (c, a)
    };
    let cases: Vec<((Value, Value), i64, &str)> = vec![
        (
            with(&|c, _| c["approval_id"] = json!("apr_01JB7P4M8T2X6N9R3Q5K1V7WZA")),
            now(),
            "approval_id_mismatch",
        ),
        (
            // 外层一致、payload 内不一致：签名会绑定到另一审批。
            with(&|c, _| c["payload"]["approval_id"] = json!("apr_01JB7P4M8T2X6N9R3Q5K1V7WZB")),
            now(),
            "approval_id_mismatch",
        ),
        (
            with(&|c, _| c["payload"]["device_id"] = json!("dev_01JB6Z3K7R9T2X5N8Q1V4W6YZA")),
            now(),
            "device_id_mismatch",
        ),
        (
            with(&|c, _| c["payload"]["v"] = json!(2)),
            now(),
            "unknown_version",
        ),
        (
            with(&|c, _| c["signing"]["alg"] = json!("ES384")),
            now(),
            "unsupported_signing",
        ),
        (
            with(&|c, _| c["signing"]["canonicalization"] = json!("JSON")),
            now(),
            "unsupported_signing",
        ),
        (
            with(&|c, _| c["signing"]["prefix"] = json!("gantry/approval/v2\n")),
            now(),
            "unsupported_signing",
        ),
        (
            with(&|c, a| {
                c["payload"]["action"] = json!("delete_branch");
                a["action"] = json!("delete_branch");
            }),
            now(),
            "unknown_action",
        ),
        (with(&|_, _| {}), expires_ms() - 9_999, "challenge_expired"),
        (with(&|_, _| {}), expires_ms() + 1, "challenge_expired"),
        (
            with(&|_, a| a["subject_digest"] = json!("0000000000000000000000000000000000000000")),
            now(),
            "approval_data_mismatch",
        ),
        (
            with(&|_, a| a["target"]["number"] = json!(13)),
            now(),
            "approval_data_mismatch",
        ),
        (
            with(&|_, a| a["action"] = json!("force_push")),
            now(),
            "approval_data_mismatch",
        ),
        (
            with(&|_, a| a["delivery_id"] = json!("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZA")),
            now(),
            "approval_data_mismatch",
        ),
        // payload 里夹带额外字段：类型化后会丢失，规范化字节不一致。
        (
            with(&|c, _| c["payload"]["extra"] = json!("x")),
            now(),
            "approval_data_mismatch",
        ),
        // target 与动作不匹配。
        (
            with(&|c, a| {
                c["payload"]["target"] = json!({"kind": "device", "id": DEV});
                a["target"] = c["payload"]["target"].clone();
            }),
            now(),
            "approval_data_mismatch",
        ),
    ];
    for (i, ((c, a), at, reason)) in cases.into_iter().enumerate() {
        let err = self_check(APR, &c, &a, DEV, at, None).unwrap_err();
        assert_eq!(err.code, ShellErrorCode::SelfCheckFailed, "case {i}");
        assert_eq!(err.reason.as_deref(), Some(reason), "case {i}");
    }
}

#[test]
fn internal_actions_require_the_shell_computed_digest() {
    let digest = format!("sha256:{}", "ab".repeat(32));
    let mut c = challenge();
    let payload = &mut c["payload"];
    payload["action"] = json!("device_revoke");
    payload["target"] = json!({"kind": "device", "id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZA"});
    payload["subject_digest"] = json!(digest);
    payload.as_object_mut().unwrap().remove("delivery_id");
    let a = json!({
        "id": APR, "action": "device_revoke", "target": c["payload"]["target"],
        "subject_digest": digest, "status": "pending"
    });
    assert!(self_check(APR, &c, &a, DEV, now(), Some(&digest)).is_ok());
    for local in [None, Some("sha256:0000")] {
        let err = self_check(APR, &c, &a, DEV, now(), local).unwrap_err();
        assert_eq!(err.reason.as_deref(), Some("internal_digest_mismatch"));
    }
}

#[test]
fn signing_is_single_flight_one_shot_and_survives_cancel() {
    let presence = PresenceDouble::new();
    let keys = keys(presence.clone());
    let signer = Signer::new();
    let checked = self_check(APR, &challenge(), &approval(), DEV, now(), None).unwrap();
    let id = signer.insert(checked, now()).unwrap();

    // 伪造的句柄。
    let err = signer
        .sign("deadbeef", keys.as_ref(), now(), ok)
        .unwrap_err();
    assert_eq!(err.code, ShellErrorCode::PreparedExpired);

    // 确认框期间再次签名 → signing_busy（在 recheck 里模拟"确认框正开着"）。
    let nested = Cell::new(None);
    presence.push(PresenceOutcome::Cancel);
    let err = signer
        .sign(&id, keys.as_ref(), now(), |_| {
            nested.set(Some(
                signer.sign(&id, keys.as_ref(), now(), ok).unwrap_err().code,
            ));
            Ok(())
        })
        .unwrap_err();
    assert_eq!(nested.get(), Some(ShellErrorCode::SigningBusy));
    // 用户取消 → user_cancelled，句柄保留。
    assert_eq!(err.code, ShellErrorCode::UserCancelled);
    assert!(signer.get(&id, now()).is_some());

    // 生物识别失败 → presence_failed，不降级。
    presence.push(PresenceOutcome::Fail);
    assert_eq!(
        signer.sign(&id, keys.as_ref(), now(), ok).unwrap_err().code,
        ShellErrorCode::PresenceFailed
    );

    // 成功后句柄作废，不能再用。
    presence.push(PresenceOutcome::Approve);
    signer.sign(&id, keys.as_ref(), now(), ok).unwrap();
    assert_eq!(
        signer.sign(&id, keys.as_ref(), now(), ok).unwrap_err().code,
        ShellErrorCode::PreparedExpired
    );
    assert!(signer.is_empty());
}

#[test]
fn recheck_failure_never_prompts() {
    let presence = PresenceDouble::with_default(PresenceOutcome::Approve);
    let keys = keys(presence.clone());
    let signer = Signer::new();
    let id = signer
        .insert(
            self_check(APR, &challenge(), &approval(), DEV, now(), None).unwrap(),
            now(),
        )
        .unwrap();
    let err = signer
        .sign(&id, keys.as_ref(), now(), |_| {
            Err(ShellError::new(ShellErrorCode::NotConnected, "连接中断"))
        })
        .unwrap_err();
    assert_eq!(err.code, ShellErrorCode::NotConnected);
    assert!(presence.prompts().is_empty(), "重检失败不得弹确认框");
}

#[test]
fn prepared_signings_expire_and_are_capped() {
    let keys = keys(PresenceDouble::with_default(PresenceOutcome::Approve));
    let signer = Signer::new();
    let check = || self_check(APR, &challenge(), &approval(), DEV, now(), None).unwrap();
    let id = signer.insert(check(), now()).unwrap();
    // challenge 过期后句柄自动清除。
    assert_eq!(
        signer
            .sign(&id, keys.as_ref(), expires_ms(), ok)
            .unwrap_err()
            .code,
        ShellErrorCode::PreparedExpired
    );

    let ids: Vec<String> = (0..MAX_PREPARED + 2)
        .map(|_| signer.insert(check(), now()).unwrap())
        .collect();
    assert_eq!(signer.len(), MAX_PREPARED);
    assert!(signer.get(&ids[0], now()).is_none(), "超出上限挤掉最早的");
    assert!(signer.get(ids.last().unwrap(), now()).is_some());
    signer.clear();
    assert!(signer.is_empty());
    // 句柄是随机的，不含语义。
    assert_eq!(ids[1].len(), 32);
    assert!(!ids[1].contains(APR));
}

#[test]
fn sheet_model_comes_from_the_same_payload() {
    let checked = self_check(APR, &challenge(), &approval(), DEV, now(), None).unwrap();
    let model = sheet_model(
        "pid",
        &checked,
        "Susan 的 MacBook",
        json!({"ok": true}),
        -120,
        json!({"note": null}),
    );
    let signed = &model["signed"];
    assert_eq!(signed["action"], "merge_pr");
    assert_eq!(signed["action_label"], "合并 PR");
    assert_eq!(signed["target_label"], "acme/web · PR #12");
    assert_eq!(signed["subject_digest"], SHA);
    assert_eq!(signed["digest_head12"], "9f2c4e7a1b3d");
    assert_eq!(signed["device_name"], "Susan 的 MacBook");
    assert_eq!(signed["tech"]["nonce"], challenge()["payload"]["nonce"]);
    assert_eq!(model["links"], json!({"commit": true, "pr": true}));
    assert_eq!(model["challenge_expires_at"], "2026-10-24T12:52:00.000Z");
    assert_eq!(model["server_time_offset_ms"], -120);
    // 提示与模型用的是同一份目标。
    assert!(presence_prompt(&checked.raw).detail[0]
        .1
        .contains("acme/web · PR #12"));
    assert_eq!(
        approval::digest_head12(&format!("sha256:{}", "cd".repeat(32))),
        "cdcdcdcdcdcd"
    );
}

#[test]
fn cross_device_digests_for_device_actions_only() {
    use gantry_core::canon::jcs_digest;
    let revoke = jcs_digest(&json!({})).unwrap().to_string();
    let a = json!({"action": "device_revoke", "subject_digest": revoke});
    assert_eq!(approval::cross_device_internal_digest(&a), Some(revoke));
    let role = jcs_digest(&json!({"role": "operator"}))
        .unwrap()
        .to_string();
    let a = json!({"action": "device_role_change", "subject_digest": role});
    assert_eq!(approval::cross_device_internal_digest(&a), Some(role));
    let a = json!({"action": "device_role_change", "subject_digest": "sha256:00"});
    assert_eq!(approval::cross_device_internal_digest(&a), None);
    // settings_update 跨设备无法核对内容：拒签（OQ-S7）。
    let a = json!({"action": "settings_update", "subject_digest": "sha256:00"});
    assert_eq!(approval::cross_device_internal_digest(&a), None);
}
