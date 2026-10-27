//! pki.md §12.2「配对」与「设备管理」中依赖 `store` 的部分：`redeem` 单一写事务、
//! 首台角色与引导期（D-18）、令牌规则、不可信字段、配对窗口，以及设备目录
//! （`by_cert_serial`、`verifying_key`、`last_seen` 节流、握手在册查询）。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::events::{DeviceRevokedData, EventType, NewEvent, Subject};
use gantry_core::ids::{DeviceId, IdGen};
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_pki::bootstrap::{self, BootstrapParams, MemoryFsProbe};
use gantry_pki::csr::encode_approval_pubkey;
use gantry_pki::directory::{DeviceDirectory, StoreDirectory};
use gantry_pki::pairing::{SessionState, SessionTable, TokenInvalidReason};
use gantry_pki::pairing_service::{
    FieldError, Issuer, NewDevice, PairError, PairRequest, PairResponse, PairingConfig,
    PairingService,
};
use gantry_pki::store_schema::{
    self, CertRowStatus, DeviceCertRow, DeviceStateRow, DEVICE_CERTS, DEVICE_STATE, MODULE,
    PAIRING_SESSIONS, SESSIONS_KEY,
};
use gantry_pki::verifier::{CertStatusLookup, DeviceCertStatus};
use gantry_pki::{fingerprint, Authority, CertPolicy, CertSerial, IssuedCert, P256PublicKey};
use gantry_store::{Store, StoreDeps};
use p256::pkcs8::EncodePublicKey;
use rcgen::{CertificateParams, KeyPair};

const NOW_MS: i64 = 1_798_761_600_000; // 2027-01-01T00:00:00Z
const MIN_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
const API: &str = "https://gantry.example.test:8443";

struct AlwaysMemory;

impl MemoryFsProbe for AlwaysMemory {
    fn is_memory_fs(&self, _dir: &Path) -> io::Result<bool> {
        Ok(true)
    }
}

