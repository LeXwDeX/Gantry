//! `Notify`：基于 `store` 的 `InboxPort` 实现（notify.md §6.1、§7.1–§7.4、§7.6）。
//!
//! 每次状态变化 = 一个写事务里追加一条 `inbox.*` 事件；`store` 的内置
//! `inbox_items` 投影在同一次 `append` 里折叠它，并兜底"open 项上 `dedup_key`
//! 唯一"。所有"先读后写"的判定都用 `tx.read()` 在同一事务里完成，单写者保证
//! 中间不会插入他人的写（§7.1 的条件更新语义）。
//!
//! `raise` / `resolve` 传入 `tx = Some` 时并入调用方事务，回滚一起回滚；
//! `dismiss` / `choose` 自开事务。

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{InboxItem, InboxKind, InboxOption, InboxPayload, InboxStatus, Seq};
use gantry_core::events::{
    EventType, InboxDismissedData, InboxRaisedData, InboxResolution, InboxResolvedData, NewEvent,
    Subject,
};
use gantry_core::ids::{IdGen, Inb, InboxId};
use gantry_core::ports::{
    ActorCtx, ApnsEnvironment, ChooseError, ClientProfile, InboxError, InboxPort, InboxQuery,
    InboxSummary, Note, OptionId, Page as PortPage, PushToken, RaiseError, RaiseOutcome,
    RaiseRequest, Resolution, ResolveError, ResolveTarget, StoreError as PortStoreError, TxHandle,
    Written,
};
use gantry_store::{
    Appended, InboxQuery as StoreInboxQuery, Order, Page, Store, StoreError, TxReader, WriteTx,
};

/// `list` 的默认与最大页大小（api.md 分页约定）。
const LIST_DEFAULT_LIMIT: u32 = 50;
const LIST_MAX_LIMIT: u32 = 200;
/// 内部全量扫描时每页取的条数（store `Page.limit` 的上限）。
const SCAN_PAGE: u32 = 200;

/// 收件箱核心。克隆共享同一个 `Store`。
#[derive(Clone)]
pub struct Notify {
    store: Store,
    ids: Arc<IdGen>,
}

impl std::fmt::Debug for Notify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notify").finish_non_exhaustive()
    }
}

impl Notify {
    /// `ids` 用于生成 `inb_` ID；与 `store` 共用同一个 `IdGen` 即可。
    pub fn new(store: Store, ids: Arc<IdGen>) -> Self {
        Self { store, ids }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 本模块自行发起的写（自动关闭）也要有 `request_id`（P-78）。
    pub(crate) fn request_id(&self) -> gantry_core::ids::Ulid {
        self.ids
            .next_ulid()
            .unwrap_or_else(|_| gantry_core::ids::Ulid::from_parts(0, 0).expect("zero ulid"))
    }

    async fn raise_on(
        &self,
        tx: &mut WriteTx,
        req: RaiseRequest,
    ) -> Result<RaiseOutcome, RaiseError> {
        crate::validate::raise_request(&req)?;
        if let Some(existing) = tx
            .read()
            .inbox_open_by_dedup_key(req.dedup_key.as_str())
            .await
            .map_err(store_error)?
        {
            let last_seq = self.last_seq(tx, &existing.id).await.map_err(store_error)?;
            return Ok(RaiseOutcome::Deduped(Written {
                item: existing,
                last_seq,
            }));
        }

        let mut supersedes = BTreeSet::new();
        for key in &req.supersedes {
            if let Some(item) = tx
                .read()
                .inbox_open_by_dedup_key(key.as_str())
                .await
                .map_err(store_error)?
            {
                supersedes.insert(item.id);
            }
        }

        let now = tx.now();
        let id = self.ids.next::<Inb>().map_err(|error| {
            RaiseError::Store(PortStoreError::Rejected {
                message: format!("inbox id generation failed: {error}"),
            })
        })?;
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
            supersedes: supersedes.into_iter().collect(),
        };
        let last_seq = append(
            tx,
            EventType::InboxRaised,
            &item,
            &data,
            req.actor,
            req.causation_id,
        )
        .await
        .map_err(RaiseError::Store)?;
        Ok(RaiseOutcome::Created(Written { item, last_seq }))
    }

