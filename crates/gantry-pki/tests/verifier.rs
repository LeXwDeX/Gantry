//! pki.md §12.2「握手校验（M1）」：用真实 rustls 客户端 / 服务端在内存里跑 TLS 1.3 握手。

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gantry_core::ids::{Clock, DeviceId};
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::test_ca::{DeviceCertSpec, DeviceEku, DeviceIssuer, TestCA};
use gantry_pki::bootstrap::{self, BootstrapParams, MemoryFsProbe};
use gantry_pki::verifier::{
    p256_only_provider, CertRecord, CertStatusLookup, ClockTimeProvider, DeviceCertStatus,
    DeviceCertVerifier, RejectObserver, RejectReason,
};
use gantry_pki::{Authority, CertPolicy, CertSerial, IssuedCert, P256PublicKey, PkiError};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};
use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

const NOW_MS: i64 = 1_798_761_600_000; // 2027-01-01T00:00:00Z
const MIN_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
const SERVER_NAME: &str = "gantry.example.test";

struct AlwaysMemory;

impl MemoryFsProbe for AlwaysMemory {
    fn is_memory_fs(&self, _dir: &Path) -> io::Result<bool> {
        Ok(true)
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "pki-verifier-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// 在册表替身：直接读一张内存表，没有缓存
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemLookup {
    rows: Mutex<HashMap<CertSerial, CertRecord>>,
    fail: Mutex<bool>,
}

impl MemLookup {
    fn put(&self, serial: &CertSerial, record: CertRecord) {
        self.rows.lock().unwrap().insert(serial.clone(), record);
    }

    fn set_status(&self, serial: &CertSerial, status: DeviceCertStatus) {
        self.rows.lock().unwrap().get_mut(serial).unwrap().status = status;
    }

    fn revoke_device(&self, device: &DeviceId) {
        for row in self.rows.lock().unwrap().values_mut() {
            if &row.device_id == device {
                row.status = DeviceCertStatus::Revoked;
                row.device_active = false;
            }
        }
    }
}

impl CertStatusLookup for MemLookup {
    fn lookup(&self, serial: &CertSerial) -> Result<Option<CertRecord>, PkiError> {
        if *self.fail.lock().unwrap() {
            return Err(PkiError::Config("lookup unavailable"));
        }
        Ok(self.rows.lock().unwrap().get(serial).cloned())
    }
}

#[derive(Default)]
struct Rejections(Mutex<Vec<RejectReason>>);

impl RejectObserver for Rejections {
    fn rejected(&self, reason: RejectReason, _serial: Option<&CertSerial>) {
        self.0.lock().unwrap().push(reason);
    }
}

impl Rejections {
    fn last(&self) -> Option<RejectReason> {
        self.0.lock().unwrap().last().copied()
    }
}

// ---------------------------------------------------------------------------
// CA 环境
// ---------------------------------------------------------------------------

struct Env {
    root_der: Vec<u8>,
    intermediate: Authority,
    intermediate_key_pem: String,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
    lookup: Arc<MemLookup>,
    rejections: Arc<Rejections>,
    clock: Arc<FakeClock>,
    entropy: SeqEntropy,
    dir: PathBuf,
}

struct Device {
    id: DeviceId,
    cert: IssuedCert,
    key_der: Vec<u8>,
}

impl Env {
    fn new(seed: u64) -> Self {
        let dir = temp_dir("ca");
        bootstrap::run_with(
            BootstrapParams {
                server_names: vec![SERVER_NAME.into()],
                root_passphrase: SecretString::new("pw".into()),
                install_id: "verifier".into(),
                out_dir: dir.clone(),
                policy: CertPolicy::default(),
            },
            &FakeClock::new(NOW_MS - DAY_MS),
            &SeqEntropy::new(seed),
            &AlwaysMemory,
        )
        .unwrap();
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap();
        let intermediate_key_pem = read("intermediate.key");
        let intermediate =
            Authority::load(&read("intermediate.crt"), &intermediate_key_pem).unwrap();
        let root_der = pem::parse(read("root.crt")).unwrap().into_contents();
        let server_chain = vec![
            CertificateDer::from(pem::parse(read("server.crt")).unwrap().into_contents()),
            CertificateDer::from(intermediate.certificate().der.clone()),
            CertificateDer::from(root_der.clone()),
        ];
        let server_key = PrivateKeyDer::from_pem_slice(read("server.key").as_bytes()).unwrap();
        Self {
            root_der,
            intermediate,
            intermediate_key_pem,
            server_chain,
            server_key,
            lookup: Arc::new(MemLookup::default()),
            rejections: Arc::new(Rejections::default()),
            clock: Arc::new(FakeClock::new(NOW_MS)),
            entropy: SeqEntropy::new(seed + 1000),
            dir,
        }
    }

    fn verifier(&self) -> Arc<DeviceCertVerifier> {
        Arc::new(
            DeviceCertVerifier::new(
                &self.root_der,
                &[self.intermediate.certificate().der.clone()],
                self.lookup.clone(),
            )
            .unwrap()
            .with_observer(self.rejections.clone()),
        )
    }

    fn new_key(&self) -> (P256PublicKey, Vec<u8>) {
        let secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let public = P256PublicKey::from_spki_der(
            secret.public_key().to_public_key_der().unwrap().as_bytes(),
        )
        .unwrap();
        (public, secret.to_pkcs8_der().unwrap().as_bytes().to_vec())
    }

    /// 签发并登记一台 active 设备。
    fn pair(&self, id: &str, issued_at_ms: i64) -> Device {
        let id: DeviceId = id.parse().unwrap();
        let (public, key_der) = self.new_key();
        let serial = CertSerial::generate(&self.entropy);
        let cert = self
            .intermediate
            .issue_device_leaf(
                &id,
                &public,
                &serial,
                Timestamp::from_millis(issued_at_ms),
                &CertPolicy::default(),
            )
            .unwrap();
        self.lookup.put(
            &serial,
            CertRecord {
                device_id: id,
                status: DeviceCertStatus::Active,
                not_after: cert.not_after,
                device_active: true,
            },
        );
        Device { id, cert, key_der }
    }

    fn handshake(&self, device: Option<&Device>) -> Result<CertSerial, rustls::Error> {
        self.handshake_with(
            device.map(|d| {
                (
                    vec![
                        CertificateDer::from(d.cert.der.clone()),
                        CertificateDer::from(self.intermediate.certificate().der.clone()),
                    ],
                    d.key_der.clone(),
                )
            }),
            self.verifier(),
        )
    }

    fn handshake_with(
        &self,
        client: Option<(Vec<CertificateDer<'static>>, Vec<u8>)>,
        verifier: Arc<DeviceCertVerifier>,
    ) -> Result<CertSerial, rustls::Error> {
        let time = Arc::new(ClockTimeProvider::new(self.clock.clone() as Arc<dyn Clock>));
        let provider = Arc::new(p256_only_provider());
        let server_config = ServerConfig::builder_with_details(provider.clone(), time.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.server_chain.clone(), self.server_key.clone_key())
            .unwrap();

        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(self.root_der.clone()))
            .unwrap();
        let builder = ClientConfig::builder_with_details(provider, time)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots);
        let client_config = match client {
            Some((chain, key)) => builder
                .with_client_auth_cert(chain, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
                .unwrap(),
            None => builder.with_no_client_auth(),
        };

        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut client = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from(SERVER_NAME).unwrap(),
        )
        .unwrap();
        pump(&mut client, &mut server)?;
        let leaf = server
            .peer_certificates()
            .and_then(|certs| certs.first())
            .expect("client certificate after handshake");
        Ok(IssuedCert::from_der(leaf.to_vec()).unwrap().serial)
    }
}

/// 在两个连接之间搬运字节，直到双方握手完成或一方报错。
fn pump(client: &mut ClientConnection, server: &mut ServerConnection) -> Result<(), rustls::Error> {
    for _ in 0..32 {
        let mut buf = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut buf).unwrap();
        }
        if !buf.is_empty() {
            server.read_tls(&mut buf.as_slice()).unwrap();
            server.process_new_packets()?;
        }
        let mut buf = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut buf).unwrap();
        }
        if !buf.is_empty() {
            client.read_tls(&mut buf.as_slice()).unwrap();
            client.process_new_packets()?;
        }
        if !client.is_handshaking() && !server.is_handshaking() && !client.wants_write() {
            return Ok(());
        }
    }
    panic!("handshake did not converge");
}

