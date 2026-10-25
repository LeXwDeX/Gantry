#![forbid(unsafe_code)]

//! Gantry 事件存储的 M0 内存实现（store.md §1.1、§6 的 `open_in_memory`）。
//!
//! 只实现单进程、单写者的最小一致性行为：追加事件、`dedup_key` 去重、投影、
//! Outbox 表、游标与 `log_epoch`。SQLite、模块投影器、CAS、备份与完整
//! `ProjectionRead` 留给 M1；未实现的入口在这里显式返回错误或记录到
//! `unprojected`，不静默吞掉。缺口清单见 `docs/m0-contract-gaps.md`。
//!
//! M0 的 `WriteTx` 在 `Inner` 的克隆上工作：`commit` 原子替换，`rollback` 丢弃。
//! 这对测试替身足够；真实存储必须用 SQLite 事务。

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use gantry_core::canon::IdempotencyKey;
use gantry_core::entities::{
    Actor, ActorKind, ForgeActionKind, GithubRef, OutboxAction, OutboxError, OutboxStatus,
    Priority, Seq, SettingsResource, SettingsScope, Stage, StageStatus,
};
use gantry_core::events::{
    Event, EventType, NewEvent, SchemaError, SchemaErrorCode, SettingsUpdatedData, Subject,
};
use gantry_core::forge::ActionPayload;
use gantry_core::ids::{
    Act, ActionId, Clock, DeliveryId, Dlv, EventId, Evt, IdError, IdGen, ProjectId,
};
use gantry_core::level::Level;
use gantry_core::time::Timestamp;
use serde_json::Value;

// ---------------------------------------------------------------------------
// 打开
// ---------------------------------------------------------------------------

pub struct StoreDeps {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<IdGen>,
}

/// 模块注册项；M0 只接受空列表（§7.10 的投影器注册从 M1 开始）。
pub struct ModuleSpec {
    pub module: &'static str,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<RwLock<Inner>>,
    gate: Arc<WriteGate>,
    deps: Arc<StoreDeps>,
}

impl Store {
    /// M0 内存实现。`modules` 非空时返回 `Unsupported`，避免假装已支持模块投影器。
    #[cfg(feature = "testing")]
    pub async fn open_in_memory(
        deps: StoreDeps,
        modules: &[ModuleSpec],
    ) -> Result<Store, StoreError> {
        if !modules.is_empty() {
            return Err(StoreError::Unsupported(
                "module projectors are M1 (store.md §7.10)",
            ));
        }
        let log_id = deps.ids.next_ulid().map_err(StoreError::Id)?.to_string();
        Ok(Store {
            inner: Arc::new(RwLock::new(Inner {
                log_id,
                log_epoch: 1,
                next_enqueue_seq: 1,
                ..Inner::default()
            })),
            gate: Arc::new(WriteGate::new()),
            deps: Arc::new(deps),
        })
    }

    pub async fn begin_write(&self) -> Result<WriteTx, StoreError> {
        let lease = self.gate.acquire();
        let snapshot = self.inner.read().map_err(|_| StoreError::Poisoned)?.clone();
        let first_seq = snapshot.head;
        Ok(WriteTx {
            store: self.clone(),
            snapshot,
            lease,
            first_seq,
        })
    }

    pub async fn snapshot(&self) -> Result<ReadSnapshot, StoreError> {
        Ok(ReadSnapshot {
            inner: self.inner.read().map_err(|_| StoreError::Poisoned)?.clone(),
        })
    }