    async fn resolve_on(
        &self,
        tx: &mut WriteTx,
        target: ResolveTarget,
        how: Resolution,
        ctx: &ActorCtx,
    ) -> Result<Vec<Written<InboxItem>>, ResolveError> {
        let targets: Vec<InboxItem> = {
            let mut reader = tx.read();
            match target {
                ResolveTarget::Id(id) => reader
                    .inbox_item(&id)
                    .await
                    .map_err(resolve_store_error)?
                    .into_iter()
                    .collect(),
                ResolveTarget::DedupKey(key) => reader
                    .inbox_open_by_dedup_key(key.as_str())
                    .await
                    .map_err(resolve_store_error)?
                    .into_iter()
                    .collect(),
                ResolveTarget::Approval(approval) => open_items(
                    &mut reader,
                    Some(InboxKind::ApprovalNeeded),
                )
                .await
                .map_err(resolve_store_error)?
                .into_iter()
                .filter(|item| {
                    matches!(
                        &item.payload,
                        InboxPayload::ApprovalNeeded { approval_id } if *approval_id == approval
                    )
                })
                .collect(),
                ResolveTarget::Delivery { id, kinds } => open_items(&mut reader, None)
                    .await
                    .map_err(resolve_store_error)?
                    .into_iter()
                    .filter(|item| {
                        item.delivery_id == Some(id)
                            && kinds
                                .as_ref()
                                .is_none_or(|kinds| kinds.contains(&item.kind))
                    })
                    .collect(),
            }
        };

        let mut written = Vec::new();
        // 目标已非 open 则跳过（§7.4，幂等）。
        for item in targets
            .into_iter()
            .filter(|item| item.status == InboxStatus::Open)
        {
            let data = resolved_data(item.id, &how);
            let last_seq = append(
                tx,
                EventType::InboxResolved,
                &item,
                &data,
                ctx.actor.clone(),
                ctx.causation_id,
            )
            .await
            .map_err(ResolveError::Store)?;
            let item = projected(tx, &item.id).await.map_err(ResolveError::Store)?;
            written.push(Written { item, last_seq });
        }
        Ok(written)
    }

    async fn dismiss_on(
        &self,
        tx: &mut WriteTx,
        id: &InboxId,
        ctx: &ActorCtx,
        note: Option<Note>,
    ) -> Result<Written<InboxItem>, InboxError> {
        let item = tx
            .read()
            .inbox_item(id)
            .await
            .map_err(inbox_store_error)?
            .ok_or(InboxError::NotFound)?;
        match item.status {
            InboxStatus::Open => {
                let data = InboxDismissedData {
                    inbox_id: *id,
                    note: note.map(|note| note.as_str().to_owned()),
                };
                let last_seq = append(
                    tx,
                    EventType::InboxDismissed,
                    &item,
                    &data,
                    ctx.actor.clone(),
                    ctx.causation_id,
                )
                .await
                .map_err(InboxError::Store)?;
                let item = projected(tx, id).await.map_err(InboxError::Store)?;
                Ok(Written { item, last_seq })
            }
            InboxStatus::Dismissed => {
                let last_seq = self.last_seq(tx, id).await.map_err(inbox_store_error)?;
                Ok(Written { item, last_seq })
            }
            // `InboxError` 没有 `NotOpen`；与 `FakeInbox` 一致用 `InvalidRequest`。
            InboxStatus::Resolved => Err(InboxError::InvalidRequest {
                message: "inbox item is resolved".into(),
            }),
        }
    }

