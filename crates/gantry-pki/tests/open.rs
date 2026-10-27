//! pki.md §12.2「引导与证书参数」中 `Pki::open` 的启动自检、「续期」中服务端叶子的续签、
//! 「应急命令」，以及配对 → 真实 TLS 1.3 握手 → 吊销后握手失败的端到端路径（A 级）。

mod common;

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{temp_dir, AlwaysMemory, Keys, API, DAY_MS, NOW_MS};
use gantry_core::entities::{Actor, ActorKind, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::events::{DeviceRevokedData, DeviceUpdatedData, EventType, Subject};
use gantry_core::ids::{Clock, IdGen, Ulid};
use gantry_core::ports::secrets::StagedSecret;
use gantry_core::ports::{
    ActorCtx, Expect, PutOutcome, Secret, SecretMeta, SecretSink, SecretSource, SecretValue,
    SecretVersion, SecretsError,
};
use gantry_core::secret::{Consumer, SecretName, SecretString};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::secrets::{MemorySecretStore, MemorySecrets};
use gantry_pki::bootstrap::{self, BootstrapParams};
use gantry_pki::journal::{JournalEntry, RevocationJournal, JOURNAL_VERSION};
use gantry_pki::open::{Health, ServerRenewal, INTERMEDIATE_KEY_SECRET, SERVER_KEY_SECRET};
use gantry_pki::pairing_service::{PairResponse, PairingConfig};
use gantry_pki::recover::{
    HostEnv, PairingOutcome, PkiControl, RecoverCmd, RecoverError, RecoverOutcome,
};
use gantry_pki::verifier::{p256_only_provider, ClockTimeProvider};
use gantry_pki::{store_schema, CertPolicy, IssuedCert, Pki, PkiConfig, PkiError};
use gantry_store::{Store, StoreDeps};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use serde_json::Value;

const SERVER_NAME: &str = "gantry.example.test";
const ROOT: HostEnv = HostEnv {
    euid: 0,
    sudo_user: None,
};

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "deploy"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(NOW_MS as u64, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

/// `stage` 可注入失败的 secrets（其余委托给 `MemorySecrets`）。
struct FlakySecrets {
    inner: MemorySecrets,
    fail_stage: AtomicBool,
}

impl SecretSource for FlakySecrets {
    fn get(&self, name: &SecretName) -> Result<Option<Secret>, SecretsError> {
        self.inner.get(name)
    }
    fn version(&self, name: &SecretName) -> Result<Option<SecretVersion>, SecretsError> {
        self.inner.version(name)
    }
    fn present(&self, name: &SecretName) -> Result<bool, SecretsError> {
        self.inner.present(name)
    }
    fn get_version(
        &self,
        name: &SecretName,
        version: SecretVersion,
    ) -> Result<Option<Secret>, SecretsError> {
        self.inner.get_version(name, version)
    }
}

impl SecretSink for FlakySecrets {
    fn put(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        ctx: &ActorCtx,
    ) -> Result<PutOutcome, SecretsError> {
        self.inner.put(name, value, expect, ctx)
    }
    fn stage(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        ctx: &ActorCtx,
    ) -> Result<StagedSecret, SecretsError> {
        if self.fail_stage.load(Ordering::SeqCst) {
            return Err(SecretsError::KeyUnavailable("injected".into()));
        }
        self.inner.stage(name, value, expect, ctx)
    }
    fn commit(&self, staged: &StagedSecret, ctx: &ActorCtx) -> Result<SecretMeta, SecretsError> {
        self.inner.commit(staged, ctx)
    }
    fn abort(&self, staged: &StagedSecret, ctx: &ActorCtx) -> Result<(), SecretsError> {
        self.inner.abort(staged, ctx)
    }
    fn staged(&self) -> Result<Vec<StagedSecret>, SecretsError> {
        self.inner.staged()
    }
    fn delete(
        &self,
        name: &SecretName,
        expect: Expect,
        ctx: &ActorCtx,
    ) -> Result<Option<SecretMeta>, SecretsError> {
        self.inner.delete(name, expect, ctx)
    }
    fn list(&self) -> Result<Vec<SecretMeta>, SecretsError> {
        self.inner.list()
    }
    fn check_refs(&self, refs: &[SecretName]) -> Result<Vec<SecretName>, SecretsError> {
        self.inner.check_refs(refs)
    }
}

struct Host {
    ca_dir: std::path::PathBuf,
    scan_dir: std::path::PathBuf,
    journal: RevocationJournal,
    secrets: Arc<FlakySecrets>,
    store: Store,
    clock: Arc<FakeClock>,
}

impl Host {
    /// 模拟 deploy 的引导：bootstrap 产出 → 证书进 ca_dir、两把私钥进 secrets。
    async fn new() -> Self {
        let ca_dir = temp_dir();
        bootstrap::run_with(
            BootstrapParams {
                server_names: vec![SERVER_NAME.into()],
                root_passphrase: SecretString::new("pw".into()),
                install_id: "open".into(),
                out_dir: ca_dir.clone(),
                policy: CertPolicy::default(),
            },
            &FakeClock::new(NOW_MS - DAY_MS),
            &SeqEntropy::new(3),
            &AlwaysMemory,
        )
        .unwrap();
        let store_secrets = MemorySecretStore::new(Timestamp::from_millis(NOW_MS));
        store_secrets.allow(Consumer::Pki);
        let scoped = store_secrets.scoped(Consumer::Pki).unwrap();
        for (name, file) in [
            (INTERMEDIATE_KEY_SECRET, "intermediate.key"),
            (SERVER_KEY_SECRET, "server.key"),
        ] {
            scoped
                .put(
                    &SecretName::parse(name).unwrap(),
                    SecretValue::from_bytes(fs::read(ca_dir.join(file)).unwrap()),
                    Expect::Absent,
                    &ctx(),
                )
                .unwrap();
            fs::remove_file(ca_dir.join(file)).unwrap();
        }
        let journal_dir = temp_dir();
        let clock = Arc::new(FakeClock::new(NOW_MS));
        let store = Store::open_in_memory(
            StoreDeps {
                clock: clock.clone(),
                ids: Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7)))),
            },
            &[store_schema::module_spec()],
        )
        .await
        .unwrap();
        Self {
            scan_dir: temp_dir(),
            ca_dir,
            journal: RevocationJournal::new(
                journal_dir.join("revocations.journal"),
                journal_dir.join("backup.journal"),
            ),
            secrets: Arc::new(FlakySecrets {
                inner: scoped,
                fail_stage: AtomicBool::new(false),
            }),
            store,
            clock,
        }
    }

    fn config(&self, server_names: &[&str]) -> PkiConfig {
        PkiConfig {
            ca_dir: self.ca_dir.clone(),
            root_key_scan_dirs: vec![self.ca_dir.clone(), self.scan_dir.clone()],
            journal: self.journal.clone(),
            server_names: server_names.iter().map(|s| s.to_string()).collect(),
            cert_policy: CertPolicy::default(),
            pairing: PairingConfig::new(API),
        }
    }

    async fn open(&self) -> Result<Pki, PkiError> {
        self.open_with(&[SERVER_NAME]).await
    }

    async fn open_with(&self, names: &[&str]) -> Result<Pki, PkiError> {
        Pki::open(
            self.config(names),
            self.store.clone(),
            self.secrets.clone(),
            self.clock.clone(),
            Arc::new(SeqEntropy::new(99)),
        )
        .await
    }
}

