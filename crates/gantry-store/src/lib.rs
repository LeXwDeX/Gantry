#![forbid(unsafe_code)]

//! Gantry 事件存储的 M0 内存实现（store.md §1.1、§6 的 `open_in_memory`）。
//!
//! 只实现单进程、单写者的最小一致性行为：追加事件、`dedup_key` 去重、投影、
//! Outbox 表、游标与 `log_epoch`。内置 `inbox_items` 投影是收件箱状态的唯一
//! 来源（store.md §7.2），`TxReader` / `ReadSnapshot` 的 `inbox_*` 方法按 §6.2 读取；
//! 内置 `devices` 投影同理，读取方法为 `device` / `device_by_cert_serial` / `devices`。SQLite、模块投影器、CAS、备份与完整
//! `ProjectionRead` 留给 M1；未实现的入口在这里显式返回错误或记录到
//! `unprojected`，不静默吞掉。缺口清单见 `docs/m0-contract-gaps.md`。
//!
//! M0 的 `WriteTx` 在 `Inner` 的克隆上工作：`commit` 校验 `revision` 后原子替换，
//! `rollback` 丢弃。所有直接写（游标、`log_epoch`、testing 模块状态）在同一把
//! `Inner` 写锁下 clone-modify-swap 并递增 `revision`，因此它们不会等待
//! `WriteGate`，也不会与同线程持有的 `WriteTx` 死锁；提交时 `revision` 已变则
//! 返回 `PreconditionFailed`，不覆盖直接写。`begin_write` 让出执行线程等待令牌，
//! `begin_write_sync` 在 `Condvar` 上阻塞；两个入口共用同一 FIFO 队列。这对测试
//! 替身足够；真实存储必须用 SQLite 事务。

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use gantry_core::canon::{IdempotencyKey, RepoSlug};
use gantry_core::entities::{
    Actor, ActorKind, Device, ForgeActionKind, GithubRef, InboxItem, InboxKind, InboxStatus,
    IssueRef, OutboxAction, OutboxError, OutboxStatus, Priority, PullRequestRef, Seq,
    SettingsResource, SettingsScope, Severity,
};
use gantry_core::events::{
    Event, EventType, InboxDismissedData, InboxRaisedData, InboxResolvedData, NewEvent,
    SchemaError, SchemaErrorCode, SettingsUpdatedData, Subject,
};
use gantry_core::forge::ActionPayload;
use gantry_core::ids::{
    Act, ActionId, Clock, DeliveryId, DeviceId, Dlv, EventId, Evt, IdError, IdGen, Inb, InboxId,
    ProjectId,
};
use gantry_core::time::Timestamp;
use serde_json::Value;

mod devices;
use module::{ModuleRegistry, ModuleTables};

pub mod cas;
pub mod github;
pub mod module;
pub mod projection;

pub use cas::{Cas, CasKind, CasReader, CasStat, GcReport, RetentionPolicy};
pub use github::{GithubSnapshots, IssueSnapshotRow, PrSnapshotRow};
pub use module::{ModuleConn, ModuleReader, ModuleRow, ModuleSpec, MODULE_SCAN_MAX};
pub use projection::{DeliveryRow, FoldError, StageEntered};

// ---------------------------------------------------------------------------
// 打开
// ---------------------------------------------------------------------------

pub struct StoreDeps {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<IdGen>,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<RwLock<Inner>>,
    gate: Arc<WriteGate>,
    deps: Arc<StoreDeps>,
    modules: Arc<ModuleRegistry>,
    cas: Cas,
}

impl Store {
    /// M0 内存实现。`modules` 声明各模块的私有表（§7.10）；名字不合规返回
    /// `InvalidModuleSpec`。
    #[cfg(feature = "testing")]
    pub async fn open_in_memory(
        deps: StoreDeps,
        modules: &[ModuleSpec],
    ) -> Result<Store, StoreError> {
        Self::open_in_memory_sync_with(deps, modules)
    }

    /// 同步版本；fake 端口的构造函数是同步的。
    #[cfg(feature = "testing")]
    pub fn open_in_memory_sync(deps: StoreDeps) -> Result<Store, StoreError> {
        Self::open_in_memory_sync_with(deps, &[])
    }

    /// 同步版本，带模块私有表（fake 端口与同步测试装配用）。
    #[cfg(feature = "testing")]
    pub fn open_in_memory_sync_with(
        deps: StoreDeps,
        modules: &[ModuleSpec],
    ) -> Result<Store, StoreError> {
        let registry = ModuleRegistry::new(modules)?;
        let log_id = deps.ids.next_ulid().map_err(StoreError::Id)?.to_string();
        let cas = Cas::in_memory(deps.clock.clone(), cas::DEFAULT_MAX_OBJECT_BYTES);
        Ok(Store {
            inner: Arc::new(RwLock::new(Inner {
                log_id,
                log_epoch: 1,
                next_enqueue_seq: 1,
                ..Inner::default()
            })),
            gate: Arc::new(WriteGate::new()),
            deps: Arc::new(deps),
            modules: Arc::new(registry),
            cas,
        })
    }

    pub async fn begin_write(&self) -> Result<WriteTx, StoreError> {
        let lease = self.gate.acquire_async().await;
        self.begin_write_parts(lease)
    }

    /// 与 `begin_write` 相同，但同步返回；fake 端口在同步辅助方法里使用。
    #[cfg(feature = "testing")]
    pub fn begin_write_sync(&self) -> Result<WriteTx, StoreError> {
        let lease = self.gate.acquire();
        self.begin_write_parts(lease)
    }

    fn begin_write_parts(&self, lease: WriteLease) -> Result<WriteTx, StoreError> {
        let snapshot = self.inner.read().map_err(|_| StoreError::Poisoned)?.clone();
        let first_seq = snapshot.head;
        let started_at = snapshot
            .last_ts
            .map(|last| last.max(self.deps.clock.now()))
            .unwrap_or_else(|| self.deps.clock.now());
        Ok(WriteTx {
            store: self.clone(),
            base_revision: snapshot.revision,
            snapshot,
            lease,
            first_seq,
            started_at,
        })
    }

    #[cfg(feature = "testing")]
    pub fn snapshot_sync(&self) -> Result<ReadSnapshot, StoreError> {
        Ok(ReadSnapshot {
            inner: self.inner.read().map_err(|_| StoreError::Poisoned)?.clone(),
            modules: Arc::clone(&self.modules),
        })
    }

    #[cfg(feature = "testing")]
    pub fn clock_now(&self) -> Timestamp {
        self.deps.clock.now()
    }

    pub async fn snapshot(&self) -> Result<ReadSnapshot, StoreError> {
        Ok(ReadSnapshot {
            inner: self.inner.read().map_err(|_| StoreError::Poisoned)?.clone(),
            modules: Arc::clone(&self.modules),
        })
    }

    pub fn events(&self) -> EventLog {
        EventLog {
            store: self.clone(),
        }
    }

    /// 内容寻址存储（store.md §6.4）。
    pub fn cas(&self) -> Cas {
        self.cas.clone()
    }

    pub fn cursors(&self) -> Cursors {
        Cursors {
            store: self.clone(),
        }
    }

    pub fn log_identity(&self) -> LogIdentity {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        LogIdentity {
            log_id: inner.log_id.clone(),
            log_epoch: inner.log_epoch,
        }
    }

    /// testing：模拟 `restore_from` 的 `log_epoch += 1`（P-67）。
    ///
    /// 这是直接写：不需要 `WriteGate`，并在同一把写锁下递增 `revision`。
    #[cfg(feature = "testing")]
    pub fn restore_epoch_bump(&self) -> LogIdentity {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.log_epoch += 1;
        inner.revision += 1;
        LogIdentity {
            log_id: inner.log_id.clone(),
            log_epoch: inner.log_epoch,
        }
    }

