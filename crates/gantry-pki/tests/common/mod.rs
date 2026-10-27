//! pki 集成测试共用的环境：bootstrap 出的 CA、内存 store、配对服务、设备目录与设备管理。

#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DevicePlatform};
use gantry_core::events::{DeviceRevokedData, EventType, NewEvent, Subject};
use gantry_core::ids::IdGen;
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_pki::admin::StoreAdmin;
use gantry_pki::bootstrap::{self, BootstrapParams, MemoryFsProbe};
use gantry_pki::csr::encode_approval_pubkey;
use gantry_pki::directory::StoreDirectory;
use gantry_pki::journal::RevocationJournal;
use gantry_pki::pairing::SessionTable;
use gantry_pki::pairing_service::{
    Issuer, NewDevice, PairRequest, PairResponse, PairingConfig, PairingService,
};
use gantry_pki::store_schema::{
    self, DeviceCertRow, DEVICE_CERTS, MODULE, PAIRING_SESSIONS, SESSIONS_KEY,
};
use gantry_pki::{Authority, CertPolicy, IssuedCert, P256PublicKey};
use gantry_store::{Store, StoreDeps};
use p256::pkcs8::EncodePublicKey;
use rcgen::{CertificateParams, KeyPair};

pub const NOW_MS: i64 = 1_798_761_600_000; // 2027-01-01T00:00:00Z
pub const MIN_MS: i64 = 60_000;
pub const DAY_MS: i64 = 86_400_000;
pub const API: &str = "https://gantry.example.test:8443";

pub struct AlwaysMemory;

impl MemoryFsProbe for AlwaysMemory {
    fn is_memory_fs(&self, _dir: &Path) -> io::Result<bool> {
        Ok(true)
    }
}

pub fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "pki-registry-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn fixture(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/csr/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

pub struct Env {
    pub store: Store,
    pub clock: Arc<FakeClock>,
    pub pairing: PairingService,
    pub directory: StoreDirectory,
    pub admin: StoreAdmin,
    pub journal: RevocationJournal,
    pub root: IssuedCert,
}

impl Env {
    pub async fn new() -> Self {
        let dir = temp_dir();
        bootstrap::run_with(
            BootstrapParams {
                server_names: vec!["gantry.example.test".into()],
                root_passphrase: SecretString::new("pw".into()),
                install_id: "registry".into(),
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
        let root =
            IssuedCert::from_der(pem::parse(read("root.crt")).unwrap().into_contents()).unwrap();
        let _ = fs::remove_dir_all(&dir);
        let journal_dir = temp_dir();
        let journal = RevocationJournal::new(
            journal_dir.join("revocations.journal"),
            journal_dir.join("backup.journal"),
        );

        let clock = Arc::new(FakeClock::new(NOW_MS));
        let entropy = Arc::new(SeqEntropy::new(42));
        let store = Store::open_in_memory(
            StoreDeps {
                clock: clock.clone(),
                ids: Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7)))),
            },
            &[store_schema::module_spec()],
        )
        .await
        .unwrap();
        let issuer = Arc::new(Issuer::new(intermediate, root.clone()));
        let pairing = PairingService::new(
            store.clone(),
            clock.clone(),
            entropy.clone(),
            issuer.clone(),
            PairingConfig::new(API),
        );
        let admin = StoreAdmin::new(
            store.clone(),
            clock.clone(),
            entropy,
            issuer,
            journal.clone(),
            CertPolicy::default(),
        );
        let directory = StoreDirectory::new(store.clone(), clock.clone());
        Self {
            store,
            clock,
            pairing,
            directory,
            admin,
            journal,
            root,
        }
    }

    pub fn advance(&self, ms: i64) {
        let now = self.clock.now_timestamp().as_millis();
        self.clock.set(Timestamp::from_millis(now + ms));
    }

    pub async fn token(&self) -> String {
        self.pairing
            .open_session(0)
            .await
            .unwrap()
            .token
            .expose_secret()
            .to_owned()
    }

    pub async fn pair(&self, platform: DevicePlatform) -> PairResponse {
        let token = self.token().await;
        self.pairing
            .redeem(Keys::new().request(&token, platform))
            .await
            .unwrap()
    }

    pub async fn sessions(&self) -> SessionTable {
        self.store
            .snapshot()
            .await
            .unwrap()
            .module_conn(MODULE)
            .get(PAIRING_SESSIONS, SESSIONS_KEY)
            .unwrap()
            .unwrap()
    }

    pub async fn cert_row(&self, serial: &str) -> Option<DeviceCertRow> {
        self.store
            .snapshot()
            .await
            .unwrap()
            .module_conn(MODULE)
            .get(DEVICE_CERTS, serial)
            .unwrap()
    }

    pub async fn revoke_event(&self, device: &PairResponse) {
        let id = device.device.id;
        let mut tx = self.store.begin_write().await.unwrap();
        tx.append(NewEvent {
            ty: EventType::DeviceRevoked,
            subject: Subject::parse(&id.to_string()).unwrap(),
            project_id: None,
            actor: Actor::new(ActorKind::System, "pki"),
            causation_id: None,
            correlation_id: None,
            data: serde_json::to_value(DeviceRevokedData {
                device_id: id,
                cert_serials: vec![device.device.cert_serial.clone()],
                remaining_approvers: 0,
                via: None,
            })
            .unwrap(),
            dedup_key: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

/// 设备端的两把密钥：mTLS 客户端密钥（出 CSR）与审批签名密钥。
pub struct Keys {
    pub client: KeyPair,
    pub approval: P256PublicKey,
}

impl Keys {
    pub fn new() -> Self {
        let secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let approval = P256PublicKey::from_spki_der(
            secret.public_key().to_public_key_der().unwrap().as_bytes(),
        )
        .unwrap();
        Self {
            client: KeyPair::generate().unwrap(),
            approval,
        }
    }

    pub fn csr(&self) -> String {
        CertificateParams::default()
            .serialize_request(&self.client)
            .unwrap()
            .pem()
            .unwrap()
    }

    pub fn client_public(&self) -> P256PublicKey {
        P256PublicKey::from_spki_der(&self.client.public_key_der()).unwrap()
    }

    pub fn request(&self, token: &str, platform: DevicePlatform) -> PairRequest {
        PairRequest {
            token: token.to_owned(),
            csr_pem: self.csr(),
            approval_pubkey: Some(encode_approval_pubkey(&self.approval)),
            device: NewDevice {
                name: "Lex 的 MacBook".into(),
                platform,
            },
        }
    }
}
