//! `FakeInbox`：内存版 `InboxPort`。
//!
//! 条目保存在 `gantry_store::Store` 的 typed 模块快照里。每次实际状态变化都在
//! 同一 `WriteTx` 内追加对应事件；`raise` / `resolve` 可以并入调用方的事务，
//! 回滚时事件与状态一起撤销。`dismiss` / `choose` 自开事务；`register_push_token`
//! 只写私有映射，不产生事件。`new(now)` 自建 Store；`new_in(store)` 与其它替身共享。
//!
//! 去重只针对 `open` 项；`resolved` / `dismissed` 后同一 `dedup_key` 可再次
//! 创建。`register_push_token` 只写私有映射，不产生事件，也不出现在任何读响应里。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{
    InboxItem, InboxKind, InboxOption, InboxPayload, InboxStatus, Seq, Severity,
};
use gantry_core::events::{
    EventType, InboxDismissedData, InboxRaisedData, InboxResolution, InboxResolvedData, NewEvent,
    Subject,
};
use gantry_core::ids::{ApprovalId, DeliveryId, DeviceId, InboxId, ProjectId};
use gantry_core::ports::{
    validate_raise_kind, ActorCtx, ApnsEnvironment, ChooseError, ClientProfile, DedupKey,
    InboxError, InboxPort, InboxQuery, InboxSummary, Note, OptionId, Page, ProjectOpenCount,
    PushToken, RaiseError, RaiseOutcome, RaiseRequest, Resolution, ResolveError, ResolveTarget,
    StoreError, TxHandle, Written,
};
use gantry_core::testing::FakeClock;
use gantry_core::time::Timestamp;
use gantry_store::{Appended, WriteTx};

use crate::{port_store_error, read_state};

const STATE_KEY: &str = "fakes.inbox";

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct InboxState {
    now: Timestamp,
    counter: u128,
    items: BTreeMap<InboxId, InboxItem>,
    seqs: BTreeMap<InboxId, Seq>,
    resolutions: BTreeMap<InboxId, Resolution>,
    choices: BTreeMap<InboxId, (String, Option<String>)>,
    push_tokens: BTreeMap<DeviceId, (PushToken, ApnsEnvironment)>,
}

impl InboxState {
    fn new(now: Timestamp) -> Self {
        Self {
            now,
            counter: 0,
            items: BTreeMap::new(),
            seqs: BTreeMap::new(),
            resolutions: BTreeMap::new(),
            choices: BTreeMap::new(),
            push_tokens: BTreeMap::new(),
        }
    }

    fn next_id(&mut self) -> InboxId {
        self.counter += 1;
        let ms = self.now.as_millis().max(0) as u64;
        gantry_core::ids::Id::from_parts(ms, self.counter)
            .expect("fake inbox counter must fit in 80 bits")
    }

    fn seq_of(&self, id: &InboxId) -> Seq {
        self.seqs.get(id).copied().unwrap_or(Seq::ZERO)
    }

    fn open_count(&self) -> usize {
        self.items
            .values()
            .filter(|item| item.status == InboxStatus::Open)
            .count()
    }
}

fn open_matches(item: &InboxItem, key: &str) -> bool {
    item.status == InboxStatus::Open && item.dedup_key == key
}

// ---------------------------------------------------------------------------
// FakeInbox
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct FakeInbox {
    store: gantry_store::Store,
    clock: Option<Arc<FakeClock>>,
}

impl std::fmt::Debug for FakeInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("FakeInbox")
            .field("items", &state.items.len())
            .field("open", &state.open_count())
            .field("now", &state.now)
            .finish()
    }
}

impl FakeInbox {
    pub fn new(now: Timestamp) -> Self {
        let (store, clock) = crate::open_store(now);
        Self::from_parts(store, Some(clock), now)
    }

    /// 共享调用方的 Store；raise/resolve 可以并入同一个写事务。
    pub fn new_in(store: gantry_store::Store) -> Self {
        let now = store.clock_now();
        Self::from_parts(store, None, now)
    }

