//! 模块装配（architecture §3、§4）。
//!
//! 模块之间只经 `core::ports` 相连（P-51）：`Assembly` 持有每个端口的
//! `Arc<dyn …>`，消费方从这里取依赖。某个模块从替身换成真实实现时，只改本文件
//! 里对应的一行构造，不改模块代码（#101 完成标准）。
//!
//! 两种装配：
//! - `Profile::Real`：真实实现。持久存储（SQLite，store.md §7）尚未落地，
//!   在它就绪前拒绝启动，不退回内存存储（失败闭合）。
//! - `Profile::Fakes`（`fakes` feature）：内存 Store；已合入且不依赖外部资源的
//!   真实模块（engine、notify、policy）直接接入，其余用 `gantry-fakes` 替身。

use std::sync::Arc;

use gantry_core::ids::{Clock, Entropy, IdGen};
use gantry_core::ports::conversation::ConversationService;
use gantry_core::ports::egress::EgressControl;
use gantry_core::ports::forge::{ForgeReader, OutboxPort};
use gantry_core::ports::inbox::InboxPort;
use gantry_core::ports::models::ModelProvider;
use gantry_core::ports::policy::PolicyPort;
use gantry_core::ports::repo::{RepoService, WorktreeLocks};
use gantry_core::ports::sandbox::SandboxService;
use gantry_core::ports::worker::WorkerAdapter;
use gantry_core::time::Timestamp;
use gantry_engine::Engine;
use gantry_store::Store;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Real,
    Fakes,
}

impl std::str::FromStr for Profile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "real" => Ok(Self::Real),
            "fakes" => Ok(Self::Fakes),
            other => Err(format!("unknown profile {other:?} (expected real | fakes)")),
        }
    }
}

/// 一个模块当前由谁提供；`status` 原样报告，便于看出哪些还是替身。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Real,
    Fake,
    /// 还没有消费方或实现，未装配。
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModuleInfo {
    pub module: &'static str,
    pub provider: Provider,
    pub note: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum AssemblyError {
    #[error("module {module} has no real implementation yet: {why}")]
    NotReady {
        module: &'static str,
        why: &'static str,
    },
    #[error("this build has no fakes (rebuild gantryd with --features fakes)")]
    FakesNotBuilt,
    #[error("store: {0}")]
    Store(String),
}

/// 装配好的进程内模块。字段即端口；没有实现的端口不出现。
pub struct Assembly {
    pub profile: Profile,
    pub store: Store,
    pub ids: Arc<IdGen>,
    pub engine: Arc<Engine>,
    pub inbox: Arc<dyn InboxPort>,
    pub policy: Arc<dyn PolicyPort>,
    pub outbox: Arc<dyn OutboxPort>,
    pub forge: Arc<dyn ForgeReader>,
    pub repo: Arc<dyn RepoService>,
    pub locks: Arc<dyn WorktreeLocks>,
    pub sandbox: Arc<dyn SandboxService>,
    pub worker: Arc<dyn WorkerAdapter>,
    pub models: Arc<dyn ModelProvider>,
    pub egress: Arc<dyn EgressControl>,
    pub conversation: Arc<dyn ConversationService>,
    pub modules: Vec<ModuleInfo>,
    /// 替身句柄：进程级测试经控制通道的 `fake.*` 命令驱动它们。
    #[cfg(feature = "fakes")]
    pub fakes: Option<FakeHandles>,
}

#[cfg(feature = "fakes")]
pub struct FakeHandles {
    pub forge: Arc<gantry_fakes::forge::FakeForge>,
    pub sandbox: Arc<gantry_fakes::sandbox::FakeSandboxService>,
    pub repo: Arc<gantry_fakes::repo::FakeRepo>,
}

pub fn assemble(profile: Profile) -> Result<Assembly, AssemblyError> {
    match profile {
        Profile::Real => Err(AssemblyError::NotReady {
            module: "store",
            why: "persistent SQLite store (store.md §7) is not implemented; \
                  refusing to run on an in-memory log",
        }),
        Profile::Fakes => assemble_fakes(),
    }
}

#[cfg(not(feature = "fakes"))]
fn assemble_fakes() -> Result<Assembly, AssemblyError> {
    Err(AssemblyError::FakesNotBuilt)
}

#[cfg(feature = "fakes")]
fn assemble_fakes() -> Result<Assembly, AssemblyError> {
    use gantry_fakes::{
        conversation::FakeConversation, egress::EgressStub, forge::FakeForge,
        models::FakeModelProvider, repo::FakeRepo, sandbox::FakeSandboxService,
        worker::StubWorkerAdapter,
    };
    use gantry_notify::Notify;
    use gantry_policy::{PolicyDeps, PolicyEngine};
    use gantry_store::StoreDeps;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(OsEntropy)));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    })
    .map_err(|error| AssemblyError::Store(error.to_string()))?;

    let forge = Arc::new(FakeForge::new_in(store.clone()));
    let repo = Arc::new(FakeRepo::new(clock.now()));
    let sandbox = Arc::new(FakeSandboxService::new());
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
        info("store", Fake, "in-memory log; SQLite store pending"),
        info("engine", Real, "decide / executor pending #70-#73"),
        info("notify", Real, ""),
        info("policy", Real, "built-in M1 rules"),
        info("forge", Fake, "FakeForge; real forge needs a GitHub App"),
        info("repo", Fake, "FakeRepo"),
        info(
            "sandbox",
            Fake,
            "FakeSandboxService; real sandbox needs Podman",
        ),
        info("worker", Fake, "StubWorkerAdapter"),
        info(
            "models",
            Fake,
            "FakeModelProvider; real models need provider keys",
        ),
        info(
            "egress",
            Fake,
            "EgressStub; real egress is a separate process",
        ),
        info("conversation", Fake, "FakeConversation"),
        info("secrets", Absent, "no consumer wired yet"),
        info("pki", Absent, "no consumer wired yet"),
        info("api", Absent, "no crate yet"),
    ];

    Ok(Assembly {
        profile: Profile::Fakes,
        store,
        ids,
        engine,
        inbox,
        policy,
        outbox: forge.clone(),
        forge: forge.clone(),
        repo: repo.clone(),
        locks: repo.clone(),
        sandbox: sandbox.clone(),
        worker: Arc::new(StubWorkerAdapter::new()),
        models: Arc::new(FakeModelProvider::new()),
        egress: Arc::new(EgressStub::new()),
        conversation: Arc::new(FakeConversation::new()),
        modules,
        fakes: Some(FakeHandles {
            forge,
            sandbox,
            repo,
        }),
    })
}

#[cfg_attr(not(feature = "fakes"), allow(dead_code))]
fn info(module: &'static str, provider: Provider, note: &'static str) -> ModuleInfo {
    ModuleInfo {
        module,
        provider,
        note,
    }
}

/// 墙钟。毫秒精度足够 ULID 与事件时间戳；早于纪元按 0 处理。
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0);
        Timestamp::from_millis(ms)
    }
}

/// 操作系统熵源。取不到随机数时进程不能安全地生成 ID，直接中止。
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("OS entropy source unavailable");
    }
}