    async fn choose_on(
        &self,
        tx: &mut WriteTx,
        id: &InboxId,
        option: &OptionId,
        note: Option<Note>,
        ctx: &ActorCtx,
    ) -> Result<Written<InboxItem>, ChooseError> {
        // 校验顺序固定（§7.4），先失败先返回。
        let item = tx
            .read()
            .inbox_item(id)
            .await
            .map_err(|error| ChooseError::Inbox(inbox_store_error(error)))?
            .ok_or(ChooseError::Inbox(InboxError::NotFound))?;
        if item.kind == InboxKind::ApprovalNeeded {
            return Err(ChooseError::UseApprovalRoute);
        }
        if ctx.client_profile == Some(ClientProfile::Mobile) && mobile_forbidden(&item, option) {
            return Err(ChooseError::ProfileForbidden);
        }
        if item.status != InboxStatus::Open {
            return Err(ChooseError::NotOpen);
        }
        let known = options(&item.payload).is_some_and(|options| {
            options
                .iter()
                .any(|candidate| candidate.id == option.as_str())
        });
        if !known {
            return Err(ChooseError::OptionUnknown);
        }
        let data = InboxResolvedData {
            inbox_id: *id,
            resolution: InboxResolution::Chosen,
            option_id: Some(option.as_str().to_owned()),
            note: note.map(|note| note.as_str().to_owned()),
            message_id: None,
            approval_id: None,
            reason: None,
        };
        let last_seq = append(
            tx,
            EventType::InboxResolved,
            &item,
            &data,
            ctx.actor.clone(),
            ctx.causation_id,
        )
        .await
        .map_err(|error| ChooseError::Inbox(InboxError::Store(error)))?;
        let item = projected(tx, id)
            .await
            .map_err(|error| ChooseError::Inbox(InboxError::Store(error)))?;
        Ok(Written { item, last_seq })
    }

    /// 该项最近一次事件的 seq。已提交的从事件日志精确取得；只存在于当前未提交
    /// 事务里的项（同一事务里先 raise 后被 dedup 命中）取事务 head 作上界——
    /// `store` 的读取不返回单项 `last_seq`（store.md §6.2），而 `X-Event-Seq`
    /// 只要求不小于真实值。
    async fn last_seq(&self, tx: &mut WriteTx, id: &InboxId) -> Result<Seq, StoreError> {
        match self.committed_last_seq(id).await? {
            Some(seq) => Ok(seq),
            None => Ok(tx.read().head_seq()),
        }
    }

    async fn committed_last_seq(&self, id: &InboxId) -> Result<Option<Seq>, StoreError> {
        let subject =
            subject_of(id).map_err(|error| StoreError::PreconditionFailed(error.to_string()))?;
        Ok(self
            .store
            .events()
            .by_subject(&subject)
            .await?
            .last()
            .map(|event| event.seq))
    }

    async fn begin(&self) -> Result<WriteTx, PortStoreError> {
        self.store.begin_write().await.map_err(port_store_error)
    }
}