struct Client {
    chain: Vec<CertificateDer<'static>>,
    key: Vec<u8>,
}

async fn pair(pki: &Pki, platform: DevicePlatform) -> (PairResponse, Client) {
    let keys = Keys::new();
    let ticket = pki.pairing().open_session(0).await.unwrap();
    let response = pki
        .pairing()
        .redeem(keys.request(ticket.token.expose_secret(), platform))
        .await
        .unwrap();
    let chain = response.certificate_chain[..2]
        .iter()
        .map(|pem_text| CertificateDer::from(pem::parse(pem_text).unwrap().into_contents()))
        .collect();
    let key = keys.client.serialize_der();
    (response, Client { chain, key })
}

/// 用 `Pki::tls()` 的校验器与服务端证书 resolver 跑一次真实 TLS 1.3 握手；返回服务端出示的叶子。
fn handshake(
    pki: &Pki,
    clock: &Arc<FakeClock>,
    client: &Client,
) -> Result<IssuedCert, rustls::Error> {
    let tls = pki.tls();
    let time = Arc::new(ClockTimeProvider::new(clock.clone() as Arc<dyn Clock>));
    let provider = Arc::new(p256_only_provider());
    let server_config = ServerConfig::builder_with_details(provider.clone(), time.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(tls.client_verifier.clone())
        .with_cert_resolver(tls.server_cert_resolver.clone());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(pki.root_certificate().der.clone()))
        .unwrap();
    let client_config = ClientConfig::builder_with_details(provider, time)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            client.chain.clone(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client.key.clone())),
        )
        .unwrap();
    let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
    let mut conn = ClientConnection::new(
        Arc::new(client_config),
        ServerName::try_from(SERVER_NAME).unwrap(),
    )
    .unwrap();
    pump(&mut conn, &mut server)?;
    let leaf = conn.peer_certificates().unwrap()[0].to_vec();
    Ok(IssuedCert::from_der(leaf).unwrap())
}

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

