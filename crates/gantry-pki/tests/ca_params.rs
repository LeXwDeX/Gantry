//! pki.md §12.2「引导与证书参数（M1）」。

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gantry_core::ids::{DeviceId, Entropy};
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_pki::bootstrap::{self, BootstrapParams, MemoryFsProbe, OUTPUTS};
use gantry_pki::{fingerprint, Authority, CertPolicy, CertSerial, IssuedCert, P256PublicKey};
use p256::pkcs8::{DecodePrivateKey, EncodePublicKey};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::prelude::{FromDer, X509Certificate};

const NOW_MS: i64 = 1_798_761_600_000; // 2027-01-01T00:00:00Z
const DAY_S: i64 = 86_400;

struct AlwaysMemory;

impl MemoryFsProbe for AlwaysMemory {
    fn is_memory_fs(&self, _dir: &Path) -> io::Result<bool> {
        Ok(true)
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "pki-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn params(dir: &Path) -> BootstrapParams {
    BootstrapParams {
        server_names: vec!["gantry.example.test".into(), "10.0.0.5".into()],
        root_passphrase: SecretString::new("correct horse battery staple".into()),
        install_id: "inst-01".into(),
        out_dir: dir.to_path_buf(),
        policy: CertPolicy::default(),
    }
}

struct Bootstrapped {
    dir: PathBuf,
    report: bootstrap::BootstrapReport,
}

impl Bootstrapped {
    fn new() -> Self {
        let dir = temp_dir("boot");
        let report = bootstrap::run_with(
            params(&dir),
            &FakeClock::new(NOW_MS),
            &SeqEntropy::new(42),
            &AlwaysMemory,
        )
        .unwrap();
        Self { dir, report }
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join(name)).unwrap()
    }

    fn der(&self, name: &str) -> Vec<u8> {
        pem::parse(self.read(name)).unwrap().into_contents()
    }

    fn intermediate(&self) -> Authority {
        Authority::load(
            &self.read("intermediate.crt"),
            &self.read("intermediate.key"),
        )
        .unwrap()
    }
}

fn assert_p256_sha256(cert: &X509Certificate<'_>) {
    assert_eq!(
        cert.signature_algorithm.algorithm.to_id_string(),
        "1.2.840.10045.4.3.2",
        "ecdsa-with-SHA256"
    );
    let spki = &cert.tbs_certificate.subject_pki;
    assert_eq!(spki.algorithm.algorithm.to_id_string(), "1.2.840.10045.2.1");
    let curve = spki
        .algorithm
        .parameters
        .as_ref()
        .unwrap()
        .as_oid()
        .unwrap();
    assert_eq!(curve.to_id_string(), "1.2.840.10045.3.1.7");
}

fn basic_constraints(cert: &X509Certificate<'_>) -> (bool, Option<u32>, bool) {
    let ext = cert.basic_constraints().unwrap().unwrap();
    (ext.value.ca, ext.value.path_len_constraint, ext.critical)
}

fn validity_secs(cert: &X509Certificate<'_>) -> (i64, i64) {
    (
        cert.validity().not_before.timestamp(),
        cert.validity().not_after.timestamp(),
    )
}