    pub fn events(&self) -> EventLog {
        EventLog {
            store: self.clone(),
        }
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
    #[cfg(feature = "testing")]
    pub fn restore_epoch_bump(&self) -> LogIdentity {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.log_epoch += 1;
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogIdentity {
    pub log_id: String,
    pub log_epoch: u64,
}

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Inner {
    events: Vec<Event<Value>>,
    head: u64,
    last_ts: Option<Timestamp>,
    log_id: String,
    log_epoch: u64,
    dedup: BTreeMap<String, Seq>,
    cursors: BTreeMap<String, Seq>,
    outbox: BTreeMap<ActionId, OutboxRow>,
    settings: BTreeMap<SettingsKey, Vec<SettingsRev>>,
    secret_changes: BTreeMap<(String, u64), Seq>,
    next_enqueue_seq: u64,
    deliveries: BTreeMap<DeliveryId, DeliveryProjection>,
    unprojected: Vec<UnprojectedEvent>,
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
struct DeliveryProjection {
    id: DeliveryId,
    project_id: ProjectId,
    level_cap: Level,
    level_effective: Level,
    stage: Stage,
    stage_status: StageStatus,
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

/// 单写者令牌：`begin_write` 阻塞到前一个事务结束；`Drop` 归还。
struct WriteGate {
    waiters: Mutex<mpsc::Receiver<()>>,
    release: mpsc::Sender<()>,
}

impl WriteGate {
    fn new() -> Self {
        let (release, waiters) = mpsc::channel();
        // 初始令牌：第一个写者不必等待。
        let _ = release.send(());
        Self {
            waiters: Mutex::new(waiters),
            release,
        }
    }

    fn acquire(self: &Arc<Self>) -> WriteLease {
        let token = self
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv()
            .ok();
        WriteLease {
            gate: Arc::clone(self),
            token,
        }
    }
}

struct WriteLease {
    gate: Arc<WriteGate>,
    token: Option<()>,
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        if self.token.take().is_some() {
            let _ = self.gate.release.send(());
        }
    }
}

pub struct WriteTx {
    store: Store,
    snapshot: Inner,
    #[allow(dead_code)]
    lease: WriteLease,
    first_seq: u64,
}

impl WriteTx {
    /// 事务开始时的时钟值；同一事务内复用，保证时间一致。
    pub fn now(&self) -> Timestamp {
        self.snapshot
            .last_ts
            .map(|last| last.max(self.store.deps.clock.now()))
            .unwrap_or_else(|| self.store.deps.clock.now())
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
        let ts = self
            .snapshot
            .last_ts
            .map(|last| last.max(self.store.deps.clock.now()))
            .unwrap_or_else(|| self.store.deps.clock.now());

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

    pub async fn commit(self) -> Result<Committed, StoreError> {
        let mut inner = self.store.inner.write().map_err(|_| StoreError::Poisoned)?;
        if inner.head != self.first_seq {
            return Err(StoreError::PreconditionFailed(
                "another writer committed first".to_owned(),
            ));
        }
        *inner = self.snapshot;
        Ok(Committed {
            first_seq: if inner.head > self.first_seq {
                self.first_seq + 1
            } else {
                0
            },
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
                let data = decode_data::<gantry_core::events::DeliveryCreatedData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Dlv>(&event.subject)?;
                if self.snapshot.deliveries.contains_key(&id) {
                    return Err(StoreError::PreconditionFailed(format!(
                        "delivery {id} already created"
                    )));
                }
                let delivery = DeliveryProjection {
                    id,
                    project_id: event.project_id.ok_or_else(|| {
                        StoreError::InvalidEvent(SchemaError::consistency(
                            "/project_id",
                            "delivery.created requires project_id",
                        ))
                    })?,
                    level_cap: data.level_cap,
                    level_effective: data.level_effective,
                    // data-model §4.1：交付单元从 spec 开始。
                    stage: Stage::Spec,
                    stage_status: StageStatus::Running,
                };
                self.snapshot.deliveries.insert(delivery.id, delivery);
            }
            EventType::DeliveryStageChanged => {
                let data = decode_data::<gantry_core::events::DeliveryStageChangedData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Dlv>(&event.subject)?;
                let current = self.snapshot.deliveries.get(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("delivery {id} has no projection row"))
                })?;
                if current.stage != data.from {
                    return Err(StoreError::PreconditionFailed(format!(
                        "delivery {id} stage is {}, event says from {}",
                        current.stage, data.from
                    )));
                }
                if let Some(delivery) = self.snapshot.deliveries.get_mut(&id) {
                    delivery.stage = data.to;
                    delivery.stage_status = data.stage_status;
                }
            }
            EventType::DeliveryLevelChanged => {
                let data = decode_data::<gantry_core::events::DeliveryLevelChangedData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Dlv>(&event.subject)?;
                let current = self.snapshot.deliveries.get(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("delivery {id} has no projection row"))
                })?;
                let expected = match data.kind {
                    gantry_core::events::LevelChangeKind::Cap => current.level_cap,
                    gantry_core::events::LevelChangeKind::Effective => current.level_effective,
                };
                if expected != data.from {
                    return Err(StoreError::PreconditionFailed(format!(
                        "delivery {id} level does not match event.from"
                    )));
                }
                if let Some(delivery) = self.snapshot.deliveries.get_mut(&id) {
                    match data.kind {
                        gantry_core::events::LevelChangeKind::Cap => {
                            delivery.level_cap = data.to;
                            if delivery.level_effective > data.to {
                                delivery.level_effective = data.to;
                            }
                        }
                        gantry_core::events::LevelChangeKind::Effective => {
                            delivery.level_effective = data.to;
                        }
                    }
                }
            }
            EventType::DeliveryAbandoned => {
                let id = subject_id::<Dlv>(&event.subject)?;
                let delivery = self.snapshot.deliveries.get_mut(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("delivery {id} has no projection row"))
                })?;
                delivery.stage = Stage::Abandoned;
                delivery.stage_status = StageStatus::Done;
            }
            EventType::DeliveryPaused => {
                let data = decode_data::<gantry_core::events::DeliveryPausedData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Dlv>(&event.subject)?;
                let delivery = self.snapshot.deliveries.get_mut(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("delivery {id} has no projection row"))
                })?;
                if delivery.stage_status != data.status_before {
                    return Err(StoreError::PreconditionFailed(
                        "delivery stage_status does not match event".to_owned(),
                    ));
                }
                delivery.stage_status = StageStatus::Paused;
            }
            EventType::DeliveryResumed => {
                let data = decode_data::<gantry_core::events::DeliveryResumedData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Dlv>(&event.subject)?;
                let delivery = self.snapshot.deliveries.get_mut(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("delivery {id} has no projection row"))
                })?;
                delivery.stage_status = data.status_after;
            }
            EventType::OutboxSucceeded => {
                let data = decode_data::<gantry_core::events::OutboxSucceededData>(event)
                    .map_err(StoreError::InvalidEvent)?;
                let id = subject_id::<Act>(&event.subject)?;
                let row = self.snapshot.outbox.get_mut(&id).ok_or_else(|| {
                    StoreError::PreconditionFailed(format!("outbox row {id} not found"))
                })?;
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
                row.action.status = OutboxStatus::Failed;
                row.action.last_error = Some(OutboxError {
                    code: failure_code_str(data.code).to_owned(),
                    message: data.message.unwrap_or_default(),
                });
                row.action.lease_until = None;
                row.action.updated_at = event.ts;
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
}

impl ReadSnapshot {
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

    pub fn outbox_action(&self, id: &ActionId) -> Option<OutboxAction> {
        TxReader { inner: &self.inner }.outbox_action(id)
    }

    pub fn unprojected(&self) -> &[UnprojectedEvent] {
        &self.inner.unprojected
    }
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

    pub async fn advance(&self, consumer: &str, seq: Seq) -> Result<(), StoreError> {
        let mut inner = self.store.inner.write().map_err(|_| StoreError::Poisoned)?;
        let current = inner.cursors.get(consumer).copied().unwrap_or(Seq::ZERO);
        if seq < current {
            return Err(StoreError::PreconditionFailed(
                "cursor must not move backwards".to_owned(),
            ));
        }
        inner.cursors.insert(consumer.to_owned(), seq);
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
        row.action.expected_effects = effects;
        row.action.updated_at = now;
        Ok(())
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
            code: format!("{reason:?}").to_lowercase(),
            message: String::new(),
        });
        row.action.lease_until = None;
        row.action.updated_at = now;
        Ok(())
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