    fn from_parts(
        store: gantry_store::Store,
        clock: Option<Arc<FakeClock>>,
        now: Timestamp,
    ) -> Self {
        store
            .register_module_state(STATE_KEY, InboxState::new(now))
            .expect("inbox module state must register");
        Self { store, clock }
    }

    fn state(&self) -> InboxState {
        read_state(&self.store, STATE_KEY).expect("inbox module state must exist")
    }

    pub fn set_now(&self, now: Timestamp) {
        if let Some(clock) = &self.clock {
            clock.set(now);
        }
        self.write_state(|state| state.now = now);
    }

    pub fn advance_ms(&self, ms: i64) {
        if let Some(clock) = &self.clock {
            clock.advance_ms(ms);
        }
        self.write_state(|state| {
            state.now = Timestamp::from_millis(state.now.as_millis().saturating_add(ms));
        });
    }

    fn write_state<R>(&self, f: impl FnOnce(&mut InboxState) -> R) -> R {
        crate::update_state(&self.store, STATE_KEY, f).expect("inbox module state must exist")
    }

    /// 同步入口只给同步测试用；async 端口必须 `self.store.begin_write().await`，
    /// 否则单线程运行时里竞争写令牌会阻塞 runtime 线程而死锁。
    pub fn begin_write(&self) -> WriteTx {
        self.store
            .begin_write_sync()
            .expect("in-memory store write must start")
    }

    pub fn resolution(&self, id: &InboxId) -> Option<Resolution> {
        self.state().resolutions.get(id).cloned()
    }

    pub fn chosen_option(&self, id: &InboxId) -> Option<(String, Option<String>)> {
        self.state().choices.get(id).cloned()
    }

    /// 仅用于测试断言：令牌本身不进入任何读端口响应。
    pub fn push_token_record(&self, device: &DeviceId) -> Option<(PushToken, ApnsEnvironment)> {
        self.state().push_tokens.get(device).cloned()
    }

    async fn apply_resolve(
        tx: &mut WriteTx,
        state: &mut InboxState,
        target: ResolveTarget,
        how: Resolution,
        ctx: &ActorCtx,
        now: Timestamp,
    ) -> Result<Vec<Written<InboxItem>>, ResolveError> {
        let ids: Vec<InboxId> = match target {
            ResolveTarget::Id(id) => state
                .items
                .keys()
                .filter(|item| **item == id)
                .copied()
                .collect(),
            ResolveTarget::Approval(approval) => state
                .items
                .iter()
                .filter(|(_, item)| {
                    matches!(
                        &item.payload,
                        InboxPayload::ApprovalNeeded { approval_id } if approval_id == &approval
                    )
                })
                .map(|(id, _)| *id)
                .collect(),
            ResolveTarget::DedupKey(key) => state
                .items
                .iter()
                .filter(|(_, item)| item.dedup_key == key.as_str())
                .map(|(id, _)| *id)
                .collect(),
            ResolveTarget::Delivery { id, kinds } => state
                .items
                .iter()
                .filter(|(_, item)| {
                    item.delivery_id == Some(id)
                        && kinds
                            .as_ref()
                            .map(|kinds| kinds.iter().any(|kind| kind == &item.kind))
                            .unwrap_or(true)
                })
                .map(|(inbox_id, _)| *inbox_id)
                .collect(),
        };
        let mut written = Vec::new();
        for id in ids {
            let Some(item) = state.items.get(&id) else {
                continue;
            };
            if item.status != InboxStatus::Open {
                continue;
            }
            let data = resolved_data(id, &how);
            let seq = append_inbox_event(tx, EventType::InboxResolved, item, &data, ctx).await?;
            let item = state.items.get_mut(&id).expect("item checked above");
            item.status = InboxStatus::Resolved;
            item.updated_at = now;
            let item = item.clone();
            state.seqs.insert(id, seq);
            state.resolutions.insert(id, how.clone());
            written.push(Written {
                item,
                last_seq: seq,
            });
        }
        Ok(written)
    }

