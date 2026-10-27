//! 信任层次与证书参数（pki.md §7.1）。
//!
//! 离线根 CA → 在线中间 CA → 服务端叶子 / 设备叶子。所有证书都是 ECDSA P-256 +
//! `ecdsa-with-SHA256`；叶子参数完全由服务端构造，**不从 CSR 复制任何字段**（§7.4.4）。

use std::net::IpAddr;
use std::time::Duration;

use gantry_core::canon::Sha256Digest;
use gantry_core::ids::{DeviceId, Entropy};
use gantry_core::time::Timestamp;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, X509Certificate};
use zeroize::Zeroizing;

use crate::config::CertPolicy;
use crate::keys::{fingerprint, generate_key, load_key_pair, CertSerial, P256PublicKey};
use crate::PkiError;

/// 一张签发好的证书。不含私钥。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedCert {
    pub serial: CertSerial,
    pub der: Vec<u8>,
    pub pem: String,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
}

impl IssuedCert {
    /// 证书 DER 的 SHA-256；对根证书即 `ca_fingerprint`（P-64）。
    pub fn fingerprint(&self) -> Sha256Digest {
        fingerprint(&self.der)
    }

    fn from_rcgen(cert: &rcgen::Certificate) -> Result<Self, PkiError> {
        Self::from_der(cert.der().to_vec())
    }

    /// 解析 DER，取序列号与有效期。
    pub fn from_der(der: Vec<u8>) -> Result<Self, PkiError> {
        let (rest, parsed) = X509Certificate::from_der(&der)
            .map_err(|error| PkiError::Crypto(format!("parse certificate: {error}")))?;
        if !rest.is_empty() {
            return Err(PkiError::Crypto("trailing bytes after certificate".into()));
        }
        let serial = CertSerial::from_be_bytes(parsed.raw_serial()).ok_or_else(|| {
            PkiError::Crypto("certificate serial is not a positive 128-bit integer".into())
        })?;
        let not_before = seconds_to_timestamp(parsed.validity().not_before.timestamp())?;
        let not_after = seconds_to_timestamp(parsed.validity().not_after.timestamp())?;
        let pem = pem_encode("CERTIFICATE", &der);
        Ok(Self {
            serial,
            der,
            pem,
            not_before,
            not_after,
        })
    }
}

/// 能签发证书的 CA（根或中间）：证书 + 私钥。私钥不出本结构，`Debug` 不输出。
pub struct Authority {
    signer: rcgen::Certificate,
    key: KeyPair,
    cert: IssuedCert,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("serial", &self.cert.serial)
            .field("not_after", &self.cert.not_after)
            .finish_non_exhaustive()
    }
}

impl Authority {
    /// 从证书 PEM 与 PKCS#8 私钥 PEM 载入（中间 CA 私钥经 `secrets` 读取后传入）。
    ///
    /// 证书必须是 CA（`basicConstraints CA:TRUE`），私钥必须与证书公钥配对。
    pub fn load(cert_pem: &str, key_pkcs8_pem: &str) -> Result<Self, PkiError> {
        let der = pem_decode_single("CERTIFICATE", cert_pem)?;
        let (_, parsed) = X509Certificate::from_der(&der)
            .map_err(|error| PkiError::Crypto(format!("parse CA certificate: {error}")))?;
        let is_ca = parsed
            .basic_constraints()
            .ok()
            .flatten()
            .is_some_and(|ext| ext.value.ca);
        if !is_ca {
            return Err(PkiError::Crypto("certificate is not a CA".into()));
        }
        let key = load_key_pair(key_pkcs8_pem)?;
        if key.public_key_der() != parsed.tbs_certificate.subject_pki.raw {
            return Err(PkiError::Crypto(
                "private key does not match CA certificate".into(),
            ));
        }
        let params = CertificateParams::from_ca_cert_der(&der.clone().into())?;
        // 只用作签发时的 issuer 句柄（DN、密钥标识）；它本身的 DER 不对外。
        let signer = params.self_signed(&key)?;
        Ok(Self {
            signer,
            key,
            cert: IssuedCert::from_der(der)?,
        })
    }

