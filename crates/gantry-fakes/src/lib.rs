//! Gantry M0 测试替身。
//!
//! 每个 `core::ports` trait 都有一个内存实现：Forge/Repo/Policy/Inbox/Secrets
//! 覆盖状态语义，Worker/Models/Sandbox/Egress/Engine/Conversation 覆盖运行时
//! 交互；`test_ca` 与 `github` 提供集成测试所需的外部边界替身。
//!
//! 替身只依赖 `core` 的公共类型，不定义与端口重复的类型；真实 IO、Git、
//! GitHub 与证书链不在替身范围内。
//!
//! Forge/Policy/Inbox 的脚本状态是 `gantry_store::Store` 里的 typed 模块快照，
//! 写事务是同一个 `Store::WriteTx`：调用方用一个事务同时追加事件、入队 Outbox
//! 与消费审批，提交/回滚原子生效。`new(now)` 保持兼容；`new_in(store)` 让多个
//! 替身共享一个 Store。

use std::sync::Arc;

use gantry_core::ids::IdGen;
use gantry_core::ports::StoreError as PortStoreError;
use gantry_core::time::Timestamp;
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{ModuleSnapshot, Store, StoreDeps, StoreError};

pub mod conversation;
pub mod egress;
pub mod engine;
pub mod forge;
pub mod github;
pub mod inbox;
pub mod models;
pub mod policy;
pub mod repo;
pub mod sandbox;
pub mod secrets;
pub mod test_ca;
pub mod worker;

/// 新建一个内存 Store，并返回可推进的时钟（与 Store 共用）。
pub(crate) fn open_store(now: Timestamp) -> (Store, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new(now.as_millis()));
    let ids = Arc::new(IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(0x5EED_FA5E)),
    ));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids,
    })
    .expect("in-memory store must open");
    (store, clock)
}

pub(crate) fn port_store_error(error: StoreError) -> PortStoreError {
    match error {
        StoreError::NotFound => PortStoreError::NotFound,
        StoreError::Poisoned => PortStoreError::Unavailable { retry_after: None },
        StoreError::Unsupported(message) => PortStoreError::Rejected {
            message: message.to_owned(),
        },
        other => PortStoreError::InvalidState {
            message: other.to_string(),
        },
    }
}

/// 读取已注册的模块状态；缺失视为调用方错误。
pub(crate) fn read_state<T: ModuleSnapshot + Clone>(
    store: &Store,
    key: &str,
) -> Result<T, PortStoreError> {
    store
        .module_state::<T>(key)
        .map_err(port_store_error)?
        .ok_or(PortStoreError::NotFound)
}

/// 直接更新模块状态（不经过 WriteGate）。
pub(crate) fn update_state<T: ModuleSnapshot + Clone, R>(
    store: &Store,
    key: &'static str,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, PortStoreError> {
    store.update_module_state(key, f).map_err(port_store_error)
}

/// 直接更新模块状态；闭包返回 `Err` 时不写回。
pub(crate) fn try_update_state<T: ModuleSnapshot + Clone, R, E>(
    store: &Store,
    key: &'static str,
    f: impl FnOnce(&mut T) -> Result<R, E>,
) -> Result<Result<R, E>, PortStoreError> {
    store
        .try_update_module_state(key, f)
        .map_err(port_store_error)
}