#[async_trait]
impl InboxPort for Notify {
    async fn raise(
        &self,
        tx: Option<&mut dyn TxHandle>,
        req: RaiseRequest,
        _ctx: &ActorCtx,
    ) -> Result<RaiseOutcome, RaiseError> {
        match tx {
            Some(tx) => {
                self.raise_on(downcast(tx).map_err(RaiseError::Store)?, req)
                    .await
            }
            None => {
                let mut tx = self.begin().await.map_err(RaiseError::Store)?;
                let outcome = self.raise_on(&mut tx, req).await?;
                tx.commit().await.map_err(store_error)?;
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
                self.resolve_on(downcast(tx).map_err(ResolveError::Store)?, target, how, ctx)
                    .await
            }
            None => {
                let mut tx = self.begin().await.map_err(ResolveError::Store)?;
                let written = self.resolve_on(&mut tx, target, how, ctx).await?;
                tx.commit().await.map_err(resolve_store_error)?;
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
        let mut tx = self.begin().await.map_err(InboxError::Store)?;
        let written = self.dismiss_on(&mut tx, id, ctx, note).await?;
        tx.commit().await.map_err(inbox_store_error)?;
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
            .begin()
            .await
            .map_err(|error| ChooseError::Inbox(InboxError::Store(error)))?;
        let written = self.choose_on(&mut tx, id, option, note, ctx).await?;
        tx.commit()
            .await
            .map_err(|error| ChooseError::Inbox(inbox_store_error(error)))?;
        Ok(written)
    }

    async fn get(&self, id: &InboxId) -> Result<Written<InboxItem>, InboxError> {
        let mut snapshot = self.store.snapshot().await.map_err(inbox_store_error)?;
        let item = snapshot
            .inbox_item(id)
            .await
            .map_err(inbox_store_error)?
            .ok_or(InboxError::NotFound)?;
        let last_seq = self
            .committed_last_seq(id)
            .await
            .map_err(inbox_store_error)?
            .unwrap_or(Seq::ZERO);
        Ok(Written { item, last_seq })
    }

    /// 游标是上一页最后一项的 `inb_` ID（与 `FakeInbox` 相同）；默认 `status = open`。
    async fn list(&self, q: InboxQuery) -> Result<PortPage<InboxItem>, InboxError> {
        let after = match q.page.cursor.as_deref() {
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
        let limit = match q.page.limit {
            0 => LIST_DEFAULT_LIMIT,
            limit => limit.min(LIST_MAX_LIMIT),
        } as usize;
        let query = StoreInboxQuery {
            project_id: q.project_id,
            status: Some(q.status.unwrap_or(InboxStatus::Open)),
            kind: q.kind,
        };
        let mut snapshot = self.store.snapshot().await.map_err(inbox_store_error)?;
        // store 的查询没有 `delivery_id` 过滤（store.md §6.2）；按 ID 顺序扫描后在此过滤。
        let mut items = Vec::with_capacity(limit + 1);
        let mut page = Page {
            limit: SCAN_PAGE,
            cursor: None,
            order: Order::Asc,
        };
        'scan: loop {
            let result = snapshot
                .inbox(&query, &page)
                .await
                .map_err(inbox_store_error)?;
            for item in result.items {
                if after.is_some_and(|after| item.id <= after)
                    || q.delivery_id.is_some_and(|id| item.delivery_id != Some(id))
                {
                    continue;
                }
                items.push(item);
                if items.len() > limit {
                    break 'scan;
                }
            }
            match result.next_cursor {
                Some(cursor) => page.cursor = Some(cursor),
                None => break,
            }
        }
        let next_cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|item| item.id.to_string())
        } else {
            None
        };
        Ok(PortPage { items, next_cursor })
    }

    async fn summary(&self) -> Result<InboxSummary, InboxError> {
        let mut snapshot = self.store.snapshot().await.map_err(inbox_store_error)?;
        let by_project = snapshot.inbox_summary().await.map_err(inbox_store_error)?;
        Ok(InboxSummary {
            total_open: by_project.iter().map(|row| row.open).sum(),
            by_project,
        })
    }

