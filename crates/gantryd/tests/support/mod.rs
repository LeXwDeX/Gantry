//! 进程级测试的替身装配。
//!
//! 只在测试里编译（dev-dependency），不进服务端二进制。测试二进制带着
//! `GANTRYD_FAKES_CHILD` 环境变量重新执行自身（只跑 [`CHILD_TEST`] 这一个测试），
//! 这个子进程就是"用替身装配的 gantryd"；父进程只经控制通道和信号跟它交互。

pub mod engine_cfg;
pub mod fake_cmds;

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::GitOid;
use gantry_core::entities::{
    Actor, ActorKind, Device, DevicePlatform, MergeMethod, Project, PullRequestRef,
};
use gantry_core::ids::ActionId;
use gantry_core::ids::{Clock, DeviceId, IdGen, ProjectId, Ulid};
use gantry_core::ports::inbox::InboxPort;
use gantry_core::ports::{ActorCtx, Expect, ForgeReader, SecretSink, SecretSource, SecretValue};
use gantry_core::secret::{Consumer, SecretName, SecretString};
use gantry_engine::actor::PrSnapshots;
use gantry_engine::runtime::{EnginePorts, EngineRuntime};
use gantry_fakes::github::FakeGitHub;
use gantry_fakes::{
    conversation::FakeConversation, egress::EgressStub, evented_repo::EventedRepo,
    forge::FakeForge, golden::GoldenSandbox, models::FakeModelProvider, repo::FakeRepo,
    worker::StubWorkerAdapter,
};
use gantry_forge::{
    CrashPoint, Executor, ExecutorDeps, ExecutorOptions, FaultInjector, Forge, ForgeConfig,
    Permission, ProjectBinding, StaticTokenProvider,
};
use gantry_pki::bootstrap::{self, BootstrapParams, MemoryFsProbe};
use gantry_pki::csr::encode_approval_pubkey;
use gantry_pki::directory::DeviceDirectory;
use gantry_pki::journal::RevocationJournal;
use gantry_pki::open::{INTERMEDIATE_KEY_SECRET, SERVER_KEY_SECRET};
use gantry_pki::pairing_service::{NewDevice, PairingConfig};
use gantry_pki::{CertPolicy, P256PublicKey, Pki, PkiConfig};
use gantry_policy::PolicyApprovals;
use gantry_secrets::{MasterKey, MasterKeySource, Secrets, SecretsConfig, StaticMasterKey};
use gantry_store::{Page, Store, StoreConfig, StoreDeps};
use gantryd::assembly::{
    build_notify, build_pki, build_policy, build_secrets, module_specs, secrets_consumers,
    Assembly, ModuleInfo, OsEntropy, Profile, Provider, SystemClock,
};
use gantryd::config::BootConfig;
use gantryd::control::{CommandSet, ControlAuth};
use p256::ecdsa::SigningKey;
use std::sync::atomic::{AtomicBool, Ordering};

/// 子进程要执行的测试名（`--exact` 过滤）。
pub const CHILD_TEST: &str = "fake_gantryd_child";
/// 子进程的配置路径与控制通道放行的 uid，格式 `<config>\n<uid>`。
pub const CHILD_ENV: &str = "GANTRYD_FAKES_CHILD";
/// 仅测试子进程可用；目录必须是测试临时目录下的 `sqlite`。
pub const CHILD_SQLITE_ENV: &str = "GANTRYD_FAKES_SQLITE_DIR";

/// 引擎经 `FakeForge` 读测试扮演 GitHub 写入的 PR 快照（生产里由 forge 同步写 store 投影）。
struct ForgeSnapshots(Arc<FakeForge>);

#[async_trait]
impl PrSnapshots for ForgeSnapshots {
    async fn pull_request(&self, project: &ProjectId, number: u64) -> Option<PullRequestRef> {
        self.0
            .pull_request(project, number)
            .await
            .expect("fake forge snapshot")
    }
}

/// `fake.*` 命令要直接操纵的替身。
pub struct FakeHandles {
    pub store: Store,
    pub approvals: PolicyApprovals,
    pub auto_approve: bool,
    pub forge: Arc<FakeForge>,
    pub repo: Arc<FakeRepo>,
    pub sandbox: Arc<GoldenSandbox>,
    pub real_executor: Option<(Executor, FakeGitHub)>,
    /// 真实构造的测试 PKI；设备经公开 `pairing()` 注册，审批经 `directory()` 实时读取。
    pub pki: Arc<Pki>,
    pub test_device: DeviceId,
}

