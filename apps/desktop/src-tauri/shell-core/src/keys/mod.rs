//! 设备的两把密钥（P-47，desktop-shell.md §1 第 1 条、§6.5、§8 S-1 / S-1a）。
//!
//! - mTLS 客户端密钥：后台连接无提示使用，签名返回 DER（供 rustls `SigningKey`）。
//! - 审批签名密钥：单独生成，密钥级访问控制要求用户在场；签名返回 IEEE P1363 `r ‖ s`。
//!
//! 两把私钥都不可导出，本模块没有任何导出私钥的接口。平台后端：
//! [`SecureEnclaveKeyStore`]（macOS）；Windows TPM 尚未实现，与其他平台一样 fail-closed。
//! 软件替身 [`software::TestSoftwareKeyStore`] 只在 `test-software-keys` feature 下存在，
//! [`platform_key_store`] 永远不会返回它（不降级为软件密钥，OQ-S2）。

mod der;
#[cfg(target_os = "macos")]
mod secure_enclave;
#[cfg(feature = "test-software-keys")]
pub mod software;

use std::fmt;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

pub use der::{der_to_p1363, DerError};
#[cfg(target_os = "macos")]
pub use secure_enclave::SecureEnclaveKeyStore;

/// P-256 公钥，未压缩点（`0x04 ‖ X ‖ Y`，65 字节）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct P256PublicKey([u8; 65]);

/// P-256 SubjectPublicKeyInfo 的固定 DER 前缀（`id-ecPublicKey` + `prime256v1` + BIT STRING 头）。
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

impl P256PublicKey {
    /// 只检查形状（长度与 `0x04` 前缀）；点是否在曲线上由验签方负责。
    pub fn from_uncompressed(bytes: &[u8]) -> Result<Self, KeyError> {
        let point: [u8; 65] = bytes
            .try_into()
            .map_err(|_| KeyError::Backend(format!("公钥长度 {} 不是 65", bytes.len())))?;
        if point[0] != 0x04 {
            return Err(KeyError::Backend("公钥不是未压缩点".into()));
        }
        Ok(Self(point))
    }

    pub fn as_bytes(&self) -> &[u8; 65] {
        &self.0
    }

    /// `approval_pubkey` 的线上形态：未压缩点的 base64url（无填充，api.md §1.3）。
    pub fn to_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn spki_der(&self) -> Vec<u8> {
        let mut der = Vec::with_capacity(91);
        der.extend_from_slice(&P256_SPKI_PREFIX);
        der.extend_from_slice(&self.0);
        der
    }

    /// `pubkey_fpr` / `approval_pubkey_fpr`：`sha256:` + SPKI DER 的 SHA-256 小写十六进制（pki.md §7.1）。
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.spki_der());
        let mut out = String::with_capacity(71);
        out.push_str("sha256:");
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

impl fmt::Debug for P256PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P256PublicKey({})", self.fingerprint())
    }
}

/// 审批签名：IEEE P1363 `r ‖ s`，各 32 字节大端（api.md §6.3）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct P1363Sig([u8; 64]);

impl P1363Sig {
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    /// `grant` 请求体里的 `signature`：base64url 无填充，86 字符。
    pub fn to_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

impl fmt::Debug for P1363Sig {
    // 签名值不进日志（desktop-shell.md §11）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("P1363Sig(..)")
    }
}

/// 系统级用户在场提示（Touch ID / Windows Hello）。文案由外壳从 payload 生成，不来自 UI（S-4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresencePrompt {
    pub title: String,
    /// 如 `merge acme/web#12 @ 9f2c4e7a1b3d`（`subject_digest` 前 12 位，C27）。
    pub summary: String,
    pub detail: Vec<(String, String)>,
}

impl PresencePrompt {
    /// 交给系统对话框的一行文案。macOS `LAContext.localizedReason` 只显示一行（OQ-S10）。
    pub fn system_reason(&self) -> String {
        if self.title.is_empty() {
            self.summary.clone()
        } else {
            format!("{}：{}", self.title, self.summary)
        }
    }
}

/// `identity_status.key_backend`（desktop-shell.md §6.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyBackend {
    SecureEnclave,
    Tpm,
    TestSoftware,
}

impl KeyBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SecureEnclave => "secure_enclave",
            Self::Tpm => "tpm",
            Self::TestSoftware => "test_software",
        }
    }
}

/// 两把密钥在后端里的引用名，存进 `identity.json` 的 `tls_key_label` / `approval_key_label`（§7.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyLabels {
    pub tls: String,
    pub approval: String,
}

impl KeyLabels {
    /// 随机生成一对引用名。只含 `[a-z0-9-]`，可直接作文件名。
    pub fn random() -> Result<Self, KeyError> {
        Ok(Self {
            tls: format!("tls-{}", random_hex()?),
            approval: format!("approval-{}", random_hex()?),
        })
    }

