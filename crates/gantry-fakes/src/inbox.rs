//! `FakeInbox`：内存版 `InboxPort`。
//!
//! 去重只针对 `open` 项；`resolved` / `dismissed` 后同一 `dedup_key` 可再次
//! 创建。`raise` / `resolve` 支持并入 [`FakeInboxTx`]，回滚时不留痕。
//! `register_push_token` 只写私有映射，不产生事件，也不出现在任何读响应里。

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gantry_core::entities::{
    InboxItem, InboxKind, InboxOption, InboxPayload, InboxStatus, Seq, Severity,
};
use gantry_core::ids::{ApprovalId, DeliveryId, DeviceId, InboxId, ProjectId};
use gantry_core::ports::{
    validate_raise_kind, ActorCtx, ApnsEnvironment, ChooseError, ClientProfile, DedupKey,
    InboxError, InboxPort, InboxQuery, InboxSummary, Note, OptionId, Page, ProjectOpenCount,
    PushToken, RaiseError, RaiseOutcome, RaiseRequest, Resolution, ResolveError, ResolveTarget,
    StoreError, TxHandle, Written,
};
use gantry_core::time::Timestamp;

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct InboxSnapshot {
    now: Timestamp,
    generation: u64,
    counter: u128,
    last_seq: u64,
    items: BTreeMap<InboxId, InboxItem>,
    seqs: BTreeMap<InboxId, Seq>,
    resolutions: BTreeMap<InboxId, Resolution>,
    choices: BTreeMap<InboxId, (String, Option<String>)>,
    push_tokens: BTreeMap<DeviceId, (PushToken, ApnsEnvironment)>,
}

