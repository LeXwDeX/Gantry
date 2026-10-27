//! TLS 客户端配置（desktop-shell.md §7.1"TLS"、§8 S-1 / S-6，api.md §1.3，P-64）。
//!
//! - **只信固定的离线根 CA**：服务端出示的链末张（根）证书 DER 的 SHA-256 必须等于已固定的指纹；
//!   随后以它为唯一信任锚校验链内签名、有效期与 `serverAuth`，并要求叶子 SAN 匹配主机名。
//!   不使用系统信任库，所以系统里合法的公共证书同样被拒。中间 CA 轮换不影响已配对设备。
//! - **仅 TLS 1.3**。
//! - **客户端证书**（mTLS）：签名交给 [`DeviceKeys::sign_tls`]，私钥不出硬件。配对端点是单向 TLS，
//!   不出示客户端证书。

use std::fmt;
use std::sync::Arc;

use gantry_core::canon::sha256;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::ResolvesClientCert;
use rustls::crypto::{verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore,
    SignatureAlgorithm, SignatureScheme,
};

use crate::keys::DeviceKeys;

/// 由 `sha256:<64 位十六进制>` 解析出的根指纹。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RootFingerprint([u8; 32]);

impl RootFingerprint {
    pub fn parse(text: &str) -> Option<Self> {
        gantry_core::canon::Sha256Digest::parse(text)
            .ok()
            .map(|d| Self(*d.as_bytes()))
    }

    fn matches(&self, der: &[u8]) -> bool {
        sha256(der).as_bytes() == &self.0
    }
}

impl fmt::Debug for RootFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RootFingerprint(sha256:{})",
            gantry_core::canon::Sha256Digest::from_bytes(self.0).to_hex()
        )
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 只认固定根的服务端证书校验器。
#[derive(Debug)]
pub struct PinnedRootVerifier {
    pinned: RootFingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedRootVerifier {
    pub fn new(pinned: RootFingerprint) -> Self {
        Self {
            pinned,
            algorithms: provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinnedRootVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // 服务端按 `[叶子, 中间 CA, 根]` 出示（P-64）；根必须是链末张且指纹相符。
        let (root, rest) = intermediates
            .split_last()
            .ok_or(Error::InvalidCertificate(CertificateError::UnknownIssuer))?;
        if !self.pinned.matches(root) {
            return Err(Error::InvalidCertificate(CertificateError::UnknownIssuer));
        }
        let mut roots = RootCertStore::empty();
        roots
            .add(root.clone().into_owned())
            .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let leaf = ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &leaf,
            &roots,
            rest,
            now,
            self.algorithms.all,
        )?;
        rustls::client::verify_server_name(&leaf, server_name)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        // 配置只启用 TLS 1.3，走不到这里；保险起见一律拒绝。
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
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// 由硬件 mTLS 客户端密钥签名的 rustls 签名器。
struct DeviceSigningKey {
    keys: Arc<dyn DeviceKeys>,
}

impl fmt::Debug for DeviceSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceSigningKey(..)")
    }
}

impl SigningKey for DeviceSigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| {
                Box::new(DeviceSigner {
                    keys: self.keys.clone(),
                }) as Box<dyn Signer>
            })
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

struct DeviceSigner {
    keys: Arc<dyn DeviceKeys>,
}

impl fmt::Debug for DeviceSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceSigner(..)")
    }
}

impl Signer for DeviceSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        // 密钥丢失 / 被删（吊销后）→ 握手失败，不会回落到无证书连接。
        self.keys
            .sign_tls(message)
            .map_err(|_| Error::General("设备密钥不可用".into()))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

#[derive(Debug)]
struct DeviceCertResolver {
    certified: Arc<CertifiedKey>,
}

impl ResolvesClientCert for DeviceCertResolver {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        sigschemes
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| self.certified.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// 设备证书链（`[叶子, 中间 CA, 根]` 的 DER）与对应的硬件密钥。
pub struct ClientIdentity {
    pub chain: Vec<CertificateDer<'static>>,
    pub keys: Arc<dyn DeviceKeys>,
}

/// 构建客户端配置：`client = None` 为单向 TLS（配对端点），否则为 mTLS（API 与事件流）。
pub fn client_config(pinned: RootFingerprint, client: Option<ClientIdentity>) -> Arc<ClientConfig> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring 支持 TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedRootVerifier::new(pinned)));
    let config = match client {
        None => builder.with_no_client_auth(),
        Some(identity) => {
            let key: Arc<dyn SigningKey> = Arc::new(DeviceSigningKey {
                keys: identity.keys,
            });
            builder.with_client_cert_resolver(Arc::new(DeviceCertResolver {
                certified: Arc::new(CertifiedKey::new(identity.chain, key)),
            }))
        }
    };
    Arc::new(config)
}

/// PEM 证书链 → DER。
pub fn chain_from_pem(pems: &[String]) -> Option<Vec<CertificateDer<'static>>> {
    pems.iter()
        .map(|p| {
            let parsed = pem::parse(p).ok()?;
            (parsed.tag() == "CERTIFICATE").then(|| CertificateDer::from(parsed.into_contents()))
        })
        .collect()
}