    /// 推送令牌归 M4（notify.md §7.9、P-77）；M1 不存储，明确拒绝而不是假装成功。
    async fn register_push_token(
        &self,
        _ctx: &ActorCtx,
        _token: PushToken,
        _env: ApnsEnvironment,
    ) -> Result<(), InboxError> {
        Err(InboxError::InvalidRequest {
            message: "push token registration is not available before M4".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 调用方事务必须是 `store::WriteTx`。内存 Store 的 `same_store` 只在 testing
/// 下可用，这里无法校验事务是否属于本 `Store`；跨 Store 传事务是调用方的编程错误。
fn downcast(tx: &mut dyn TxHandle) -> Result<&mut WriteTx, PortStoreError> {
    tx.as_any_mut()
        .downcast_mut::<WriteTx>()
        .ok_or_else(|| PortStoreError::Rejected {
            message: "notify requires a store WriteTx when a transaction is supplied".into(),
        })
}

async fn append<T: serde::Serialize>(
    tx: &mut WriteTx,
    ty: EventType,
    item: &InboxItem,
    data: &T,
    actor: gantry_core::entities::Actor,
    causation_id: Option<gantry_core::ids::EventId>,
) -> Result<Seq, PortStoreError> {
    let subject = subject_of(&item.id).map_err(|error| PortStoreError::Rejected {
        message: error.to_string(),
    })?;
    let data = serde_json::to_value(data).map_err(|error| PortStoreError::Rejected {
        message: format!("inbox event data must serialize: {error}"),
    })?;
    // 信封 project_id = 该项的 project_id（§7.6 客户端增量计数依赖它）。
    let event = NewEvent {
        ty,
        subject,
        project_id: item.project_id,
        actor,
        causation_id,
        correlation_id: item.delivery_id.map(|id| id.to_string()),
        data,
        dedup_key: None,
    };
    match tx.append(event).await.map_err(port_store_error)? {
        Appended::New(event) => Ok(event.seq),
        Appended::Duplicate { .. } => Err(PortStoreError::Rejected {
            message: "inbox events carry no event dedup_key".into(),
        }),
    }
}

/// 追加之后从投影读回该项，返回值与 `get` 一致（含 `updated_at`）。
async fn projected(tx: &mut WriteTx, id: &InboxId) -> Result<InboxItem, PortStoreError> {
    tx.read()
        .inbox_item(id)
        .await
        .map_err(port_store_error)?
        .ok_or(PortStoreError::NotFound)
}

async fn open_items(
    reader: &mut TxReader<'_>,
    kind: Option<InboxKind>,
) -> Result<Vec<InboxItem>, StoreError> {
    let query = StoreInboxQuery {
        project_id: None,
        status: Some(InboxStatus::Open),
        kind,
    };
    let mut page = Page {
        limit: SCAN_PAGE,
        cursor: None,
        order: Order::Asc,
    };
    let mut items = Vec::new();
    loop {
        let result = reader.inbox(&query, &page).await?;
        items.extend(result.items);
        match result.next_cursor {
            Some(cursor) => page.cursor = Some(cursor),
            None => return Ok(items),
        }
    }
}

fn subject_of(id: &InboxId) -> Result<Subject, gantry_core::events::SubjectError> {
    Subject::parse(&id.to_string())
}

/// 手机档案：`escalation` 的任何选项、标"仅桌面"的选项（`raise_cap`）→ 403（P-35、P-79）。
fn mobile_forbidden(item: &InboxItem, option: &OptionId) -> bool {
    match item.kind {
        InboxKind::Escalation => true,
        InboxKind::BudgetExhausted => option.as_str() == "raise_cap",
        _ => false,
    }
}

fn options(payload: &InboxPayload) -> Option<&[InboxOption]> {
    match payload {
        InboxPayload::DecisionNeeded { options, .. }
        | InboxPayload::BudgetExhausted { options, .. } => Some(options),
        InboxPayload::Escalation { options, .. } => options.as_deref(),
        InboxPayload::ApprovalNeeded { .. }
        | InboxPayload::SpecNeeded { .. }
        | InboxPayload::SecurityFinding { .. } => None,
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

fn port_store_error(error: StoreError) -> PortStoreError {
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

fn store_error(error: StoreError) -> RaiseError {
    RaiseError::Store(port_store_error(error))
}

fn resolve_store_error(error: StoreError) -> ResolveError {
    ResolveError::Store(port_store_error(error))
}

fn inbox_store_error(error: StoreError) -> InboxError {
    InboxError::Store(port_store_error(error))
}
