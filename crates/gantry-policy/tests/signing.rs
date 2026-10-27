//! policy.md §12.2「签名原语与规范化（M1）」：api.md §6.3 测试向量与严格验签。

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use gantry_core::canon::{jcs, sha256, ApprovalPayload, Nonce, SubjectDigest};
use gantry_core::entities::{Approval, ApprovalAction, ApprovalStatus, ApprovalTarget};
use gantry_core::ids::{Apr, Att, Id};
use gantry_core::time::Timestamp;
use gantry_policy::signing::{
    build_payload, parse_approval_pubkey, request_digest, verify_es256, SigningError,
};
use serde_json::{json, Value};

const PUBKEY: &str =
    "BHL4aSHzHatNyrq8TX4bJNAxH7no8LM4qb5TVlHTT7ZaASo8h6hMNHwYgitHzpH-S9kLN8ImOTHNk0q-vuB-sTk";
const CANONICAL: &str = r#"{"action":"merge_pr","approval_id":"apr_01JB7P4M8T2X6N9R3Q5K1V7WZK","delivery_id":"dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB","device_id":"dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM","expires_at":"2026-10-24T12:52:00.000Z","nonce":"lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js","subject_digest":"9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345","target":{"kind":"pr","number":12,"repo":"acme/web"},"v":1}"#;
const DIGEST: &str = "be72c31cfcdc6b5eed579c8d77def70ea64afa994f89205904cd05ba07fd1d7a";
const SIGNATURE: &str =
    "QAzQTiBfxzxWOP2G_q8M4i405Cr8GOZvAFZlbZGCC96ICBp04L9nG2vAWEbWF-jQAkPYVrwZjbBeJGf8nUdDLg";
const PREFIX: &[u8] = b"gantry/approval/v1\n";

fn vector_payload() -> ApprovalPayload {
    serde_json::from_str(CANONICAL).expect("vector payload parses")
}

fn signing_bytes() -> Vec<u8> {
    vector_payload().signing_bytes().expect("signing bytes")
}

#[test]
fn api_vector_bytes_digest_and_signature() {
    let bytes = signing_bytes();
    assert_eq!(&bytes[..PREFIX.len()], PREFIX);
    assert_eq!(&bytes[PREFIX.len()..], CANONICAL.as_bytes());
    assert_eq!(sha256(&bytes).to_hex(), DIGEST);
    let key = parse_approval_pubkey(PUBKEY).expect("vector key");
    assert_eq!(verify_es256(&key, &bytes, SIGNATURE), Ok(()));
}

#[test]
fn key_order_does_not_change_canonical_bytes() {
    let shuffled = r#"{"v":1,"target":{"repo":"acme/web","number":12,"kind":"pr"},"subject_digest":"9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345","nonce":"lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js","expires_at":"2026-10-24T12:52:00.000Z","device_id":"dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM","delivery_id":"dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB","approval_id":"apr_01JB7P4M8T2X6N9R3Q5K1V7WZK","action":"merge_pr"}"#;
    let payload: ApprovalPayload = serde_json::from_str(shuffled).expect("shuffled parses");
    assert_eq!(payload.signing_bytes().expect("bytes"), signing_bytes());
}

#[test]
fn jcs_keeps_utf8_and_escapes_control_characters() {
    let bytes = jcs(&json!({"b": "中文é", "a": "tab\there\u{1}"})).expect("jcs");
    assert_eq!(
        String::from_utf8(bytes).expect("utf8"),
        "{\"a\":\"tab\\there\\u0001\",\"b\":\"中文é\"}"
    );
}

/// 待签载荷由服务端组装、从不解析客户端提交的版本；时间的严格形（三位毫秒、`Z`）
/// 由 core 的 `Timestamp::parse_canonical` 保证，输出恒为规范形。
#[test]
fn malformed_payload_fields_are_rejected() {
    for bad in [
        "2026-10-24T12:52:00Z",
        "2026-10-24T12:52:00.00Z",
        "2026-10-24T12:52:00.0000Z",
        "2026-10-24T12:52:00.000+00:00",
    ] {
        assert!(Timestamp::parse_canonical(bad).is_err(), "{bad}");
    }
    let canonical = Timestamp::parse_canonical("2026-10-24T12:52:00.000Z").expect("canonical");
    assert_eq!(canonical.to_canonical_string(), "2026-10-24T12:52:00.000Z");
    assert_eq!(vector_payload().expires_at, canonical);

    let mut value: Value = serde_json::from_str(CANONICAL).expect("vector");
    value["v"] = json!(1.0);
    assert!(serde_json::from_value::<ApprovalPayload>(value).is_err());
    let mut value: Value = serde_json::from_str(CANONICAL).expect("vector");
    value["target"]["number"] = json!(12.5);
    assert!(serde_json::from_value::<ApprovalPayload>(value).is_err());
    let mut value: Value = serde_json::from_str(CANONICAL).expect("vector");
    value["nonce"] = Value::Null;
    assert!(serde_json::from_value::<ApprovalPayload>(value).is_err());
}

