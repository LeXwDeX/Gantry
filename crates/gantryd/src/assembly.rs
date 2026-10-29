//! 模块装配（architecture §3、§4）。
//!
//! 模块之间只经 `core::ports` 相连（P-51）：`Assembly` 持有每个端口的
//! `Arc<dyn …>`，消费方从这里取依赖。某个模块从替身换成真实实现时，只改
//! 构造 `Assembly` 的那一行，不改模块代码（#101 完成标准）。
//!
//! 服务端二进制只有真实装配 [`assemble_real`]。持久存储（SQLite，store.md §7）
//! 就绪前它拒绝启动，不退回内存存储（失败闭合）。替身装配在
//! `tests/support`，只用 dev-dependency，不进服务端二进制。
//!
//! 各模块的生产构造函数（[`build_policy`]、[`build_notify`]）也在这里：它们只接收
//! 引导配置、settings 与已打开的 store（及其端口依赖），供 `assemble_real` 与测试
//! 装配共用，保证“替换替身”验证的是同一条生产构造路径。

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
use gantry_engine::runtime::EngineRuntime;
use gantry_notify::Notify;
use gantry_policy::{PolicyApprovals, PolicyDeps, PolicyEngine};
use gantry_store::{ModuleSpec, Store, StoreConfig, StoreDeps};
use serde::Serialize;

/// 装配来源；`status` 原样报告。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Real,
    Fakes,
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

impl ModuleInfo {
    pub fn new(module: &'static str, provider: Provider, note: &'static str) -> Self {
        Self {
            module,
            provider,
            note,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AssemblyError {
    #[error("module {module} has no real implementation yet: {why}")]
    NotReady {
        module: &'static str,
        why: &'static str,
    },
    #[error(transparent)]
    Store(#[from] gantry_store::StoreError),
}

/// 装配好的进程内模块。字段即端口；没有实现的端口不出现。
pub struct Assembly {
    pub profile: Profile,
    pub store: Store,
    pub ids: Arc<IdGen>,
    /// 引擎运行时（engine.md §7.15）：`serve` 负责 `run` 到停机；写入口经 `engine()`。
    pub engine: Arc<EngineRuntime>,
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
}

/// 已接入生产构造函数的模块私有表（store.md §7.10）。notify 的状态在 store
/// 内置的 `inbox_items` 投影里，不需要私有表。sandbox 的 `BuiltinSchemas` 属于
/// sandbox 片，本片不构造 sandbox，也不注入。
pub fn module_specs() -> Vec<ModuleSpec> {
    vec![gantry_policy::POLICY_MODULE]
}

/// policy 的生产构造：`PolicyEngine`（`PolicyPort`）与 `ApprovalService` 共用同一
/// 实例与 store。审批 nonce 用操作系统熵源。
pub struct PolicyStack {
    pub port: Arc<dyn PolicyPort>,
    pub approvals: PolicyApprovals,
}

pub fn build_policy(
    store: &Store,
    forge: Arc<dyn ForgeReader>,
    clock: Arc<dyn Clock>,
    ids: Arc<IdGen>,
) -> PolicyStack {
    let engine = Arc::new(PolicyEngine::new(PolicyDeps {
        store: store.clone(),
        forge,
        clock,
        ids,
    }));
    let port: Arc<dyn PolicyPort> = engine.clone();
    let approvals = PolicyApprovals::new(engine, Arc::new(OsEntropy));
    PolicyStack { port, approvals }
}

/// notify 的生产构造；状态在 store 的内置投影里，不注册私有表。
pub fn build_notify(store: &Store, ids: Arc<IdGen>) -> Arc<dyn InboxPort> {
    Arc::new(Notify::new(store.clone(), ids))
}

/// 真实装配。各模块按端口接入的顺序见 `tests/support` 的替身装配。
///
/// 当前已接持久化 store 与 notify 生产构造函数；policy 构造函数已就绪，待 forge
/// 接入后调用。其余模块（repo、sandbox、worker、models、egress、api、pki、secrets）
/// 尚未接线，因此仍拒绝启动（失败闭合）。
pub async fn assemble_real(config: &crate::config::BootConfig) -> Result<Assembly, AssemblyError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(OsEntropy)));
    let modules = module_specs();
    let store = Store::open(
        StoreConfig::new(&config.server.data_dir),
        StoreDeps {
            clock,
            ids: ids.clone(),
        },
        &modules,
    )
    .await?;
    // notify 只依赖 store 与 ID 生成，生产路径可以先构造；消费方（engine/api）
    // 尚未接线，因此本片只保留构造点，不把实例放进 `Assembly`。
    let _notify = build_notify(&store, ids);
    Err(AssemblyError::NotReady {
        module: "forge",
        why: "real forge/engine/api wiring is not complete (gantryd.md §7.1); \
              persistent store and notify are ready, refusing to run without the \
              remaining modules",
    })
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