async fn last_event(pki: &Pki, id: &gantry_core::ids::DeviceId) -> (EventType, Actor, Value) {
    let event = pki
        .store()
        .events()
        .by_subject(&Subject::parse(&id.to_string()).unwrap())
        .await
        .unwrap()
        .pop()
        .unwrap();
    (event.ty, event.actor, event.data)
}

// ---------------------------------------------------------------------------
// 启动自检
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_refuses_when_the_root_private_key_is_on_the_server() {
    let host = Host::new().await;
    host.open().await.unwrap();

    // 把口令加密的根私钥解开、以明文 PKCS#8 放进被扫描目录的子目录。
    let enc = fs::read_to_string(host.ca_dir.join("root.key.enc")).unwrap();
    let root_key = p256::SecretKey::from_pkcs8_encrypted_pem(&enc, b"pw").unwrap();
    use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
    let pem_text = root_key
        .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
        .unwrap()
        .to_string();
    let nested = host.scan_dir.join("var/lib/gantry");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("leftover.pem"), pem_text).unwrap();

    match host.open().await {
        Err(PkiError::RootKeyPresent(path)) => assert!(path.ends_with("leftover.pem")),
        other => panic!("expected RootKeyPresent, got {:?}", other.err()),
    }
}

#[tokio::test]
async fn open_requires_the_intermediate_key_in_secrets() {
    let host = Host::new().await;
    host.secrets
        .delete(
            &SecretName::parse(INTERMEDIATE_KEY_SECRET).unwrap(),
            Expect::Any,
            &ctx(),
        )
        .unwrap();
    assert!(matches!(host.open().await, Err(PkiError::Config(_))));
}

#[tokio::test]
async fn open_refuses_while_journaled_revocations_are_unapplied() {
    let host = Host::new().await;
    let pki = host.open().await.unwrap();
    let (device, _) = pair(&pki, DevicePlatform::Macos).await;
    drop(pki);
    let identity = host.store.log_identity();
    host.journal
        .append(&JournalEntry {
            v: JOURNAL_VERSION,
            device_id: device.device.id,
            cert_serials: vec![device.device.cert_serial.parse().unwrap()],
            approval_pubkey_fpr: device.device.approval_pubkey_fpr,
            revoked_at: host.clock.now_timestamp(),
            actor: Actor::new(ActorKind::System, "pki"),
            via: None,
            log_id: identity.log_id,
            log_epoch: identity.log_epoch,
        })
        .unwrap();
    assert!(matches!(
        host.open().await,
        Err(PkiError::PendingRevocations(1))
    ));
}

