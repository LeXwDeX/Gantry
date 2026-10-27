//! 签名原语：验签与待签载荷构造（policy.md §6.7；api.md §6.2–§6.3、§6.6）。
//!
//! 待签字节与 JCS 的唯一构造点在 `core`（`ApprovalPayload::signing_bytes`、
//! `jcs_digest`）；本模块只做验证与载荷组装，不自己序列化。验签只用设备配对时登记的
//! 审批签名公钥 `approval_pubkey`（P-47），与 mTLS 客户端证书的密钥无关。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::canon::{jcs_digest, ApprovalPayload, Nonce, SubjectDigest};
use gantry_core::entities::{Approval, ApprovalAction};
use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use p256::ecdsa::signature::Verifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::Value;

/// `ApprovalPayload.v` 的当前版本。
pub const PAYLOAD_VERSION: u32 = 1;
/// ES256 原始 `r ‖ s` 的字节数与其 base64url（无填充）长度。
const SIGNATURE_BYTES: usize = 64;
const SIGNATURE_B64_LEN: usize = 86;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SigningError {
    /// 签名串格式不对或验签不通过。两者对外不区分，避免给出验签细节。
    #[error("signature invalid")]
    SignatureInvalid,
    #[error("approval public key invalid")]
    PublicKeyInvalid,
    #[error("payload cannot be canonicalized: {0}")]
    Canonical(String),
}

/// 解析审批签名公钥：P-256 未压缩点的 base64url（无填充），即 `Device.approval_pubkey`。
pub fn parse_approval_pubkey(b64: &str) -> Result<VerifyingKey, SigningError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(b64)
        .map_err(|_| SigningError::PublicKeyInvalid)?;
    // 只接受未压缩点（0x04 ‖ X ‖ Y）。
    if bytes.len() != 65 || bytes[0] != 0x04 {
        return Err(SigningError::PublicKeyInvalid);
    }
    VerifyingKey::from_sec1_bytes(&bytes).map_err(|_| SigningError::PublicKeyInvalid)
}

/// 严格校验签名串并 ES256 验签：base64url、无填充、长度 86、解码后恰为 64 字节
/// （`r ‖ s`，非 DER）；其余一律 `SignatureInvalid`。
pub fn verify_es256(key: &VerifyingKey, input: &[u8], sig_b64: &str) -> Result<(), SigningError> {
    if sig_b64.len() != SIGNATURE_B64_LEN
        || !sig_b64
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(SigningError::SignatureInvalid);
    }
    let raw = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| SigningError::SignatureInvalid)?;
    if raw.len() != SIGNATURE_BYTES {
        return Err(SigningError::SignatureInvalid);
    }
    let signature = Signature::from_slice(&raw).map_err(|_| SigningError::SignatureInvalid)?;
    key.verify(input, &signature)
        .map_err(|_| SigningError::SignatureInvalid)
}

/// 由已存的审批、设备与 nonce 构造待签载荷（`v = 1`）。
///
/// `delivery_id` 取审批自身的值；内部动作（`settings_update`、`device_*`）没有交付单元，
/// 一律省略。
pub fn build_payload(
    approval: &Approval,
    device: &DeviceId,
    nonce: Nonce,
    challenge_expires_at: Timestamp,
) -> ApprovalPayload {
    let internal = matches!(
        approval.action,
        ApprovalAction::SettingsUpdate
            | ApprovalAction::DeviceRoleChange
            | ApprovalAction::DeviceRevoke
    );
    ApprovalPayload {
        action: approval.action,
        approval_id: approval.id,
        delivery_id: if internal { None } else { approval.delivery_id },
        device_id: *device,
        expires_at: challenge_expires_at,
        nonce,
        subject_digest: approval.subject_digest,
        target: approval.target.clone(),
        v: PAYLOAD_VERSION,
    }
}

/// 内部动作的请求体摘要：`"sha256:" + hex(sha256(JCS(body)))`（api.md §6.6）。
/// 无请求体时调用方传 `{}`。
pub fn request_digest(body: &Value) -> Result<SubjectDigest, SigningError> {
    jcs_digest(body)
        .map(SubjectDigest::sha256)
        .map_err(|error| SigningError::Canonical(error.to_string()))
}