    pub fn certificate(&self) -> &IssuedCert {
        &self.cert
    }

    /// 自签离线根 CA：`CA:TRUE, pathlen:1`，`keyCertSign | cRLSign`，
    /// `CN = "Gantry Root CA <install_id>"`。返回根和它的私钥（调用方加密导出后丢弃）。
    pub(crate) fn new_root(
        install_id: &str,
        now: Timestamp,
        policy: &CertPolicy,
        entropy: &dyn Entropy,
    ) -> Result<(Self, p256::SecretKey), PkiError> {
        let generated = generate_key(entropy)?;
        let mut params = base_params(
            format!("Gantry Root CA {install_id}"),
            &CertSerial::generate(entropy),
            now,
            policy,
            policy.root_validity,
            None,
        )?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let signer = params.self_signed(&generated.key_pair)?;
        let cert = IssuedCert::from_rcgen(&signer)?;
        Ok((
            Self {
                signer,
                key: generated.key_pair,
                cert,
            },
            generated.secret,
        ))
    }

    /// 由本 CA（根）签发在线中间 CA：`CA:TRUE, pathlen:0`，`keyCertSign | cRLSign`，
    /// `CN = "Gantry Intermediate CA <seq>"`。返回中间 CA 和它的 PKCS#8 私钥 PEM（交给 `secrets`）。
    pub(crate) fn issue_intermediate(
        &self,
        seq: u32,
        now: Timestamp,
        policy: &CertPolicy,
        entropy: &dyn Entropy,
    ) -> Result<(Self, Zeroizing<String>), PkiError> {
        let generated = generate_key(entropy)?;
        let mut params = base_params(
            format!("Gantry Intermediate CA {seq}"),
            &CertSerial::generate(entropy),
            now,
            policy,
            policy.intermediate_validity,
            Some(self.cert.not_after),
        )?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.use_authority_key_identifier_extension = true;
        let signer = params.signed_by(&generated.key_pair, &self.signer, &self.key)?;
        let cert = IssuedCert::from_rcgen(&signer)?;
        Ok((
            Self {
                signer,
                key: generated.key_pair,
                cert,
            },
            generated.pkcs8_pem,
        ))
    }

    /// 服务端叶子：`CA:FALSE`、`digitalSignature`、`EKU = serverAuth`；SAN = `server_names`
    /// （DNS 或 IP），`CN` = 第一个名字；`notAfter` 不晚于本 CA。返回证书和新私钥 PEM。
    pub fn issue_server_leaf(
        &self,
        server_names: &[String],
        now: Timestamp,
        policy: &CertPolicy,
        entropy: &dyn Entropy,
    ) -> Result<(IssuedCert, Zeroizing<String>), PkiError> {
        let first = server_names
            .first()
            .ok_or(PkiError::Config("server_names must not be empty"))?;
        let mut sans = Vec::with_capacity(server_names.len());
        for name in server_names {
            sans.push(server_san(name)?);
        }
        let generated = generate_key(entropy)?;
        let mut params = base_params(
            first.clone(),
            &CertSerial::generate(entropy),
            now,
            policy,
            policy.server_cert_validity,
            Some(self.cert.not_after),
        )?;
        params.subject_alt_names = sans;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&generated.key_pair, &self.signer, &self.key)?;
        Ok((IssuedCert::from_rcgen(&cert)?, generated.pkcs8_pem))
    }

    /// 设备叶子：`CA:FALSE`、`digitalSignature`、`EKU = clientAuth` 仅此一项、无 SAN、
    /// `CN = <device.id>`；`notAfter` 不晚于本 CA。只用 `public_key`，不接触 CSR 的其他内容。
    ///
    /// `serial` 由调用方生成并保证全库唯一。
    pub fn issue_device_leaf(
        &self,
        device_id: &DeviceId,
        public_key: &P256PublicKey,
        serial: &CertSerial,
        now: Timestamp,
        policy: &CertPolicy,
    ) -> Result<IssuedCert, PkiError> {
        let mut params = base_params(
            device_id.to_string(),
            serial,
            now,
            policy,
            policy.client_cert_validity,
            Some(self.cert.not_after),
        )?;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.use_authority_key_identifier_extension = true;
        let spki = rcgen::SubjectPublicKeyInfo::from_der(public_key.spki_der())?;
        let cert = params.signed_by(&spki, &self.signer, &self.key)?;
        IssuedCert::from_rcgen(&cert)
    }
}