    async fn raise_on(tx: &mut WriteTx, req: RaiseRequest) -> Result<RaiseOutcome, RaiseError> {
        let now = tx.now();
        let mut state = tx
            .module_state::<InboxState>(STATE_KEY)
            .map_err(|error| RaiseError::Store(port_store_error(error)))?
            .ok_or(RaiseError::Store(StoreError::NotFound))?;
        state.now = now;
        if !validate_raise_kind(&req.payload, req.kind.clone()) {
            return Err(RaiseError::InvalidPayload {
                pointer: "/payload".into(),
                message: "payload kind does not match the inbox kind".into(),
            });
        }
        if let Some(existing) = state
            .items
            .values()
            .find(|item| open_matches(item, req.dedup_key.as_str()))
            .cloned()
        {
            let last_seq = state.seq_of(&existing.id);
            return Ok(RaiseOutcome::Deduped(Written {
                item: existing,
                last_seq,
            }));
        }

        let id = state.next_id();
        let mut supersedes = BTreeSet::new();
        for key in &req.supersedes {
            supersedes.extend(
                state
                    .items
                    .iter()
                    .filter(|(_, item)| open_matches(item, key.as_str()))
                    .map(|(id, _)| *id),
            );
        }
        let item = InboxItem {
            id,
            project_id: req.project_id,
            delivery_id: req.delivery_id,
            kind: req.kind,
            severity: req.severity,
            summary: req.summary.as_str().to_owned(),
            payload: req.payload,
            status: InboxStatus::Open,
            dedup_key: req.dedup_key.as_str().to_owned(),
            created_at: now,
            updated_at: now,
        };
        let data = InboxRaisedData {
            item: item.clone(),
            supersedes: supersedes.iter().copied().collect(),
        };
        let event = NewEvent {
            ty: EventType::InboxRaised,
            subject: Subject::parse(&id.to_string()).map_err(|_| {
                RaiseError::Store(StoreError::Rejected {
                    message: "inbox id is not a valid subject".into(),
                })
            })?,
            project_id: item.project_id,
            actor: req.actor,
            causation_id: req.causation_id,
            correlation_id: item.delivery_id.map(|id| id.to_string()),
            data: serde_json::to_value(&data).map_err(|_| {
                RaiseError::Store(StoreError::Rejected {
                    message: "inbox.raised data must serialize".into(),
                })
            })?,
            dedup_key: None,
        };
        let Appended::New(event) = tx
            .append(event)
            .await
            .map_err(|error| RaiseError::Store(port_store_error(error)))?
        else {
            return Err(RaiseError::Store(StoreError::Rejected {
                message: "inbox.raised must not be deduplicated".into(),
            }));
        };
        let seq = event.seq;

        for target in supersedes {
            if let Some(item) = state.items.get_mut(&target) {
                item.status = InboxStatus::Resolved;
                item.updated_at = now;
            }
            state
                .resolutions
                .insert(target, Resolution::Superseded { by: id });
            state.seqs.insert(target, seq);
        }
        state.items.insert(id, item.clone());
        state.seqs.insert(id, seq);
        tx.update_module_state(STATE_KEY, |current: &mut InboxState| *current = state)
            .map_err(|error| RaiseError::Store(port_store_error(error)))?;
        Ok(RaiseOutcome::Created(Written {
            item,
            last_seq: seq,
        }))
    }