const DEV_A: &str = "dev_01J0000000000000000000000A";
const DEV_B: &str = "dev_01J0000000000000000000000B";

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[test]
fn valid_device_certificate_handshakes_and_exposes_serial() {
    let env = Env::new(1);
    let device = env.pair(DEV_A, NOW_MS);
    assert_eq!(env.handshake(Some(&device)).unwrap(), device.cert.serial);
    assert_eq!(env.rejections.last(), None);
}

#[test]
fn missing_client_certificate_fails() {
    let env = Env::new(2);
    assert!(env.handshake(None).is_err());
}

#[test]
fn expired_and_not_yet_valid_certificates_fail() {
    let env = Env::new(3);
    let device = env.pair(DEV_A, NOW_MS);

    env.clock.set(Timestamp::from_millis(
        device.cert.not_after.as_millis() + 1000,
    ));
    assert!(env.handshake(Some(&device)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::Expired));

    // notBefore = 签发时刻 − 5 分钟：服务端比签发时刻早 4 分钟仍可握手，早 6 分钟则尚未生效。
    env.clock.set(Timestamp::from_millis(NOW_MS - 4 * MIN_MS));
    assert!(env.handshake(Some(&device)).is_ok());
    env.clock.set(Timestamp::from_millis(NOW_MS - 6 * MIN_MS));
    assert!(env.handshake(Some(&device)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::NotYetValid));
}

