//! 进程级测试的替身装配。
//!
//! 只在测试里编译（dev-dependency），不进服务端二进制。测试二进制带着
//! `GANTRYD_FAKES_CHILD` 环境变量重新执行自身（只跑 [`CHILD_TEST`] 这一个测试），
//! 这个子进程就是"用替身装配的 gantryd"；父进程只经控制通道和信号跟它交互。

pub mod fake_cmds;

use std::path::PathBuf;
use std::sync::Arc;

use gantry_core::ids::{Clock, IdGen};
use gantry_core::ports::inbox::InboxPort;
use gantry_core::ports::policy::PolicyPort;
use gantry_engine::Engine;
use gantry_fakes::{
    conversation::FakeConversation, egress::EgressStub, forge::FakeForge,
    models::FakeModelProvider, repo::FakeRepo, sandbox::FakeSandboxService,
    worker::StubWorkerAdapter,
};
use gantry_notify::Notify;
use gantry_policy::{PolicyDeps, PolicyEngine};
use gantry_store::{Store, StoreDeps};
use gantryd::assembly::{Assembly, ModuleInfo, OsEntropy, Profile, Provider, SystemClock};
use gantryd::config::BootConfig;
use gantryd::control::{CommandSet, ControlAuth};

/// 子进程要执行的测试名（`--exact` 过滤）。
pub const CHILD_TEST: &str = "fake_gantryd_child";
/// 子进程的配置路径与控制通道放行的 uid，格式 `<config>\n<uid>`。
pub const CHILD_ENV: &str = "GANTRYD_FAKES_CHILD";

/// engine、notify、policy 用真实实现；store（内存）与其余模块用替身。
/// 换成真实实现的模块从这里挪到 `gantryd::assembly::assemble_real`。
pub fn assemble_fakes() -> Assembly {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(OsEntropy)));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    })
    .expect("in-memory store");

    let forge = Arc::new(FakeForge::new_in(store.clone()));
    let repo = Arc::new(FakeRepo::new(clock.now()));
    let inbox: Arc<dyn InboxPort> = Arc::new(Notify::new(store.clone(), ids.clone()));
    let policy: Arc<dyn PolicyPort> = Arc::new(PolicyEngine::new(PolicyDeps {
        store: store.clone(),
        forge: forge.clone(),
        clock: clock.clone(),
        ids: ids.clone(),
    }));
    let engine = Arc::new(Engine::new(store.clone(), inbox.clone()));

    use Provider::{Absent, Fake, Real};
    let modules = vec![
        ModuleInfo::new("store", Fake, "in-memory log; SQLite store not wired"),
        ModuleInfo::new("engine", Real, "decide / executor pending #70-#73"),
        ModuleInfo::new("notify", Real, ""),
        ModuleInfo::new("policy", Real, "built-in M1 rules"),
        ModuleInfo::new("forge", Fake, "FakeForge; real forge needs a GitHub App"),
        ModuleInfo::new("repo", Fake, "FakeRepo"),
        ModuleInfo::new(
            "sandbox",
            Fake,
            "FakeSandboxService; real sandbox needs Podman",
        ),
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

    Assembly {
        profile: Profile::Fakes,
        store,
        ids,
        engine,
        inbox,
        policy,
        outbox: forge.clone(),
        forge,
        repo: repo.clone(),
        locks: repo,
        sandbox: Arc::new(FakeSandboxService::new()),
        worker: Arc::new(StubWorkerAdapter::new()),
        models: Arc::new(FakeModelProvider::new()),
        egress: Arc::new(EgressStub::new()),
        conversation: Arc::new(FakeConversation::new()),
        modules,
    }
}

/// 子进程主体：读配置、替身装配、服务到 SIGTERM。启动失败打印到 stderr 并 panic
/// （测试进程以非零码退出），与 gantryd 二进制的失败语义一致。
pub fn child_main(spec: &str) {
    let (config, uid) = spec.split_once('\n').expect("child spec");
    let config = BootConfig::load(&PathBuf::from(config)).expect("child config");
    let uid: u32 = uid.parse().expect("child uid");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let assembly = Arc::new(assemble_fakes());
        let commands: Vec<Arc<dyn CommandSet>> =
            vec![Arc::new(fake_cmds::FakeCommands::new(assembly.clone()))];
        if let Err(error) = gantryd::serve(&config, assembly, commands, ControlAuth { uid }).await {
            eprintln!("gantryd: {error}");
            panic!("gantryd failed to start");
        }
    });
}