// ---------------------------------------------------------------------------
// 端到端：配对 → 握手 → 吊销
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paired_device_handshakes_until_revoked_by_host_recover() {
    let host = Host::new().await;
    let pki = host.open().await.unwrap();
    let (approver, client) = pair(&pki, DevicePlatform::Macos).await;

    let server_leaf = handshake(&pki, &host.clock, &client).unwrap();
    assert!(server_leaf.not_after.as_millis() > NOW_MS);

    let outcome = pki
        .device_recover(
            RecoverCmd::Revoke {
                device_id: approver.device.id,
            },
            HostEnv {
                euid: 0,
                sudo_user: Some("lex".into()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        RecoverOutcome::Revoked { already: false, .. }
    ));
    // 下一次握手即失败：没有缓存可过期。
    assert!(handshake(&pki, &host.clock, &client).is_err());

    let (ty, actor, data) = last_event(&pki, &approver.device.id).await;
    assert_eq!(ty, EventType::DeviceRevoked);
    assert_eq!(actor, Actor::new(ActorKind::System, "host_recover"));
    let data: DeviceRevokedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.via.as_deref(), Some("host_recover"));
    assert_eq!(host.journal.read_all().unwrap().len(), 1);

    // 重复吊销：成功、无副作用。
    let again = pki
        .device_recover(
            RecoverCmd::Revoke {
                device_id: approver.device.id,
            },
            ROOT,
        )
        .await
        .unwrap();
    assert!(matches!(
        again,
        RecoverOutcome::Revoked { already: true, .. }
    ));
    assert_eq!(host.journal.read_all().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// 服务端证书续签（§7.7）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_certificate_renews_within_thirty_days_and_hot_swaps() {
    let host = Host::new().await;
    let pki = host.open().await.unwrap();
    let (_, client) = pair(&pki, DevicePlatform::Macos).await;
    let before = handshake(&pki, &host.clock, &client).unwrap();
    assert_eq!(pki.tick().await.unwrap(), ServerRenewal::NotDue);

    let version_before = host
        .secrets
        .version(&SecretName::parse(SERVER_KEY_SECRET).unwrap())
        .unwrap();
    host.clock.set(Timestamp::from_millis(
        before.not_after.as_millis() - 29 * DAY_MS,
    ));
    let ServerRenewal::Renewed { serial, not_after } = pki.tick().await.unwrap() else {
        panic!("expected renewal");
    };
    assert_eq!(pki.health(), Health::Ok);
    assert_ne!(serial, before.serial);
    // 设备证书只有 30 天，拨钟之后换一台新配对的设备握手。
    let (_, client) = pair(&pki, DevicePlatform::Windows).await;
    let after = handshake(&pki, &host.clock, &client).unwrap();
    assert_eq!(after.serial, serial);
    assert_eq!(after.not_after, not_after);
    assert!(not_after > before.not_after);
    // 新私钥已提交进 secrets；客户端固定的根不变（同一个 RootCertStore 仍能验证）。
    assert_ne!(
        host.secrets
            .version(&SecretName::parse(SERVER_KEY_SECRET).unwrap())
            .unwrap(),
        version_before
    );
    assert_eq!(pki.tick().await.unwrap(), ServerRenewal::NotDue);

    // 重启后沿用库里的新证书与 secrets 里的新私钥。
    drop(pki);
    let reopened = host.open().await.unwrap();
    assert_eq!(
        handshake(&reopened, &host.clock, &client).unwrap().serial,
        serial
    );
}

#[tokio::test]
async fn san_change_triggers_reissue() {
    let host = Host::new().await;
    host.open().await.unwrap();
    let pki = host
        .open_with(&[SERVER_NAME, "gantry.internal.test"])
        .await
        .unwrap();
    assert!(matches!(
        pki.tick().await.unwrap(),
        ServerRenewal::Renewed { .. }
    ));
}

#[tokio::test]
async fn failed_renewal_keeps_the_old_certificate_and_reports_health() {
    let host = Host::new().await;
    let pki = host.open().await.unwrap();
    let (_, client) = pair(&pki, DevicePlatform::Macos).await;
    let before = handshake(&pki, &host.clock, &client).unwrap();
    host.secrets.fail_stage.store(true, Ordering::SeqCst);

    host.clock.set(Timestamp::from_millis(
        before.not_after.as_millis() - 20 * DAY_MS,
    ));
    assert_eq!(pki.tick().await.unwrap(), ServerRenewal::NotDue);
    assert_eq!(pki.health(), Health::Degraded);
    let (_, client) = pair(&pki, DevicePlatform::Windows).await;
    assert_eq!(
        handshake(&pki, &host.clock, &client).unwrap().serial,
        before.serial
    );

    host.clock.set(Timestamp::from_millis(
        before.not_after.as_millis() - 6 * DAY_MS,
    ));
    pki.tick().await.unwrap();
    assert_eq!(pki.health(), Health::Unhealthy);

    host.secrets.fail_stage.store(false, Ordering::SeqCst);
    assert!(matches!(
        pki.tick().await.unwrap(),
        ServerRenewal::Renewed { .. }
    ));
    assert_eq!(pki.health(), Health::Ok);
}

// ---------------------------------------------------------------------------
// 应急命令（§7.8）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recover_requires_root_and_touches_nothing_otherwise() {
    let host = Host::new().await;
    let pki = host.open().await.unwrap();
    let (device, _) = pair(&pki, DevicePlatform::Macos).await;
    let head = pki.store().snapshot().await.unwrap().head_seq();
    for cmd in [
        RecoverCmd::List,
        RecoverCmd::Revoke {
            device_id: device.device.id,
        },
        RecoverCmd::Promote {
            device_id: device.device.id,
        },
    ] {
        let result = pki
            .device_recover(
                cmd,
                HostEnv {
                    euid: 1000,
                    sudo_user: None,
                },
            )
            .await;
        assert!(matches!(result, Err(RecoverError::NotRoot)));
    }
    assert_eq!(pki.store().snapshot().await.unwrap().head_seq(), head);
    assert!(host.journal.read_all().unwrap().is_empty());
}