/// 测试设备的审批签名密钥种子；只存在于测试进程内存。
pub const TEST_APPROVAL_KEY: [u8; 32] = [7; 32];
/// 测试实例的主密钥种子；只在测试临时目录里使用。
pub const TEST_MASTER_KEY: [u8; 32] = [9; 32];
/// 测试设备名，用于重启后识别已登记设备而不是重复配对。
pub const TEST_DEVICE_NAME: &str = "gantryd test device";
/// 测试 CA 材料在 scratch 下的目录名。
pub const TEST_CA_DIR: &str = "pki-ca";

pub fn test_signing_key() -> SigningKey {
    SigningKey::from_slice(&TEST_APPROVAL_KEY).expect("test approval key")
}

/// 测试设备登记的 `approval_pubkey` 指纹（SPKI DER 的 SHA-256）。
pub fn test_signing_fingerprint() -> gantry_core::canon::Sha256Digest {
    let point = test_signing_key().verifying_key().to_encoded_point(false);
    let key = P256PublicKey::from_uncompressed_point(point.as_bytes()).expect("test approval key");
    key.fingerprint()
}

fn test_master_key() -> Box<dyn MasterKeySource> {
    Box::new(StaticMasterKey::new(MasterKey::from_bytes(TEST_MASTER_KEY)))
}

fn test_actor_ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "gantryd-test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(1, 1).expect("fixture"),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

/// 测试专用内存盘探测：`bootstrap` 要求 tmpfs / ramfs，测试目录不是。
struct TestMemoryFs;

impl MemoryFsProbe for TestMemoryFs {
    fn is_memory_fs(&self, _dir: &Path) -> std::io::Result<bool> {
        Ok(true)
    }
}

/// 在测试临时目录里一次性准备好 secrets 实例与 CA 材料（父进程调用，子进程只打开）。
///
/// 这是**测试**的显式初始化；生产的 [`build_secrets`] 只 `open`，不会走到这里。
pub fn provision_test_pki(config: &BootConfig, test_dir: &Path) {
    let secrets_dir = &config.server.secrets_dir;
    if !secrets_dir.exists() {
        Secrets::init(SecretsConfig::new(
            secrets_dir,
            secrets_consumers(),
            test_master_key(),
        ))
        .expect("initialize test secrets");
    }
    let secrets = build_secrets(config, test_master_key()).expect("open test secrets");
    let ca_dir = test_dir.join(TEST_CA_DIR);
    if !ca_dir.join(bootstrap::ROOT_CRT).exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&ca_dir)
            .expect("create test ca dir");
        std::fs::set_permissions(&ca_dir, std::fs::Permissions::from_mode(0o700))
            .expect("test ca dir mode");
        bootstrap::run_with(
            BootstrapParams {
                server_names: config.server.api.san.clone(),
                root_passphrase: SecretString::new("test passphrase".into()),
                install_id: "gantryd-test".into(),
                out_dir: ca_dir.clone(),
                policy: CertPolicy::default(),
            },
            &SystemClock,
            &OsEntropy,
            &TestMemoryFs,
        )
        .expect("bootstrap test ca");
    }
    // deploy 的引导步骤：两把私钥进 Pki 命名空间，再删掉临时明文。
    let scoped = secrets.scoped(Consumer::Pki).expect("pki secrets");
    let ctx = test_actor_ctx();
    for (name, file) in [
        (INTERMEDIATE_KEY_SECRET, bootstrap::INTERMEDIATE_KEY),
        (SERVER_KEY_SECRET, bootstrap::SERVER_KEY),
    ] {
        let name = SecretName::parse(name).expect("secret name");
        if !scoped.present(&name).expect("secret presence") {
            let value = std::fs::read(ca_dir.join(file)).expect("bootstrap key");
            scoped
                .put(&name, SecretValue::from_bytes(value), Expect::Absent, &ctx)
                .expect("store pki key");
        }
        match std::fs::remove_file(ca_dir.join(file)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove plaintext pki key: {error}"),
        }
    }
    for dir in [
        test_dir.join("journal"),
        config
            .backup
            .journal_target
            .parent()
            .expect("journal target parent")
            .to_path_buf(),
    ] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create journal dir");
    }
}