#[test]
fn tampered_payload_or_malformed_signatures_are_rejected() {
    let key = parse_approval_pubkey(PUBKEY).expect("key");
    let mut bytes = signing_bytes();
    let last = bytes.len() - 2;
    bytes[last] ^= 0x01;
    assert_eq!(
        verify_es256(&key, &bytes, SIGNATURE),
        Err(SigningError::SignatureInvalid)
    );

    let good = signing_bytes();
    let raw = URL_SAFE_NO_PAD.decode(SIGNATURE).expect("raw");
    let padded = URL_SAFE.encode(&raw);
    assert!(padded.ends_with('='));
    let short = &SIGNATURE[..85];
    let plus = SIGNATURE.replacen('-', "+", 1);
    let long = URL_SAFE_NO_PAD.encode([raw.as_slice(), &[0u8]].concat());
    let der = URL_SAFE_NO_PAD.encode(
        p256::ecdsa::Signature::from_slice(&raw)
            .expect("sig")
            .to_der()
            .as_bytes(),
    );
    for bad in [
        padded.as_str(),
        short,
        plus.as_str(),
        long.as_str(),
        der.as_str(),
        "",
    ] {
        assert_eq!(
            verify_es256(&key, &good, bad),
            Err(SigningError::SignatureInvalid),
            "{bad}"
        );
    }
}

#[test]
fn pubkey_must_be_an_uncompressed_point() {
    assert_eq!(
        parse_approval_pubkey("AAAA").err(),
        Some(SigningError::PublicKeyInvalid)
    );
    let raw = URL_SAFE_NO_PAD.decode(PUBKEY).expect("raw");
    let compressed = p256::ecdsa::VerifyingKey::from_sec1_bytes(&raw)
        .expect("key")
        .to_encoded_point(true);
    assert_eq!(
        parse_approval_pubkey(&URL_SAFE_NO_PAD.encode(compressed.as_bytes())).err(),
        Some(SigningError::PublicKeyInvalid)
    );
}

#[test]
fn patch_target_string_arrays_canonicalize() {
    let approval = Approval {
        id: Id::<Apr>::from_parts(1_700_000_000_000, 1).expect("id"),
        delivery_id: Some(Id::from_parts(1_700_000_000_000, 2).expect("id")),
        action: ApprovalAction::PatchApplyProtected,
        target: ApprovalTarget::Patch {
            repo: gantry_core::canon::RepoSlug::parse("acme/web").expect("repo"),
            attempt_id: Id::<Att>::from_parts(1_700_000_000_000, 3).expect("id"),
            rules: vec!["lockfiles".into(), "dependency_manifests".into()],
            paths: vec!["Cargo.lock".into()],
        },
        subject_digest: SubjectDigest::sha256(sha256(b"patch")),
        device_id: None,
        signature: None,
        status: ApprovalStatus::Pending,
        expires_at: Timestamp::from_millis(1_700_000_100_000),
        consumed_by: None,
        created_at: Timestamp::from_millis(1_700_000_000_000),
        updated_at: Timestamp::from_millis(1_700_000_000_000),
    };
    let nonce = Nonce::parse("lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js").expect("nonce");
    let device = Id::from_parts(1_700_000_000_000, 4).expect("id");
    let payload = build_payload(&approval, &device, nonce, approval.expires_at);
    assert_eq!(payload.v, 1);
    assert_eq!(payload.delivery_id, approval.delivery_id);
    let bytes = payload.signing_bytes().expect("arrays canonicalize");
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(text.contains(r#""rules":["lockfiles","dependency_manifests"]"#));
}

#[test]
fn request_digest_is_canonical_and_defaults_to_empty_object() {
    let a: Value =
        serde_json::from_str(r#"{ "b": 1,   "a": {"y": "z", "x": [1, 2]} }"#).expect("a");
    let b: Value = serde_json::from_str(r#"{"a":{"x":[1,2],"y":"z"},"b":1}"#).expect("b");
    assert_eq!(request_digest(&a), request_digest(&b));
    let empty = request_digest(&json!({})).expect("empty");
    assert_eq!(empty, SubjectDigest::sha256(sha256(b"{}")));
    // 带 secret value 的请求体按原样计算（不剔除字段）。
    let secret = json!({"value": "s3cr3t"});
    assert_ne!(request_digest(&secret).expect("secret"), empty);
}
