//! mTLS 握手校验器（pki.md §7.3）。
//!
//! 按顺序检查，任一步失败即握手失败（TLS 告警，不产生 HTTP 响应）：
//!
//! 1. **链**：以根证书为信任锚验证 `[叶子, 中间 CA]`；有效期按 rustls 传入的时间（由注入的时钟决定）；
//!    EKU 含 `clientAuth`；叶子 `CA:FALSE`；签名算法只接受 `ecdsa-with-SHA256`（P-256）。
//! 2. **中间 CA 钉扎**：出示的中间 CA 指纹必须属于登记的集合（`pki_ca` 中 `current` / `previous`）。
//! 3. **身份格式**：叶子 `CN` 恰好一个，形如 `dev_<ULID>`；公钥是 P-256。
//! 4. **在册 / 吊销**：按叶子序列号查 [`CertStatusLookup`]（读库，不缓存）：必须在册，状态 `active`，
//!    或 `superseded` 且未过原 `notAfter`；所属设备 `active`。**不在册 → 拒绝**（失败即关闭）。
//!
//! verifier 本身不持有业务状态；`api` 从 `peer_certificates()` 取叶子序列号，再经
//! `DeviceDirectory::by_cert_serial` 映射到 Device。

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use gantry_core::canon::Sha256Digest;
use gantry_core::ids::{Clock, DeviceId};
use gantry_core::time::Timestamp;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::time_provider::TimeProvider;
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, RootCertStore,
    SignatureScheme,
};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::keys::{fingerprint, CertSerial, P256PublicKey};
use crate::PkiError;

/// 设备证书在 `pki_device_certs` 里的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceCertStatus {
    Active,
    /// 续期后被取代；原 `notAfter` 之前仍可握手（api.md §4.10）。
    Superseded,
    Revoked,
}

/// 一次在册查询的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertRecord {
    pub device_id: DeviceId,
    pub status: DeviceCertStatus,
    pub not_after: Timestamp,
    /// 所属设备的 `status = active`。
    pub device_active: bool,
}

/// 握手时的在册查询。实现必须直接读库，**不加带 TTL 的缓存**：吊销对下一次握手立即生效。
pub trait CertStatusLookup: Send + Sync {
    /// 序列号不在册 → `Ok(None)`。查询失败 → `Err`（握手失败，失败即关闭）。
    fn lookup(&self, serial: &CertSerial) -> Result<Option<CertRecord>, PkiError>;
}

/// 握手拒绝原因（§9），只记本地日志与指标，不回显给对端。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    UnknownCa,
    Expired,
    NotYetValid,
    BadEku,
    BadAlgorithm,
    BadIdentity,
    UnknownSerial,
    Revoked,
    LookupFailed,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownCa => "unknown_ca",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::BadEku => "bad_eku",
            Self::BadAlgorithm => "bad_algorithm",
            Self::BadIdentity => "bad_identity",
            Self::UnknownSerial => "unknown_serial",
            Self::Revoked => "revoked",
            Self::LookupFailed => "lookup_failed",
        }
    }
}

/// 拒绝时的回调（日志 / 指标）。默认什么都不做。
pub trait RejectObserver: Send + Sync {
    fn rejected(&self, reason: RejectReason, serial: Option<&CertSerial>);
}

struct NoopObserver;

impl RejectObserver for NoopObserver {
    fn rejected(&self, _reason: RejectReason, _serial: Option<&CertSerial>) {}
}

/// 把注入的 [`Clock`] 交给 rustls 作时间来源（证书有效期按它判断，测试可控）。
#[derive(Clone)]
pub struct ClockTimeProvider(Arc<dyn Clock>);

impl ClockTimeProvider {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self(clock)
    }
}

impl fmt::Debug for ClockTimeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClockTimeProvider")
    }
}

impl TimeProvider for ClockTimeProvider {
    fn current_time(&self) -> Option<UnixTime> {
        let ms = self.0.now().as_millis();
        u64::try_from(ms.div_euclid(1000))
            .ok()
            .map(|secs| UnixTime::since_unix_epoch(std::time::Duration::from_secs(secs)))
    }
}

/// 只含 ECDSA P-256 + SHA-256 的密码学提供方：链上签名与握手签名都只认这一种（§7.1）。
pub fn p256_only_provider() -> CryptoProvider {
    static ALGS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
        all: &[webpki::ring::ECDSA_P256_SHA256],
        mapping: &[(
            SignatureScheme::ECDSA_NISTP256_SHA256,
            &[webpki::ring::ECDSA_P256_SHA256],
        )],
    };
    CryptoProvider {
        signature_verification_algorithms: ALGS,
        ..rustls::crypto::ring::default_provider()
    }
}

/// 设备证书的 rustls `ClientCertVerifier`。
pub struct DeviceCertVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    root_der: Vec<u8>,
    intermediates: HashSet<Sha256Digest>,
    hints: Vec<DistinguishedName>,
    lookup: Arc<dyn CertStatusLookup>,
    observer: Arc<dyn RejectObserver>,
}

impl fmt::Debug for DeviceCertVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCertVerifier")
            .field("root", &fingerprint(&self.root_der))
            .field("intermediates", &self.intermediates)
            .finish_non_exhaustive()
    }
}

