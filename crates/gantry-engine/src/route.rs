//! 事件归属：engine.md §7.14.2 路由表（M1 收尾评审 A4）。
//!
//! 按事件类型取契约指定的归属字段，不按 `actor`、也不笼统按 `correlation_id` 过滤；
//! 只有契约把归属放在信封里的两类（`approval.requested`、`forge.*`）读 `correlation_id`
//! （event-data.md 的 approval / forge 信封规则）。没有交付单元字段的后续事件
//! （`node.*`、`attempt.finished`、`inbox.resolved`、`outbox.succeeded`、`worktree.unlocked` 等）
//! 按先前事件建立的索引联查；索引随事件顺序增量建立，所以路由器必须按 `seq` 顺序看到
//! 本项目的全部事件（包括不投给本交付单元的）。

use std::collections::BTreeMap;

use gantry_core::events::{
    AttemptStartedData, Event, EventType, InboxDismissedData, InboxRaisedData, InboxResolvedData,
    NodeEventData, NodeRetryRequestedData, OutboxEnqueuedData, PatchRejectedData,
    PatchValidatedData, SchemaError, WorkflowCreatedData, WorkflowRevisedData, WorktreeCreatedData,
    WorktreeLockedData, WorktreeRemovedData, WorktreeUnlockedData,
};
use gantry_core::ids::{
    ActionId, ApprovalId, AttemptId, DeliveryId, InboxId, NodeId, WorkflowId, WorktreeId,
};
use serde_json::Value;

/// 一条事件的去向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// 归属某个交付单元：折叠进它的状态；`input` 表示还要作为 `Input::Event` 投递。
    Delivery { id: DeliveryId, input: bool },
    /// `settings.updated`：投给所有活跃执行体，不折叠。
    Broadcast,
    /// 不归属任何交付单元（含未绑定的 `forge.*`、会话临时图）。
    None,
}