    pub fn unprojected(&self) -> Vec<UnprojectedEvent> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unprojected
            .clone()
    }

    // -----------------------------------------------------------------------
    // testing 模块状态：typed 快照，随 `Inner` 克隆/提交
    // -----------------------------------------------------------------------

    /// 注册模块状态；同一 key 已存在时保持原值（类型必须一致）。
    #[cfg(feature = "testing")]
    pub fn register_module_state<T>(&self, key: &'static str, state: T) -> Result<(), StoreError>
    where
        T: ModuleSnapshot,
    {
        let mut inner = self.inner.write().map_err(|_| StoreError::Poisoned)?;
        match inner.module_states.get(key) {
            // `as_ref` 先退到底层 `dyn ModuleSnapshot`，否则方法解析会把 `&Box<..>`
            // 当成 `T`，要求借用满足 `'static`。`is` 只看擦除后的真实类型。
            Some(existing) if ModuleSnapshot::as_any(existing.as_ref()).is::<T>() => Ok(()),
            Some(_) => Err(StoreError::PreconditionFailed(format!(
                "module state {key} has another type"
            ))),
            None => {
                inner.module_states.insert(key, Box::new(state));
                inner.revision += 1;
                Ok(())
            }
        }
    }

    /// 读取模块状态的克隆；未注册返回 `None`。
    #[cfg(feature = "testing")]
    pub fn module_state<T>(&self, key: &str) -> Result<Option<T>, StoreError>
    where
        T: ModuleSnapshot + Clone,
    {
        let inner = self.inner.read().map_err(|_| StoreError::Poisoned)?;
        module_state_from(&inner, key)
    }

    /// 直接更新模块状态：闭包成功才写回并递增 `revision`；失败不写回。
    #[cfg(feature = "testing")]
    pub fn try_update_module_state<T, R, E>(
        &self,
        key: &'static str,
        f: impl FnOnce(&mut T) -> Result<R, E>,
    ) -> Result<Result<R, E>, StoreError>
    where
        T: ModuleSnapshot + Clone,
    {
        let mut inner = self.inner.write().map_err(|_| StoreError::Poisoned)?;
        let Some(current) = module_state_from::<T>(&inner, key)? else {
            return Err(StoreError::PreconditionFailed(format!(
                "module state {key} is not registered"
            )));
        };
        let mut next = current;
        match f(&mut next) {
            Ok(value) => {
                inner.module_states.insert(key, Box::new(next));
                inner.revision += 1;
                Ok(Ok(value))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    /// `try_update_module_state` 的不可失败封装。
    #[cfg(feature = "testing")]
    pub fn update_module_state<T, R>(
        &self,
        key: &'static str,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, StoreError>
    where
        T: ModuleSnapshot + Clone,
    {
        let result =
            self.try_update_module_state(key, |state| Ok::<R, std::convert::Infallible>(f(state)))?;
        Ok(result.unwrap_or_else(|error| match error {}))
    }
}

/// testing 模块状态的类型擦除克隆。
#[cfg(feature = "testing")]
pub trait ModuleSnapshot: Any + Send + Sync {
    fn clone_snapshot(&self) -> Box<dyn ModuleSnapshot>;
    fn as_any(&self) -> &dyn Any;
}

#[cfg(feature = "testing")]
impl<T> ModuleSnapshot for T
where
    T: Any + Send + Sync + Clone + 'static,
{
    fn clone_snapshot(&self) -> Box<dyn ModuleSnapshot> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(feature = "testing")]
fn module_state_from<T: ModuleSnapshot + Clone>(
    inner: &Inner,
    key: &str,
) -> Result<Option<T>, StoreError> {
    let Some(state) = inner.module_states.get(key) else {
        return Ok(None);
    };
    ModuleSnapshot::as_any(state.as_ref())
        .downcast_ref::<T>()
        .map(|state| Some(state.clone()))
        .ok_or_else(|| {
            StoreError::PreconditionFailed(format!("module state {key} has another type"))
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogIdentity {
    pub log_id: String,
    pub log_epoch: u64,
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    events: Vec<Event<Value>>,
    head: u64,
    last_ts: Option<Timestamp>,
    log_id: String,
    log_epoch: u64,
    /// 每次直接写与每次提交都递增；`commit` 用它检测中间的直接写。
    revision: u64,
    dedup: BTreeMap<String, Seq>,
    cursors: BTreeMap<String, Seq>,
    outbox: BTreeMap<ActionId, OutboxRow>,
    settings: BTreeMap<SettingsKey, Vec<SettingsRev>>,
    secret_changes: BTreeMap<(String, u64), Seq>,
    next_enqueue_seq: u64,
    deliveries: BTreeMap<DeliveryId, DeliveryRow>,
    /// 内置 `inbox_items` 投影（store.md §7.2）；收件箱状态的唯一来源。
    inbox: BTreeMap<InboxId, InboxItem>,
    /// 内置 `devices` 投影（store.md §7.2）。
    devices: devices::DeviceTable,
    /// GitHub 快照投影（#150，store.md §7.2 `pr_snapshots` / `issue_snapshots`）。
    github: GithubSnapshots,
    /// 模块私有表（§7.10）；不是投影，重建投影时不动。
    module_tables: ModuleTables,
    unprojected: Vec<UnprojectedEvent>,
    /// testing 的 typed 模块快照；克隆时逐项深拷贝，避免浅共享可变状态。
    #[cfg(feature = "testing")]
    module_states: BTreeMap<&'static str, Box<dyn ModuleSnapshot>>,
}

// 不用 `#[derive(Clone)]`：`dyn ModuleSnapshot` 不能浅克隆。
impl Clone for Inner {
    fn clone(&self) -> Self {
        Self {
            events: self.events.clone(),
            head: self.head,
            last_ts: self.last_ts,
            log_id: self.log_id.clone(),
            log_epoch: self.log_epoch,
            revision: self.revision,
            dedup: self.dedup.clone(),
            cursors: self.cursors.clone(),
            outbox: self.outbox.clone(),
            settings: self.settings.clone(),
            secret_changes: self.secret_changes.clone(),
            next_enqueue_seq: self.next_enqueue_seq,
            deliveries: self.deliveries.clone(),
            inbox: self.inbox.clone(),
            devices: self.devices.clone(),
            github: self.github.clone(),
            module_tables: self.module_tables.clone(),
            unprojected: self.unprojected.clone(),
            #[cfg(feature = "testing")]
            module_states: self
                .module_states
                .iter()
                // 必须走底层 `dyn`：对 `Box`/引用调 `clone_snapshot` 会选中
                // 包装类型的 blanket 实现并再次装箱，导致递归或浅共享。
                .map(|(key, state)| (*key, state.as_ref().clone_snapshot()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SettingsKey {
    scope: String,
    project: String,
    resource: String,
}

#[derive(Clone, Debug, PartialEq)]
struct SettingsRev {
    rev: Seq,
    value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
struct OutboxRow {
    action: OutboxAction,
    enqueued_seq: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnprojectedEvent {
    pub seq: Seq,
    pub ty: EventType,
    /// 为什么不投影；M0 只有 `no_projector`。
    pub reason: &'static str,
}

// ---------------------------------------------------------------------------
// 写事务
// ---------------------------------------------------------------------------

/// 单写者令牌：`begin_write` 等待前一个事务结束；`Drop` 归还。
///
/// 异步与同步入口共用同一 FIFO 队列：异步等待者登记 waker 并返回 `Pending`，
/// 同步等待者在 `Condvar` 上阻塞。取消异步等待会移除自己的队列条目；若它已在
/// 队首且令牌空闲，则唤醒下一位，因此取消不会吞掉令牌。
struct WriteGate {
    state: Mutex<GateState>,
    released: Condvar,
}

#[derive(Default)]
struct GateState {
    busy: bool,
    waiters: VecDeque<Waiter>,
    next_waiter_id: u64,
}

struct Waiter {
    id: u64,
    /// `Some` = 异步等待者；`None` = 同步等待者（由 `Condvar` 唤醒）。
    waker: Option<Waker>,
}

/// 唤醒动作在释放 `state` 锁之后执行，避免在持锁时调用任意外部代码。
enum Wake {
    Async(Waker),
    Sync,
}

impl WriteGate {
    // 只有 `open_in_memory` 造新 gate；其余构造入口不存在。
    #[cfg(feature = "testing")]
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            released: Condvar::new(),
        }
    }

    /// 同步入口：在 `Condvar` 上阻塞，直到轮到自己且前一个事务已结束。
    #[cfg(feature = "testing")]
    fn acquire(self: &Arc<Self>) -> WriteLease {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = state.next_waiter_id;
        state.next_waiter_id += 1;
        state.waiters.push_back(Waiter { id, waker: None });
        while state.busy || state.waiters.front().map(|waiter| waiter.id) != Some(id) {
            state = self
                .released
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.waiters.pop_front();
        state.busy = true;
        WriteLease {
            gate: Arc::clone(self),
        }
    }

    /// 异步入口：返回的 future 只登记 waker，不阻塞执行线程。
    fn acquire_async(self: &Arc<Self>) -> AcquireFuture {
        AcquireFuture {
            gate: Arc::clone(self),
            waiter_id: None,
        }
    }

    fn release(&self) {
        let wake = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.busy = false;
            wake_front(&state)
        };
        wake_now(self, wake);
    }
}

fn wake_front(state: &GateState) -> Option<Wake> {
    match state.waiters.front() {
        Some(Waiter {
            waker: Some(waker), ..
        }) => Some(Wake::Async(waker.clone())),
        Some(Waiter { waker: None, .. }) => Some(Wake::Sync),
        None => None,
    }
}

fn wake_now(gate: &WriteGate, wake: Option<Wake>) {
    match wake {
        Some(Wake::Async(waker)) => waker.wake(),
        Some(Wake::Sync) => gate.released.notify_all(),
        None => {}
    }
}

/// `begin_write` 的等待 future；首次 poll 入队，Drop 时若仍在排队则撤销。
struct AcquireFuture {
    gate: Arc<WriteGate>,
    /// `None` = 尚未入队或已取得令牌。
    waiter_id: Option<u64>,
}

impl Future for AcquireFuture {
    type Output = WriteLease;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(id) = this.waiter_id {
            let position = state
                .waiters
                .iter()
                .position(|waiter| waiter.id == id)
                .expect("write gate waiter lost");
            if position == 0 && !state.busy {
                state.waiters.pop_front();
                state.busy = true;
                this.waiter_id = None;
                return Poll::Ready(WriteLease {
                    gate: Arc::clone(&this.gate),
                });
            }
            state.waiters[position].waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let id = state.next_waiter_id;
        state.next_waiter_id += 1;
        state.waiters.push_back(Waiter {
            id,
            waker: Some(cx.waker().clone()),
        });
        this.waiter_id = Some(id);
        if state.waiters.len() == 1 && !state.busy {
            state.waiters.pop_front();
            state.busy = true;
            this.waiter_id = None;
            Poll::Ready(WriteLease {
                gate: Arc::clone(&this.gate),
            })
        } else {
            Poll::Pending
        }
    }
}

impl Drop for AcquireFuture {
    fn drop(&mut self) {
        let Some(id) = self.waiter_id.take() else {
            return;
        };
        let wake = {
            let mut state = self
                .gate
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(position) = state.waiters.iter().position(|waiter| waiter.id == id) else {
                return;
            };
            state.waiters.remove(position);
            if position == 0 && !state.busy {
                wake_front(&state)
            } else {
                None
            }
        };
        wake_now(&self.gate, wake);
    }
}

struct WriteLease {
    gate: Arc<WriteGate>,
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        self.gate.release();
    }
}

pub struct WriteTx {
    store: Store,
    /// `begin_write` 时的 `Inner.revision`；提交时不等则冲突。
    base_revision: u64,
    snapshot: Inner,
    #[allow(dead_code)]
    lease: WriteLease,
    first_seq: u64,
    /// 事务固定时钟：`begin_write` 时取 max(最后事件时间, 当前时钟)。
    started_at: Timestamp,
}

impl WriteTx {
    /// 事务开始时的时钟值；同一事务内固定，且不早于最后一条事件。
    pub fn now(&self) -> Timestamp {
        self.started_at
    }

    /// 事务是否属于这个 `Store`；fake 端口用它拒绝跨 Store 的 TxHandle。
    #[cfg(feature = "testing")]
    pub fn same_store(&self, store: &Store) -> bool {
        Arc::ptr_eq(&self.store.inner, &store.inner)
    }

    /// 从事务快照读取 testing 模块状态。
    #[cfg(feature = "testing")]
    pub fn module_state<T>(&self, key: &str) -> Result<Option<T>, StoreError>
    where
        T: ModuleSnapshot + Clone,
    {
        module_state_from(&self.snapshot, key)
    }

    /// 更新事务快照里的 testing 模块状态；失败不写回，提交时才生效。
    #[cfg(feature = "testing")]
    pub fn try_update_module_state<T, R, E>(
        &mut self,
        key: &'static str,
        f: impl FnOnce(&mut T) -> Result<R, E>,
    ) -> Result<Result<R, E>, StoreError>
    where
        T: ModuleSnapshot + Clone,
    {
        let Some(current) = module_state_from::<T>(&self.snapshot, key)? else {
            return Err(StoreError::PreconditionFailed(format!(
                "module state {key} is not registered"
            )));
        };
        let mut next = current;
        match f(&mut next) {
            Ok(value) => {
                self.snapshot.module_states.insert(key, Box::new(next));
                Ok(Ok(value))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    /// `try_update_module_state` 的不可失败封装。
    #[cfg(feature = "testing")]
    pub fn update_module_state<T, R>(
        &mut self,
        key: &'static str,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, StoreError>
    where
        T: ModuleSnapshot + Clone,
    {
        let result =
            self.try_update_module_state(key, |state| Ok::<R, std::convert::Infallible>(f(state)))?;
        Ok(result.unwrap_or_else(|error| match error {}))
    }

    pub async fn append(&mut self, ev: NewEvent) -> Result<Appended, StoreError> {
        self.append_with(ev, AppendOpts::default()).await
    }

    pub async fn append_with(
        &mut self,
        ev: NewEvent,
        opts: AppendOpts,
    ) -> Result<Appended, StoreError> {
        ev.validate().map_err(StoreError::InvalidEvent)?;

        if let Some(key) = &ev.dedup_key {
            if let Some(existing) = self.snapshot.dedup.get(key) {
                return Ok(Appended::Duplicate {
                    existing_seq: *existing,
                });
            }
        }

        let actual = self.last_subject_seq(&ev.subject);
        let guard_matches = match opts.expect_subject_seq {
            None => true,
            Some(None) => actual.is_none(),
            Some(Some(want)) => actual == Some(want),
        };
        if !guard_matches {
            return Err(StoreError::PreconditionFailed(format!(
                "subject moved: expected {:?}, actual {:?}",
                opts.expect_subject_seq, actual
            )));
        }

        let seq_value = self.snapshot.head + 1;
        let seq = Seq::new(seq_value).map_err(StoreError::Seq)?;
        let id = self.store.deps.ids.next::<Evt>().map_err(StoreError::Id)?;
        let ts = self.started_at;

        let event = Event {
            id,
            seq,
            ts,
            ty: ev.ty,
            subject: ev.subject,
            project_id: ev.project_id,
            actor: ev.actor,
            causation_id: ev.causation_id,
            correlation_id: ev.correlation_id,
            data: ev.data,
        };

        self.project(&event)?;
        self.snapshot.events.push(event.clone());
        self.snapshot.head = seq_value;
        self.snapshot.last_ts = Some(ts);
        if let Some(key) = ev.dedup_key {
            self.snapshot.dedup.insert(key, seq);
        }
        Ok(Appended::New(event))
    }

    fn last_subject_seq(&self, subject: &Subject) -> Option<Seq> {
        self.snapshot
            .events
            .iter()
            .rev()
            .find(|event| &event.subject == subject)
            .map(|event| event.seq)
    }

    pub async fn advance_cursor(&mut self, consumer: &str, seq: Seq) -> Result<(), StoreError> {
        let current = self
            .snapshot
            .cursors
            .get(consumer)
            .copied()
            .unwrap_or(Seq::ZERO);
        if seq < current {
            return Err(StoreError::PreconditionFailed(
                "cursor must not move backwards".to_owned(),
            ));
        }
        self.snapshot.cursors.insert(consumer.to_owned(), seq);
        Ok(())
    }

    pub fn read(&mut self) -> TxReader<'_> {
        TxReader {
            inner: &self.snapshot,
        }
    }

    pub fn outbox(&mut self) -> OutboxTx<'_> {
        OutboxTx { tx: self }
    }

    /// 本事务内读写 `module` 的私有表（§7.10）；与本事务的事件一起提交或回滚。
    /// 访问未注册或属于其他模块的表返回 `PreconditionFailed`。
    pub fn module_conn(&mut self, module: &'static str) -> ModuleConn<'_> {
        ModuleConn {
            module,
            registry: &self.store.modules,
            tables: &mut self.snapshot.module_tables,
        }
    }

    pub async fn commit(self) -> Result<Committed, StoreError> {
        let mut inner = self.store.inner.write().map_err(|_| StoreError::Poisoned)?;
        if inner.revision != self.base_revision {
            return Err(StoreError::PreconditionFailed(
                "store revision moved: a direct write landed after begin_write".to_owned(),
            ));
        }
        if inner.head != self.first_seq {
            return Err(StoreError::PreconditionFailed(
                "another writer committed first".to_owned(),
            ));
        }
        let base_revision = self.base_revision;
        let first_new = if self.snapshot.head > self.first_seq {
            self.first_seq + 1
        } else {
            0
        };
        *inner = self.snapshot;
        inner.revision = base_revision + 1;
        Ok(Committed {
            first_seq: first_new,
            last_seq: inner.head,
        })
    }

    pub async fn rollback(self) -> Result<(), StoreError> {
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 投影
    // -----------------------------------------------------------------------

    fn project(&mut self, event: &Event<Value>) -> Result<(), StoreError> {
        match event.ty {
            EventType::SettingsUpdated => {
                let data: SettingsUpdatedData =
                    decode_data(event).map_err(StoreError::InvalidEvent)?;
                self.project_settings(event, &data)?;
            }
            EventType::DeliveryCreated => {
                let row = DeliveryRow::created(event).map_err(fold_error)?;
                if self.snapshot.deliveries.contains_key(&row.id) {
                    return Err(StoreError::PreconditionFailed(format!(
                        "delivery {} already created",
                        row.id
                    )));
                }
                self.snapshot.deliveries.insert(row.id, row);
            }
            ty if DeliveryRow::folds(ty) => {
                let id = subject_id::<Dlv>(&event.subject)?;
                let row = self.snapshot.deliveries.get_mut(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("delivery {id} has no projection row"))
                })?;
                // 先在副本上折叠：前置条件失败时行保持不变。
                let mut next = row.clone();
                next.apply(event).map_err(fold_error)?;
                *row = next;
            }
            EventType::OutboxSucceeded => {
                let data = decode_data::<gantry_core::events::OutboxSucceededData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Act>(&event.subject)?;
                let row = self.snapshot.outbox.get_mut(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("outbox row {id} not found"))
                })?;
                // 事件投影也不允许从终态回退：只有执行中的动作可以成功。
                if row.action.status != OutboxStatus::Executing {
                    return Err(StoreError::PreconditionFailed(format!(
                        "outbox.succeeded for {id} requires executing, found {:?}",
                        row.action.status
                    )));
                }
                row.action.status = OutboxStatus::Succeeded;
                row.action.github_ref = Some(data.github_ref);
                row.action.lease_until = None;
                row.action.updated_at = event.ts;
            }
            EventType::OutboxFailed => {
                let data = decode_data::<gantry_core::events::OutboxFailedData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Act>(&event.subject)?;
                let row = self.snapshot.outbox.get_mut(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("outbox row {id} not found"))
                })?;
                if row.action.status != OutboxStatus::Executing {
                    return Err(StoreError::PreconditionFailed(format!(
                        "outbox.failed for {id} requires executing, found {:?}",
                        row.action.status
                    )));
                }
                row.action.status = OutboxStatus::Failed;
                row.action.last_error = Some(OutboxError {
                    code: failure_code_str(data.code).to_owned(),
                    message: data.message.unwrap_or_default(),
                });
                row.action.lease_until = None;
                row.action.updated_at = event.ts;
            }
            ty if GithubSnapshots::folds(ty) => {
                // `apply` 失败时不改动自身，因此直接在快照上折叠。
                self.snapshot.github.apply(event).map_err(fold_error)?;
            }
            EventType::InboxRaised => {
                let data =
                    decode_data::<InboxRaisedData>(event).map_err(StoreError::InvalidEvent)?;
                self.project_inbox_raised(event, data)?;
            }
            EventType::InboxResolved => {
                let data =
                    decode_data::<InboxResolvedData>(event).map_err(StoreError::InvalidEvent)?;
                self.project_inbox_transition(event, data.inbox_id, InboxStatus::Resolved)?;
            }
            EventType::InboxDismissed => {
                let data =
                    decode_data::<InboxDismissedData>(event).map_err(StoreError::InvalidEvent)?;
                self.project_inbox_transition(event, data.inbox_id, InboxStatus::Dismissed)?;
            }
            ty if devices::DeviceTable::folds(ty) => {
                // 先在副本上折叠：前置条件失败时投影保持不变。
                let mut next = self.snapshot.devices.clone();
                next.apply(event)?;
                self.snapshot.devices = next;
            }
            _ => {
                self.snapshot.unprojected.push(UnprojectedEvent {
                    seq: event.seq,
                    ty: event.ty,
                    reason: "no_projector",
                });
            }
        }
        Ok(())
    }

    /// `inbox.raised`：先把 `supersedes` 列出的 open 项置为 `resolved`，再插入新项；
    /// open 项上 `dedup_key` 唯一（部分唯一索引，P-63）。
    fn project_inbox_raised(
        &mut self,
        event: &Event<Value>,
        data: InboxRaisedData,
    ) -> Result<(), StoreError> {
        let item = data.item;
        let id = subject_id::<Inb>(&event.subject)?;
        if item.id != id {
            return Err(StoreError::InvalidEvent(SchemaError::consistency(
                "/data/item/id",
                "inbox.raised item.id must equal subject",
            )));
        }
        if item.project_id != event.project_id {
            return Err(StoreError::InvalidEvent(SchemaError::consistency(
                "/project_id",
                "inbox.raised project_id must equal item.project_id",
            )));
        }
        if item.status != InboxStatus::Open {
            return Err(StoreError::PreconditionFailed(format!(
                "inbox {id} must be raised as open"
            )));
        }
        if self.snapshot.inbox.contains_key(&id) {
            return Err(StoreError::PreconditionFailed(format!(
                "inbox {id} already raised"
            )));
        }
        // 先全部校验、后统一修改：失败时事务快照里不留下半折叠的状态。
        for target in &data.supersedes {
            let row = self.snapshot.inbox.get(target).ok_or_else(|| {
                StoreError::PreconditionFailed(format!("superseded inbox {target} not found"))
            })?;
            gantry_core::machines::inbox::check(row.status, InboxStatus::Resolved)
                .map_err(|error| StoreError::PreconditionFailed(error.to_string()))?;
        }
        if self.snapshot.inbox.values().any(|row| {
            row.status == InboxStatus::Open
                && row.dedup_key == item.dedup_key
                && !data.supersedes.contains(&row.id)
        }) {
            return Err(StoreError::PreconditionFailed(
                "an open inbox item already uses this dedup_key".to_owned(),
            ));
        }
        for target in &data.supersedes {
            if let Some(row) = self.snapshot.inbox.get_mut(target) {
                row.status = InboxStatus::Resolved;
                row.updated_at = event.ts;
            }
        }
        self.snapshot.inbox.insert(id, item);
        Ok(())
    }

    fn project_inbox_transition(
        &mut self,
        event: &Event<Value>,
        id: InboxId,
        to: InboxStatus,
    ) -> Result<(), StoreError> {
        if subject_id::<Inb>(&event.subject)? != id {
            return Err(StoreError::InvalidEvent(SchemaError::consistency(
                "/data/inbox_id",
                "inbox event inbox_id must equal subject",
            )));
        }
        let row = self
            .snapshot
            .inbox
            .get_mut(&id)
            .ok_or_else(|| StoreError::PreconditionFailed(format!("inbox {id} not found")))?;
        if row.project_id != event.project_id {
            return Err(StoreError::InvalidEvent(SchemaError::consistency(
                "/project_id",
                "inbox event project_id must equal item.project_id",
            )));
        }
        gantry_core::machines::inbox::check(row.status, to)
            .map_err(|error| StoreError::PreconditionFailed(error.to_string()))?;
        row.status = to;
        row.updated_at = event.ts;
        Ok(())
    }

    fn project_settings(
        &mut self,
        event: &Event<Value>,
        data: &SettingsUpdatedData,
    ) -> Result<(), StoreError> {
        let (scope, project) = match data.scope {
            SettingsScope::Global => {
                if event.project_id.is_some() {
                    return Err(StoreError::InvalidEvent(SchemaError::consistency(
                        "/project_id",
                        "global settings.updated must have project_id = null",
                    )));
                }
                ("global".to_owned(), String::new())
            }
            SettingsScope::Project => {
                let project = event.project_id.ok_or_else(|| {
                    StoreError::InvalidEvent(SchemaError::consistency(
                        "/project_id",
                        "project settings.updated requires project_id",
                    ))
                })?;
                ("project".to_owned(), project.to_string())
            }
        };
        let key = SettingsKey {
            scope,
            project,
            resource: resource_key(data.resource),
        };
        let current_rev = self
            .snapshot
            .settings
            .get(&key)
            .and_then(|revisions| revisions.last())
            .map(|entry| entry.rev)
            .unwrap_or(Seq::ZERO);
        if data.base_rev != current_rev {
            return Err(StoreError::PreconditionFailed(format!(
                "settings.updated base_rev {} does not match current rev {}",
                data.base_rev.as_u64(),
                current_rev.as_u64()
            )));
        }
        self.snapshot
            .settings
            .entry(key)
            .or_default()
            .push(SettingsRev {
                rev: event.seq,
                value: data.value.clone(),
            });
        for change in &data.secret_changes {
            self.snapshot
                .secret_changes
                .insert((change.secret_ref.to_string(), change.version), event.seq);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Outbox（同一事务）
    // -----------------------------------------------------------------------
}

impl gantry_core::ports::TxHandle for WriteTx {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct AppendOpts {
    /// 外层 `None` = 不守卫；`Some(None)` = 要求该 subject 尚无事件。
    pub expect_subject_seq: Option<Option<Seq>>,
}

// 签名按 store.md §6.1 固定为 `New(Event)`；不为了 clippy 的尺寸建议改契约形状。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Appended {
    New(Event<Value>),
    Duplicate { existing_seq: Seq },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Committed {
    pub first_seq: u64,
    pub last_seq: u64,
}

// ---------------------------------------------------------------------------
// 读
// ---------------------------------------------------------------------------

pub struct TxReader<'a> {
    inner: &'a Inner,
}

impl TxReader<'_> {
    pub fn head_seq(&self) -> Seq {
        Seq::from_wire(self.inner.head).unwrap_or(Seq::ZERO)
    }

    pub fn settings_current(
        &self,
        scope: &SettingsScope,
        resource: SettingsResource,
    ) -> SettingsEntry {
        self.settings_at(scope, None, resource, None)
    }

    /// 项目 scope 必须传入 `project`；`SettingsScope::Project` 缺项目时返回空。
    pub fn settings_current_for(
        &self,
        scope: &SettingsScope,
        project: Option<&ProjectId>,
        resource: SettingsResource,
    ) -> SettingsEntry {
        self.settings_at(scope, project, resource, None)
    }

    pub fn settings_at_rev(
        &self,
        scope: &SettingsScope,
        resource: SettingsResource,
        rev: Seq,
    ) -> Option<SettingsEntry> {
        self.settings_at_rev_for(scope, None, resource, rev)
    }

    pub fn settings_at_rev_for(
        &self,
        scope: &SettingsScope,
        project: Option<&ProjectId>,
        resource: SettingsResource,
        rev: Seq,
    ) -> Option<SettingsEntry> {
        let key = settings_key(scope, project, resource)?;
        self.inner
            .settings
            .get(&key)?
            .iter()
            .find(|entry| entry.rev == rev)
            .map(|entry| SettingsEntry {
                rev: entry.rev,
                value: entry.value.clone(),
            })
    }

    pub fn settings_as_of(
        &self,
        scope: &SettingsScope,
        resource: SettingsResource,
        seq: Seq,
    ) -> SettingsEntry {
        self.settings_at(scope, None, resource, Some(seq))
    }

    /// 项目 scope 必须传入 `project`；`SettingsScope::Project` 缺项目时返回空。
    pub fn settings_as_of_for(
        &self,
        scope: &SettingsScope,
        project: Option<&ProjectId>,
        resource: SettingsResource,
        seq: Seq,
    ) -> SettingsEntry {
        self.settings_at(scope, project, resource, Some(seq))
    }

    pub fn config_rev(&self, project: Option<&ProjectId>) -> Seq {
        let mut max = Seq::ZERO;
        for (key, revisions) in &self.inner.settings {
            let applies = key.scope == "global"
                || project
                    .map(|project| key.project == project.to_string())
                    .unwrap_or(false);
            if !applies {
                continue;
            }
            if let Some(entry) = revisions.last() {
                max = max.max(entry.rev);
            }
        }
        max
    }

    /// 交付单元行（`delivery.*` 的唯一折叠结果，见 [`projection`]）。
    pub fn delivery(&self, id: &DeliveryId) -> Option<DeliveryRow> {
        self.inner.deliveries.get(id).cloned()
    }

    pub fn outbox_action(&self, id: &ActionId) -> Option<OutboxAction> {
        self.inner.outbox.get(id).map(|row| row.action.clone())
    }

    pub fn outbox_by_key(&self, key: &str) -> Option<OutboxAction> {
        self.inner
            .outbox
            .values()
            .find(|row| row.action.idempotency_key.to_string() == key)
            .map(|row| row.action.clone())
    }

    pub fn outbox_list(&self) -> Vec<OutboxAction> {
        let mut rows: Vec<&OutboxRow> = self.inner.outbox.values().collect();
        rows.sort_by_key(|row| (priority_rank(row.action.priority), row.enqueued_seq));
        rows.into_iter().map(|row| row.action.clone()).collect()
    }

    pub fn unprojected(&self) -> &[UnprojectedEvent] {
        &self.inner.unprojected
    }

    // 收件箱（store.md §6.2）。能读到本事务尚未提交的写入。

    pub async fn inbox_item(&mut self, id: &InboxId) -> Result<Option<InboxItem>, StoreError> {
        Ok(inbox_item_in(self.inner, id))
    }

    pub async fn inbox(
        &mut self,
        q: &InboxQuery,
        p: &Page,
    ) -> Result<Paged<InboxItem>, StoreError> {
        inbox_in(self.inner, q, p)
    }

    pub async fn inbox_open_by_dedup_key(
        &mut self,
        key: &str,
    ) -> Result<Option<InboxItem>, StoreError> {
        Ok(inbox_open_by_dedup_key_in(self.inner, key))
    }

    pub async fn inbox_summary(&mut self) -> Result<Vec<InboxProjectCount>, StoreError> {
        Ok(inbox_summary_in(self.inner))
    }

    pub async fn device(&mut self, id: &DeviceId) -> Result<Option<Device>, StoreError> {
        Ok(self.inner.devices.get(id))
    }

    pub async fn device_by_cert_serial(
        &mut self,
        serial: &str,
    ) -> Result<Option<Device>, StoreError> {
        Ok(self.inner.devices.by_cert_serial(serial))
    }

    pub async fn devices(&mut self, p: &Page) -> Result<Paged<Device>, StoreError> {
        self.inner.devices.list(p)
    }

    // GitHub 快照（store.md §6.2）。PR 的列全部同步过才返回。

    pub async fn pr_snapshot(
        &mut self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Option<PullRequestRef>, StoreError> {
        Ok(self.inner.github.pr_snapshot(repo, number))
    }

    pub async fn issue_snapshot(
        &mut self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Option<IssueRef>, StoreError> {
        Ok(self.inner.github.issue_snapshot(repo, number))
    }

    fn settings_at(
        &self,
        scope: &SettingsScope,
        project: Option<&ProjectId>,
        resource: SettingsResource,
        as_of: Option<Seq>,
    ) -> SettingsEntry {
        let Some(key) = settings_key_with(scope, project, resource) else {
            return SettingsEntry {
                rev: Seq::ZERO,
                value: None,
            };
        };
        let Some(revisions) = self.inner.settings.get(&key) else {
            return SettingsEntry {
                rev: Seq::ZERO,
                value: None,
            };
        };
        let entry = match as_of {
            None => revisions.last(),
            Some(seq) => revisions.iter().rev().find(|entry| entry.rev <= seq),
        };
        entry
            .map(|entry| SettingsEntry {
                rev: entry.rev,
                value: entry.value.clone(),
            })
            .unwrap_or(SettingsEntry {
                rev: Seq::ZERO,
                value: None,
            })
    }
}

pub struct ReadSnapshot {
    inner: Inner,
    modules: Arc<ModuleRegistry>,
}

impl ReadSnapshot {
    /// 快照上只读访问 `module` 的私有表（§7.10）。
    pub fn module_conn(&self, module: &'static str) -> ModuleReader<'_> {
        ModuleReader {
            module,
            registry: &self.modules,
            tables: &self.inner.module_tables,
        }
    }

    pub fn head_seq(&self) -> Seq {
        Seq::from_wire(self.inner.head).unwrap_or(Seq::ZERO)
    }

    pub fn settings_current(
        &self,
        scope: &SettingsScope,
        resource: SettingsResource,
    ) -> SettingsEntry {
        TxReader { inner: &self.inner }.settings_current(scope, resource)
    }

    pub fn settings_current_for(
        &self,
        scope: &SettingsScope,
        project: Option<&ProjectId>,
        resource: SettingsResource,
    ) -> SettingsEntry {
        TxReader { inner: &self.inner }.settings_current_for(scope, project, resource)
    }

    pub fn settings_at_rev(
        &self,
        scope: &SettingsScope,
        resource: SettingsResource,
        rev: Seq,
    ) -> Option<SettingsEntry> {
        TxReader { inner: &self.inner }.settings_at_rev(scope, resource, rev)
    }

    pub fn settings_at_rev_for(
        &self,
        scope: &SettingsScope,
        project: Option<&ProjectId>,
        resource: SettingsResource,
        rev: Seq,
    ) -> Option<SettingsEntry> {
        TxReader { inner: &self.inner }.settings_at_rev_for(scope, project, resource, rev)
    }

    pub fn settings_as_of(
        &self,
        scope: &SettingsScope,
        resource: SettingsResource,
        seq: Seq,
    ) -> SettingsEntry {
        TxReader { inner: &self.inner }.settings_as_of(scope, resource, seq)
    }

    pub fn settings_as_of_for(
        &self,
        scope: &SettingsScope,
        project: Option<&ProjectId>,
        resource: SettingsResource,
        seq: Seq,
    ) -> SettingsEntry {
        TxReader { inner: &self.inner }.settings_as_of_for(scope, project, resource, seq)
    }

    pub fn config_rev(&self, project: Option<&ProjectId>) -> Seq {
        TxReader { inner: &self.inner }.config_rev(project)
    }

    /// 交付单元行（`delivery.*` 的唯一折叠结果，见 [`projection`]）。
    pub fn delivery(&self, id: &DeliveryId) -> Option<DeliveryRow> {
        self.inner.deliveries.get(id).cloned()
    }

    pub fn outbox_action(&self, id: &ActionId) -> Option<OutboxAction> {
        TxReader { inner: &self.inner }.outbox_action(id)
    }

    pub fn outbox_list(&self) -> Vec<OutboxAction> {
        TxReader { inner: &self.inner }.outbox_list()
    }

    pub fn unprojected(&self) -> &[UnprojectedEvent] {
        &self.inner.unprojected
    }

    pub async fn inbox_item(&mut self, id: &InboxId) -> Result<Option<InboxItem>, StoreError> {
        Ok(inbox_item_in(&self.inner, id))
    }

    pub async fn inbox(
        &mut self,
        q: &InboxQuery,
        p: &Page,
    ) -> Result<Paged<InboxItem>, StoreError> {
        inbox_in(&self.inner, q, p)
    }

    pub async fn inbox_open_by_dedup_key(
        &mut self,
        key: &str,
    ) -> Result<Option<InboxItem>, StoreError> {
        Ok(inbox_open_by_dedup_key_in(&self.inner, key))
    }

    pub async fn inbox_summary(&mut self) -> Result<Vec<InboxProjectCount>, StoreError> {
        Ok(inbox_summary_in(&self.inner))
    }

    pub async fn device(&mut self, id: &DeviceId) -> Result<Option<Device>, StoreError> {
        Ok(self.inner.devices.get(id))
    }

    pub async fn device_by_cert_serial(
        &mut self,
        serial: &str,
    ) -> Result<Option<Device>, StoreError> {
        Ok(self.inner.devices.by_cert_serial(serial))
    }

    pub async fn devices(&mut self, p: &Page) -> Result<Paged<Device>, StoreError> {
        self.inner.devices.list(p)
    }

    pub async fn pr_snapshot(
        &mut self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Option<PullRequestRef>, StoreError> {
        Ok(self.inner.github.pr_snapshot(repo, number))
    }

    pub async fn issue_snapshot(
        &mut self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Option<IssueRef>, StoreError> {
        Ok(self.inner.github.issue_snapshot(repo, number))
    }
}

// ---------------------------------------------------------------------------
// 分页与收件箱读取（store.md §6.2）
// ---------------------------------------------------------------------------

/// 分页请求；`limit` 取 1..=200，默认 50。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub limit: u32,
    pub cursor: Option<PageCursor>,
    pub order: Order,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            limit: 50,
            cursor: None,
            order: Order::Asc,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<PageCursor>,
}

/// 不透明游标：资源标签 + 方向 + 排序键。跨资源复用、方向不符或被篡改 →
/// `StoreError::InvalidCursor`。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PageCursor(String);

impl PageCursor {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PageCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `inbox` 的过滤条件：`status` 缺省 = `open`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxQuery {
    pub project_id: Option<ProjectId>,
    pub status: Option<InboxStatus>,
    pub kind: Option<InboxKind>,
}

/// `GET /v1/inbox/summary` 的一行（P-63），形状与 api.md §4.8 相同。
pub use gantry_core::ports::ProjectOpenCount as InboxProjectCount;

const INBOX_CURSOR_TAG: &str = "inbox";

fn order_tag(order: Order) -> &'static str {
    match order {
        Order::Asc => "asc",
        Order::Desc => "desc",
    }
}

fn inbox_item_in(inner: &Inner, id: &InboxId) -> Option<InboxItem> {
    inner.inbox.get(id).cloned()
}

fn inbox_open_by_dedup_key_in(inner: &Inner, key: &str) -> Option<InboxItem> {
    inner
        .inbox
        .values()
        .find(|item| item.status == InboxStatus::Open && item.dedup_key == key)
        .cloned()
}

fn inbox_in(inner: &Inner, q: &InboxQuery, p: &Page) -> Result<Paged<InboxItem>, StoreError> {
    if !(1..=200).contains(&p.limit) {
        return Err(StoreError::PreconditionFailed(
            "page limit must be 1..=200".to_owned(),
        ));
    }
    let after = match &p.cursor {
        None => None,
        Some(cursor) => {
            let mut parts = cursor.as_str().splitn(3, ':');
            let (Some(tag), Some(order), Some(key)) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(StoreError::InvalidCursor);
            };
            if tag != INBOX_CURSOR_TAG || order != order_tag(p.order) {
                return Err(StoreError::InvalidCursor);
            }
            Some(
                key.parse::<InboxId>()
                    .map_err(|_| StoreError::InvalidCursor)?,
            )
        }
    };
    let status = q.status.unwrap_or(InboxStatus::Open);
    let matches = |item: &&InboxItem| {
        item.status == status
            && q.project_id.is_none_or(|id| item.project_id == Some(id))
            && q.kind.as_ref().is_none_or(|kind| &item.kind == kind)
    };
    let limit = p.limit as usize;
    let mut items: Vec<InboxItem> = match p.order {
        Order::Asc => inner
            .inbox
            .values()
            .filter(|item| after.is_none_or(|after| item.id > after))
            .filter(matches)
            .take(limit + 1)
            .cloned()
            .collect(),
        Order::Desc => inner
            .inbox
            .values()
            .rev()
            .filter(|item| after.is_none_or(|after| item.id < after))
            .filter(matches)
            .take(limit + 1)
            .cloned()
            .collect(),
    };
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|item| {
            PageCursor(format!(
                "{INBOX_CURSOR_TAG}:{}:{}",
                order_tag(p.order),
                item.id
            ))
        })
    } else {
        None
    };
    Ok(Paged { items, next_cursor })
}

/// 只含 open 数 > 0 的项目；全局项归入 `project_id = None` 的一行（排在最前）。
fn inbox_summary_in(inner: &Inner) -> Vec<InboxProjectCount> {
    let mut by_project: BTreeMap<Option<ProjectId>, (u32, Severity)> = BTreeMap::new();
    for item in inner
        .inbox
        .values()
        .filter(|item| item.status == InboxStatus::Open)
    {
        let entry = by_project
            .entry(item.project_id)
            .or_insert((0, Severity::Info));
        entry.0 += 1;
        if item.severity > entry.1 {
            entry.1 = item.severity.clone();
        }
    }
    by_project
        .into_iter()
        .map(|(project_id, (open, max_severity))| InboxProjectCount {
            project_id,
            open,
            max_severity,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsEntry {
    pub rev: Seq,
    pub value: Option<Value>,
}

fn resource_key(resource: SettingsResource) -> String {
    format!("{resource:?}").to_lowercase()
}

fn settings_key(
    scope: &SettingsScope,
    project: Option<&ProjectId>,
    resource: SettingsResource,
) -> Option<SettingsKey> {
    settings_key_with(scope, project, resource)
}

fn settings_key_with(
    scope: &SettingsScope,
    project: Option<&ProjectId>,
    resource: SettingsResource,
) -> Option<SettingsKey> {
    let (scope, project) = match scope {
        SettingsScope::Global => ("global".to_owned(), String::new()),
        SettingsScope::Project => ("project".to_owned(), project?.to_string()),
    };
    Some(SettingsKey {
        scope,
        project,
        resource: resource_key(resource),
    })
}

// ---------------------------------------------------------------------------
// 事件流
// ---------------------------------------------------------------------------

pub struct EventLog {
    store: Store,
}

impl EventLog {
    pub async fn head(&self) -> Result<Seq, StoreError> {
        let inner = self.store.inner.read().map_err(|_| StoreError::Poisoned)?;
        Ok(Seq::from_wire(inner.head).unwrap_or(Seq::ZERO))
    }

    pub async fn check_cursor(&self, after: Seq) -> Result<(), StoreError> {
        let head = self.head().await?;
        if after > head {
            return Err(StoreError::CursorAhead { head });
        }
        Ok(())
    }

    pub async fn scan(
        &self,
        after: Seq,
        filter: &EventFilter,
        limit: u32,
    ) -> Result<ScanPage, StoreError> {
        if !(1..=200).contains(&limit) {
            return Err(StoreError::PreconditionFailed(
                "scan limit must be 1..=200".to_owned(),
            ));
        }
        let inner = self.store.inner.read().map_err(|_| StoreError::Poisoned)?;
        let head = Seq::from_wire(inner.head).unwrap_or(Seq::ZERO);
        if after > head {
            return Err(StoreError::CursorAhead { head });
        }
        let mut events = Vec::new();
        let mut last_hit = None;
        for event in inner.events.iter().filter(|event| event.seq > after) {
            if !filter.matches(event) {
                continue;
            }
            events.push(event.clone());
            last_hit = Some(event.seq);
            if events.len() == limit as usize {
                break;
            }
        }
        let scanned_to = if events.len() == limit as usize {
            last_hit.unwrap_or(after)
        } else {
            head
        };
        Ok(ScanPage {
            events,
            scanned_to,
            head,
        })
    }

    pub async fn get(&self, id: &EventId) -> Result<Option<Event<Value>>, StoreError> {
        let inner = self.store.inner.read().map_err(|_| StoreError::Poisoned)?;
        Ok(inner.events.iter().find(|event| event.id == *id).cloned())
    }

    pub async fn get_by_seq(&self, seq: Seq) -> Result<Option<Event<Value>>, StoreError> {
        let inner = self.store.inner.read().map_err(|_| StoreError::Poisoned)?;
        Ok(inner.events.iter().find(|event| event.seq == seq).cloned())
    }

    pub async fn by_dedup_key(&self, key: &str) -> Result<Option<Seq>, StoreError> {
        let inner = self.store.inner.read().map_err(|_| StoreError::Poisoned)?;
        Ok(inner.dedup.get(key).copied())
    }

    pub async fn by_subject(&self, subject: &Subject) -> Result<Vec<Event<Value>>, StoreError> {
        let inner = self.store.inner.read().map_err(|_| StoreError::Poisoned)?;
        Ok(inner
            .events
            .iter()
            .filter(|event| &event.subject == subject)
            .cloned()
            .collect())
    }
}

#[derive(Clone, Debug, Default)]
pub struct EventFilter {
    /// `None` = 全部；`project_id` 为 `NULL` 的全局事件始终命中。
    pub project_ids: Option<BTreeSet<ProjectId>>,
    pub include: Vec<TypePattern>,
    pub exclude: Vec<TypePattern>,
}

impl EventFilter {
    fn matches(&self, event: &Event<Value>) -> bool {
        if let Some(projects) = &self.project_ids {
            match event.project_id {
                None => {}
                Some(project) if projects.contains(&project) => {}
                Some(_) => return false,
            }
        }
        if !self.include.is_empty() && !self.include.iter().any(|pattern| pattern.matches(event.ty))
        {
            return false;
        }
        if self.exclude.iter().any(|pattern| pattern.matches(event.ty)) {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypePattern {
    Exact(EventType),
    Prefix(String),
}

impl TypePattern {
    pub fn matches(self: &TypePattern, ty: EventType) -> bool {
        match self {
            TypePattern::Exact(want) => *want == ty,
            TypePattern::Prefix(prefix) => ty.as_str().starts_with(prefix.as_str()),
        }
    }

    pub fn parse(value: &str) -> Result<Self, StoreError> {
        if let Some(prefix) = value.strip_suffix('*') {
            if !prefix.ends_with('.') || prefix.len() < 2 {
                return Err(StoreError::PreconditionFailed(
                    "prefix patterns must end with \".*\"".to_owned(),
                ));
            }
            return Ok(TypePattern::Prefix(prefix.to_owned()));
        }
        value
            .parse::<EventType>()
            .map(TypePattern::Exact)
            .map_err(|_| StoreError::PreconditionFailed(format!("unknown event type {value}")))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanPage {
    pub events: Vec<Event<Value>>,
    pub scanned_to: Seq,
    pub head: Seq,
}

// ---------------------------------------------------------------------------
// 游标
// ---------------------------------------------------------------------------

pub struct Cursors {
    store: Store,
}

impl Cursors {
    pub async fn load(&self, consumer: &str) -> Result<Seq, StoreError> {
        let inner = self.store.inner.read().map_err(|_| StoreError::Poisoned)?;
        Ok(inner.cursors.get(consumer).copied().unwrap_or(Seq::ZERO))
    }

    /// 直接写游标：不经过 `WriteGate`，递增 `revision`，使已打开的旧事务提交冲突。
    pub async fn advance(&self, consumer: &str, seq: Seq) -> Result<(), StoreError> {
        let mut inner = self.store.inner.write().map_err(|_| StoreError::Poisoned)?;
        let current = inner.cursors.get(consumer).copied().unwrap_or(Seq::ZERO);
        if seq < current {
            return Err(StoreError::PreconditionFailed(
                "cursor must not move backwards".to_owned(),
            ));
        }
        if seq == current {
            return Ok(());
        }
        inner.cursors.insert(consumer.to_owned(), seq);
        inner.revision += 1;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

pub struct OutboxTx<'a> {
    tx: &'a mut WriteTx,
}

/// store 层的行插入请求（store.md §6.3）。
pub struct OutboxInsert {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub kind: ForgeActionKind,
    pub target: String,
    pub intent: Value,
    pub payload: Value,
    pub requires_approval: Option<gantry_core::ids::ApprovalId>,
    pub depends_on: Vec<ActionId>,
    pub priority: Priority,
    pub not_before: Option<Timestamp>,
    pub epoch: u32,
    pub coalesce_key: Option<String>,
    pub enqueued_by: EnqueuedBy,
}

pub struct EnqueuedBy {
    pub module: String,
    pub causation_id: Option<EventId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Enqueued {
    pub action: OutboxAction,
    pub created: bool,
}

#[derive(Clone, Debug)]
pub struct ClaimOpts {
    pub lease: Duration,
    pub exclude_deliveries: BTreeSet<DeliveryId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Claimed {
    pub action: OutboxAction,
    pub replay: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Superseded,
    DependencyFailed,
    ApprovalInvalid,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AfterFailure {
    Terminal,
    Retry { not_before: Timestamp },
}

impl OutboxTx<'_> {
    pub async fn enqueue(&mut self, req: OutboxInsert) -> Result<Enqueued, StoreError> {
        let key = IdempotencyKey::derive(
            &req.project_id,
            &req.delivery_id,
            req.kind,
            &req.target,
            &req.intent,
            req.epoch,
        )
        .map_err(|_| StoreError::PreconditionFailed("invalid idempotency key".to_owned()))?;
        if let Some(existing) = self
            .tx
            .snapshot
            .outbox
            .values()
            .find(|row| row.action.idempotency_key == key)
        {
            return Ok(Enqueued {
                action: existing.action.clone(),
                created: false,
            });
        }

        let id = self
            .tx
            .store
            .deps
            .ids
            .next::<Act>()
            .map_err(StoreError::Id)?;
        let now = self.tx.now();
        let enqueued_seq = Seq::new(self.tx.snapshot.next_enqueue_seq).map_err(StoreError::Seq)?;
        self.tx.snapshot.next_enqueue_seq += 1;
        let payload = ActionPayload::from_value(req.kind, req.payload).map_err(|error| {
            StoreError::PreconditionFailed(format!("invalid outbox payload: {error}"))
        })?;
        let action = OutboxAction {
            id,
            project_id: req.project_id,
            delivery_id: req.delivery_id,
            idempotency_key: key,
            kind: req.kind,
            payload,
            requires_approval: req.requires_approval,
            depends_on: req.depends_on,
            priority: req.priority,
            not_before: req.not_before,
            epoch: req.epoch,
            status: OutboxStatus::Queued,
            attempts: 0,
            lease_until: None,
            last_error: None,
            expected_effects: Vec::new(),
            github_ref: None,
            created_at: now,
            updated_at: now,
        };
        let _ = &req.coalesce_key;
        let _ = &req.enqueued_by;
        self.tx.snapshot.outbox.insert(
            id,
            OutboxRow {
                action: action.clone(),
                enqueued_seq,
            },
        );
        Ok(Enqueued {
            action,
            created: true,
        })
    }

    pub async fn claim(&mut self, opts: ClaimOpts) -> Result<Option<Claimed>, StoreError> {
        let now = self.tx.now();
        cancel_failed_dependents(&mut self.tx.snapshot, now);
        let mut candidates: Vec<(u8, Seq, ActionId)> = Vec::new();
        for (id, row) in &self.tx.snapshot.outbox {
            if opts.exclude_deliveries.contains(&row.action.delivery_id) {
                continue;
            }
            let ready = match row.action.status {
                OutboxStatus::Queued => row
                    .action
                    .not_before
                    .map(|not_before| not_before <= now)
                    .unwrap_or(true),
                OutboxStatus::Executing => row
                    .action
                    .lease_until
                    .map(|lease| lease <= now)
                    .unwrap_or(true),
                _ => false,
            };
            if !ready {
                continue;
            }
            let dependencies_ok = row.action.depends_on.iter().all(|dep| {
                self.tx
                    .snapshot
                    .outbox
                    .get(dep)
                    .map(|dependency| dependency.action.status == OutboxStatus::Succeeded)
                    .unwrap_or(false)
            });
            if dependencies_ok {
                candidates.push((priority_rank(row.action.priority), row.enqueued_seq, *id));
            }
        }
        candidates.sort();
        let Some((_, _, id)) = candidates.first().copied() else {
            return Ok(None);
        };
        let replay = self.tx.snapshot.outbox[&id].action.status == OutboxStatus::Executing;
        let lease_until = Timestamp::from_millis(
            now.as_millis()
                .saturating_add(opts.lease.as_millis() as i64),
        );
        let row = self
            .tx
            .snapshot
            .outbox
            .get_mut(&id)
            .ok_or(StoreError::NotFound)?;
        row.action.status = OutboxStatus::Executing;
        row.action.lease_until = Some(lease_until);
        if !replay {
            row.action.attempts += 1;
        }
        row.action.updated_at = now;
        Ok(Some(Claimed {
            action: row.action.clone(),
            replay,
        }))
    }

    pub async fn record_effects(
        &mut self,
        id: &ActionId,
        effects: Vec<gantry_core::entities::ExpectedEffect>,
    ) -> Result<(), StoreError> {
        let now = self.tx.now();
        let row = self
            .tx
            .snapshot
            .outbox
            .get_mut(id)
            .ok_or(StoreError::NotFound)?;
        if row.action.status != OutboxStatus::Executing {
            return Err(StoreError::PreconditionFailed(
                "outbox record_effects requires executing".to_owned(),
            ));
        }
        row.action.expected_effects = effects;
        row.action.updated_at = now;
        Ok(())
    }

    pub async fn renew_lease(&mut self, id: &ActionId, until: Timestamp) -> Result<(), StoreError> {
        let now = self.tx.now();
        let row = self
            .tx
            .snapshot
            .outbox
            .get_mut(id)
            .ok_or(StoreError::NotFound)?;
        if row.action.status != OutboxStatus::Executing {
            return Err(StoreError::PreconditionFailed(
                "outbox lease renewal requires executing".to_owned(),
            ));
        }
        if until < now {
            return Err(StoreError::PreconditionFailed(
                "outbox lease must not end in the past".to_owned(),
            ));
        }
        row.action.lease_until = Some(until);
        row.action.updated_at = now;
        Ok(())
    }

    pub async fn requeue(
        &mut self,
        id: &ActionId,
        not_before: Timestamp,
        refund_attempt: bool,
    ) -> Result<(), StoreError> {
        let now = self.tx.now();
        let row = self
            .tx
            .snapshot
            .outbox
            .get_mut(id)
            .ok_or(StoreError::NotFound)?;
        if row.action.status != OutboxStatus::Executing {
            return Err(StoreError::PreconditionFailed(
                "outbox requeue requires executing".to_owned(),
            ));
        }
        row.action.status = OutboxStatus::Queued;
        row.action.not_before = Some(not_before);
        row.action.lease_until = None;
        if refund_attempt {
            row.action.attempts = row.action.attempts.saturating_sub(1);
        }
        row.action.updated_at = now;
        Ok(())
    }

    pub async fn cancel_blocked(&mut self) -> Result<Vec<ActionId>, StoreError> {
        let now = self.tx.now();
        Ok(cancel_failed_dependents(&mut self.tx.snapshot, now))
    }

    pub async fn count_succeeded_since(
        &mut self,
        delivery_id: &DeliveryId,
        since: Timestamp,
    ) -> Result<u64, StoreError> {
        Ok(self
            .tx
            .snapshot
            .outbox
            .values()
            .filter(|row| {
                row.action.delivery_id == *delivery_id
                    && row.action.status == OutboxStatus::Succeeded
                    && row.action.updated_at >= since
            })
            .count() as u64)
    }

    pub async fn complete(
        &mut self,
        id: &ActionId,
        github_ref: GithubRef,
    ) -> Result<(), StoreError> {
        let row = self
            .tx
            .snapshot
            .outbox
            .get(id)
            .ok_or(StoreError::NotFound)?;
        if row.action.status != OutboxStatus::Executing {
            return Err(StoreError::PreconditionFailed(
                "outbox complete requires executing".to_owned(),
            ));
        }
        let project_id = row.action.project_id;
        let subject = Subject::parse(&id.to_string())
            .map_err(|_| StoreError::PreconditionFailed("invalid action id".to_owned()))?;
        let data = serde_json::json!({ "github_ref": github_ref });
        self.tx
            .append(NewEvent {
                ty: EventType::OutboxSucceeded,
                subject,
                project_id: Some(project_id),
                actor: Actor::new(ActorKind::Forge, "forge"),
                causation_id: None,
                correlation_id: None,
                data,
                dedup_key: None,
            })
            .await?;
        Ok(())
    }

    pub async fn fail(
        &mut self,
        id: &ActionId,
        error: OutboxError,
        then: AfterFailure,
    ) -> Result<(), StoreError> {
        let now = self.tx.now();
        let row = self
            .tx
            .snapshot
            .outbox
            .get_mut(id)
            .ok_or(StoreError::NotFound)?;
        match then {
            AfterFailure::Retry { not_before } => {
                if row.action.status != OutboxStatus::Executing {
                    return Err(StoreError::PreconditionFailed(
                        "outbox retry requires executing".to_owned(),
                    ));
                }
                row.action.status = OutboxStatus::Queued;
                row.action.not_before = Some(not_before);
                row.action.last_error = Some(error);
                row.action.lease_until = None;
                row.action.updated_at = now;
                Ok(())
            }
            AfterFailure::Terminal => {
                // 只有执行中的动作可以进入终态失败；queued / 已终态一律拒绝。
                if row.action.status != OutboxStatus::Executing {
                    return Err(StoreError::PreconditionFailed(format!(
                        "outbox terminal fail for {id} requires executing, found {:?}",
                        row.action.status
                    )));
                }
                let code = parse_failure_code(&error.code).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!(
                        "unknown outbox failure code {}",
                        error.code
                    ))
                })?;
                let project_id = row.action.project_id;
                let subject = Subject::parse(&id.to_string())
                    .map_err(|_| StoreError::PreconditionFailed("invalid action id".to_owned()))?;
                let data = serde_json::json!({
                    "code": code,
                    "message": error.message,
                });
                self.tx
                    .append(NewEvent {
                        ty: EventType::OutboxFailed,
                        subject,
                        project_id: Some(project_id),
                        actor: Actor::new(ActorKind::Forge, "forge"),
                        causation_id: None,
                        correlation_id: None,
                        data,
                        dedup_key: None,
                    })
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn cancel(&mut self, id: &ActionId, reason: CancelReason) -> Result<(), StoreError> {
        let now = self.tx.now();
        let row = self
            .tx
            .snapshot
            .outbox
            .get_mut(id)
            .ok_or(StoreError::NotFound)?;
        if !matches!(
            row.action.status,
            OutboxStatus::Queued | OutboxStatus::Executing
        ) {
            return Err(StoreError::PreconditionFailed(
                "only queued or executing actions can be cancelled".to_owned(),
            ));
        }
        row.action.status = OutboxStatus::Cancelled;
        row.action.last_error = Some(OutboxError {
            code: cancel_reason_code(reason).to_owned(),
            message: String::new(),
        });
        row.action.lease_until = None;
        row.action.updated_at = now;
        Ok(())
    }
}

/// 显式 snake_case；不用 `Debug` 小写，避免改枚举名时静默改协议值。
fn cancel_reason_code(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::Superseded => "superseded",
        CancelReason::DependencyFailed => "dependency_failed",
        CancelReason::ApprovalInvalid => "approval_invalid",
        CancelReason::Manual => "manual",
    }
}

/// 认领前把依赖已失败/已取消的 queued 动作级联取消（直到不动点）。
///
/// 迭代到不动点处理多层链与排序倒置；缺失的依赖继续阻塞（fail-closed）。
/// 不追加 `outbox.cancelled` 事件：该事件未定义。
fn cancel_failed_dependents(inner: &mut Inner, now: Timestamp) -> Vec<ActionId> {
    let mut cancelled = Vec::new();
    loop {
        let failed: BTreeSet<ActionId> = inner
            .outbox
            .iter()
            .filter(|(_, row)| {
                matches!(
                    row.action.status,
                    OutboxStatus::Failed | OutboxStatus::Cancelled
                )
            })
            .map(|(id, _)| *id)
            .collect();
        let mut changed = 0usize;
        for row in inner.outbox.values_mut() {
            if row.action.status != OutboxStatus::Queued {
                continue;
            }
            if !row
                .action
                .depends_on
                .iter()
                .any(|dependency| failed.contains(dependency))
            {
                continue;
            }
            row.action.status = OutboxStatus::Cancelled;
            row.action.last_error = Some(OutboxError {
                code: "dependency_failed".to_owned(),
                message: String::new(),
            });
            row.action.lease_until = None;
            row.action.updated_at = now;
            cancelled.push(row.action.id);
            changed += 1;
        }
        if changed == 0 {
            return cancelled;
        }
    }
}

fn parse_failure_code(code: &str) -> Option<gantry_core::events::OutboxFailureCode> {
    use gantry_core::events::OutboxFailureCode;
    Some(match code {
        "rate_limited" => OutboxFailureCode::RateLimited,
        "transient" => OutboxFailureCode::Transient,
        "token_expired" => OutboxFailureCode::TokenExpired,
        "precondition_failed" => OutboxFailureCode::PreconditionFailed,
        "not_mergeable" => OutboxFailureCode::NotMergeable,
        "validation_failed" => OutboxFailureCode::ValidationFailed,
        "permission_denied" => OutboxFailureCode::PermissionDenied,
        "target_missing" => OutboxFailureCode::TargetMissing,
        "approval_invalid" => OutboxFailureCode::ApprovalInvalid,
        "signature_required" => OutboxFailureCode::SignatureRequired,
        _ => return None,
    })
}

fn failure_code_str(code: gantry_core::events::OutboxFailureCode) -> &'static str {
    use gantry_core::events::OutboxFailureCode;
    match code {
        OutboxFailureCode::RateLimited => "rate_limited",
        OutboxFailureCode::Transient => "transient",
        OutboxFailureCode::TokenExpired => "token_expired",
        OutboxFailureCode::PreconditionFailed => "precondition_failed",
        OutboxFailureCode::NotMergeable => "not_mergeable",
        OutboxFailureCode::ValidationFailed => "validation_failed",
        OutboxFailureCode::PermissionDenied => "permission_denied",
        OutboxFailureCode::TargetMissing => "target_missing",
        OutboxFailureCode::ApprovalInvalid => "approval_invalid",
        OutboxFailureCode::SignatureRequired => "signature_required",
    }
}

fn priority_rank(priority: Priority) -> u8 {
    match priority {
        Priority::High => 0,
        Priority::Normal => 1,
        Priority::Low => 2,
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn fold_error(error: FoldError) -> StoreError {
    match error {
        FoldError::Data(error) => StoreError::InvalidEvent(error),
        FoldError::Precondition(message) => StoreError::PreconditionFailed(message),
    }
}

fn decode_data<T: gantry_core::events::EventData>(event: &Event<Value>) -> Result<T, SchemaError> {
    gantry_core::events::decode_event_data(event)
}

fn subject_id<K: gantry_core::ids::IdKind>(
    subject: &Subject,
) -> Result<gantry_core::ids::Id<K>, StoreError> {
    let text = subject.as_str();
    let kind = text
        .split_once('_')
        .map(|(prefix, _)| prefix)
        .unwrap_or_default();
    if kind != K::PREFIX {
        return Err(StoreError::PreconditionFailed(format!(
            "subject {text} is not a {} id",
            K::PREFIX
        )));
    }
    text.parse()
        .map_err(|_| StoreError::PreconditionFailed(format!("invalid subject {text}")))
}

#[cfg(feature = "testing")]
pub mod testing {
    //! 测试支撑：与 `core::testing` 的假时钟 / 确定性熵源一起使用。
    pub use gantry_core::testing::{FakeClock, SeqEntropy};
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid event: {0}")]
    InvalidEvent(SchemaError),
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
    #[error("not found")]
    NotFound,
    #[error("invalid cursor")]
    InvalidCursor,
    #[error("cursor ahead of head {head:?}")]
    CursorAhead { head: Seq },
    #[error("id generation failed: {0}")]
    Id(IdError),
    #[error("seq error: {0}")]
    Seq(gantry_core::entities::SeqError),
    #[error("unsupported in M0: {0}")]
    Unsupported(&'static str),
    #[error("store lock poisoned")]
    Poisoned,
    /// `ModuleSpec` 的模块名或表名不合规（§7.10 的前缀检查）。
    #[error("invalid module spec: {0}")]
    InvalidModuleSpec(String),
    /// 模块表文档无法编码 / 解码为调用方给的类型。
    #[error("module table value: {0}")]
    ModuleValue(String),
    /// CAS 对象超过 `store.cas.max_object_bytes`，或超过 `get_bytes` 的 `max`。
    #[error("cas object of {bytes} bytes exceeds {max}")]
    CasTooLarge { bytes: u64, max: u64 },
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("io: {0}")]
    Io(String),
}

impl StoreError {
    pub fn is_contract_undefined(&self) -> bool {
        matches!(
            self,
            StoreError::InvalidEvent(SchemaError {
                code: SchemaErrorCode::ContractUndefined,
                ..
            })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_codes_roundtrip() {
        for code in [
            "rate_limited",
            "transient",
            "token_expired",
            "precondition_failed",
            "not_mergeable",
            "validation_failed",
            "permission_denied",
            "target_missing",
            "approval_invalid",
            "signature_required",
        ] {
            let parsed = parse_failure_code(code).unwrap();
            assert_eq!(failure_code_str(parsed), code);
        }
        assert!(parse_failure_code("bogus").is_none());
    }
}

#[cfg(all(test, feature = "testing"))]
mod tx_behavior {
    use super::*;
    use gantry_core::testing::{FakeClock, SeqEntropy};
    use serde_json::json;
    use std::sync::Arc;

    const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");
    const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
    const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";

    async fn store() -> Store {
        let clock = Arc::new(FakeClock::new(1_700_000_000_000));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(11))));
        Store::open_in_memory(StoreDeps { clock, ids }, &[])
            .await
            .unwrap()
    }

    fn project() -> ProjectId {
        PROJECT.parse().unwrap()
    }

    fn delivery_created() -> NewEvent {
        let cases: Value = serde_json::from_str(CASES).unwrap();
        NewEvent {
            ty: EventType::DeliveryCreated,
            subject: Subject::parse(DELIVERY).unwrap(),
            project_id: Some(project()),
            actor: Actor::new(ActorKind::Engine, "engine"),
            causation_id: None,
            correlation_id: None,
            data: cases["valid"]["delivery.created"].clone(),
            dedup_key: None,
        }
    }

    fn project_settings(base_rev: u64) -> NewEvent {
        NewEvent {
            ty: EventType::SettingsUpdated,
            subject: Subject::parse(PROJECT).unwrap(),
            project_id: Some(project()),
            actor: Actor::new(ActorKind::Engine, "engine"),
            causation_id: None,
            correlation_id: None,
            data: json!({
                "scope": "project",
                "resource": "levels",
                "schema_version": 1,
                "base_rev": base_rev,
                "value": {"high": {"budget": {"nodes": 10}}},
                "relaxes": [],
                "secret_changes": [],
            }),
            dedup_key: None,
        }
    }

    #[tokio::test]
    async fn project_settings_reads_require_project_id() {
        let store = store().await;
        let mut tx = store.begin_write().await.unwrap();
        tx.append(project_settings(0)).await.unwrap();
        tx.commit().await.unwrap();

        let snapshot = store.snapshot().await.unwrap();
        let project = project();
        assert_eq!(
            snapshot
                .settings_current(&SettingsScope::Project, SettingsResource::Levels)
                .rev,
            Seq::ZERO
        );
        let current = snapshot.settings_current_for(
            &SettingsScope::Project,
            Some(&project),
            SettingsResource::Levels,
        );
        assert_eq!(current.rev.as_u64(), 1);
        assert_eq!(current.value.unwrap()["high"]["budget"]["nodes"], json!(10));
        assert_eq!(
            snapshot
                .settings_current_for(&SettingsScope::Project, None, SettingsResource::Levels)
                .rev,
            Seq::ZERO
        );
        let at_rev = snapshot
            .settings_at_rev_for(
                &SettingsScope::Project,
                Some(&project),
                SettingsResource::Levels,
                Seq::new(1).unwrap(),
            )
            .expect("project settings at rev 1");
        assert_eq!(at_rev.rev.as_u64(), 1);
        assert!(snapshot
            .settings_at_rev_for(
                &SettingsScope::Project,
                Some(&project),
                SettingsResource::Levels,
                Seq::ZERO,
            )
            .is_none());
        assert_eq!(
            snapshot
                .settings_as_of_for(
                    &SettingsScope::Project,
                    Some(&project),
                    SettingsResource::Levels,
                    Seq::new(1).unwrap(),
                )
                .rev
                .as_u64(),
            1
        );
    }

    #[tokio::test]
    async fn settings_base_rev_guard_rejects_and_rolls_back() {
        let store = store().await;
        let mut tx = store.begin_write().await.unwrap();
        tx.append(project_settings(0)).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin_write().await.unwrap();
        let mut second = project_settings(0);
        second.data["value"] = json!({"high": {"budget": {"nodes": 20}}});
        let error = tx.append(second).await.unwrap_err();
        assert!(matches!(error, StoreError::PreconditionFailed(_)));
        tx.rollback().await.unwrap();

        assert_eq!(store.events().head().await.unwrap().as_u64(), 1);
        let snapshot = store.snapshot().await.unwrap();
        let current = snapshot.settings_current_for(
            &SettingsScope::Project,
            Some(&project()),
            SettingsResource::Levels,
        );
        assert_eq!(current.rev.as_u64(), 1);
        assert_eq!(current.value.unwrap()["high"]["budget"]["nodes"], json!(10));
    }

    #[tokio::test]
    async fn duplicate_delivery_created_is_rejected() {
        let store = store().await;
        let mut tx = store.begin_write().await.unwrap();
        tx.append(delivery_created()).await.unwrap();
        let error = tx.append(delivery_created()).await.unwrap_err();
        assert!(matches!(error, StoreError::PreconditionFailed(_)));
        tx.rollback().await.unwrap();
        assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
    }
}