impl DeviceCertVerifier {
    /// `root_der`：离线根证书；`intermediates_der`：登记为 `current` / `previous` 的中间 CA 证书。
    pub fn new(
        root_der: &[u8],
        intermediates_der: &[Vec<u8>],
        lookup: Arc<dyn CertStatusLookup>,
    ) -> Result<Self, PkiError> {
        if intermediates_der.is_empty() {
            return Err(PkiError::Config(
                "at least one intermediate CA must be pinned",
            ));
        }
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(root_der.to_vec()))
            .map_err(|error| PkiError::Crypto(format!("root certificate: {error}")))?;
        let inner = WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(p256_only_provider()),
        )
        .build()
        .map_err(|error| PkiError::Crypto(format!("client verifier: {error}")))?;

        let mut intermediates = HashSet::new();
        let mut hints = Vec::new();
        for der in intermediates_der {
            let (_, cert) = X509Certificate::from_der(der)
                .map_err(|error| PkiError::Crypto(format!("intermediate certificate: {error}")))?;
            intermediates.insert(fingerprint(der));
            hints.push(DistinguishedName::from(cert.subject().as_raw().to_vec()));
        }
        Ok(Self {
            inner,
            root_der: root_der.to_vec(),
            intermediates,
            hints,
            lookup,
            observer: Arc::new(NoopObserver),
        })
    }

    pub fn with_observer(mut self, observer: Arc<dyn RejectObserver>) -> Self {
        self.observer = observer;
        self
    }

    fn reject(&self, reason: RejectReason, serial: Option<&CertSerial>, error: Error) -> Error {
        self.observer.rejected(reason, serial);
        error
    }

    fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        // 2. 钉扎：出示的每张中间证书都必须是登记的中间 CA（或根本身），且至少有一张中间 CA。
        //    只放行这些证书，路径就只可能经过登记的中间 CA。
        let mut pinned = false;
        for cert in intermediates {
            if fingerprint(cert.as_ref()) == fingerprint(&self.root_der) {
                continue;
            }
            if !self.intermediates.contains(&fingerprint(cert.as_ref())) {
                return Err(self.reject(
                    RejectReason::UnknownCa,
                    None,
                    Error::InvalidCertificate(CertificateError::UnknownIssuer),
                ));
            }
            pinned = true;
        }
        if !pinned {
            return Err(self.reject(
                RejectReason::UnknownCa,
                None,
                Error::InvalidCertificate(CertificateError::UnknownIssuer),
            ));
        }

        // 1. 链、有效期、EKU、CA 位、签名算法。
        if let Err(error) = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)
        {
            return Err(self.reject(chain_reason(&error), None, error));
        }

        // 3. 身份格式与公钥算法。
        let (_, leaf) = X509Certificate::from_der(end_entity.as_ref()).map_err(|_| {
            self.reject(
                RejectReason::BadIdentity,
                None,
                Error::InvalidCertificate(CertificateError::BadEncoding),
            )
        })?;
        let serial = CertSerial::from_be_bytes(leaf.raw_serial());
        let bad_identity = |reason| {
            self.reject(
                reason,
                serial.as_ref(),
                Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure),
            )
        };
        let cns: Vec<_> = leaf.subject().iter_common_name().collect();
        let device_id = match cns.as_slice() {
            [cn] => cn
                .as_str()
                .ok()
                .and_then(|text| DeviceId::from_str(text).ok()),
            _ => None,
        };
        let Some(device_id) = device_id else {
            return Err(bad_identity(RejectReason::BadIdentity));
        };
        if P256PublicKey::from_spki_der(leaf.tbs_certificate.subject_pki.raw).is_err() {
            return Err(bad_identity(RejectReason::BadAlgorithm));
        }
        let Some(serial) = serial.clone() else {
            return Err(bad_identity(RejectReason::BadIdentity));
        };

        // 4. 在册 / 吊销：直接查库。
        let record = match self.lookup.lookup(&serial) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(self.reject(
                    RejectReason::UnknownSerial,
                    Some(&serial),
                    Error::InvalidCertificate(CertificateError::UnknownIssuer),
                ))
            }
            Err(_) => {
                return Err(self.reject(
                    RejectReason::LookupFailed,
                    Some(&serial),
                    Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure),
                ))
            }
        };
        let now_ms = (now.as_secs() as i64).saturating_mul(1000);
        let usable = match record.status {
            DeviceCertStatus::Active => true,
            DeviceCertStatus::Superseded => now_ms < record.not_after.as_millis(),
            DeviceCertStatus::Revoked => false,
        };
        if record.device_id != device_id || !usable || !record.device_active {
            return Err(self.reject(
                RejectReason::Revoked,
                Some(&serial),
                Error::InvalidCertificate(CertificateError::Revoked),
            ));
        }
        Ok(ClientCertVerified::assertion())
    }
}

fn chain_reason(error: &Error) -> RejectReason {
    match error {
        Error::InvalidCertificate(CertificateError::Expired)
        | Error::InvalidCertificate(CertificateError::ExpiredContext { .. }) => {
            RejectReason::Expired
        }
        Error::InvalidCertificate(CertificateError::NotValidYet)
        | Error::InvalidCertificate(CertificateError::NotValidYetContext { .. }) => {
            RejectReason::NotYetValid
        }
        Error::InvalidCertificate(CertificateError::InvalidPurpose)
        | Error::InvalidCertificate(CertificateError::InvalidPurposeContext { .. }) => {
            RejectReason::BadEku
        }
        Error::InvalidCertificate(CertificateError::BadSignature)
        | Error::InvalidCertificate(CertificateError::UnsupportedSignatureAlgorithmContext {
            ..
        }) => RejectReason::BadAlgorithm,
        _ => RejectReason::UnknownCa,
    }
}

impl ClientCertVerifier for DeviceCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check(end_entity, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        // 仅 TLS 1.3（api.md §1.2）。
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ECDSA_NISTP256_SHA256]
    }
}