fn temp_dir() -> PathBuf {
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

fn fixture(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/csr/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

struct Env {
    store: Store,
    clock: Arc<FakeClock>,
    pairing: PairingService,
    directory: StoreDirectory,
    root: IssuedCert,
}

impl Env {
    async fn new() -> Self {
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
        let pairing = PairingService::new(
            store.clone(),
            clock.clone(),
            entropy,
            Arc::new(Issuer::new(intermediate, root.clone())),
            PairingConfig::new(API),
        );
        let directory = StoreDirectory::new(store.clone(), clock.clone());
        Self {
            store,
            clock,
            pairing,
            directory,
            root,
        }
    }

    fn advance(&self, ms: i64) {
        let now = self.clock.now_timestamp().as_millis();
        self.clock.set(Timestamp::from_millis(now + ms));
    }

    async fn token(&self) -> String {
        self.pairing
            .open_session(0)
            .await
            .unwrap()
            .token
            .expose_secret()
            .to_owned()
    }

    async fn pair(&self, platform: DevicePlatform) -> PairResponse {
        let token = self.token().await;
        self.pairing
            .redeem(Keys::new().request(&token, platform))
            .await
            .unwrap()
    }

    async fn sessions(&self) -> SessionTable {
        self.store
            .snapshot()
            .await
            .unwrap()
            .module_conn(MODULE)
            .get(PAIRING_SESSIONS, SESSIONS_KEY)
            .unwrap()
            .unwrap()
    }

    async fn cert_row(&self, serial: &str) -> Option<DeviceCertRow> {
        self.store
            .snapshot()
            .await
            .unwrap()
            .module_conn(MODULE)
            .get(DEVICE_CERTS, serial)
            .unwrap()
    }

    async fn revoke_event(&self, device: &PairResponse) {
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
struct Keys {
    client: KeyPair,
    approval: P256PublicKey,
}

impl Keys {
    fn new() -> Self {
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

    fn csr(&self) -> String {
        CertificateParams::default()
            .serialize_request(&self.client)
            .unwrap()
            .pem()
            .unwrap()
    }

    fn client_public(&self) -> P256PublicKey {
        P256PublicKey::from_spki_der(&self.client.public_key_der()).unwrap()
    }

    fn request(&self, token: &str, platform: DevicePlatform) -> PairRequest {
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

fn field_errors(result: Result<PairResponse, PairError>) -> Vec<FieldError> {
    match result {
        Err(PairError::Validation(errors)) => errors,
        other => panic!("expected validation error, got {other:?}"),
    }
}

fn token_reason(result: Result<PairResponse, PairError>) -> TokenInvalidReason {
    match result {
        Err(PairError::TokenInvalid(reason)) => reason,
        other => panic!("expected token invalid, got {other:?}"),
    }
}

fn has(errors: &[FieldError], pointer: &str, code: &str) -> bool {
    errors
        .iter()
        .any(|error| error.pointer == pointer && error.code == code)
}

// ---------------------------------------------------------------------------
// 成功路径
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redeem_issues_chain_and_writes_device_rows_in_one_transaction() {
    let env = Env::new().await;
    let keys = Keys::new();
    let ticket = env.pairing.open_session(1000).await.unwrap();
    let response = env
        .pairing
        .redeem(keys.request(ticket.token.expose_secret(), DevicePlatform::Macos))
        .await
        .unwrap();

    // 响应：链三张，末张就是离线根，指纹等于 ca_fingerprint。
    let ca_fpr = format!("sha256:{}", env.root.fingerprint().to_hex());
    assert_eq!(response.ca_fingerprint, ca_fpr);
    assert_eq!(ticket.ca_fingerprint, ca_fpr);
    assert_eq!(response.certificate_chain[2], env.root.pem);
    let leaf_der = pem::parse(&response.certificate_chain[0])
        .unwrap()
        .into_contents();
    let leaf = IssuedCert::from_der(leaf_der).unwrap();
    assert_eq!(leaf.serial.to_string(), response.device.cert_serial);
    assert_eq!(response.api_base_url, API);
    assert_eq!(response.confirmation_code.len(), 6);
    assert!(response
        .confirmation_code
        .bytes()
        .all(|b| b.is_ascii_digit()));

    // Device：事件派生列来自 store 的 devices 投影。
    let device = &response.device;
    assert_eq!(device.role, DeviceRole::Approver);
    assert_eq!(device.status, DeviceStatus::Active);
    assert_eq!(device.platform, DevicePlatform::Macos);
    assert_eq!(device.pubkey_fpr, keys.client_public().fingerprint());
    assert_eq!(
        device.approval_pubkey_fpr,
        Some(keys.approval.fingerprint())
    );
    assert!(device.id.to_string().starts_with("dev_"));

    // 恰好一条 device.paired（开会话不产生事件）。
    let mut snapshot = env.store.snapshot().await.unwrap();
    assert_eq!(snapshot.head_seq().as_u64(), 1);
    assert_eq!(
        snapshot.device(&device.id).await.unwrap().as_ref(),
        Some(device)
    );

    // 模块表：证书行 active、状态行带 SPKI；会话 redeemed 且确认码一致。
    let cert = env.cert_row(&device.cert_serial).await.unwrap();
    assert_eq!(cert.device_id, device.id);
    assert_eq!(cert.status, CertRowStatus::Active);
    assert_eq!(cert.not_after, leaf.not_after);
    let state: DeviceStateRow = snapshot
        .module_conn(MODULE)
        .get(DEVICE_STATE, &device.id.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(
        fingerprint(&base64_decode(&state.pubkey_spki)),
        device.pubkey_fpr
    );
    let session = env
        .sessions()
        .await
        .get(ticket.session_id)
        .cloned()
        .unwrap();
    assert_eq!(session.state, SessionState::Redeemed);
    assert_eq!(session.redeemed_device_id, Some(device.id));
    assert_eq!(
        session.confirmation_code.as_deref(),
        Some(response.confirmation_code.as_str())
    );
    assert_eq!(session.created_by_uid, 1000);
    assert_eq!(
        env.pairing.session(ticket.session_id).await.unwrap(),
        Some(session)
    );
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .unwrap()
}

// ---------------------------------------------------------------------------
// 首台角色与引导期（D-18）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_desktop_is_approver_and_later_devices_are_operators() {
    let env = Env::new().await;
    assert!(env.directory.bootstrap_period().await.unwrap());
    let first = env.pair(DevicePlatform::Macos).await;
    let second = env.pair(DevicePlatform::Windows).await;
    let phone = env.pair(DevicePlatform::Ios).await;
    assert_eq!(first.device.role, DeviceRole::Approver);
    assert_eq!(second.device.role, DeviceRole::Operator);
    assert_eq!(phone.device.role, DeviceRole::Operator);
    assert!(!env.directory.bootstrap_period().await.unwrap());

    // 吊销全部设备后，引导期仍结束；新桌面设备是 operator（OQ-K11，只能 recover --promote）。
    for device in [&first, &second, &phone] {
        env.revoke_event(device).await;
    }
    assert!(!env.directory.bootstrap_period().await.unwrap());
    let after = env.pair(DevicePlatform::Macos).await;
    assert_eq!(after.device.role, DeviceRole::Operator);
}

#[tokio::test]
async fn phone_first_then_desktop_makes_the_desktop_approver() {
    let env = Env::new().await;
    let phone = env.pair(DevicePlatform::Ios).await;
    assert_eq!(phone.device.role, DeviceRole::Operator);
    // 只配过手机：引导期仍在持续。
    assert!(env.directory.bootstrap_period().await.unwrap());
    let desktop = env.pair(DevicePlatform::Windows).await;
    assert_eq!(desktop.device.role, DeviceRole::Approver);
    assert!(!env.directory.bootstrap_period().await.unwrap());
}

#[tokio::test]
async fn concurrent_first_pairings_yield_exactly_one_approver() {
    let env = Env::new().await;
    let token = env.token().await;
    let (a, b) = tokio::join!(
        env.pairing
            .redeem(Keys::new().request(&token, DevicePlatform::Macos)),
        env.pairing
            .redeem(Keys::new().request(&token, DevicePlatform::Windows)),
    );
    let ok: Vec<_> = [a, b].into_iter().filter_map(Result::ok).collect();
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].device.role, DeviceRole::Approver);
    let mut snapshot = env.store.snapshot().await.unwrap();
    let all = snapshot
        .devices(&gantry_store::Page::default())
        .await
        .unwrap()
        .items;
    assert_eq!(
        all.iter()
            .filter(|device| device.role == DeviceRole::Approver)
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// 令牌
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_is_single_use_expires_and_is_voided_after_five_failures() {
    let env = Env::new().await;
    let keys = Keys::new();

    // 单次有效。
    let token = env.token().await;
    env.pairing
        .redeem(keys.request(&token, DevicePlatform::Macos))
        .await
        .unwrap();
    let reused = env
        .pairing
        .redeem(Keys::new().request(&token, DevicePlatform::Macos))
        .await;
    assert!(matches!(reused, Err(PairError::TokenInvalid(_))));

    // 过期：推进 10 分钟。
    let token = env.token().await;
    env.advance(10 * MIN_MS);
    assert_eq!(
        token_reason(
            env.pairing
                .redeem(Keys::new().request(&token, DevicePlatform::Macos))
                .await
        ),
        TokenInvalidReason::Expired
    );

    // 5 次错误即作废，之后正确令牌也被拒；失败计数随各自的事务提交。
    let token = env.token().await;
    for _ in 0..4 {
        assert_eq!(
            token_reason(
                env.pairing
                    .redeem(Keys::new().request("gp1_wrong", DevicePlatform::Macos))
                    .await
            ),
            TokenInvalidReason::Mismatch
        );
    }
    assert_eq!(
        token_reason(
            env.pairing
                .redeem(Keys::new().request("gp1_wrong", DevicePlatform::Macos))
                .await
        ),
        TokenInvalidReason::Voided
    );
    assert!(matches!(
        env.pairing
            .redeem(Keys::new().request(&token, DevicePlatform::Macos))
            .await,
        Err(PairError::TokenInvalid(_))
    ));
}

#[tokio::test]
async fn new_session_voids_the_old_token() {
    let env = Env::new().await;
    let old = env.token().await;
    let new = env.token().await;
    assert!(matches!(
        env.pairing
            .redeem(Keys::new().request(&old, DevicePlatform::Macos))
            .await,
        Err(PairError::TokenInvalid(_))
    ));
    env.pairing
        .redeem(Keys::new().request(&new, DevicePlatform::Macos))
        .await
        .unwrap();
}

#[tokio::test]
async fn plaintext_token_is_never_stored() {
    let env = Env::new().await;
    let token = env.token().await;
    let table = env.sessions().await;
    let stored = serde_json::to_string(&table).unwrap();
    assert!(!stored.contains(&token));
    assert!(!stored.contains(token.trim_start_matches("gp1_")));
    env.pairing
        .redeem(Keys::new().request(&token, DevicePlatform::Macos))
        .await
        .unwrap();
    let stored = serde_json::to_string(&env.sessions().await).unwrap();
    assert!(!stored.contains(token.trim_start_matches("gp1_")));
}

// ---------------------------------------------------------------------------
// 不可信字段：失败为 422，令牌不消耗
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bad_csrs_are_rejected_without_consuming_the_token() {
    let env = Env::new().await;
    let token = env.token().await;
    let good = fixture("p256.csr");
    let mut broken = pem::parse(&good).unwrap().into_contents();
    let last = broken.len() - 1;
    broken[last] ^= 0x01;
    let cases = [
        fixture("rsa2048.csr"),
        fixture("ed25519.csr"),
        fixture("p384.csr"),
        pem::encode(&pem::Pem::new("CERTIFICATE REQUEST", broken)),
        format!("{good}{good}"),
        "x".repeat(5000),
    ];
    for csr_pem in cases {
        let mut request = Keys::new().request(&token, DevicePlatform::Macos);
        request.csr_pem = csr_pem;
        let errors = field_errors(env.pairing.redeem(request).await);
        assert!(has(&errors, "/csr", "invalid"), "{errors:?}");
    }
    // 令牌仍然有效。
    let paired = env
        .pairing
        .redeem(Keys::new().request(&token, DevicePlatform::Macos))
        .await
        .unwrap();

    // 公钥已属于 active 设备 → 拒绝；吊销后可再用（OQ-K9 只拒绝与 active 重复）。
    let keys = Keys::new();
    let first = env
        .pairing
        .redeem(keys.request(&env.token().await, DevicePlatform::Windows))
        .await
        .unwrap();
    let mut again = keys.request(&env.token().await, DevicePlatform::Windows);
    again.approval_pubkey = Some(encode_approval_pubkey(&Keys::new().approval));
    let errors = field_errors(env.pairing.redeem(again.clone()).await);
    assert!(has(&errors, "/csr", "duplicate_key"), "{errors:?}");
    env.revoke_event(&first).await;
    env.pairing.redeem(again).await.unwrap();
    assert_eq!(paired.device.role, DeviceRole::Approver);
}

#[tokio::test]
async fn bad_approval_pubkeys_are_rejected() {
    let env = Env::new().await;
    let token = env.token().await;
    let keys = Keys::new();

    let mut missing = keys.request(&token, DevicePlatform::Windows);
    missing.approval_pubkey = None;
    assert!(has(
        &field_errors(env.pairing.redeem(missing).await),
        "/approval_pubkey",
        "required"
    ));

    let mut same = keys.request(&token, DevicePlatform::Macos);
    same.approval_pubkey = Some(encode_approval_pubkey(&keys.client_public()));
    assert!(has(
        &field_errors(env.pairing.redeem(same).await),
        "/approval_pubkey",
        "same_as_csr_key"
    ));

    let point = keys.approval.uncompressed_point();
    let mut off_curve = point;
    off_curve[64] ^= 0x01;
    let mut compressed = vec![0x02 | (point[64] & 1)];
    compressed.extend_from_slice(&point[1..33]);
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    for value in [
        fixture("approval_p384.txt").trim().to_owned(),
        b64.encode(off_curve),
        b64.encode(&compressed),
        "not base64!".to_owned(),
    ] {
        let mut request = keys.request(&token, DevicePlatform::Macos);
        request.approval_pubkey = Some(value);
        assert!(has(
            &field_errors(env.pairing.redeem(request).await),
            "/approval_pubkey",
            "invalid"
        ));
    }

    // 与 active 设备已登记的审批公钥重复。
    let first = env
        .pairing
        .redeem(keys.request(&token, DevicePlatform::Macos))
        .await
        .unwrap();
    let mut dup = Keys::new().request(&env.token().await, DevicePlatform::Macos);
    dup.approval_pubkey = Some(encode_approval_pubkey(&keys.approval));
    assert!(has(
        &field_errors(env.pairing.redeem(dup).await),
        "/approval_pubkey",
        "duplicate_key"
    ));

    // 手机可以不带审批公钥（P-76），Device 不登记审批公钥。
    let mut phone = Keys::new().request(&env.token().await, DevicePlatform::Ios);
    phone.approval_pubkey = None;
    let phone = env.pairing.redeem(phone).await.unwrap();
    assert_eq!(phone.device.approval_pubkey_fpr, None);
    assert_eq!(
        env.directory.verifying_key(&phone.device.id).await.unwrap(),
        None
    );
    assert_eq!(first.device.role, DeviceRole::Approver);
}

#[tokio::test]
async fn bad_device_names_are_rejected() {
    let env = Env::new().await;
    let token = env.token().await;
    for name in ["", "a\nb", "\u{1b}[31mred", &"长".repeat(65)] {
        let mut request = Keys::new().request(&token, DevicePlatform::Macos);
        request.device.name = name.to_owned();
        assert!(has(
            &field_errors(env.pairing.redeem(request).await),
            "/device/name",
            "invalid"
        ));
    }
}

// ---------------------------------------------------------------------------
// 配对窗口
// ---------------------------------------------------------------------------

#[tokio::test]
async fn window_opens_with_a_session_and_closes_on_redeem_cancel_or_expiry() {
    let env = Env::new().await;
    let window = env.pairing.window();
    assert_eq!(*window.borrow(), None);

    let token = env.token().await;
    assert!(window.borrow().is_some());
    env.pairing
        .redeem(Keys::new().request(&token, DevicePlatform::Macos))
        .await
        .unwrap();
    assert_eq!(*window.borrow(), None);

    let ticket = env.pairing.open_session(0).await.unwrap();
    assert_eq!(
        window.borrow().map(|w| w.session_id),
        Some(ticket.session_id)
    );
    env.pairing.cancel_session(ticket.session_id).await.unwrap();
    assert_eq!(*window.borrow(), None);

    env.pairing.open_session(0).await.unwrap();
    env.advance(10 * MIN_MS);
    env.pairing.expire_sessions().await.unwrap();
    assert_eq!(*window.borrow(), None);
    let table = env.sessions().await;
    assert!(table
        .sessions()
        .iter()
        .all(|session| session.state != SessionState::Open));
}

// ---------------------------------------------------------------------------
// 设备目录
// ---------------------------------------------------------------------------

#[tokio::test]
async fn directory_maps_serials_and_returns_the_approval_key() {
    let env = Env::new().await;
    let keys = Keys::new();
    let paired = env
        .pairing
        .redeem(keys.request(&env.token().await, DevicePlatform::Macos))
        .await
        .unwrap();
    let serial: CertSerial = paired.device.cert_serial.parse().unwrap();

    let found = env
        .directory
        .by_cert_serial(&serial)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, paired.device.id);
    let unknown: CertSerial = "7f".parse().unwrap();
    assert_eq!(env.directory.by_cert_serial(&unknown).await.unwrap(), None);

    // verifying_key 是审批公钥，不是 mTLS 公钥。
    let key = env
        .directory
        .verifying_key(&paired.device.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        key.to_encoded_point(false).as_bytes(),
        keys.approval.uncompressed_point().as_slice()
    );

    // 吊销后：verifying_key 为 None；by_cert_serial 仍返回设备（status = revoked）。
    env.revoke_event(&paired).await;
    assert_eq!(
        env.directory
            .verifying_key(&paired.device.id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        env.directory
            .by_cert_serial(&serial)
            .await
            .unwrap()
            .map(|device| device.status),
        Some(DeviceStatus::Revoked)
    );
    let list = env
        .directory
        .list(&gantry_store::Page::default())
        .await
        .unwrap();
    assert_eq!(list.items.len(), 1);
}

#[tokio::test]
async fn handshake_lookup_reads_the_cert_table_without_caching() {
    let env = Env::new().await;
    let paired = env.pair(DevicePlatform::Macos).await;
    let serial: CertSerial = paired.device.cert_serial.parse().unwrap();

    let record = env.directory.lookup(&serial).unwrap().unwrap();
    assert_eq!(record.device_id, paired.device.id);
    assert_eq!(record.status, DeviceCertStatus::Active);
    assert!(record.device_active);
    assert_eq!(env.directory.lookup(&"7f".parse().unwrap()).unwrap(), None);

    // 证书被取代：握手查询照实返回，by_cert_serial 在原 notAfter 之后不再映射。
    let mut row = env.cert_row(serial.as_str()).await.unwrap();
    row.status = CertRowStatus::Superseded;
    let mut tx = env.store.begin_write().await.unwrap();
    tx.module_conn(MODULE)
        .put(DEVICE_CERTS, serial.as_str(), &row)
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        env.directory.lookup(&serial).unwrap().unwrap().status,
        DeviceCertStatus::Superseded
    );
    assert!(env
        .directory
        .by_cert_serial(&serial)
        .await
        .unwrap()
        .is_some());
    env.clock.set(row.not_after);
    assert_eq!(env.directory.by_cert_serial(&serial).await.unwrap(), None);

    // 设备吊销对下一次查询立即可见。
    env.revoke_event(&paired).await;
    assert!(
        !env.directory
            .lookup(&serial)
            .unwrap()
            .unwrap()
            .device_active
    );
}

#[tokio::test]
async fn last_seen_is_throttled_to_once_per_minute() {
    let env = Env::new().await;
    let paired = env.pair(DevicePlatform::Macos).await;
    let id: DeviceId = paired.device.id;
    let head = env.store.snapshot().await.unwrap().head_seq();

    let t0 = env.clock.now_timestamp();
    env.directory.touch_last_seen(&id).await;
    for _ in 0..100 {
        env.advance(500);
        env.directory.touch_last_seen(&id).await;
    }
    // 50 秒内的 100 次调用只写了第一次。
    let seen = env.directory.get(&id).await.unwrap().unwrap().last_seen;
    assert_eq!(seen, Some(t0));

    env.clock
        .set(Timestamp::from_millis(t0.as_millis() + 61_000));
    env.directory.touch_last_seen(&id).await;
    let seen = env.directory.get(&id).await.unwrap().unwrap().last_seen;
    assert_eq!(seen, Some(Timestamp::from_millis(t0.as_millis() + 61_000)));
    // 不产生事件。
    assert_eq!(env.store.snapshot().await.unwrap().head_seq(), head);
}
