//! 真实 `pki` 的测试环境（本文 §12.1：组件测试用真实 pki）：bootstrap 出的 CA、
//! 内存 store（注册 pki 模块表）、配对服务、设备目录与设备管理，装进 `ApiCore`。
//! 做法与 `gantry-pki/tests/common` 相同。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gantry_api::idempotency::MemoryIdempotency;
use gantry_api::{ApiConfig, ApiCore, PeerCert, RequestHead, Services};
use gantry_core::entities::DevicePlatform;
use gantry_core::ids::IdGen;
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_pki::admin::StoreAdmin;
use gantry_pki::bootstrap::{self, BootstrapParams, MemoryFsProbe};
use gantry_pki::csr::encode_approval_pubkey;
use gantry_pki::directory::StoreDirectory;
use gantry_pki::journal::RevocationJournal;
use gantry_pki::pairing_service::{
    Issuer, NewDevice, PairRequest, PairResponse, PairingConfig, PairingService,
};
use gantry_pki::{store_schema, Authority, CertPolicy, IssuedCert, P256PublicKey};
use gantry_store::{Store, StoreDeps};
use http::Method;
use p256::pkcs8::EncodePublicKey;
use rcgen::{CertificateParams, KeyPair};

pub const NOW_MS: i64 = 1_798_761_600_000; // 2027-01-01T00:00:00Z
pub const DAY_MS: i64 = 86_400_000;

struct AlwaysMemory;

impl MemoryFsProbe for AlwaysMemory {
    fn is_memory_fs(&self, _dir: &Path) -> io::Result<bool> {
        Ok(true)
    }
}

fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "api-pki-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 设备端的两把密钥：mTLS 客户端密钥（出 CSR）与审批签名密钥（P-47）。
pub struct Keys {
    pub client: KeyPair,
    pub approval: p256::SecretKey,
}

impl Keys {
    pub fn new() -> Self {
        Self {
            client: KeyPair::generate().unwrap(),
            approval: p256::SecretKey::random(&mut rand_core::OsRng),
        }
    }

    pub fn csr(&self) -> String {
        CertificateParams::default()
            .serialize_request(&self.client)
            .unwrap()
            .pem()
            .unwrap()
    }

    pub fn approval_public(&self) -> P256PublicKey {
        let der = self.approval.public_key().to_public_key_der().unwrap();
        P256PublicKey::from_spki_der(der.as_bytes()).unwrap()
    }
}

/// 一台已配对的设备与它出示的证书。
pub struct Paired {
    pub response: PairResponse,
    pub keys: Keys,
    pub peer: PeerCert,
}

impl Paired {
    pub fn req(&self, method: Method, uri: &str) -> RequestHead {
        RequestHead::new(method, uri, Some(self.peer.clone()))
    }
}

pub struct PkiEnv {
    pub core: ApiCore,
    pub store: Store,
    pub clock: Arc<FakeClock>,
    pub pairing: PairingService,
    pub admin: Arc<StoreAdmin>,
    pub root_pem: String,
    pub forge: Arc<gantry_fakes::forge::FakeForge>,
    pub policy: Arc<gantry_policy::PolicyEngine>,
}

/// 由 PEM 叶子证书得到握手层交给 api 的字段。
pub fn peer_of(leaf_pem: &str) -> PeerCert {
    let der = pem::parse(leaf_pem).unwrap().into_contents();
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
    let serial = gantry_pki::CertSerial::from_be_bytes(cert.raw_serial()).unwrap();
    let ms = |t: x509_parser::time::ASN1Time| t.timestamp() * 1000;
    PeerCert {
        serial: serial.as_str().to_owned(),
        not_before: Timestamp::from_millis(ms(cert.validity().not_before)),
        not_after: Timestamp::from_millis(ms(cert.validity().not_after)),
    }
}

impl PkiEnv {
    pub async fn new() -> Self {
        let dir = temp_dir();
        bootstrap::run_with(
            BootstrapParams {
                server_names: vec!["gantry.example.test".into()],
                root_passphrase: SecretString::new("pw".into()),
                install_id: "api".into(),
                out_dir: dir.clone(),
                policy: CertPolicy::default(),
            },
            &FakeClock::new(NOW_MS - DAY_MS),
            &SeqEntropy::new(1),
            &AlwaysMemory,
        )
        .unwrap();
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap();
        let intermediate =
            Authority::load(&read("intermediate.crt"), &read("intermediate.key")).unwrap();
        let root_pem = read("root.crt");
        let root = IssuedCert::from_der(pem::parse(&root_pem).unwrap().into_contents()).unwrap();
        let _ = fs::remove_dir_all(&dir);
        let journal_dir = temp_dir();
        let journal = RevocationJournal::new(
            journal_dir.join("revocations.journal"),
            journal_dir.join("backup.journal"),
        );

        let clock = Arc::new(FakeClock::new(NOW_MS));
        let entropy = Arc::new(SeqEntropy::new(42));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
        let store = Store::open_in_memory(
            StoreDeps {
                clock: clock.clone(),
                ids: ids.clone(),
            },
            &[store_schema::module_spec(), gantry_policy::POLICY_MODULE],
        )
        .await
        .unwrap();
        let issuer = Arc::new(Issuer::new(intermediate, root));
        let pairing = PairingService::new(
            store.clone(),
            clock.clone(),
            entropy.clone(),
            issuer.clone(),
            PairingConfig::new("https://gantry.example.test:8443"),
        );
        let admin = Arc::new(StoreAdmin::new(
            store.clone(),
            clock.clone(),
            entropy,
            issuer,
            journal,
            CertPolicy::default(),
        ));
        let directory = Arc::new(StoreDirectory::new(store.clone(), clock.clone()));
        let forge = Arc::new(gantry_fakes::forge::FakeForge::new_in(store.clone()));
        let (policy, approvals) =
            super::policy_approvals(&store, clock.clone(), ids.clone(), forge.clone());
        let svc = Services {
            clock: clock.clone(),
            ids: ids.clone(),
            store: store.clone(),
            devices: directory,
            admin: admin.clone(),
            approvals: approvals.clone(),
            forge: forge.clone(),
            engine: Arc::new(gantry_fakes::engine::FakeEngine::new()),
            conversation: Arc::new(gantry_fakes::conversation::FakeConversation::new()),
            idempotency: Arc::new(MemoryIdempotency::new(clock.clone())),
        };
        Self {
            core: ApiCore::new(ApiConfig::default(), svc),
            store,
            clock,
            pairing,
            admin,
            root_pem,
            forge,
            policy,
        }
    }

    pub async fn pair(&self, platform: DevicePlatform, name: &str) -> Paired {
        let token = self
            .pairing
            .open_session(0)
            .await
            .unwrap()
            .token
            .expose_secret()
            .to_owned();
        let keys = Keys::new();
        let response = self
            .pairing
            .redeem(PairRequest {
                token,
                csr_pem: keys.csr(),
                approval_pubkey: (platform != DevicePlatform::Ios)
                    .then(|| encode_approval_pubkey(&keys.approval_public())),
                device: NewDevice {
                    name: name.into(),
                    platform,
                },
            })
            .await
            .unwrap();
        let peer = peer_of(&response.certificate_chain[0]);
        Paired {
            response,
            keys,
            peer,
        }
    }
}