    /// 来自 `identity.json` 的引用名不可信（§4），只接受 [`KeyLabels::random`] 生成的形态。
    pub fn validate(&self) -> Result<(), KeyError> {
        let ok = |label: &str, prefix: &str| {
            label.strip_prefix(prefix).is_some_and(|hex| {
                hex.len() == 32
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        };
        if ok(&self.tls, "tls-") && ok(&self.approval, "approval-") {
            Ok(())
        } else {
            Err(KeyError::KeyUnavailable)
        }
    }
}

fn random_hex() -> Result<String, KeyError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| KeyError::Backend(format!("随机数：{e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    /// 本机没有可用的 Secure Enclave / TPM；不降级为软件密钥（OQ-S2）。
    #[error("硬件密钥不可用：{0}")]
    HardwareUnavailable(String),
    /// 密钥丢失（硬件被重置、TPM 清除、包装块被删）→ `shell.key_unavailable`。
    #[error("密钥不可用")]
    KeyUnavailable,
    /// 用户在系统确认框中取消 → `shell.user_cancelled`。
    #[error("用户取消")]
    UserCancelled,
    /// 生物识别失败或不可用 → `shell.presence_failed`；不降级为无提示签名。
    #[error("用户在场验证失败：{0}")]
    PresenceFailed(String),
    #[error("密钥后端错误：{0}")]
    Backend(String),
}

/// 一台设备的两把密钥（desktop-shell.md §6.5）。
pub trait DeviceKeys: Send + Sync {
    fn backend(&self) -> KeyBackend;
    fn labels(&self) -> &KeyLabels;
    /// mTLS 客户端密钥：用于 CSR、`pubkey_fpr` 与握手。
    fn tls_public_key(&self) -> P256PublicKey;
    /// 无用户提示；返回 DER 编码的 ECDSA-SHA256 签名，供 rustls `SigningKey`。
    fn sign_tls(&self, msg: &[u8]) -> Result<Vec<u8>, KeyError>;
    /// 审批签名公钥，即配对时提交的 `approval_pubkey`。
    fn approval_public_key(&self) -> P256PublicKey;
    /// 弹出系统用户在场验证；未通过则失败，不存在无提示路径。对 `msg` 做 ECDSA-SHA256。
    fn sign_approval(&self, msg: &[u8], prompt: &PresencePrompt) -> Result<P1363Sig, KeyError>;
}

/// 密钥的生成、按引用名加载与删除。
pub trait KeyStore: Send + Sync {
    fn backend(&self) -> KeyBackend;
    /// 生成两把新密钥（P-47）。失败时不留下半成品。
    fn generate(&self) -> Result<Box<dyn DeviceKeys>, KeyError>;
    /// 任一把找不到 → [`KeyError::KeyUnavailable`]。
    fn load(&self, labels: &KeyLabels) -> Result<Box<dyn DeviceKeys>, KeyError>;
    /// 删除两把密钥；不存在视为成功（幂等）。
    fn delete(&self, labels: &KeyLabels) -> Result<(), KeyError>;
}

/// 本平台的硬件密钥库。`key_dir` 是应用数据目录下仅当前用户可访问的目录。
///
/// 没有硬件后端的平台返回 [`KeyError::HardwareUnavailable`]，绝不回落到软件密钥（OQ-S2）。
pub fn platform_key_store(key_dir: &Path) -> Result<Box<dyn KeyStore>, KeyError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(SecureEnclaveKeyStore::open(key_dir)?))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = key_dir;
        // Windows TPM（CNG + Windows Hello）尚未实现（#102）。
        Err(KeyError::HardwareUnavailable(
            "本平台尚无硬件密钥后端".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spki_prefix_matches_p256_crate() {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let secret = p256::SecretKey::from_slice(&[7u8; 32]).unwrap();
        let point = secret.public_key().to_encoded_point(false);
        let key = P256PublicKey::from_uncompressed(point.as_bytes()).unwrap();
        // p256 不开 pkcs8 feature，手写对照：SPKI = 前缀 ‖ 点，总长 91。
        let der = key.spki_der();
        assert_eq!(der.len(), 91);
        assert_eq!(&der[26..], point.as_bytes());
        assert!(key.fingerprint().starts_with("sha256:"));
        assert_eq!(key.fingerprint().len(), 71);
        assert_eq!(key.to_base64url().len(), 87);
    }

    #[test]
    fn public_key_shape_is_checked() {
        assert!(P256PublicKey::from_uncompressed(&[4u8; 64]).is_err());
        let mut compressed = [0u8; 65];
        compressed[0] = 0x02;
        assert!(P256PublicKey::from_uncompressed(&compressed).is_err());
    }

    #[test]
    fn labels_are_random_and_validated() {
        let a = KeyLabels::random().unwrap();
        let b = KeyLabels::random().unwrap();
        assert_ne!(a, b);
        a.validate().unwrap();
        for bad in [
            "tls-../../etc",
            "tls-",
            "TLS-00000000000000000000000000000000",
        ] {
            let labels = KeyLabels {
                tls: bad.into(),
                approval: a.approval.clone(),
            };
            assert_eq!(labels.validate(), Err(KeyError::KeyUnavailable), "{bad}");
        }
    }

    #[test]
    fn signature_debug_hides_value() {
        assert_eq!(
            format!("{:?}", P1363Sig::from_bytes([1; 64])),
            "P1363Sig(..)"
        );
        assert_eq!(P1363Sig::from_bytes([1; 64]).to_base64url().len(), 86);
    }

    #[test]
    fn prompt_reason_is_one_line() {
        let prompt = PresencePrompt {
            title: "批准合并".into(),
            summary: "merge acme/web#12 @ 9f2c4e7a1b3d".into(),
            detail: vec![],
        };
        assert_eq!(
            prompt.system_reason(),
            "批准合并：merge acme/web#12 @ 9f2c4e7a1b3d"
        );
    }
}