async fn open_test_pki(
    config: &BootConfig,
    test_dir: &Path,
    store: &Store,
    secrets: &Secrets,
    clock: Arc<dyn Clock>,
) -> Pki {
    let scoped: Arc<dyn SecretSink> = Arc::new(secrets.scoped(Consumer::Pki).expect("pki secrets"));
    let pki_config = PkiConfig {
        ca_dir: test_dir.join(TEST_CA_DIR),
        root_key_scan_dirs: vec![
            test_dir.join(TEST_CA_DIR),
            config.server.data_dir.clone(),
            config.server.secrets_dir.clone(),
        ],
        journal: RevocationJournal::new(
            test_dir.join("journal/revocations.journal"),
            config.backup.journal_target.clone(),
        ),
        server_names: config.server.api.san.clone(),
        cert_policy: CertPolicy::default(),
        pairing: PairingConfig::new(config.server.api.public_url.clone()),
    };
    build_pki(pki_config, store, scoped, clock, Arc::new(OsEntropy))
        .await
        .expect("test pki")
}

async fn find_test_device(pki: &Pki) -> Option<Device> {
    let mut cursor = None;
    loop {
        let page = pki
            .directory()
            .list(&Page {
                limit: 200,
                cursor,
                ..Page::default()
            })
            .await
            .expect("list devices");
        if let Some(device) = page
            .items
            .into_iter()
            .find(|device| device.name == TEST_DEVICE_NAME)
        {
            return Some(device);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return None,
        }
    }
}

/// 首次启动经公开配对服务登记测试设备；重启时按名字复用，避免重复配对与第二台设备。
async fn ensure_test_device(pki: &Pki) -> DeviceId {
    if let Some(device) = find_test_device(pki).await {
        assert_eq!(
            device.approval_pubkey_fpr,
            Some(test_signing_fingerprint()),
            "a device with the test name but a different approval key must not be reused"
        );
        return device.id;
    }
    let approval = test_signing_key().verifying_key().to_encoded_point(false);
    let approval =
        P256PublicKey::from_uncompressed_point(approval.as_bytes()).expect("approval key");
    let ticket = pki
        .pairing()
        .open_session(0)
        .await
        .expect("pairing session");
    let client = rcgen::KeyPair::generate().expect("client key");
    let csr = rcgen::CertificateParams::default()
        .serialize_request(&client)
        .expect("csr")
        .pem()
        .expect("csr pem");
    let response = pki
        .pairing()
        .redeem(gantry_pki::pairing_service::PairRequest {
            token: ticket.token.expose_secret().to_owned(),
            csr_pem: csr,
            approval_pubkey: Some(encode_approval_pubkey(&approval)),
            device: NewDevice {
                name: TEST_DEVICE_NAME.into(),
                platform: DevicePlatform::Macos,
            },
        })
        .await
        .expect("pair test device");
    assert_eq!(
        response.device.role,
        gantry_core::entities::DeviceRole::Approver,
        "the first desktop device must be the approver"
    );
    response.device.id
}

struct CrashAfterConsume(AtomicBool);
impl FaultInjector for CrashAfterConsume {
    fn crash(&self, _: &ActionId, point: CrashPoint) -> bool {
        point == CrashPoint::AfterConsume && !self.0.swap(true, Ordering::SeqCst)
    }
}

pub fn project_id() -> ProjectId {
    engine_cfg::PROJECT.parse().expect("fixture")
}

/// engine（`EngineRuntime`）、notify、policy、secrets 与 pki 用真实实现；store 默认
/// 内存，可选 SQLite。secrets / pki 经 `gantryd::assembly` 的生产构造函数打开，
/// 测试设备经真实配对服务登记；`fake.drive` 用该设备登记过的审批密钥签名。
pub async fn assemble_fakes(
    config: &BootConfig,
    test_dir: &Path,
    sqlite_dir: Option<&Path>,
    auto_approve: bool,
) -> (Assembly, FakeHandles) {
    assemble_variant(config, test_dir, sqlite_dir, false, auto_approve).await
}