    async fn resolve_on(
        tx: &mut WriteTx,
        target: ResolveTarget,
        how: Resolution,
        ctx: &ActorCtx,
    ) -> Result<Vec<Written<InboxItem>>, ResolveError> {
        let now = tx.now();
        let mut state = tx
            .module_state::<InboxState>(STATE_KEY)
            .map_err(|error| ResolveError::Store(port_store_error(error)))?
            .ok_or(ResolveError::Store(StoreError::NotFound))?;
        state.now = now;
        let written = Self::apply_resolve(tx, &mut state, target, how, ctx, now).await?;
        tx.update_module_state(STATE_KEY, |current: &mut InboxState| *current = state)
            .map_err(|error| ResolveError::Store(port_store_error(error)))?;
        Ok(written)
    }

    async fn dismiss_on(
        tx: &mut WriteTx,
        id: &InboxId,
        ctx: &ActorCtx,
        note: Option<Note>,
    ) -> Result<Written<InboxItem>, InboxError> {
        let now = tx.now();
        let mut state = tx
            .module_state::<InboxState>(STATE_KEY)
            .map_err(|error| InboxError::Store(port_store_error(error)))?
            .ok_or(InboxError::Store(StoreError::NotFound))?;
        let item = state.items.get(id).cloned().ok_or(InboxError::NotFound)?;
        match item.status {
            InboxStatus::Open => {
                let data = InboxDismissedData {
                    inbox_id: *id,
                    note: note.map(|note| note.as_str().to_owned()),
                };
                let seq =
                    append_inbox_event(tx, EventType::InboxDismissed, &item, &data, ctx).await?;
                state.now = now;
                let item = state.items.get_mut(id).expect("item checked before append");
                item.status = InboxStatus::Dismissed;
                item.updated_at = now;
                let item = item.clone();
                state.seqs.insert(*id, seq);
                tx.update_module_state(STATE_KEY, |current: &mut InboxState| *current = state)
                    .map_err(|error| InboxError::Store(port_store_error(error)))?;
                Ok(Written {
                    item,
                    last_seq: seq,
                })
            }
            InboxStatus::Dismissed => Ok(Written {
                item,
                last_seq: state.seq_of(id),
            }),
            InboxStatus::Resolved => Err(InboxError::InvalidRequest {
                message: "inbox item is resolved".into(),
            }),
        }
    }

    async fn choose_on(
        tx: &mut WriteTx,
        id: &InboxId,
        option: &OptionId,
        note: Option<Note>,
        ctx: &ActorCtx,
    ) -> Result<Written<InboxItem>, ChooseError> {
        let now = tx.now();
        let mut state = tx
            .module_state::<InboxState>(STATE_KEY)
            .map_err(|error| ChooseError::Inbox(InboxError::Store(port_store_error(error))))?
            .ok_or(ChooseError::Inbox(InboxError::Store(StoreError::NotFound)))?;
        let Some(item) = state.items.get(id).cloned() else {
            return Err(ChooseError::Inbox(InboxError::NotFound));
        };
        if matches!(&item.kind, InboxKind::ApprovalNeeded) {
            return Err(ChooseError::UseApprovalRoute);
        }
        if ctx.client_profile == Some(ClientProfile::Mobile) {
            if matches!(&item.kind, InboxKind::Escalation) {
                return Err(ChooseError::ProfileForbidden);
            }
            if matches!(&item.kind, InboxKind::BudgetExhausted) && option.as_str() == "raise_cap" {
                return Err(ChooseError::ProfileForbidden);
            }
        }
        if item.status != InboxStatus::Open {
            return Err(ChooseError::NotOpen);
        }
        let known = options(&item.payload)
            .map(|options| {
                options
                    .iter()
                    .any(|candidate| candidate.id.as_str() == option.as_str())
            })
            .unwrap_or(false);
        if !known {
            return Err(ChooseError::OptionUnknown);
        }
        let note = note.map(|note| note.as_str().to_owned());
        let data = InboxResolvedData {
            inbox_id: *id,
            resolution: InboxResolution::Chosen,
            option_id: Some(option.as_str().to_owned()),
            note: note.clone(),
            message_id: None,
            approval_id: None,
            reason: None,
        };
        let seq = append_inbox_event(tx, EventType::InboxResolved, &item, &data, ctx)
            .await
            .map_err(|error| ChooseError::Inbox(InboxError::Store(error)))?;
        state.now = now;
        let item = state.items.get_mut(id).expect("item checked before append");
        item.status = InboxStatus::Resolved;
        item.updated_at = now;
        let item = item.clone();
        state.seqs.insert(*id, seq);
        state
            .choices
            .insert(*id, (option.as_str().to_owned(), note));
        tx.update_module_state(STATE_KEY, |current: &mut InboxState| *current = state)
            .map_err(|error| ChooseError::Inbox(InboxError::Store(port_store_error(error))))?;
        Ok(Written {
            item,
            last_seq: seq,
        })
    }
}