#[test]
fn certificate_from_foreign_ca_fails() {
    let env = Env::new(4);
    let foreign = Env::new(5);
    let device = foreign.pair(DEV_A, NOW_MS);
    // 即使序列号恰好在册，也不能放行外部 CA 签发的证书。
    env.lookup.put(
        &device.cert.serial,
        CertRecord {
            device_id: device.id,
            status: DeviceCertStatus::Active,
            not_after: device.cert.not_after,
            device_active: true,
        },
    );
    let chain = vec![
        CertificateDer::from(device.cert.der.clone()),
        CertificateDer::from(foreign.intermediate.certificate().der.clone()),
    ];
    assert!(env
        .handshake_with(Some((chain, device.key_der.clone())), env.verifier())
        .is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::UnknownCa));
}

#[test]
fn unregistered_intermediate_from_same_root_fails() {
    let env = Env::new(6);
    // 用离线根（测试里解开导出文件）再签一张未登记的中间 CA。
    let enc = fs::read_to_string(env.dir.join("root.key.enc")).unwrap();
    let root_key = p256::SecretKey::from_pkcs8_encrypted_pem(&enc, b"pw").unwrap();
    let root_pem = root_key.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
    let root = Authority::load(
        &fs::read_to_string(env.dir.join("root.crt")).unwrap(),
        &root_pem,
    )
    .unwrap();
    let (rogue, _key) = root
        .issue_intermediate(
            2,
            Timestamp::from_millis(NOW_MS),
            &CertPolicy::default(),
            &env.entropy,
        )
        .unwrap();
    let id: DeviceId = DEV_A.parse().unwrap();
    let (public, key_der) = env.new_key();
    let serial = CertSerial::generate(&env.entropy);
    let cert = rogue
        .issue_device_leaf(
            &id,
            &public,
            &serial,
            Timestamp::from_millis(NOW_MS),
            &CertPolicy::default(),
        )
        .unwrap();
    env.lookup.put(
        &serial,
        CertRecord {
            device_id: id,
            status: DeviceCertStatus::Active,
            not_after: cert.not_after,
            device_active: true,
        },
    );
    let chain = vec![
        CertificateDer::from(cert.der.clone()),
        CertificateDer::from(rogue.certificate().der.clone()),
    ];
    assert!(env
        .handshake_with(Some((chain, key_der)), env.verifier())
        .is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::UnknownCa));
}

