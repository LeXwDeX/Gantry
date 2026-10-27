//! P-256 公钥、指纹与证书序列号（pki.md §7.1）。

use std::fmt;
use std::str::FromStr;

use gantry_core::canon::Sha256Digest;
use gantry_core::ids::Entropy;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::rng::EntropyRng;
use crate::PkiError;

/// `id-ecPublicKey`（1.2.840.10045.2.1）。
const OID_EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
/// `prime256v1` / `secp256r1`（1.2.840.10045.3.1.7）。
const OID_PRIME256V1: &str = "1.2.840.10045.3.1.7";

/// `"sha256:" + 小写十六进制`：对 DER 编码取 SHA-256。
///
/// 用于根 CA 指纹（证书 DER）与公钥指纹（SPKI DER，OQ-K2 的临时口径）。
pub fn fingerprint(der: &[u8]) -> Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(der).into())
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("public key is not a valid SubjectPublicKeyInfo")]
    Spki,
    #[error("public key algorithm is not ECDSA P-256")]
    NotP256,
    #[error("public key point is not in uncompressed form")]
    NotUncompressed,
    #[error("public key point is not on the P-256 curve")]
    NotOnCurve,
}

/// 已校验的 P-256 公钥：曲线正确、点在曲线上、未压缩格式。
///
/// 设备的 mTLS 客户端公钥与审批签名公钥 `approval_pubkey`（P-47）都用它表示。
#[derive(Clone, PartialEq, Eq)]
pub struct P256PublicKey {
    key: p256::PublicKey,
    spki_der: Vec<u8>,
}

impl P256PublicKey {
    /// 从 SPKI DER 解析（CSR 与证书里的公钥形态）。
    pub fn from_spki_der(der: &[u8]) -> Result<Self, KeyError> {
        let spki = p256::pkcs8::spki::SubjectPublicKeyInfoRef::try_from(der)
            .map_err(|_| KeyError::Spki)?;
        if spki.algorithm.oid.to_string() != OID_EC_PUBLIC_KEY {
            return Err(KeyError::NotP256);
        }
        let curve = spki
            .algorithm
            .parameters_oid()
            .map_err(|_| KeyError::NotP256)?;
        if curve.to_string() != OID_PRIME256V1 {
            return Err(KeyError::NotP256);
        }
        let point = spki.subject_public_key.as_bytes().ok_or(KeyError::Spki)?;
        Self::from_uncompressed_point(point)
    }

    /// 从 65 字节的未压缩点（`0x04 || X || Y`）解析（`approval_pubkey` 的形态）。
    pub fn from_uncompressed_point(point: &[u8]) -> Result<Self, KeyError> {
        if point.len() != 65 || point[0] != 0x04 {
            return Err(KeyError::NotUncompressed);
        }
        let key = p256::PublicKey::from_sec1_bytes(point).map_err(|_| KeyError::NotOnCurve)?;
        let spki_der = key
            .to_public_key_der()
            .map_err(|_| KeyError::Spki)?
            .into_vec();
        Ok(Self { key, spki_der })
    }

    /// 规范化的 SPKI DER。
    pub fn spki_der(&self) -> &[u8] {
        &self.spki_der
    }

    /// 未压缩点。
    pub fn uncompressed_point(&self) -> [u8; 65] {
        let encoded = p256::EncodedPoint::from(&self.key);
        let mut out = [0u8; 65];
        out.copy_from_slice(encoded.as_bytes());
        out
    }

    /// `pubkey_fpr` / `approval_pubkey_fpr`：SPKI DER 的 SHA-256。
    pub fn fingerprint(&self) -> Sha256Digest {
        fingerprint(&self.spki_der)
    }
}

impl fmt::Debug for P256PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P256PublicKey({})", self.fingerprint())
    }
}

/// 证书序列号：小写十六进制、去前导零（§7.1）。
///
/// 本模块签发时生成 128 位随机数、最高位清零保证为正；解析时接受 RFC 5280 允许的
/// 至多 20 字节正整数（外部 CA 签发的证书也要能读出序列号再拒绝）。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CertSerial(String);

/// RFC 5280 §4.1.2.2：序列号不超过 20 个八位组。
const MAX_SERIAL_BYTES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("certificate serial must be 1–40 lowercase hex digits without leading zeros")]
pub struct SerialError;

impl CertSerial {
    /// 新的随机序列号。全库唯一性由调用方（签发事务）检查，冲突则重新生成。
    pub fn generate(entropy: &dyn Entropy) -> Self {
        loop {
            let mut bytes = [0u8; 16];
            entropy.fill(&mut bytes);
            bytes[0] &= 0x7f;
            if let Some(serial) = Self::from_be_bytes(&bytes) {
                return serial;
            }
        }
    }