impl Route {
    pub fn delivery(&self) -> Option<DeliveryId> {
        match self {
            Route::Delivery { id, .. } => Some(*id),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("event {ty} data does not match its contract: {source}")]
pub struct RouteError {
    pub ty: &'static str,
    #[source]
    pub source: SchemaError,
}

/// 增量路由器：按 `seq` 顺序喂入事件，返回归属并维护联查索引。
#[derive(Clone, Debug, Default)]
pub struct Router {
    workflows: BTreeMap<WorkflowId, DeliveryId>,
    nodes: BTreeMap<NodeId, DeliveryId>,
    attempts: BTreeMap<AttemptId, DeliveryId>,
    inbox: BTreeMap<InboxId, DeliveryId>,
    actions: BTreeMap<ActionId, DeliveryId>,
    approvals: BTreeMap<ApprovalId, DeliveryId>,
    worktrees: BTreeMap<WorktreeId, DeliveryId>,
}

fn decode<T: gantry_core::events::EventData>(event: &Event<Value>) -> Result<T, RouteError> {
    gantry_core::events::decode_event_data(event).map_err(|source| RouteError {
        ty: event.ty.as_str(),
        source,
    })
}

fn subject<T: std::str::FromStr>(event: &Event<Value>) -> Option<T> {
    event.subject.as_str().parse().ok()
}

fn correlated(event: &Event<Value>) -> Option<DeliveryId> {
    event.correlation_id.as_deref()?.parse().ok()
}

impl Router {
    pub fn route(&mut self, event: &Event<Value>) -> Result<Route, RouteError> {
        use EventType as T;
        let fold_only = |id: Option<DeliveryId>| match id {
            Some(id) => Route::Delivery { id, input: false },
            None => Route::None,
        };
        let deliver = |id: Option<DeliveryId>| match id {
            Some(id) => Route::Delivery { id, input: true },
            None => Route::None,
        };
        Ok(match event.ty {
            T::DeliveryCreated
            | T::DeliveryStageChanged
            | T::DeliveryLevelChanged
            | T::DeliveryAbandoned
            | T::DeliveryPaused
            | T::DeliveryResumed
            | T::BudgetCharged
            | T::BudgetExhausted
            | T::BudgetLimitRaised => fold_only(subject(event)),
            T::WorkflowCreated => {
                let data: WorkflowCreatedData = decode(event)?;
                let owner = data.delivery_id;
                if let (Some(owner), Some(id)) = (owner, subject::<WorkflowId>(event)) {
                    self.workflows.insert(id, owner);
                    for node in &data.nodes {
                        self.nodes.insert(node.node_id, owner);
                    }
                }
                fold_only(owner)
            }
            T::WorkflowRevised => {
                let data: WorkflowRevisedData = decode(event)?;
                let owner =
                    subject::<WorkflowId>(event).and_then(|id| self.workflows.get(&id).copied());
                if let Some(owner) = owner {
                    for node in &data.added {
                        self.nodes.insert(node.node_id, owner);
                    }
                }
                fold_only(owner)
            }
            T::WorkflowFinished => fold_only(
                subject::<WorkflowId>(event).and_then(|id| self.workflows.get(&id).copied()),
            ),
            T::NodeQueued | T::NodeStarted | T::NodeFinished | T::NodeSuperseded => {
                let data: NodeEventData = decode(event)?;
                fold_only(self.nodes.get(&data.node_id).copied())
            }
            T::NodeRetryRequested => {
                let data: NodeRetryRequestedData = decode(event)?;
                fold_only(self.nodes.get(&data.node_id).copied())
            }
            T::AttemptStarted => {
                let data: AttemptStartedData = decode(event)?;
                let owner = self.nodes.get(&data.node_id).copied();
                if let Some(owner) = owner {
                    self.attempts.insert(data.attempt_id, owner);
                }
                fold_only(owner)
            }
            T::AttemptFinished | T::AttemptProgress => fold_only(
                subject::<AttemptId>(event).and_then(|id| self.attempts.get(&id).copied()),
            ),
            T::InboxRaised => {
                let data: InboxRaisedData = decode(event)?;
                if let Some(owner) = data.item.delivery_id {
                    self.inbox.insert(data.item.id, owner);
                }
                fold_only(data.item.delivery_id)
            }
            T::InboxResolved => {
                let data: InboxResolvedData = decode(event)?;
                deliver(self.inbox.get(&data.inbox_id).copied())
            }
            T::InboxDismissed => {
                let data: InboxDismissedData = decode(event)?;
                deliver(self.inbox.get(&data.inbox_id).copied())
            }
            T::OutboxEnqueued => {
                let data: OutboxEnqueuedData = decode(event)?;
                if let Some(id) = subject::<ActionId>(event) {
                    self.actions.insert(id, data.action.delivery_id);
                }
                fold_only(Some(data.action.delivery_id))
            }
            T::OutboxExecuting => fold_only(self.action_owner(event)),
            T::OutboxSucceeded | T::OutboxFailed | T::OutboxCancelled => {
                deliver(self.action_owner(event))
            }
            T::ApprovalRequested => {
                let owner = correlated(event);
                if let (Some(owner), Some(id)) = (owner, subject::<ApprovalId>(event)) {
                    self.approvals.insert(id, owner);
                }
                deliver(owner)
            }
            T::ApprovalGranted
            | T::ApprovalConsumed
            | T::ApprovalInvalidated
            | T::ApprovalExpired
            | T::ApprovalDenied => deliver(
                subject::<ApprovalId>(event).and_then(|id| self.approvals.get(&id).copied()),
            ),
            T::PatchValidated => {
                let data: PatchValidatedData = decode(event)?;
                deliver(self.attempts.get(&data.job_id).copied())
            }
            T::PatchRejected => {
                let data: PatchRejectedData = decode(event)?;
                deliver(self.attempts.get(&data.job_id).copied())
            }
            T::WorktreeCreated => {
                let data: WorktreeCreatedData = decode(event)?;
                self.worktrees.insert(data.worktree_id, data.delivery_id);
                deliver(Some(data.delivery_id))
            }
            T::WorktreeLocked => {
                let data: WorktreeLockedData = decode(event)?;
                deliver(Some(data.delivery_id))
            }
            T::WorktreeUnlocked => {
                let data: WorktreeUnlockedData = decode(event)?;
                deliver(self.worktrees.get(&data.worktree_id).copied())
            }
            T::WorktreeRemoved => {
                let data: WorktreeRemovedData = decode(event)?;
                deliver(self.worktrees.get(&data.worktree_id).copied())
            }
            // 绑定由 forge 写进信封（event-data.md forge 信封）；未绑定为 null，不投。
            T::Forge(_) | T::ForgeSynced | T::ForgeDriftDetected => deliver(correlated(event)),
            T::SettingsUpdated => Route::Broadcast,
            _ => Route::None,
        })
    }

    fn action_owner(&self, event: &Event<Value>) -> Option<DeliveryId> {
        subject::<ActionId>(event).and_then(|id| self.actions.get(&id).copied())
    }

    /// 节点所属交付单元（`retry_node` 只拿到 `node_id`）。
    pub fn node_owner(&self, node_id: &NodeId) -> Option<DeliveryId> {
        self.nodes.get(node_id).copied()
    }
}