/// 用中间 CA 的私钥直接拼一张不合规范的叶子（模拟签发逻辑之外的证书）。
fn forge_leaf(
    env: &Env,
    configure: impl FnOnce(&mut CertificateParams),
    key: &KeyPair,
) -> (CertSerial, Vec<u8>) {
    let issuer_key = KeyPair::from_pem(&env.intermediate_key_pem).unwrap();
    let issuer =
        CertificateParams::from_ca_cert_der(&env.intermediate.certificate().der.clone().into())
            .unwrap()
            .self_signed(&issuer_key)
            .unwrap();
    let serial = CertSerial::generate(&env.entropy);
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial.to_be_bytes()));
    params.not_before = time::OffsetDateTime::from_unix_timestamp(NOW_MS / 1000 - 600).unwrap();
    params.not_after = time::OffsetDateTime::from_unix_timestamp(NOW_MS / 1000 + 86_400).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, DEV_B.to_string());
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.use_authority_key_identifier_extension = true;
    configure(&mut params);
    let cert = params.signed_by(key, &issuer, &issuer_key).unwrap();
    env.lookup.put(
        &serial,
        CertRecord {
            device_id: DEV_B.parse().unwrap(),
            status: DeviceCertStatus::Active,
            not_after: Timestamp::from_millis(NOW_MS + DAY_MS),
            device_active: true,
        },
    );
    (serial, cert.der().to_vec())
}

fn try_forged(env: &Env, leaf_der: Vec<u8>, key: &KeyPair) -> Result<CertSerial, rustls::Error> {
    let chain = vec![
        CertificateDer::from(leaf_der),
        CertificateDer::from(env.intermediate.certificate().der.clone()),
    ];
    env.handshake_with(Some((chain, key.serialize_der())), env.verifier())
}

#[test]
fn forged_leaf_with_correct_parameters_passes_control() {
    // 对照组：forge_leaf 的默认参数与规格一致，应能握手；下面的反例只改一项。
    let env = Env::new(7);
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (serial, der) = forge_leaf(&env, |_| {}, &key);
    assert_eq!(try_forged(&env, der, &key).unwrap(), serial);
}

#[test]
fn server_auth_only_ca_leaf_bad_cn_and_non_p256_fail() {
    let env = Env::new(8);
    let p256 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();

    let (_, der) = forge_leaf(
        &env,
        |p| p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth],
        &p256,
    );
    assert!(try_forged(&env, der, &p256).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::BadEku));

    let (_, der) = forge_leaf(
        &env,
        |p| p.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained),
        &p256,
    );
    assert!(try_forged(&env, der, &p256).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::UnknownCa));

    for cn in ["laptop", "dev_test", "dev_01J0000000000000000000000B extra"] {
        let (_, der) = forge_leaf(
            &env,
            |p| {
                p.distinguished_name = rcgen::DistinguishedName::new();
                p.distinguished_name.push(DnType::CommonName, cn);
            },
            &p256,
        );
        assert!(try_forged(&env, der, &p256).is_err(), "{cn}");
        assert_eq!(
            env.rejections.last(),
            Some(RejectReason::BadIdentity),
            "{cn}"
        );
    }

    let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let (_, der) = forge_leaf(&env, |_| {}, &p384);
    assert!(try_forged(&env, der, &p384).is_err());
}

#[test]
fn revocation_takes_effect_on_the_next_handshake_for_every_serial() {
    let env = Env::new(9);
    let verifier = env.verifier();
    let first = env.pair(DEV_A, NOW_MS);
    // 续期：同一设备第二张证书，旧证书 superseded 但未到期。
    let second = env.pair(DEV_A, NOW_MS + MIN_MS);
    env.lookup
        .set_status(&first.cert.serial, DeviceCertStatus::Superseded);
    for device in [&first, &second] {
        let chain = vec![
            CertificateDer::from(device.cert.der.clone()),
            CertificateDer::from(env.intermediate.certificate().der.clone()),
        ];
        assert!(env
            .handshake_with(Some((chain, device.key_der.clone())), verifier.clone())
            .is_ok());
    }

    // 同一个 verifier 实例（无重启、无缓存刷新）：吊销后下一次握手即失败。
    env.lookup.revoke_device(&first.id);
    for device in [&first, &second] {
        let chain = vec![
            CertificateDer::from(device.cert.der.clone()),
            CertificateDer::from(env.intermediate.certificate().der.clone()),
        ];
        assert!(env
            .handshake_with(Some((chain, device.key_der.clone())), verifier.clone())
            .is_err());
        assert_eq!(env.rejections.last(), Some(RejectReason::Revoked));
    }
}