fn resolved_data(id: InboxId, how: &Resolution) -> InboxResolvedData {
    let (resolution, message_id, approval_id, reason) = match how {
        Resolution::Answered { message_id } => {
            (InboxResolution::Answered, Some(*message_id), None, None)
        }
        Resolution::ApprovalSettled { approval_id, .. } => (
            InboxResolution::ApprovalSettled,
            None,
            Some(*approval_id),
            None,
        ),
        Resolution::DeliveryClosed => (InboxResolution::DeliveryClosed, None, None, None),
        Resolution::DeliveryAbandoned => (InboxResolution::DeliveryAbandoned, None, None, None),
        Resolution::Superseded { .. } => (InboxResolution::Superseded, None, None, None),
        Resolution::External { reason } => {
            (InboxResolution::External, None, None, Some(reason.clone()))
        }
        Resolution::System { reason } => {
            (InboxResolution::System, None, None, Some(reason.clone()))
        }
    };
    InboxResolvedData {
        inbox_id: id,
        resolution,
        option_id: None,
        note: None,
        message_id,
        approval_id,
        reason,
    }
}

async fn append_inbox_event<T: serde::Serialize>(
    tx: &mut WriteTx,
    ty: EventType,
    item: &InboxItem,
    data: &T,
    ctx: &ActorCtx,
) -> Result<Seq, StoreError> {
    let subject = Subject::parse(&item.id.to_string()).map_err(|_| StoreError::Rejected {
        message: "inbox id is not a valid subject".into(),
    })?;
    let data = serde_json::to_value(data).map_err(|_| StoreError::Rejected {
        message: "inbox event data must serialize".into(),
    })?;
    let event = NewEvent {
        ty,
        subject,
        project_id: item.project_id,
        actor: ctx.actor.clone(),
        causation_id: ctx.causation_id,
        correlation_id: item.delivery_id.map(|id| id.to_string()),
        data,
        dedup_key: None,
    };
    match tx.append(event).await {
        Ok(Appended::New(event)) => Ok(event.seq),
        Ok(Appended::Duplicate { .. }) => Err(StoreError::Rejected {
            message: "inbox event must not be deduplicated".into(),
        }),
        Err(error) => Err(port_store_error(error)),
    }
}

// ---------------------------------------------------------------------------
// InboxPort
// ---------------------------------------------------------------------------

#[async_trait]
impl InboxPort for FakeInbox {
    async fn raise(
        &self,
        tx: Option<&mut dyn TxHandle>,
        req: RaiseRequest,
        _ctx: &ActorCtx,
    ) -> Result<RaiseOutcome, RaiseError> {
        match tx {
            Some(tx) => {
                let tx = tx
                    .as_any_mut()
                    .downcast_mut::<WriteTx>()
                    .ok_or_else(|| RaiseError::Store(inbox_tx_required()))?;
                if !tx.same_store(&self.store) {
                    return Err(RaiseError::Store(StoreError::Rejected {
                        message: "WriteTx belongs to another Store".into(),
                    }));
                }
                Self::raise_on(tx, req).await
            }
            None => {
                let mut tx = self
                    .store
                    .begin_write()
                    .await
                    .map_err(|error| RaiseError::Store(port_store_error(error)))?;
                let outcome = Self::raise_on(&mut tx, req).await?;
                tx.commit()
                    .await
                    .map_err(|error| RaiseError::Store(port_store_error(error)))?;
                Ok(outcome)
            }
        }
    }

