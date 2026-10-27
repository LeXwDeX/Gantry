//! 模块装配（architecture §3、§4）。
//!
//! 模块之间只经 `core::ports` 相连（P-51）：`Assembly` 持有每个端口的
//! `Arc<dyn …>`，消费方从这里取依赖。某个模块从替身换成真实实现时，只改
//! 构造 `Assembly` 的那一行，不改模块代码（#101 完成标准）。
//!
//! 服务端二进制只有真实装配 [`assemble_real`]。持久存储（SQLite，store.md §7）
//! 就绪前它拒绝启动，不退回内存存储（失败闭合）。替身装配在
//! `tests/support`，只用 dev-dependency，不进服务端二进制。

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
use gantry_store::Store;
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

/// 真实装配。各模块按端口接入的顺序见 `tests/support` 的替身装配。
pub fn assemble_real() -> Result<Assembly, AssemblyError> {
    Err(AssemblyError::NotReady {
        module: "store",
        why: "persistent SQLite store (store.md §7) is not wired yet; \
              refusing to run on an in-memory log",
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