#[test]
fn superseded_certificate_is_rejected_after_its_original_not_after() {
    let env = Env::new(10);
    let device = env.pair(DEV_A, NOW_MS);
    env.lookup.put(
        &device.cert.serial,
        CertRecord {
            device_id: device.id,
            status: DeviceCertStatus::Superseded,
            not_after: Timestamp::from_millis(NOW_MS - 1000),
            device_active: true,
        },
    );
    assert!(env.handshake(Some(&device)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::Revoked));
}

#[test]
fn chain_valid_but_unknown_serial_fails_closed() {
    let env = Env::new(11);
    let device = env.pair(DEV_A, NOW_MS);
    env.lookup.rows.lock().unwrap().clear();
    assert!(env.handshake(Some(&device)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::UnknownSerial));

    // 查库失败同样拒绝。
    let device = env.pair(DEV_B, NOW_MS);
    *env.lookup.fail.lock().unwrap() = true;
    assert!(env.handshake(Some(&device)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::LookupFailed));
}

#[test]
fn serial_registered_to_another_device_or_inactive_device_fails() {
    let env = Env::new(12);
    let device = env.pair(DEV_A, NOW_MS);
    env.lookup.put(
        &device.cert.serial,
        CertRecord {
            device_id: DEV_B.parse().unwrap(),
            status: DeviceCertStatus::Active,
            not_after: device.cert.not_after,
            device_active: true,
        },
    );
    assert!(env.handshake(Some(&device)).is_err());
    env.lookup.put(
        &device.cert.serial,
        CertRecord {
            device_id: device.id,
            status: DeviceCertStatus::Active,
            not_after: device.cert.not_after,
            device_active: false,
        },
    );
    assert!(env.handshake(Some(&device)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::Revoked));
}

// ---------------------------------------------------------------------------
// 与 M0 TestCA 的一致性：TestCA 签出的合规叶子能通过本模块的 verifier；反例证书被拒
// ---------------------------------------------------------------------------

fn openssl_supports_explicit_dates() -> bool {
    let Ok(output) = std::process::Command::new("openssl")
        .args(["x509", "-help"])
        .output()
    else {
        return false;
    };
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    help.contains("-not_before") && help.contains("-not_after")
}

struct TestCaEnv {
    ca: TestCA,
    lookup: Arc<MemLookup>,
    rejections: Arc<Rejections>,
}

impl TestCaEnv {
    fn new() -> Self {
        Self {
            ca: TestCA::new().expect("openssl must be available for TestCA"),
            lookup: Arc::new(MemLookup::default()),
            rejections: Arc::new(Rejections::default()),
        }
    }

    fn der(pem_text: &str) -> Vec<u8> {
        pem::parse(pem_text).unwrap().into_contents()
    }

    fn issue(&self, spec: DeviceCertSpec) -> (Vec<CertificateDer<'static>>, Vec<u8>) {
        let cert = self.ca.issue_device_with(spec).unwrap();
        let leaf = Self::der(&cert.cert_pem);
        let issued = IssuedCert::from_der(leaf.clone()).unwrap();
        self.lookup.put(
            &issued.serial,
            CertRecord {
                device_id: cert
                    .common_name
                    .parse()
                    .unwrap_or_else(|_| DEV_A.parse().unwrap()),
                status: DeviceCertStatus::Active,
                not_after: issued.not_after,
                device_active: true,
            },
        );
        let blocks = pem::parse_many(&cert.chain_pem).unwrap();
        let chain = vec![
            CertificateDer::from(leaf),
            CertificateDer::from(blocks[1].contents().to_vec()),
        ];
        let key = PrivateKeyDer::from_pem_slice(cert.key_pem.as_bytes()).unwrap();
        (chain, key.secret_der().to_vec())
    }