#[tokio::test]
async fn revoking_the_only_approver_then_promote_restores_approval() {
    let host = Host::new().await;
    let pki = host.open().await.unwrap();
    let (approver, _) = pair(&pki, DevicePlatform::Macos).await;
    let (phone, _) = pair(&pki, DevicePlatform::Ios).await;
    pki.device_recover(
        RecoverCmd::Revoke {
            device_id: approver.device.id,
        },
        ROOT,
    )
    .await
    .unwrap();

    // OQ-K11：新配对的桌面设备是 operator，不会自动成为 approver。
    let (desktop, _) = pair(&pki, DevicePlatform::Windows).await;
    assert_eq!(desktop.device.role, DeviceRole::Operator);
    // 手机永远不能提升（P-76）。
    assert!(matches!(
        pki.device_recover(
            RecoverCmd::Promote {
                device_id: phone.device.id
            },
            ROOT
        )
        .await,
        Err(RecoverError::MobileCannotApprove)
    ));
    // 已吊销的设备不能提升。
    assert!(matches!(
        pki.device_recover(
            RecoverCmd::Promote {
                device_id: approver.device.id
            },
            ROOT
        )
        .await,
        Err(RecoverError::Revoked)
    ));

    let promoted = pki
        .device_recover(
            RecoverCmd::Promote {
                device_id: desktop.device.id,
            },
            HostEnv {
                euid: 0,
                sudo_user: Some("lex".into()),
            },
        )
        .await
        .unwrap();
    let RecoverOutcome::Promoted { device, already } = promoted else {
        panic!("expected Promoted");
    };
    assert!(!already);
    assert_eq!(device.role, DeviceRole::Approver);
    let (ty, actor, data) = last_event(&pki, &desktop.device.id).await;
    assert_eq!(ty, EventType::DeviceUpdated);
    assert_eq!(actor, Actor::new(ActorKind::System, "host_recover"));
    let data: DeviceUpdatedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.via.as_deref(), Some("host_recover"));
    assert_eq!(data.host_uid, Some(0));
    assert_eq!(data.sudo_user.as_deref(), Some("lex"));

    // 幂等：再次提升不发事件。
    let count = pki
        .store()
        .events()
        .by_subject(&Subject::parse(&desktop.device.id.to_string()).unwrap())
        .await
        .unwrap()
        .len();
    let again = pki
        .device_recover(
            RecoverCmd::Promote {
                device_id: desktop.device.id,
            },
            ROOT,
        )
        .await
        .unwrap();
    assert!(matches!(
        again,
        RecoverOutcome::Promoted { already: true, .. }
    ));
    assert_eq!(
        pki.store()
            .events()
            .by_subject(&Subject::parse(&desktop.device.id.to_string()).unwrap())
            .await
            .unwrap()
            .len(),
        count
    );

    // list 覆盖全部设备（含已吊销），名字可直接打印。
    let RecoverOutcome::Listed(lines) = pki.device_recover(RecoverCmd::List, ROOT).await.unwrap()
    else {
        panic!("expected Listed");
    };
    assert_eq!(lines.len(), 3);
    assert!(lines
        .iter()
        .any(|line| line.id == approver.device.id && line.status == DeviceStatus::Revoked));
    assert!(lines.iter().all(|line| !line.name.contains('\u{1b}')));
    assert_eq!(pki.device_list().await.unwrap(), lines);
}

// ---------------------------------------------------------------------------
// gantry pair 的等待（PkiControl）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pair_wait_reports_redeem_cancel_and_timeout() {
    let host = Host::new().await;
    let pki = Arc::new(host.open().await.unwrap());

    let ticket = pki.pair_open(0).await.unwrap();
    let token = ticket.token.expose_secret().to_owned();
    let redeemer = {
        let pki = pki.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            pki.pairing()
                .redeem(Keys::new().request(&token, DevicePlatform::Macos))
                .await
                .unwrap()
        })
    };
    let outcome = pki
        .pair_wait(ticket.session_id, Duration::from_secs(10))
        .await
        .unwrap();
    let response = redeemer.await.unwrap();
    match outcome {
        PairingOutcome::Redeemed {
            device,
            confirmation_code,
        } => {
            assert_eq!(device.id, response.device.id);
            assert_eq!(confirmation_code, response.confirmation_code);
        }
        other => panic!("expected Redeemed, got {other:?}"),
    }

    let ticket = pki.pair_open(0).await.unwrap();
    assert_eq!(
        pki.pair_wait(ticket.session_id, Duration::from_millis(20))
            .await
            .unwrap(),
        PairingOutcome::TimedOut
    );
    pki.pair_cancel(ticket.session_id).await.unwrap();
    assert_eq!(
        pki.pair_wait(ticket.session_id, Duration::from_secs(1))
            .await
            .unwrap(),
        PairingOutcome::Closed(gantry_pki::pairing::SessionState::Voided)
    );
}