    async fn resolve(
        &self,
        tx: Option<&mut dyn TxHandle>,
        target: ResolveTarget,
        how: Resolution,
        ctx: &ActorCtx,
    ) -> Result<Vec<Written<InboxItem>>, ResolveError> {
        match tx {
            Some(tx) => {
                let tx = tx
                    .as_any_mut()
                    .downcast_mut::<WriteTx>()
                    .ok_or_else(|| ResolveError::Store(inbox_tx_required()))?;
                if !tx.same_store(&self.store) {
                    return Err(ResolveError::Store(StoreError::Rejected {
                        message: "WriteTx belongs to another Store".into(),
                    }));
                }
                Self::resolve_on(tx, target, how, ctx).await
            }
            None => {
                let mut tx = self
                    .store
                    .begin_write()
                    .await
                    .map_err(|error| ResolveError::Store(port_store_error(error)))?;
                let written = Self::resolve_on(&mut tx, target, how, ctx).await?;
                tx.commit()
                    .await
                    .map_err(|error| ResolveError::Store(port_store_error(error)))?;
                Ok(written)
            }
        }
    }

    async fn dismiss(
        &self,
        id: &InboxId,
        ctx: &ActorCtx,
        note: Option<Note>,
    ) -> Result<Written<InboxItem>, InboxError> {
        let mut tx = self
            .store
            .begin_write()
            .await
            .map_err(|error| InboxError::Store(port_store_error(error)))?;
        let written = Self::dismiss_on(&mut tx, id, ctx, note).await?;
        tx.commit()
            .await
            .map_err(|error| InboxError::Store(port_store_error(error)))?;
        Ok(written)
    }

    async fn choose(
        &self,
        id: &InboxId,
        option: &OptionId,
        note: Option<Note>,
        ctx: &ActorCtx,
    ) -> Result<Written<InboxItem>, ChooseError> {
        let mut tx = self
            .store
            .begin_write()
            .await
            .map_err(|error| ChooseError::Inbox(InboxError::Store(port_store_error(error))))?;
        let written = Self::choose_on(&mut tx, id, option, note, ctx).await?;
        tx.commit()
            .await
            .map_err(|error| ChooseError::Inbox(InboxError::Store(port_store_error(error))))?;
        Ok(written)
    }

    async fn get(&self, id: &InboxId) -> Result<Written<InboxItem>, InboxError> {
        let state = self.state();
        let item = state.items.get(id).ok_or(InboxError::NotFound)?;
        Ok(Written {
            item: item.clone(),
            last_seq: state.seq_of(id),
        })
    }