impl InboxSnapshot {
    fn new(now: Timestamp) -> Self {
        Self {
            now,
            generation: 0,
            counter: 0,
            last_seq: 0,
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

    fn bump(&mut self) -> Seq {
        self.last_seq += 1;
        Seq::new(self.last_seq).expect("fake inbox seq must be at least 1")
    }

    fn seq_of(&self, id: &InboxId) -> Seq {
        self.seqs.get(id).copied().unwrap_or(Seq::ZERO)
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
    state: Arc<Mutex<InboxSnapshot>>,
}

impl std::fmt::Debug for FakeInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("FakeInbox")
            .field("items", &state.items.len())
            .field("open", &state.open_count())
            .field("now", &state.now)
            .finish()
    }
}

impl FakeInbox {
    pub fn new(now: Timestamp) -> Self {
        Self {
            state: Arc::new(Mutex::new(InboxSnapshot::new(now))),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InboxSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_now(&self, now: Timestamp) {
        self.lock().now = now;
    }

    pub fn advance_ms(&self, ms: i64) {
        let mut state = self.lock();
        state.now = Timestamp::from_millis(state.now.as_millis().saturating_add(ms));
    }

    pub fn begin_write(&self) -> FakeInboxTx {
        let snapshot = self.lock().clone();
        FakeInboxTx {
            inbox: self.clone(),
            snapshot,
        }
    }

    pub fn resolution(&self, id: &InboxId) -> Option<Resolution> {
        self.lock().resolutions.get(id).cloned()
    }

    pub fn chosen_option(&self, id: &InboxId) -> Option<(String, Option<String>)> {
        self.lock().choices.get(id).cloned()
    }

    /// 仅用于测试断言：令牌本身不进入任何读端口响应。
    pub fn push_token_record(&self, device: &DeviceId) -> Option<(PushToken, ApnsEnvironment)> {
        self.lock().push_tokens.get(device).cloned()
    }

    fn apply_raise(
        state: &mut InboxSnapshot,
        req: RaiseRequest,
    ) -> Result<RaiseOutcome, RaiseError> {
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
        let now = state.now;
        for key in req.supersedes {
            let superseded: Vec<InboxId> = state
                .items
                .iter()
                .filter(|(_, item)| open_matches(item, key.as_str()))
                .map(|(id, _)| *id)
                .collect();
            for target in superseded {
                if let Some(item) = state.items.get_mut(&target) {
                    item.status = InboxStatus::Resolved;
                    item.updated_at = now;
                }
                state
                    .resolutions
                    .insert(target, Resolution::Superseded { by: id });
                let seq = state.bump();
                state.seqs.insert(target, seq);
            }
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
        let seq = state.bump();
        state.items.insert(id, item.clone());
        state.seqs.insert(id, seq);
        Ok(RaiseOutcome::Created(Written {
            item,
            last_seq: seq,
        }))
    }

    fn apply_resolve(
        state: &mut InboxSnapshot,
        target: ResolveTarget,
        how: Resolution,
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
        let now = state.now;
        let mut written = Vec::new();
        for id in ids {
            let Some(item) = state.items.get_mut(&id) else {
                continue;
            };
            if item.status != InboxStatus::Open {
                continue;
            }
            item.status = InboxStatus::Resolved;
            item.updated_at = now;
            let item = item.clone();
            let seq = state.bump();
            state.seqs.insert(id, seq);
            state.resolutions.insert(id, how.clone());
            written.push(Written {
                item,
                last_seq: seq,
            });
        }
        Ok(written)
    }
}

impl InboxSnapshot {
    fn open_count(&self) -> usize {
        self.items
            .values()
            .filter(|item| item.status == InboxStatus::Open)
            .count()
    }
}

// ---------------------------------------------------------------------------
// 事务句柄
// ---------------------------------------------------------------------------

pub struct FakeInboxTx {
    inbox: FakeInbox,
    snapshot: InboxSnapshot,
}

impl FakeInboxTx {
    pub async fn commit(mut self) -> Result<(), StoreError> {
        let mut shared = self.inbox.lock();
        if shared.generation != self.snapshot.generation {
            return Err(StoreError::Conflict {
                code: "fake_inbox_tx_conflict",
            });
        }
        self.snapshot.generation += 1;
        *shared = self.snapshot;
        Ok(())
    }

    pub async fn rollback(self) -> Result<(), StoreError> {
        Ok(())
    }
}

impl TxHandle for FakeInboxTx {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn inbox_tx_required() -> StoreError {
    StoreError::Rejected {
        message: "FakeInbox requires a FakeInboxTx when a transaction is supplied".into(),
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
                    .downcast_mut::<FakeInboxTx>()
                    .ok_or_else(|| RaiseError::Store(inbox_tx_required()))?;
                if !Arc::ptr_eq(&tx.inbox.state, &self.state) {
                    return Err(RaiseError::Store(StoreError::Rejected {
                        message: "FakeInboxTx belongs to another FakeInbox".into(),
                    }));
                }
                FakeInbox::apply_raise(&mut tx.snapshot, req)
            }
            None => {
                let mut state = self.lock();
                FakeInbox::apply_raise(&mut state, req)
            }
        }
    }

    async fn resolve(
        &self,
        tx: Option<&mut dyn TxHandle>,
        target: ResolveTarget,
        how: Resolution,
        _ctx: &ActorCtx,
    ) -> Result<Vec<Written<InboxItem>>, ResolveError> {
        match tx {
            Some(tx) => {
                let tx = tx
                    .as_any_mut()
                    .downcast_mut::<FakeInboxTx>()
                    .ok_or_else(|| ResolveError::Store(inbox_tx_required()))?;
                if !Arc::ptr_eq(&tx.inbox.state, &self.state) {
                    return Err(ResolveError::Store(StoreError::Rejected {
                        message: "FakeInboxTx belongs to another FakeInbox".into(),
                    }));
                }
                FakeInbox::apply_resolve(&mut tx.snapshot, target, how)
            }
            None => {
                let mut state = self.lock();
                FakeInbox::apply_resolve(&mut state, target, how)
            }
        }
    }

    async fn dismiss(
        &self,
        id: &InboxId,
        _ctx: &ActorCtx,
        _note: Option<Note>,
    ) -> Result<Written<InboxItem>, InboxError> {
        let mut state = self.lock();
        let now = state.now;
        let item = state.items.get_mut(id).ok_or(InboxError::NotFound)?;
        match item.status {
            InboxStatus::Open => {
                item.status = InboxStatus::Dismissed;
                item.updated_at = now;
                let item = item.clone();
                let seq = state.bump();
                state.seqs.insert(*id, seq);
                Ok(Written {
                    item,
                    last_seq: seq,
                })
            }
            InboxStatus::Dismissed => Ok(Written {
                item: item.clone(),
                last_seq: state.seq_of(id),
            }),
            InboxStatus::Resolved => Err(InboxError::InvalidRequest {
                message: "inbox item is resolved".into(),
            }),
        }
    }

    async fn choose(
        &self,
        id: &InboxId,
        option: &OptionId,
        note: Option<Note>,
        ctx: &ActorCtx,
    ) -> Result<Written<InboxItem>, ChooseError> {
        let mut state = self.lock();
        let Some(item) = state.items.get(id) else {
            return Err(ChooseError::Inbox(InboxError::NotFound));
        };
        if matches!(item.kind, InboxKind::ApprovalNeeded) {
            return Err(ChooseError::UseApprovalRoute);
        }
        if ctx.client_profile == Some(ClientProfile::Mobile) {
            if matches!(item.kind, InboxKind::Escalation) {
                return Err(ChooseError::ProfileForbidden);
            }
            if matches!(item.kind, InboxKind::BudgetExhausted) && option.as_str() == "raise_cap" {
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
        let now = state.now;
        let snapshot = {
            let item = state
                .items
                .get_mut(id)
                .ok_or(ChooseError::Inbox(InboxError::NotFound))?;
            item.status = InboxStatus::Resolved;
            item.updated_at = now;
            item.clone()
        };
        let seq = state.bump();
        state.seqs.insert(*id, seq);
        state.choices.insert(
            *id,
            (
                option.as_str().to_owned(),
                note.map(|n| n.as_str().to_owned()),
            ),
        );
        Ok(Written {
            item: snapshot,
            last_seq: seq,
        })
    }

    async fn get(&self, id: &InboxId) -> Result<Written<InboxItem>, InboxError> {
        let state = self.lock();
        let item = state.items.get(id).ok_or(InboxError::NotFound)?;
        Ok(Written {
            item: item.clone(),
            last_seq: state.seq_of(id),
        })
    }

    async fn list(&self, q: InboxQuery) -> Result<Page<InboxItem>, InboxError> {
        let state = self.lock();
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
        let state = self.lock();
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
        self.lock().push_tokens.insert(device, (token, env));
        Ok(())
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
