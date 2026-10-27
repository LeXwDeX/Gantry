//! 固定根 CA 的 TLS 客户端（desktop-shell.md §12"mTLS 与固定 CA"的 A 级部分）。
//!
//! 本进程内起 rustls 服务端，在回环 TCP 上做真实握手。证书由 TestCA（gantry-fakes）与
//! gantry-pki 的 `Authority` 签发；设备叶子按软件替身的 mTLS 公钥签发。
#![cfg(feature = "test-software-keys")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use gantry_core::ids::DeviceId;
use gantry_core::testing::SeqEntropy;
use gantry_core::time::Timestamp;
use gantry_desktop_shell::keys::software::{PresenceDouble, TestSoftwareKeyStore};
use gantry_desktop_shell::keys::{DeviceKeys, KeyStore};
use gantry_desktop_shell::tls::{client_config, ClientIdentity, RootFingerprint};
use gantry_fakes::test_ca::TestCA;
use gantry_pki::{Authority, CertPolicy, CertSerial};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned};

fn now() -> Timestamp {
    Timestamp::from_millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    )
}

fn der(pem_text: &str) -> CertificateDer<'static> {
    CertificateDer::from(pem::parse(pem_text).unwrap().into_contents())
}

struct Pki {
    ca: TestCA,
    root: Authority,
    inter: Authority,
}

impl Pki {
    fn new() -> Self {
        let ca = TestCA::new().unwrap();
        let read = |n: &str| std::fs::read_to_string(ca.root_dir().join(n)).unwrap();
        let root = Authority::load(&read("root.crt"), &read("root.key")).unwrap();
        let inter = Authority::load(&read("inter.crt"), &read("inter.key")).unwrap();
        Self { ca, root, inter }
    }

    fn fingerprint(&self) -> RootFingerprint {
        RootFingerprint::parse(&self.ca.root_fingerprint().to_string()).unwrap()
    }

    fn root_der(&self) -> CertificateDer<'static> {
        der(self.ca.root_cert_pem())
    }

    /// 服务端证书链 `[叶子, 中间, 根]` 与私钥。
    fn server(
        &self,
        inter: &Authority,
        names: &[&str],
        at: Timestamp,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let (leaf, key) = inter
            .issue_server_leaf(&names, at, &CertPolicy::default(), &SeqEntropy::new(11))
            .unwrap();
        let chain = vec![
            der(&leaf.pem),
            der(&inter.certificate().pem),
            self.root_der(),
        ];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            pem::parse(key.as_str()).unwrap().into_contents(),
        ));
        (chain, key)
    }

    fn device_chain(&self, keys: &dyn DeviceKeys) -> Vec<CertificateDer<'static>> {
        let id: DeviceId = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().unwrap();
        let public =
            gantry_pki::P256PublicKey::from_spki_der(&keys.tls_public_key().spki_der()).unwrap();
        let leaf = self
            .inter
            .issue_device_leaf(
                &id,
                &public,
                &CertSerial::generate(&SeqEntropy::new(5)),
                now(),
                &CertPolicy::default(),
            )
            .unwrap();
        vec![
            der(&leaf.pem),
            der(&self.inter.certificate().pem),
            self.root_der(),
        ]
    }

    fn client_verifier(&self) -> Arc<dyn rustls::server::danger::ClientCertVerifier> {
        let mut roots = RootCertStore::empty();
        roots.add(self.root_der()).unwrap();
        WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap()
    }
}

enum ClientAuth {
    None,
    Required(Arc<dyn rustls::server::danger::ClientCertVerifier>),
}

fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    auth: ClientAuth,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<ServerConfig> {
    let builder =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(versions)
            .unwrap();
    let builder = match auth {
        ClientAuth::None => builder.with_no_client_auth(),
        ClientAuth::Required(v) => builder.with_client_cert_verifier(v),
    };
    Arc::new(builder.with_single_cert(chain, key).unwrap())
}

/// 握手并往返一个字节。返回（客户端结果，服务端看到的客户端证书数）。
fn handshake(
    server: Arc<ServerConfig>,
    client: Arc<rustls::ClientConfig>,
    name: &str,
) -> (Result<(), String>, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let conn = ServerConnection::new(server).unwrap();
        let mut tls = StreamOwned::new(conn, tcp);
        let mut buf = [0u8; 1];
        let ok = tls.read_exact(&mut buf).is_ok() && tls.write_all(&buf).is_ok();
        let _ = tls.flush();
        let peers = tls.conn.peer_certificates().map(|c| c.len()).unwrap_or(0);
        (ok, peers)
    });
    let tcp = TcpStream::connect(addr).unwrap();
    let conn =
        ClientConnection::new(client, ServerName::try_from(name.to_string()).unwrap()).unwrap();
    let mut tls = StreamOwned::new(conn, tcp);
    let result = tls
        .write_all(b"x")
        .and_then(|_| tls.flush())
        .and_then(|_| {
            let mut buf = [0u8; 1];
            tls.read_exact(&mut buf)
        })
        .map_err(|e| e.to_string());
    let version = tls.conn.protocol_version();
    drop(tls);
    let (_, peers) = srv.join().unwrap();
    if result.is_ok() {
        assert_eq!(version, Some(rustls::ProtocolVersion::TLSv1_3));
    }
    (result, peers)
}

fn device_keys() -> Arc<dyn DeviceKeys> {
    Arc::from(
        TestSoftwareKeyStore::new(PresenceDouble::new())
            .generate()
            .unwrap(),
    )
}

const TLS13: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];