    /// 从大端字节构造（证书里的 INTEGER 内容）。全零或超过 20 字节返回 `None`。
    pub fn from_be_bytes(bytes: &[u8]) -> Option<Self> {
        let start = bytes.iter().position(|b| *b != 0)?;
        let trimmed = &bytes[start..];
        if trimmed.len() > MAX_SERIAL_BYTES {
            return None;
        }
        let hex: String = trimmed.iter().map(|b| format!("{b:02x}")).collect();
        Some(Self(hex.trim_start_matches('0').to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// DER INTEGER 内容：最短的正数大端编码。
    pub fn to_be_bytes(&self) -> Vec<u8> {
        let padded = if self.0.len() % 2 == 1 {
            format!("0{}", self.0)
        } else {
            self.0.clone()
        };
        let mut out: Vec<u8> = padded
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap_or("00");
                u8::from_str_radix(text, 16).unwrap_or(0)
            })
            .collect();
        if out.first().is_some_and(|b| b & 0x80 != 0) {
            out.insert(0, 0);
        }
        out
    }
}

impl FromStr for CertSerial {
    type Err = SerialError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = !s.is_empty()
            && s.len() <= MAX_SERIAL_BYTES * 2
            && !s.starts_with('0')
            && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(SerialError)
        }
    }
}

impl fmt::Display for CertSerial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for CertSerial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CertSerial({})", self.0)
    }
}

/// 新生成的 P-256 私钥。`Debug` 不输出密钥；PKCS#8 文本用 `Zeroizing` 持有。
pub(crate) struct GeneratedKey {
    pub(crate) secret: p256::SecretKey,
    pub(crate) pkcs8_pem: Zeroizing<String>,
    pub(crate) key_pair: rcgen::KeyPair,
}

pub(crate) fn generate_key(entropy: &dyn Entropy) -> Result<GeneratedKey, PkiError> {
    let secret = p256::SecretKey::random(&mut EntropyRng(entropy));
    let pkcs8_pem = secret
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|error| PkiError::Crypto(format!("encode PKCS#8: {error}")))?;
    let key_pair = load_key_pair(&pkcs8_pem)?;
    Ok(GeneratedKey {
        secret,
        pkcs8_pem,
        key_pair,
    })
}

/// 从 PKCS#8 PEM 载入签名密钥，只接受 P-256。
pub(crate) fn load_key_pair(pkcs8_pem: &str) -> Result<rcgen::KeyPair, PkiError> {
    let key_pair =
        rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(pkcs8_pem, &rcgen::PKCS_ECDSA_P256_SHA256)?;
    Ok(key_pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_round_trips_and_is_positive() {
        let serial = CertSerial::from_be_bytes(&[0x00, 0x00, 0x8f, 0x01]).unwrap();
        assert_eq!(serial.as_str(), "8f01");
        assert_eq!(serial.to_be_bytes(), vec![0x00, 0x8f, 0x01]);
        let odd = CertSerial::from_be_bytes(&[0x0a, 0xbc]).unwrap();
        assert_eq!(odd.as_str(), "abc");
        assert_eq!(odd.to_be_bytes(), vec![0x0a, 0xbc]);
        assert!(CertSerial::from_be_bytes(&[0, 0]).is_none());
        assert_eq!("abc".parse::<CertSerial>().unwrap(), odd);
        assert!(CertSerial::from_be_bytes(&[0x7f; 20]).is_some());
        assert!(CertSerial::from_be_bytes(&[0x7f; 21]).is_none());
        for bad in ["", "0abc", "ABC", "xyz", &"f".repeat(41)] {
            assert!(bad.parse::<CertSerial>().is_err(), "{bad}");
        }
    }

    #[test]
    fn generated_serial_is_at_most_127_bits() {
        let entropy = gantry_core::testing::SeqEntropy::new(7);
        for _ in 0..64 {
            let serial = CertSerial::generate(&entropy);
            let bytes = serial.to_be_bytes();
            assert!(bytes.len() <= 16);
            assert_eq!(bytes[0] & 0x80, 0);
        }
    }

    #[test]
    fn rejects_compressed_and_off_curve_points() {
        let entropy = gantry_core::testing::SeqEntropy::new(1);
        let key = generate_key(&entropy).unwrap();
        let public = key.secret.public_key();
        let compressed = p256::EncodedPoint::from_affine_coordinates(
            &p256::elliptic_curve::sec1::ToEncodedPoint::to_encoded_point(&public, false)
                .x()
                .copied()
                .unwrap(),
            &p256::elliptic_curve::sec1::ToEncodedPoint::to_encoded_point(&public, false)
                .y()
                .copied()
                .unwrap(),
            true,
        );
        assert_eq!(
            P256PublicKey::from_uncompressed_point(compressed.as_bytes()),
            Err(KeyError::NotUncompressed)
        );
        let mut off_curve = [0x04u8; 65];
        off_curve[1..].fill(0x11);
        assert_eq!(
            P256PublicKey::from_uncompressed_point(&off_curve),
            Err(KeyError::NotOnCurve)
        );
        let good =
            P256PublicKey::from_uncompressed_point(p256::EncodedPoint::from(&public).as_bytes())
                .unwrap();
        let reparsed = P256PublicKey::from_spki_der(good.spki_der()).unwrap();
        assert_eq!(reparsed, good);
    }
}