fn eku_list(cert: &X509Certificate<'_>) -> Vec<&'static str> {
    let Some(ext) = cert.extended_key_usage().unwrap() else {
        return Vec::new();
    };
    let eku = ext.value;
    let mut out = Vec::new();
    if eku.server_auth {
        out.push("serverAuth");
    }
    if eku.client_auth {
        out.push("clientAuth");
    }
    if eku.any || eku.code_signing || eku.email_protection || eku.time_stamping || eku.ocsp_signing
    {
        out.push("other");
    }
    if !eku.other.is_empty() {
        out.push("other");
    }
    out
}

#[test]
fn bootstrap_produces_root_intermediate_server_with_spec_parameters() {
    let boot = Bootstrapped::new();
    let now_s = NOW_MS / 1000;
    let skew = 5 * 60;

    let root_der = boot.der("root.crt");
    let (_, root) = X509Certificate::from_der(&root_der).unwrap();
    assert_p256_sha256(&root);
    assert_eq!(root.subject(), root.issuer(), "self-signed");
    assert!(root.verify_signature(None).is_ok());
    assert_eq!(basic_constraints(&root), (true, Some(1), true));
    let ku = root.key_usage().unwrap().unwrap();
    assert!(ku.critical);
    assert!(ku.value.key_cert_sign() && ku.value.crl_sign());
    assert!(!ku.value.digital_signature());
    assert_eq!(
        validity_secs(&root),
        (now_s - skew, now_s + 3650 * DAY_S),
        "10 years"
    );
    assert_eq!(
        root.subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap(),
        "Gantry Root CA inst-01"
    );

    let inter_der = boot.der("intermediate.crt");
    let (_, inter) = X509Certificate::from_der(&inter_der).unwrap();
    assert_p256_sha256(&inter);
    assert_eq!(inter.issuer(), root.subject());
    assert!(inter
        .verify_signature(Some(&root.tbs_certificate.subject_pki))
        .is_ok());
    assert_eq!(basic_constraints(&inter), (true, Some(0), true));
    let ku = inter.key_usage().unwrap().unwrap();
    assert!(ku.value.key_cert_sign() && ku.value.crl_sign());
    assert_eq!(
        validity_secs(&inter),
        (now_s - skew, now_s + 730 * DAY_S),
        "2 years"
    );
    assert_eq!(
        inter
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap(),
        "Gantry Intermediate CA 1"
    );

    let server_der = boot.der("server.crt");
    let (_, server) = X509Certificate::from_der(&server_der).unwrap();
    assert_p256_sha256(&server);
    assert_eq!(server.issuer(), inter.subject());
    assert!(server
        .verify_signature(Some(&inter.tbs_certificate.subject_pki))
        .is_ok());
    assert!(!basic_constraints(&server).0);
    assert!(server
        .key_usage()
        .unwrap()
        .unwrap()
        .value
        .digital_signature());
    assert_eq!(eku_list(&server), vec!["serverAuth"]);
    assert_eq!(
        validity_secs(&server),
        (now_s - skew, now_s + 90 * DAY_S),
        "90 days"
    );
    assert_eq!(
        server
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap(),
        "gantry.example.test"
    );
    let san = server.subject_alternative_name().unwrap().unwrap();
    let names: Vec<String> = san
        .value
        .general_names
        .iter()
        .map(|name| match name {
            GeneralName::DNSName(dns) => format!("dns:{dns}"),
            GeneralName::IPAddress(ip) => format!("ip:{ip:?}"),
            other => format!("other:{other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["dns:gantry.example.test", "ip:[10, 0, 0, 5]"]);

    assert_eq!(
        boot.report.root_fingerprint,
        fingerprint(&root_der).to_string()
    );
    assert_eq!(
        boot.report.intermediate_serial,
        CertSerial::from_be_bytes(inter.raw_serial())
            .unwrap()
            .to_string()
    );
    assert_eq!(
        boot.report.server_cert_serial,
        CertSerial::from_be_bytes(server.raw_serial())
            .unwrap()
            .to_string()
    );
}

#[test]
fn bootstrap_writes_only_the_six_outputs_with_private_modes() {
    let boot = Bootstrapped::new();
    let mut names: Vec<String> = fs::read_dir(&boot.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut expected: Vec<String> = OUTPUTS.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(names, expected);

    #[cfg(unix)]
    for name in OUTPUTS {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(boot.dir.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }

    // 根私钥只以加密形态出现。
    for name in OUTPUTS {
        let text = boot.read(name);
        assert!(!text.contains("BEGIN PRIVATE KEY") || name != "root.key.enc");
        assert!(!text.contains("BEGIN EC PRIVATE KEY"));
    }
    assert!(boot
        .read("root.key.enc")
        .contains("BEGIN ENCRYPTED PRIVATE KEY"));
}

#[test]
fn root_key_requires_passphrase_and_matches_root_certificate() {
    let boot = Bootstrapped::new();
    let enc = boot.read("root.key.enc");
    assert!(p256::SecretKey::from_pkcs8_encrypted_pem(&enc, b"wrong").is_err());
    let key =
        p256::SecretKey::from_pkcs8_encrypted_pem(&enc, b"correct horse battery staple").unwrap();
    let spki = key.public_key().to_public_key_der().unwrap();
    let root_der = boot.der("root.crt");
    let (_, root) = X509Certificate::from_der(&root_der).unwrap();
    assert_eq!(spki.as_bytes(), root.tbs_certificate.subject_pki.raw);
}

#[test]
fn bootstrap_rejects_non_memory_out_dir_and_existing_outputs() {
    // CARGO_TARGET_TMPDIR 在磁盘上：真实探测必须拒绝。
    let disk = temp_dir("disk");
    let err =
        bootstrap::run(params(&disk), &FakeClock::new(NOW_MS), &SeqEntropy::new(1)).unwrap_err();
    assert!(
        matches!(err, gantry_pki::PkiError::NotMemoryFs(_)),
        "{err:?}"
    );
    assert_eq!(fs::read_dir(&disk).unwrap().count(), 0, "nothing written");

    let dir = temp_dir("exists");
    fs::write(dir.join("server.crt"), "old").unwrap();
    let err = bootstrap::run_with(
        params(&dir),
        &FakeClock::new(NOW_MS),
        &SeqEntropy::new(1),
        &AlwaysMemory,
    )
    .unwrap_err();
    assert!(
        matches!(err, gantry_pki::PkiError::OutputExists(_)),
        "{err:?}"
    );
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        1,
        "nothing else written"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn statfs_probe_recognises_dev_shm_as_memory_fs() {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        assert!(bootstrap::StatfsProbe.is_memory_fs(shm).unwrap());
    }
}

fn device_key(entropy: &dyn Entropy) -> P256PublicKey {
    let mut seed = [0u8; 32];
    entropy.fill(&mut seed);
    seed[0] = 0x01; // 保证落在曲线阶之内
    let secret = p256::SecretKey::from_slice(&seed).unwrap();
    P256PublicKey::from_spki_der(secret.public_key().to_public_key_der().unwrap().as_bytes())
        .unwrap()
}

fn issue_device(
    inter: &Authority,
    entropy: &dyn Entropy,
    now: Timestamp,
) -> (DeviceId, IssuedCert) {
    let id: DeviceId = "dev_01J0000000000000000000000A".parse().unwrap();
    let key = device_key(entropy);
    let serial = CertSerial::generate(entropy);
    let cert = inter
        .issue_device_leaf(&id, &key, &serial, now, &CertPolicy::default())
        .unwrap();
    assert_eq!(cert.serial, serial);
    (id, cert)
}

#[test]
fn device_leaf_has_client_auth_only_and_no_san() {
    let boot = Bootstrapped::new();
    let inter = boot.intermediate();
    let entropy = SeqEntropy::new(9);
    let now = Timestamp::from_millis(NOW_MS + 1234);
    let (id, issued) = issue_device(&inter, &entropy, now);

    let (_, cert) = X509Certificate::from_der(&issued.der).unwrap();
    assert_p256_sha256(&cert);
    assert!(!basic_constraints(&cert).0);
    assert!(cert.key_usage().unwrap().unwrap().value.digital_signature());
    assert_eq!(eku_list(&cert), vec!["clientAuth"]);
    assert!(cert.subject_alternative_name().unwrap().is_none(), "no SAN");
    assert_eq!(
        cert.subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap(),
        id.to_string()
    );
    let (nb, na) = validity_secs(&cert);
    assert_eq!(
        nb,
        NOW_MS / 1000 + 1 - 5 * 60,
        "notBefore = now − 5 min (秒精度)"
    );
    assert_eq!(na, NOW_MS / 1000 + 1 + 30 * DAY_S);
    assert!(na <= inter.certificate().not_after.as_millis() / 1000);
    assert_eq!(issued.not_before.as_millis(), nb * 1000);
    assert_eq!(issued.not_after.as_millis(), na * 1000);

    // 链 [叶子, 中间] 可验证到根。
    let (_, inter_cert) = X509Certificate::from_der(&inter.certificate().der).unwrap();
    let root_der = boot.der("root.crt");
    let (_, root) = X509Certificate::from_der(&root_der).unwrap();
    assert!(cert
        .verify_signature(Some(&inter_cert.tbs_certificate.subject_pki))
        .is_ok());
    assert!(inter_cert
        .verify_signature(Some(&root.tbs_certificate.subject_pki))
        .is_ok());

    // 序列号为正：DER 首字节最高位为 0。
    assert_eq!(cert.raw_serial()[0] & 0x80, 0);
    let extensions: Vec<_> = cert
        .extensions()
        .iter()
        .map(|ext| ext.parsed_extension())
        .collect();
    assert!(!extensions
        .iter()
        .any(|ext| matches!(ext, ParsedExtension::SubjectAlternativeName(_))));
}

#[test]
fn device_leaf_not_after_is_capped_by_intermediate() {
    let boot = Bootstrapped::new();
    let inter = boot.intermediate();
    let entropy = SeqEntropy::new(3);
    // 中间 CA 剩 10 天时签发：30 天有效期被截到中间 CA 的 notAfter。
    let late =
        Timestamp::from_millis(inter.certificate().not_after.as_millis() - 10 * DAY_S * 1000);
    let (_, issued) = issue_device(&inter, &entropy, late);
    assert_eq!(issued.not_after, inter.certificate().not_after);
}

#[test]
fn generated_serials_are_unique_and_positive() {
    let entropy = SeqEntropy::new(11);
    let serials: HashSet<CertSerial> = (0..512).map(|_| CertSerial::generate(&entropy)).collect();
    assert_eq!(serials.len(), 512);
    for serial in &serials {
        assert!(serial.to_be_bytes()[0] & 0x80 == 0);
    }
}

#[test]
fn fingerprints_have_sha256_prefix_and_cover_der() {
    let boot = Bootstrapped::new();
    let inter = boot.intermediate();
    let entropy = SeqEntropy::new(5);
    let key = device_key(&entropy);
    let fpr = key.fingerprint().to_string();
    assert!(fpr.starts_with("sha256:"));
    assert_eq!(fpr.len(), 7 + 64);
    assert!(fpr[7..]
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));

    // pubkey_fpr = 设备证书里 SPKI DER 的 SHA-256。
    let serial = CertSerial::generate(&entropy);
    let id: DeviceId = "dev_01J0000000000000000000000B".parse().unwrap();
    let issued = inter
        .issue_device_leaf(
            &id,
            &key,
            &serial,
            Timestamp::from_millis(NOW_MS),
            &CertPolicy::default(),
        )
        .unwrap();
    let (_, cert) = X509Certificate::from_der(&issued.der).unwrap();
    assert_eq!(
        fingerprint(cert.tbs_certificate.subject_pki.raw),
        key.fingerprint()
    );

    // ca_fingerprint = 根证书 DER 的 SHA-256。
    let root_der = boot.der("root.crt");
    assert_eq!(
        boot.report.root_fingerprint,
        fingerprint(&root_der).to_string()
    );
}

#[test]
fn authority_load_rejects_mismatched_key_and_leaf_certificates() {
    let a = Bootstrapped::new();
    let b_dir = temp_dir("boot-b");
    bootstrap::run_with(
        params(&b_dir),
        &FakeClock::new(NOW_MS),
        &SeqEntropy::new(43),
        &AlwaysMemory,
    )
    .unwrap();
    let other_key = fs::read_to_string(b_dir.join("intermediate.key")).unwrap();
    assert!(Authority::load(&a.read("intermediate.crt"), &other_key).is_err());
    assert!(Authority::load(&a.read("server.crt"), &a.read("server.key")).is_err());
    assert!(Authority::load(&a.read("intermediate.crt"), &a.read("intermediate.key")).is_ok());
}

#[test]
fn server_leaf_rejects_bad_names() {
    let boot = Bootstrapped::new();
    let inter = boot.intermediate();
    let entropy = SeqEntropy::new(8);
    let now = Timestamp::from_millis(NOW_MS);
    let policy = CertPolicy::default();
    assert!(inter
        .issue_server_leaf(&[], now, &policy, &entropy)
        .is_err());
    for bad in ["", "bad name", "-x.test", "a..b", "*.example.test"] {
        assert!(
            inter
                .issue_server_leaf(&[bad.to_string()], now, &policy, &entropy)
                .is_err(),
            "{bad}"
        );
    }
}
