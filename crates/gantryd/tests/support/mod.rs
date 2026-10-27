//! 进程级测试的替身装配。
//!
//! 只在测试里编译（dev-dependency），不进服务端二进制。测试二进制带着
//! `GANTRYD_FAKES_CHILD` 环境变量重新执行自身（只跑 [`CHILD_TEST`] 这一个测试），
//! 这个子进程就是"用替身装配的 gantryd"；父进程只经控制通道和信号跟它交互。

pub mod engine_cfg;
pub mod fake_cmds;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::GitOid;
use gantry_core::entities::{MergeMethod, Project, PullRequestRef};
use gantry_core::ids::{Clock, IdGen, ProjectId};
use gantry_core::ports::inbox::InboxPort;
use gantry_engine::actor::PrSnapshots;
use gantry_engine::runtime::{EnginePorts, EngineRuntime, RuntimeSettings};
use gantry_fakes::{
    conversation::FakeConversation, egress::EgressStub, evented_repo::EventedRepo,
    forge::FakeForge, golden::GoldenSandbox, models::FakeModelProvider, policy::FakePolicy,
    repo::FakeRepo, worker::StubWorkerAdapter,
};
use gantry_notify::Notify;
use gantry_store::{Store, StoreConfig, StoreDeps};
use gantryd::assembly::{Assembly, ModuleInfo, OsEntropy, Profile, Provider, SystemClock};
use gantryd::config::BootConfig;
use gantryd::control::{CommandSet, ControlAuth};

/// 子进程要执行的测试名（`--exact` 过滤）。
pub const CHILD_TEST: &str = "fake_gantryd_child";
/// 子进程的配置路径与控制通道放行的 uid，格式 `<config>\n<uid>`。
pub const CHILD_ENV: &str = "GANTRYD_FAKES_CHILD";
/// 仅测试子进程可用；目录必须是测试临时目录下的 `sqlite`。
pub const CHILD_SQLITE_ENV: &str = "GANTRYD_FAKES_SQLITE_DIR";

/// 测试扮演 GitHub 时写入的 PR 快照（生产里由 forge 同步写 store 投影）。
#[derive(Default)]
pub struct Snapshots(pub Mutex<Option<PullRequestRef>>);

#[async_trait]
impl PrSnapshots for Snapshots {
    async fn pull_request(&self, _: &ProjectId, number: u64) -> Option<PullRequestRef> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .filter(|pr| pr.number == number)
    }
}

/// `fake.*` 命令要直接操纵的替身。
pub struct FakeHandles {
    pub store: Store,
    pub policy: Arc<FakePolicy>,
    pub repo: Arc<FakeRepo>,
    pub sandbox: Arc<GoldenSandbox>,
    pub snapshots: Arc<Snapshots>,
}

pub fn project_id() -> ProjectId {
    engine_cfg::PROJECT.parse().expect("fixture")
}

/// engine（`EngineRuntime`）与 notify 用真实实现；store 默认内存，可选 SQLite。
/// policy 用 `FakePolicy`：§12.2 的合并审批要按脚本放行（真实 policy 需要设备签名）。
/// 换成真实实现的模块从这里挪到 `gantryd::assembly::assemble_real`。
pub async fn assemble_fakes(sqlite_dir: Option<&Path>) -> (Assembly, FakeHandles) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(OsEntropy)));
    let deps = StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    };
    let store = match sqlite_dir {
        Some(dir) => Store::open(StoreConfig::new(dir), deps, &[])
            .await
            .expect("SQLite store"),
        None => Store::open_in_memory_sync(deps).expect("in-memory store"),
    };
    let base = GitOid::parse(engine_cfg::BASE).expect("fixture");
    let now = clock.now();

    let forge = Arc::new(FakeForge::new_in(store.clone()));
    forge.insert_project(Project {
        id: project_id(),
        repo: engine_cfg::repo_slug(),
        installation_id: 1,
        bot_user_id: 2,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: now,
        updated_at: now,
    });
    let repo = Arc::new(FakeRepo::new(now));
    repo.set_ref(project_id(), "main", base);
    let evented = Arc::new(EventedRepo::new(repo.clone(), store.clone()));
    let sandbox = Arc::new(GoldenSandbox::new(store.cas(), base, clock.clone()));
    let policy = Arc::new(FakePolicy::new_in(store.clone()));
    let snapshots = Arc::new(Snapshots::default());
    let inbox: Arc<dyn InboxPort> = Arc::new(Notify::new(store.clone(), ids.clone()));
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
            snapshots: Some(snapshots.clone()),
        },
        RuntimeSettings {
            actor: engine_cfg::actor_config(),
            runner: engine_cfg::runner_config(),
            poll_interval: Duration::from_millis(5),
            cleanup_lease: Duration::from_secs(60),
        },
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
        ModuleInfo::new("engine", Real, "EngineRuntime with fixture settings"),
        ModuleInfo::new("notify", Real, ""),
        ModuleInfo::new("policy", Fake, "FakePolicy; merge approval is scripted"),
        ModuleInfo::new("forge", Fake, "FakeForge; real forge needs a GitHub App"),
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
        ModuleInfo::new("secrets", Absent, "no consumer wired yet"),
        ModuleInfo::new("pki", Absent, "no consumer wired yet"),
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
        forge,
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
        policy,
        repo,
        sandbox,
        snapshots,
    };
    (assembly, handles)
}

/// 子进程主体：读配置、替身装配、服务到 SIGTERM。启动失败打印到 stderr 并 panic
/// （测试进程以非零码退出），与 gantryd 二进制的失败语义一致。
pub fn child_main(spec: &str) {
    let (config, uid) = spec.split_once('\n').expect("child spec");
    let config_path = PathBuf::from(config);
    let sqlite_dir = std::env::var_os(CHILD_SQLITE_ENV).map(|dir| {
        let dir = PathBuf::from(dir);
        let scratch = config_path
            .parent()
            .expect("config parent")
            .canonicalize()
            .expect("scratch directory");
        let temp = std::env::temp_dir().canonicalize().expect("temp directory");
        assert_eq!(scratch.parent(), Some(temp.as_path()));
        assert_eq!(std::fs::metadata(&scratch).unwrap().mode() & 0o777, 0o700);
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
    let config = BootConfig::load(&config_path).expect("child config");
    let uid: u32 = uid.parse().expect("child uid");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (assembly, handles) = assemble_fakes(sqlite_dir.as_deref()).await;
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