    async fn list(&self, q: InboxQuery) -> Result<Page<InboxItem>, InboxError> {
        let state = self.state();
        let status = q.status.unwrap_or(InboxStatus::Open);
        let cursor = match q.page.cursor.as_ref() {
            Some(cursor) => {
                Some(
                    cursor
                        .parse::<InboxId>()
                        .map_err(|_| InboxError::InvalidRequest {
                            message: "invalid inbox cursor".into(),
                        })?,
                )
            }
            None => None,
        };
        let limit = if q.page.limit == 0 {
            50
        } else {
            q.page.limit.min(200)
        } as usize;
        let mut items: Vec<InboxItem> = state
            .items
            .values()
            .filter(|item| {
                item.status == status
                    && q.project_id
                        .map(|id| item.project_id == Some(id))
                        .unwrap_or(true)
                    && q.delivery_id
                        .map(|id| item.delivery_id == Some(id))
                        .unwrap_or(true)
                    && q.kind
                        .as_ref()
                        .map(|kind| &item.kind == kind)
                        .unwrap_or(true)
                    && cursor.map(|cursor| item.id > cursor).unwrap_or(true)
            })
            .cloned()
            .collect();
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if has_more {
            items.last().map(|item| item.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn summary(&self) -> Result<InboxSummary, InboxError> {
        let state = self.state();
        let mut by_project: BTreeMap<Option<ProjectId>, (u32, Severity)> = BTreeMap::new();
        let mut total_open = 0;
        for item in state
            .items
            .values()
            .filter(|item| item.status == InboxStatus::Open)
        {
            total_open += 1;
            let entry = by_project
                .entry(item.project_id)
                .or_insert((0, Severity::Info));
            entry.0 += 1;
            if item.severity > entry.1 {
                entry.1 = item.severity.clone();
            }
        }
        Ok(InboxSummary {
            total_open,
            by_project: by_project
                .into_iter()
                .map(|(project_id, (open, max_severity))| ProjectOpenCount {
                    project_id,
                    open,
                    max_severity,
                })
                .collect(),
        })
    }

    async fn register_push_token(
        &self,
        ctx: &ActorCtx,
        token: PushToken,
        env: ApnsEnvironment,
    ) -> Result<(), InboxError> {
        let device = ctx.device.ok_or_else(|| InboxError::InvalidRequest {
            message: "push token registration requires a device".into(),
        })?;
        self.write_state(|state| {
            state.push_tokens.insert(device, (token, env));
        });
        Ok(())
    }
}

fn inbox_tx_required() -> StoreError {
    StoreError::Rejected {
        message: "FakeInbox requires a Store WriteTx when a transaction is supplied".into(),
    }
}

fn options(payload: &InboxPayload) -> Option<&Vec<InboxOption>> {
    match payload {
        InboxPayload::DecisionNeeded { options, .. }
        | InboxPayload::BudgetExhausted { options, .. } => Some(options),
        InboxPayload::Escalation { options, .. } => options.as_ref(),
        InboxPayload::ApprovalNeeded { .. }
        | InboxPayload::SpecNeeded { .. }
        | InboxPayload::SecurityFinding { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// 一致性用例
// ---------------------------------------------------------------------------

/// 去重只覆盖 open 项；resolve 后可再次创建。
///
/// 前置条件：`req.dedup_key` 当前没有 open 项。
pub async fn dedup_conformance(
    port: &dyn InboxPort,
    req: RaiseRequest,
    ctx: &ActorCtx,
) -> Result<(), String> {
    let first = port
        .raise(None, req.clone(), ctx)
        .await
        .map_err(|error| format!("first raise failed: {error}"))?;
    let RaiseOutcome::Created(first) = first else {
        return Err("first raise must create an item".into());
    };
    let second = port
        .raise(None, req.clone(), ctx)
        .await
        .map_err(|error| format!("second raise failed: {error}"))?;
    let RaiseOutcome::Deduped(second) = second else {
        return Err("an open dedup_key must deduplicate".into());
    };
    if second.item.id != first.item.id {
        return Err("dedup must return the existing item".into());
    }
    port.resolve(
        None,
        ResolveTarget::Id(first.item.id),
        Resolution::System {
            reason: "conformance".into(),
        },
        ctx,
    )
    .await
    .map_err(|error| format!("resolve failed: {error}"))?;
    let third = port
        .raise(None, req, ctx)
        .await
        .map_err(|error| format!("third raise failed: {error}"))?;
    let RaiseOutcome::Created(third) = third else {
        return Err("a resolved dedup_key must allow a new item".into());
    };
    if third.item.id == first.item.id {
        return Err("the new item must have a new inbox id".into());
    }
    Ok(())
}

// 编译期检查。
const _: fn() = || {
    fn assert_inbox<T: InboxPort>() {}
    assert_inbox::<FakeInbox>();
};

#[allow(dead_code)]
fn _type_anchor(_: Option<DeliveryId>, _: Option<ApprovalId>, _: Option<DedupKey>) {}