async fn assemble_variant(
    config: &BootConfig,
    test_dir: &Path,
    sqlite_dir: Option<&Path>,
    real_executor: bool,
    auto_approve: bool,
) -> (Assembly, FakeHandles) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(OsEntropy)));
    let deps = StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    };
    let mut modules = module_specs();
    if real_executor {
        modules.push(gantry_forge::module_spec());
    }
    let store = match sqlite_dir {
        Some(dir) => Store::open(StoreConfig::new(dir), deps, &modules)
            .await
            .expect("SQLite store"),
        None => Store::open_in_memory_sync_with(deps, &modules).expect("in-memory store"),
    };
    let secrets = Arc::new(build_secrets(config, test_master_key()).expect("test secrets"));
    let pki = Arc::new(open_test_pki(config, test_dir, &store, &secrets, clock.clone()).await);
    let test_device = ensure_test_device(&pki).await;
    let base = GitOid::parse(engine_cfg::BASE).expect("fixture");
    let now = clock.now();

    // FakeForge 的直接写也须与 Store 事务串行，避免覆盖事务的快照。
    let gate = store.begin_write().await.expect("forge setup write lease");
    let forge = Arc::new(FakeForge::new_in(store.clone()));
    let project = Project {
        id: project_id(),
        repo: engine_cfg::repo_slug(),
        installation_id: 1,
        bot_user_id: 2,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: now,
        updated_at: now,
    };
    forge.insert_project(project.clone());
    gate.rollback()
        .await
        .expect("release forge setup write lease");
    let repo = Arc::new(FakeRepo::new(now));
    repo.set_ref(project_id(), "refs/mirror/heads/main", base);
    let evented = Arc::new(EventedRepo::new(repo.clone(), store.clone()));
    let sandbox = Arc::new(GoldenSandbox::new(store.cas(), base, clock.clone()));
    let gantryd::assembly::PolicyStack {
        port: policy,
        approvals,
    } = build_policy(&store, forge.clone(), clock.clone(), ids.clone());
    let real_executor = real_executor.then(|| {
        let github = FakeGitHub::start();
        let real_forge = Forge::new(
            store.clone(),
            ForgeConfig::default().with_project(ProjectBinding {
                project,
                granted: [Permission::ContentsWrite, Permission::PullRequestsWrite]
                    .into_iter()
                    .collect(),
            }),
        );
        let executor = real_forge
            .executor(ExecutorDeps {
                policy: policy.clone(),
                tokens: Arc::new(StaticTokenProvider::new(SecretString::new(
                    "test-only".into(),
                ))),
                api_base: github.base_url(),
                ids: ids.clone(),
                options: ExecutorOptions {
                    lease: Duration::from_millis(10),
                    http_timeout: Duration::from_secs(5),
                    ..ExecutorOptions::default()
                },
                faults: Some(Arc::new(CrashAfterConsume(AtomicBool::new(false)))),
                repo: repo.clone(),
                git_base: github.base_url(),
            })
            .expect("test executor");
        (executor, github)
    });
    let inbox: Arc<dyn InboxPort> = build_notify(&store, ids.clone());
    let worker = Arc::new(StubWorkerAdapter::new());

    let engine = EngineRuntime::new(
        store.clone(),
        EnginePorts {
            inbox: inbox.clone(),
            outbox: forge.clone(),
            policy: policy.clone(),
            forge: forge.clone(),
            repo: evented.clone(),
            sandbox: sandbox.clone(),
            worker: worker.clone(),
            cas: Arc::new(store.cas()),
            clock: clock.clone(),
            entropy: Arc::new(OsEntropy),
            snapshots: Some(Arc::new(ForgeSnapshots(forge.clone()))),
        },
        engine_cfg::runtime_settings(),
    )
    .await
    .expect("engine runtime");

    use Provider::{Absent, Fake, Real};
    let modules = vec![
        if sqlite_dir.is_some() {
            ModuleInfo::new("store", Real, "SQLite store in test directory")
        } else {
            ModuleInfo::new("store", Fake, "in-memory log; SQLite store not wired")
        },
        ModuleInfo::new(
            "engine",
            Real,
            "EngineRuntime; settings via from_settings; model table is a test fixture",
        ),
        ModuleInfo::new("notify", Real, "Notify; production constructor"),
        ModuleInfo::new(
            "policy",
            Real,
            if real_executor.is_some() {
                "PolicyEngine; test device, real merge executor"
            } else {
                "PolicyEngine; test device, fake outbox executor"
            },
        ),
        ModuleInfo::new(
            "forge",
            Fake,
            if real_executor.is_some() {
                "FakeForge ports; real merge executor against loopback FakeGitHub"
            } else {
                "FakeForge; real forge needs a GitHub App"
            },
        ),
        ModuleInfo::new("repo", Fake, "EventedRepo over FakeRepo"),
        ModuleInfo::new("sandbox", Fake, "GoldenSandbox; real sandbox needs Podman"),
        ModuleInfo::new("worker", Fake, "StubWorkerAdapter"),
        ModuleInfo::new(
            "models",
            Fake,
            "FakeModelProvider; real models need provider keys",
        ),
        ModuleInfo::new(
            "egress",
            Fake,
            "EgressStub; real egress is a separate process",
        ),
        ModuleInfo::new("conversation", Fake, "FakeConversation"),
        ModuleInfo::new(
            "secrets",
            Real,
            "FileSecrets; M1 Forge/Pki only (Notify joins in M4), never Egress",
        ),
        ModuleInfo::new(
            "pki",
            Real,
            "Pki; test CA, device paired via pairing service, revocation journal in scratch",
        ),
        ModuleInfo::new("api", Absent, "not wired yet"),
    ];

    let assembly = Assembly {
        profile: Profile::Fakes,
        store: store.clone(),
        ids,
        engine,
        inbox,
        policy: policy.clone(),
        outbox: forge.clone(),
        forge: forge.clone(),
        repo: evented,
        locks: repo.clone(),
        sandbox: sandbox.clone(),
        worker,
        models: Arc::new(FakeModelProvider::new()),
        egress: Arc::new(EgressStub::new()),
        conversation: Arc::new(FakeConversation::new()),
        modules,
    };
    let handles = FakeHandles {
        store,
        approvals,
        auto_approve,
        forge,
        repo,
        sandbox,
        real_executor,
        pki,
        test_device,
    };
    (assembly, handles)
}

