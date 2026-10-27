//! 软件密钥替身的行为（desktop-shell.md §12"密钥与身份"、"生物识别路径"的 A 级部分）。
#![cfg(feature = "test-software-keys")]

use gantry_desktop_shell::keys::software::{PresenceDouble, PresenceOutcome, TestSoftwareKeyStore};
use gantry_desktop_shell::keys::{KeyBackend, KeyError, KeyStore, PresencePrompt};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{DerSignature, Signature, VerifyingKey};

fn prompt() -> PresencePrompt {
    PresencePrompt {
        title: "批准合并".into(),
        summary: "merge acme/web#12 @ 9f2c4e7a1b3d".into(),
        detail: vec![("subject_digest".into(), "9f2c4e7a1b3d…".into())],
    }
}

#[test]
fn two_distinct_keys_and_tls_signs_without_presence() {
    let store = TestSoftwareKeyStore::new(PresenceDouble::new());
    let keys = store.generate().unwrap();
    assert_eq!(keys.backend(), KeyBackend::TestSoftware);
    assert_ne!(keys.tls_public_key(), keys.approval_public_key());
    assert_ne!(
        keys.tls_public_key().fingerprint(),
        keys.approval_public_key().fingerprint()
    );

    let der = keys.sign_tls(b"handshake").unwrap();
    let vk = VerifyingKey::from_sec1_bytes(keys.tls_public_key().as_bytes()).unwrap();
    vk.verify(b"handshake", &DerSignature::from_bytes(&der).unwrap())
        .unwrap();
    // mTLS 签名不经过用户在场。
    assert!(store.presence().prompts().is_empty());
}

#[test]
fn approval_needs_presence_and_never_degrades() {
    let presence = PresenceDouble::new();
    let store = TestSoftwareKeyStore::new(presence.clone());
    let keys = store.generate().unwrap();
    let msg = b"gantry/approval/v1\n{}";

    // 默认不通过：不签名。
    assert!(matches!(
        keys.sign_approval(msg, &prompt()),
        Err(KeyError::PresenceFailed(_))
    ));
    presence.push(PresenceOutcome::Cancel);
    assert_eq!(
        keys.sign_approval(msg, &prompt()),
        Err(KeyError::UserCancelled)
    );

    presence.push(PresenceOutcome::Approve);
    let sig = keys.sign_approval(msg, &prompt()).unwrap();
    assert_eq!(sig.to_base64url().len(), 86);
    let vk = VerifyingKey::from_sec1_bytes(keys.approval_public_key().as_bytes()).unwrap();
    vk.verify(msg, &Signature::from_slice(sig.as_bytes()).unwrap())
        .unwrap();
    // 审批签名不能用 mTLS 公钥验过（P-47 两把钥分开）。
    let tls_vk = VerifyingKey::from_sec1_bytes(keys.tls_public_key().as_bytes()).unwrap();
    assert!(tls_vk
        .verify(msg, &Signature::from_slice(sig.as_bytes()).unwrap())
        .is_err());

    // 每次签名都弹一次提示，文案含 subject_digest 前 12 位（C27）。
    let prompts = presence.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(prompts
        .iter()
        .all(|p| p.system_reason().contains("9f2c4e7a1b3d")));

    // 脚本耗尽后回到默认（不通过）。
    assert!(keys.sign_approval(msg, &prompt()).is_err());
}

#[test]
fn hardware_unavailable_fails_closed() {
    let store = TestSoftwareKeyStore::new(PresenceDouble::new());
    store.set_hardware_available(false);
    assert!(matches!(
        store.generate().err(),
        Some(KeyError::HardwareUnavailable(_))
    ));
    assert_eq!(store.key_count(), 0);
}

#[test]
fn reload_delete_and_wipe() {
    let store = TestSoftwareKeyStore::new(PresenceDouble::with_default(PresenceOutcome::Approve));
    let keys = store.generate().unwrap();
    let labels = keys.labels().clone();

    // "重启"后按引用名恢复同一对公钥。
    let reloaded = store.load(&labels).unwrap();
    assert_eq!(reloaded.tls_public_key(), keys.tls_public_key());
    assert_eq!(reloaded.approval_public_key(), keys.approval_public_key());

    // 模拟 TPM 清除：加载与旧句柄签名都报 key_unavailable。
    store.wipe();
    assert_eq!(store.load(&labels).err(), Some(KeyError::KeyUnavailable));
    assert_eq!(keys.sign_tls(b"x"), Err(KeyError::KeyUnavailable));
    assert_eq!(
        keys.sign_approval(b"x", &prompt()),
        Err(KeyError::KeyUnavailable)
    );

    // 删除幂等。
    let keys = store.generate().unwrap();
    assert_eq!(store.key_count(), 2);
    store.delete(keys.labels()).unwrap();
    store.delete(keys.labels()).unwrap();
    assert_eq!(store.key_count(), 0);
}

#[test]
fn tampered_labels_are_rejected() {
    let store = TestSoftwareKeyStore::new(PresenceDouble::new());
    let keys = store.generate().unwrap();
    let mut labels = keys.labels().clone();
    labels.approval = labels.tls.clone();
    assert_eq!(store.load(&labels).err(), Some(KeyError::KeyUnavailable));
}