    fn handshake(
        &self,
        client: (Vec<CertificateDer<'static>>, Vec<u8>),
    ) -> Result<(), rustls::Error> {
        let root_der = Self::der(self.ca.root_cert_pem());
        let verifier = Arc::new(
            DeviceCertVerifier::new(
                &root_der,
                &[Self::der(self.ca.intermediate_cert_pem())],
                self.lookup.clone(),
            )
            .unwrap()
            .with_observer(self.rejections.clone()),
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_chain: Vec<_> = pem::parse_many(format!(
            "{}{}",
            self.ca.server_cert_pem(),
            self.ca.intermediate_cert_pem()
        ))
        .unwrap()
        .into_iter()
        .map(|block| CertificateDer::from(block.into_contents()))
        .collect();
        let server_key =
            PrivateKeyDer::from_pem_slice(self.ca.server_key_pem().as_bytes()).unwrap();
        let server_config = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(server_chain, server_key)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(root_der)).unwrap();
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(client.0, PrivateKeyDer::try_from(client.1).unwrap())
            .unwrap();
        let server_name = server_name_of(self.ca.server_cert_pem());
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut client = ClientConnection::new(Arc::new(client_config), server_name).unwrap();
        pump(&mut client, &mut server)
    }
}

fn server_name_of(pem_text: &str) -> ServerName<'static> {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let der = pem::parse(pem_text).unwrap().into_contents();
    let (_, cert) = X509Certificate::from_der(&der).unwrap();
    let san = cert.subject_alternative_name().unwrap().unwrap();
    for name in &san.value.general_names {
        match name {
            GeneralName::DNSName(dns) => return ServerName::try_from(dns.to_string()).unwrap(),
            GeneralName::IPAddress(ip) if ip.len() == 4 => {
                let addr = std::net::Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]);
                return ServerName::IpAddress(std::net::IpAddr::V4(addr).into());
            }
            _ => {}
        }
    }
    panic!("TestCA server certificate has no usable SAN");
}

#[test]
fn test_ca_leaves_agree_with_pki_verifier() {
    let env = TestCaEnv::new();
    let good = DeviceCertSpec {
        common_name: DEV_A.into(),
        ..DeviceCertSpec::default()
    };
    env.handshake(env.issue(good.clone())).unwrap();

    let foreign = DeviceCertSpec {
        issuer: DeviceIssuer::ForeignRoot,
        ..good.clone()
    };
    assert!(env.handshake(env.issue(foreign)).is_err());

    let server_auth = DeviceCertSpec {
        eku: DeviceEku::ServerAuth,
        ..good.clone()
    };
    assert!(env.handshake(env.issue(server_auth)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::BadEku));

    // TestCA 默认 CN 是 "dev_test"，不是 dev_<ULID>：本模块按规格拒绝。
    let default_cn = DeviceCertSpec::default();
    assert!(env.handshake(env.issue(default_cn)).is_err());
    assert_eq!(env.rejections.last(), Some(RejectReason::BadIdentity));

    // 显式起止日期需要 OpenSSL 3.4+（#12）；Ubuntu 24.04 的 3.0 跳过这一段。
    if openssl_supports_explicit_dates() {
        let expired = DeviceCertSpec {
            not_before: Some("20200101000000Z".into()),
            not_after: Some("20200102000000Z".into()),
            ..good.clone()
        };
        assert!(env.handshake(env.issue(expired)).is_err());
        assert_eq!(env.rejections.last(), Some(RejectReason::Expired));
        let future = DeviceCertSpec {
            not_before: Some("20990101000000Z".into()),
            not_after: Some("20990102000000Z".into()),
            ..good
        };
        assert!(env.handshake(env.issue(future)).is_err());
        assert_eq!(env.rejections.last(), Some(RejectReason::NotYetValid));
    } else {
        eprintln!(
            "skip explicit-date TestCA leaves: openssl lacks -not_before/-not_after (needs 3.4+)"
        );
    }
}