/// 子进程主体：读配置、替身装配、服务到 SIGTERM。启动失败打印到 stderr 并 panic
/// （测试进程以非零码退出），与 gantryd 二进制的失败语义一致。
pub fn child_main(spec: &str) {
    let (config, uid) = spec.split_once('\n').expect("child spec");
    let (uid, auto_approve, real_executor) = match uid.split_once('\n') {
        Some((uid, "manual")) => (uid, false, false),
        Some((uid, "real-executor")) => (uid, false, true),
        Some(_) => panic!("invalid child mode"),
        None => (uid, true, false),
    };
    let config_path = PathBuf::from(config);
    let scratch = config_path
        .parent()
        .expect("config parent")
        .canonicalize()
        .expect("scratch directory");
    let temp = std::env::temp_dir().canonicalize().expect("temp directory");
    assert_eq!(scratch.parent(), Some(temp.as_path()));
    assert_eq!(std::fs::metadata(&scratch).unwrap().mode() & 0o777, 0o700);
    let config = BootConfig::load(&config_path).expect("child config");
    let sqlite_dir = std::env::var_os(CHILD_SQLITE_ENV).map(|dir| {
        let dir = PathBuf::from(dir);
        let metadata = std::fs::symlink_metadata(&dir).expect("SQLite directory");
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        let inherited = std::fs::File::from(
            rustix::io::dup(std::io::stdin()).expect("duplicate parent handle"),
        )
        .metadata()
        .expect("parent SQLite directory handle");
        assert!(inherited.is_dir());
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (inherited.dev(), inherited.ino())
        );
        assert_eq!(
            dir.canonicalize().expect("SQLite directory"),
            scratch.join("sqlite")
        );
        reject_symlinks(&dir);
        dir
    });
    let uid: u32 = uid.parse().expect("child uid");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (assembly, handles) = if real_executor {
            assemble_variant(&config, &scratch, sqlite_dir.as_deref(), true, false).await
        } else {
            assemble_variant(
                &config,
                &scratch,
                sqlite_dir.as_deref(),
                false,
                auto_approve,
            )
            .await
        };
        let assembly = Arc::new(assembly);
        let commands: Vec<Arc<dyn CommandSet>> = vec![Arc::new(
            fake_cmds::FakeCommands::new(assembly.clone(), handles).await,
        )];
        if let Err(error) = gantryd::serve(&config, assembly, commands, ControlAuth { uid }).await {
            eprintln!("gantryd: {error}");
            panic!("gantryd failed to start");
        }
    });
}

fn reject_symlinks(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("read SQLite directory") {
        let entry = entry.expect("SQLite entry");
        let ty = entry.file_type().expect("SQLite entry type");
        assert!(!ty.is_symlink(), "SQLite directory contains a symlink");
        if ty.is_dir() {
            reject_symlinks(&entry.path());
        } else {
            assert!(ty.is_file(), "unexpected SQLite directory entry");
            assert_eq!(
                entry.metadata().expect("SQLite file metadata").nlink(),
                1,
                "SQLite directory contains a hard link"
            );
        }
    }
}