fn base_params(
    common_name: String,
    serial: &CertSerial,
    now: Timestamp,
    policy: &CertPolicy,
    validity: Duration,
    cap: Option<Timestamp>,
) -> Result<CertificateParams, PkiError> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    params.distinguished_name = dn;
    params.serial_number = Some(SerialNumber::from_slice(&serial.to_be_bytes()));
    let not_before = offset(now, -(policy.not_before_skew.as_millis() as i64))?;
    let mut not_after = offset(now, validity.as_millis() as i64)?;
    if let Some(cap) = cap {
        not_after = not_after.min(cap);
    }
    if not_after <= not_before {
        return Err(PkiError::Config(
            "issuer expires before the new certificate starts",
        ));
    }
    params.not_before = to_offset_date_time(not_before)?;
    params.not_after = to_offset_date_time(not_after)?;
    Ok(params)
}

fn server_san(name: &str) -> Result<SanType, PkiError> {
    if let Ok(ip) = name.parse::<IpAddr>() {
        return Ok(SanType::IpAddress(ip));
    }
    let valid = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if !valid {
        return Err(PkiError::Config(
            "server_names entry is not a DNS name or IP",
        ));
    }
    let ia5 = rcgen::Ia5String::try_from(name.to_ascii_lowercase())
        .map_err(|_| PkiError::Config("server_names entry is not a DNS name or IP"))?;
    Ok(SanType::DnsName(ia5))
}

/// X.509 时间只有秒精度：向下取整到秒。
fn offset(ts: Timestamp, delta_ms: i64) -> Result<Timestamp, PkiError> {
    let ms = ts
        .as_millis()
        .checked_add(delta_ms)
        .ok_or(PkiError::TimeRange)?;
    Ok(Timestamp::from_millis(ms.div_euclid(1000) * 1000))
}

fn to_offset_date_time(ts: Timestamp) -> Result<OffsetDateTime, PkiError> {
    OffsetDateTime::from_unix_timestamp(ts.as_millis().div_euclid(1000))
        .map_err(|_| PkiError::TimeRange)
}

fn seconds_to_timestamp(secs: i64) -> Result<Timestamp, PkiError> {
    secs.checked_mul(1000)
        .map(Timestamp::from_millis)
        .ok_or(PkiError::TimeRange)
}

pub(crate) fn pem_encode(tag: &str, der: &[u8]) -> String {
    pem::encode_config(
        &pem::Pem::new(tag, der.to_vec()),
        pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF),
    )
}

/// 解析恰好一个指定标签的 PEM 块；前后只允许空白。
pub(crate) fn pem_decode_single(tag: &str, text: &str) -> Result<Vec<u8>, PkiError> {
    let trimmed = text.trim();
    let begin = format!("-----BEGIN {tag}-----");
    let end = format!("-----END {tag}-----");
    if !trimmed.starts_with(&begin)
        || !trimmed.ends_with(&end)
        || trimmed.matches("-----BEGIN ").count() != 1
    {
        return Err(PkiError::Crypto(format!(
            "expected exactly one {tag} PEM block"
        )));
    }
    let block = pem::parse(trimmed)
        .map_err(|error| PkiError::Crypto(format!("parse {tag} PEM: {error}")))?;
    if block.tag() != tag {
        return Err(PkiError::Crypto(format!(
            "expected exactly one {tag} PEM block"
        )));
    }
    Ok(block.into_contents())
}
