//! 不可信输入的校验：CSR（pki.md §7.4.4）与审批签名公钥 `approval_pubkey`（§7.4.3 第 3 步，P-47）。
//!
//! 只产出校验过的公钥；CSR 里的 subject、扩展、属性一律不读（签发参数由 [`crate::ca`] 构造）。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::prelude::FromDer;

use crate::keys::{KeyError, P256PublicKey};

/// `ecdsa-with-SHA256`（1.2.840.10045.4.3.2）。
const OID_ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";

const CSR_TAG: &str = "CERTIFICATE REQUEST";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CsrError {
    #[error("CSR exceeds {limit} bytes")]
    TooLarge { limit: usize },
    #[error("CSR must be exactly one PEM CERTIFICATE REQUEST block")]
    Pem,
    #[error("CSR is not valid DER")]
    Der,
    #[error("CSR signature algorithm is not ecdsa-with-SHA256")]
    SignatureAlgorithm,
    #[error("CSR public key is invalid: {0}")]
    PublicKey(KeyError),
    #[error("CSR signature does not verify")]
    Signature,
}

/// 校验 CSR 并取出其中的公钥。
///
/// 规则（§7.4.4）：恰好一个 PEM `CERTIFICATE REQUEST` 块、不超过 `max_bytes`、无尾随垃圾；
/// 公钥是 ECDSA P-256 未压缩点且在曲线上；签名算法 `ecdsa-with-SHA256` 且签名有效（持有私钥证明）。
pub fn validate_csr(csr_pem: &str, max_bytes: usize) -> Result<P256PublicKey, CsrError> {
    if csr_pem.len() > max_bytes {
        return Err(CsrError::TooLarge { limit: max_bytes });
    }
    let der = crate::ca::pem_decode_single(CSR_TAG, csr_pem).map_err(|_| CsrError::Pem)?;
    let (rest, csr) = X509CertificationRequest::from_der(&der).map_err(|_| CsrError::Der)?;
    if !rest.is_empty() {
        return Err(CsrError::Der);
    }
    let key = P256PublicKey::from_spki_der(csr.certification_request_info.subject_pki.raw)
        .map_err(CsrError::PublicKey)?;
    if csr.signature_algorithm.algorithm.to_id_string() != OID_ECDSA_SHA256 {
        return Err(CsrError::SignatureAlgorithm);
    }
    csr.verify_signature().map_err(|_| CsrError::Signature)?;
    Ok(key)
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalKeyError {
    #[error("approval_pubkey is not unpadded base64url")]
    Encoding,
    #[error("approval_pubkey is invalid: {0}")]
    Key(KeyError),
}

/// 解析 `approval_pubkey`：无填充 base64url 编码的 65 字节 P-256 未压缩点，点必须在曲线上。
///
/// "与 CSR 公钥不同""与在册设备不重复"属于跨字段 / 跨记录的检查，由配对流程做。
pub fn parse_approval_pubkey(value: &str) -> Result<P256PublicKey, ApprovalKeyError> {
    // base64 引擎默认拒绝填充与非规范的尾随位；这里再显式拒绝 `=`，避免依赖默认值。
    if value.contains('=') {
        return Err(ApprovalKeyError::Encoding);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ApprovalKeyError::Encoding)?;
    P256PublicKey::from_uncompressed_point(&bytes).map_err(ApprovalKeyError::Key)
}

/// `approval_pubkey` 的规范编码（响应与事件里用）。
pub fn encode_approval_pubkey(key: &P256PublicKey) -> String {
    URL_SAFE_NO_PAD.encode(key.uncompressed_point())
}