#[test]
fn one_way_tls_to_pinned_root_succeeds() {
    let pki = Pki::new();
    let (chain, key) = pki.server(&pki.inter, &["gantry.test"], now());
    let (result, peers) = handshake(
        server_config(chain, key, ClientAuth::None, TLS13),
        client_config(pki.fingerprint(), None),
        "gantry.test",
    );
    result.unwrap();
    assert_eq!(peers, 0);
}

#[test]
fn mtls_presents_device_certificate_signed_by_hardware_key() {
    let pki = Pki::new();
    let keys = device_keys();
    let (chain, key) = pki.server(&pki.inter, &["gantry.test"], now());
    let identity = ClientIdentity {
        chain: pki.device_chain(keys.as_ref()),
        keys,
    };
    let (result, peers) = handshake(
        server_config(
            chain,
            key,
            ClientAuth::Required(pki.client_verifier()),
            TLS13,
        ),
        client_config(pki.fingerprint(), Some(identity)),
        "gantry.test",
    );
    result.unwrap();
    assert!(peers >= 1, "服务端应看到设备证书");
}

#[test]
fn mtls_without_client_certificate_fails_at_tls_layer() {
    let pki = Pki::new();
    let (chain, key) = pki.server(&pki.inter, &["gantry.test"], now());
    let (result, _) = handshake(
        server_config(
            chain,
            key,
            ClientAuth::Required(pki.client_verifier()),
            TLS13,
        ),
        client_config(pki.fingerprint(), None),
        "gantry.test",
    );
    assert!(result.is_err());
}

#[test]
fn wrong_ca_is_rejected_even_if_otherwise_valid() {
    // 另一套 CA 的合法证书（相当于系统信任库里的合法公共证书）：根指纹不符即拒绝。
    let pki = Pki::new();
    let other = Pki::new();
    let (chain, key) = other.server(&other.inter, &["gantry.test"], now());
    let (result, _) = handshake(
        server_config(chain, key, ClientAuth::None, TLS13),
        client_config(pki.fingerprint(), None),
        "gantry.test",
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("UnknownIssuer") || err.contains("unknown issuer"),
        "{err}"
    );
}

#[test]
fn root_swapped_into_chain_does_not_help_a_foreign_leaf() {
    // 攻击者把真正的根附在自己的链末：指纹相符，但叶子不是由它签发的。
    let pki = Pki::new();
    let other = Pki::new();
    let (mut chain, key) = other.server(&other.inter, &["gantry.test"], now());
    *chain.last_mut().unwrap() = pki.root_der();
    let (result, _) = handshake(
        server_config(chain, key, ClientAuth::None, TLS13),
        client_config(pki.fingerprint(), None),
        "gantry.test",
    );
    assert!(result.is_err());
}

#[test]
fn hostname_mismatch_is_rejected() {
    let pki = Pki::new();
    let (chain, key) = pki.server(&pki.inter, &["gantry.test"], now());
    let (result, _) = handshake(
        server_config(chain, key, ClientAuth::None, TLS13),
        client_config(pki.fingerprint(), None),
        "other.test",
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("NotValidForName") || err.to_lowercase().contains("name"),
        "{err}"
    );
}

#[test]
fn expired_server_certificate_is_rejected() {
    let pki = Pki::new();
    // 签发时刻放在 120 天前：90 天有效期的叶子已过期。
    let past = Timestamp::from_millis(now().as_millis() - 120 * 86_400_000);
    let (chain, key) = pki.server(&pki.inter, &["gantry.test"], past);
    let (result, _) = handshake(
        server_config(chain, key, ClientAuth::None, TLS13),
        client_config(pki.fingerprint(), None),
        "gantry.test",
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("Expired") || err.to_lowercase().contains("expired"),
        "{err}"
    );
}

#[test]
fn tls12_only_server_is_rejected() {
    let pki = Pki::new();
    let (chain, key) = pki.server(&pki.inter, &["gantry.test"], now());
    let (result, _) = handshake(
        server_config(chain, key, ClientAuth::None, &[&rustls::version::TLS12]),
        client_config(pki.fingerprint(), None),
        "gantry.test",
    );
    assert!(result.is_err());
}

#[test]
fn rotated_intermediate_from_same_root_still_works() {
    // 已配对设备只固定根（P-64）：服务端换成同一根签发的新中间 CA，握手照常。
    let pki = Pki::new();
    let (new_inter, _key) = pki
        .root
        .issue_intermediate(2, now(), &CertPolicy::default(), &SeqEntropy::new(21))
        .unwrap();
    let (chain, key) = pki.server(&new_inter, &["gantry.test"], now());
    let (result, _) = handshake(
        server_config(chain, key, ClientAuth::None, TLS13),
        client_config(pki.fingerprint(), None),
        "gantry.test",
    );
    result.unwrap();
}

#[test]
fn deleted_device_key_breaks_the_handshake_without_fallback() {
    let pki = Pki::new();
    let store = TestSoftwareKeyStore::new(PresenceDouble::new());
    let keys: Arc<dyn DeviceKeys> = Arc::from(store.generate().unwrap());
    let chain_dev = pki.device_chain(keys.as_ref());
    store.delete(keys.labels()).unwrap();
    let (chain, key) = pki.server(&pki.inter, &["gantry.test"], now());
    let (result, peers) = handshake(
        server_config(
            chain,
            key,
            ClientAuth::Required(pki.client_verifier()),
            TLS13,
        ),
        client_config(
            pki.fingerprint(),
            Some(ClientIdentity {
                chain: chain_dev,
                keys,
            }),
        ),
        "gantry.test",
    );
    assert!(result.is_err());
    assert_eq!(peers, 0);
}
