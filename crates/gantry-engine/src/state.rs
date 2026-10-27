//! `DeliveryState`：从事件日志折叠出的决策读视图（engine.md §7.2、§7.14.4）。
//!
//! `delivery.*` 的折叠规则不在这里写：交付单元行由 `gantry_store::DeliveryRow`
//! 折叠，store 的前置校验与本状态共用同一份（M1 收尾评审 A1）。其余事件按
//! §7.14.2 路由表归属本交付单元后折叠进来（路由见 [`crate::route`]）。
//!
//! 未从事件折叠的字段（规格里有、事件里还没有）：
//! - `pr_snapshot` 不折叠，由加载方从 store 的 GitHub 快照投影读取（§7.14.4，#150）；
//! - `Evidence.judged_head`、`stage_verdicts`、`round_history` 在 M1 恒空（无判定节点、无轮次）；
//! - `lock_owner` 是执行体内存：本结构有该字段，但折叠不写它，由执行体维护。

use std::collections::{BTreeMap, BTreeSet};

use gantry_core::canon::{sha256, GitOid, IdempotencyKey, Sha256Digest, SubjectDigest};
use gantry_core::entities::{
    role_profile, ActorKind, ApprovalAction, ApprovalStatus, AttemptStatus, BudgetDimension,
    BudgetUsed, Exec, FailureClass, FailureDetail, ForgeActionKind, GithubPrRef, InboxKind,
    InboxPayload, LockHolder, NodeStatus, PullRequestRef, ReasonCode, Role, Seq, Severity, Stage,
    StageStatus, Tier, Verdict, WorkflowStatus,
};
use gantry_core::events::{
    AttemptFinishedData, AttemptStartedData, BudgetChargeSource, BudgetChargedData,
    BudgetChargedDimension, BudgetExhaustedData, Event, EventType, InboxRaisedData,
    InboxResolvedData, NodeDescriptor, NodeEventData, NodeRetryRequestedData, OutboxEnqueuedData,
    OutboxFailedData, OutboxFailureCode, OutboxSucceededData, PatchValidatedData,
    WorkflowCreatedData, WorkflowFinishedData, WorkflowRevisedData, WorktreeCreatedData,
    WorktreeLockedData, WorktreeRemovedData, WorktreeUnlockedData,
};
use gantry_core::ids::{
    ActionId, ApprovalId, AttemptId, DeliveryId, InboxId, NodeId, WorkflowId, WorktreeId,
};
use gantry_core::level::Level;
use gantry_core::ports::inbox::{DedupKey, OptionId};
use gantry_core::ports::repo::LockToken;
use gantry_core::time::Timestamp;
use gantry_core::worker::PatchVerdict;
use gantry_store::{DeliveryRow, FoldError};
use serde_json::Value;

/// Outbox 动作的幂等键（forge-actions §2.2，由 core 派生）。
pub type ActionKey = IdempotencyKey;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("delivery row: {0}")]
    Row(#[from] FoldError),
    #[error("event {ty} data does not match its contract: {source}")]
    Data {
        ty: &'static str,
        #[source]
        source: gantry_core::events::SchemaError,
    },
    #[error("event {ty} arrived before its parent state")]
    MissingParent { ty: &'static str },
    #[error("event {ty} is not a delivery.created")]
    NotCreated { ty: &'static str },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryState {
    /// `delivery.*` 的折叠结果；与 store 投影同源。
    pub delivery: DeliveryRow,
    /// `create_pr` 的 `outbox.succeeded.github_ref.pr` 折叠出的绑定。
    pub pr: Option<GithubPrRef>,
    /// PR 的 GitHub 快照（I4）。**只读，不由引擎折叠**：加载方从 store 的快照投影读取（#150、#71）。
    pub pr_snapshot: Option<PullRequestRef>,
    pub worktree: Option<WorktreeInfo>,
    pub ledger: LedgerState,
    pub workflows: BTreeMap<WorkflowId, WorkflowState>,
    pub evidence: Evidence,
    pub open_inbox: BTreeMap<InboxId, OpenInbox>,
    pub outbox: BTreeMap<ActionKey, OutboxState>,
    pub approvals: BTreeMap<ApprovalId, ApprovalLite>,
    pub locks: BTreeMap<WorktreeId, LockLite>,
    /// **内存**：锁是为哪个节点取的（`Report::LockAcquired` 时由执行体记下，§7.14.4）。
    /// 折叠不写它，重启后为空，由 `Tick(Recover)` 重新认领。
    pub lock_owner: BTreeMap<WorktreeId, NodeId>,
    pub round_history: Vec<RoundRecord>,
    pub last_seq: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub id: WorktreeId,
    pub branch: String,
    pub base_ref: String,
    pub base_sha: GitOid,
}

/// 台账的可重建部分：限额取自 `delivery.created`，已用取自 `budget.charged.used_after`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerState {
    pub used: BudgetUsed,
    /// 已写过的 `budget.exhausted`：(维度, seq)；"本阶段尚无同维度耗尽"以 `stage_entered` 为界。
    pub exhausted: Vec<(BudgetDimension, Seq)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowState {
    pub id: WorkflowId,
    pub stage: Option<Stage>,
    pub graph_rev: u32,
    pub status: WorkflowStatus,
    pub template: String,
    pub nodes: BTreeMap<NodeId, NodeState>,
    pub round: u32,
    /// 建图事件的 seq；"自 `stage_entered` 以来是否已建图"用它判断。
    pub created_seq: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeState {
    pub id: NodeId,
    pub key: String,
    pub role: Role,
    pub tier: Tier,
    pub exec: Exec,
    pub depends_on: Vec<NodeId>,
    pub required: bool,
    pub critical: bool,
    pub status: NodeStatus,
    pub verdict: Option<Verdict>,
    pub superseded_by: Option<NodeId>,
    pub attempts: Vec<AttemptState>,
    /// 最近一次 `node.retry_requested` 时已有的尝试数；`after_retry_request` 由它推出。
    pub attempts_at_retry: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptState {
    pub id: AttemptId,
    pub n: u32,
    pub exec: Exec,
    pub status: AttemptStatus,
    pub base_sha: GitOid,
    pub tokens_reserved: u64,
    pub wall_time_s: u64,
    pub config_rev: Seq,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub failure_class: Option<FailureClass>,
    pub failure_detail: Option<FailureDetail>,
    pub result_ref: Option<Sha256Digest>,
    pub result_tree: Option<GitOid>,
    /// 本地提交 OID（E1，#64）；`Evidence.candidate_head` 由它重建。
    pub result_commit: Option<GitOid>,
    /// 被接受的 `open_pr` 提议的 `proposal_id`；原文在 `result_ref` 指向的 CAS 对象里（E5，#68）。
    pub accepted_open_pr: Option<String>,
    /// 自该节点最近一次 `node.retry_requested` 起的序号（1 起）。
    pub after_retry_request: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evidence {
    pub judged_head: Option<GitOid>,
    pub candidate_head: Option<GitOid>,
    pub candidate_tree: Option<GitOid>,
    pub patch_sha256: Option<Sha256Digest>,
    pub stage_verdicts: BTreeMap<WorkflowId, Verdict>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaisedBy {
    Engine,
    Guardian,
    Other,
}

/// 上报项对阶段状态的影响（engine.md §7.11 表的 `stage_status` 列）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocking {
    WaitingHuman,
    Blocked,
    None,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OpenInbox {
    pub id: InboxId,
    pub kind: InboxKind,
    pub reason_code: Option<ReasonCode>,
    pub dedup_key: DedupKey,
    pub raised_by: RaisedBy,
    pub effect: Blocking,
    pub node_id: Option<NodeId>,
    pub attempt_id: Option<AttemptId>,
    pub approval_id: Option<ApprovalId>,
    pub target_level: Option<Level>,
    pub options: Vec<OptionId>,
    pub raised_seq: Seq,
}

// `OptionId` 没有 `Debug`；按字符串打印。
impl std::fmt::Debug for OpenInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenInbox")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("reason_code", &self.reason_code)
            .field("dedup_key", &self.dedup_key)
            .field("raised_by", &self.raised_by)
            .field("effect", &self.effect)
            .field("node_id", &self.node_id)
            .field("attempt_id", &self.attempt_id)
            .field("approval_id", &self.approval_id)
            .field("target_level", &self.target_level)
            .field(
                "options",
                &self
                    .options
                    .iter()
                    .map(OptionId::as_str)
                    .collect::<Vec<_>>(),
            )
            .field("raised_seq", &self.raised_seq)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundRecord {
    pub workflow_id: WorkflowId,
    pub stage: Stage,
    pub round: u32,
    pub verdict: Verdict,
    pub judged_head: Option<GitOid>,
    pub fingerprints: BTreeSet<FindingFingerprint>,
}

/// sha256(category ‖ 0x00 ‖ location.path)；只存摘要，不存 finding 原文（I5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindingFingerprint(pub Sha256Digest);

impl FindingFingerprint {
    pub fn of(category: &str, path: &str) -> Self {
        let mut bytes = Vec::with_capacity(category.len() + 1 + path.len());
        bytes.extend_from_slice(category.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(path.as_bytes());
        Self(sha256(&bytes))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxState {
    pub action_id: ActionId,
    pub kind: ForgeActionKind,
    pub epoch: u32,
    pub status: gantry_core::entities::OutboxStatus,
    pub failure_code: Option<OutboxFailureCode>,
    /// `budget.charged(forge_writes)` 是否已为它记账（`source.action_id`）。
    pub charged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalLite {
    pub action: ApprovalAction,
    pub subject_digest: SubjectDigest,
    pub status: ApprovalStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockLite {
    pub holder: LockHolder,
    pub lease_until: Timestamp,
    /// `worktree.locked` 事件自身的 seq（repo.md §7.6）。
    pub fence: Seq,
}

fn decode<T: gantry_core::events::EventData>(
    event: &Event<Value>,
    ty: &'static str,
) -> Result<T, StateError> {
    gantry_core::events::decode_event_data(event).map_err(|source| StateError::Data { ty, source })
}

fn subject<T: std::str::FromStr>(event: &Event<Value>, ty: &'static str) -> Result<T, StateError> {
    event
        .subject
        .as_str()
        .parse()
        .map_err(|_| StateError::MissingParent { ty })
}

impl DeliveryState {
    /// 由 `delivery.created` 建立初始状态。
    pub fn created(event: &Event<Value>) -> Result<Self, StateError> {
        if event.ty != EventType::DeliveryCreated {
            return Err(StateError::NotCreated {
                ty: event.ty.as_str(),
            });
        }
        Ok(Self {
            delivery: DeliveryRow::created(event)?,
            pr: None,
            pr_snapshot: None,
            worktree: None,
            ledger: LedgerState::default(),
            workflows: BTreeMap::new(),
            evidence: Evidence::default(),
            open_inbox: BTreeMap::new(),
            outbox: BTreeMap::new(),
            approvals: BTreeMap::new(),
            locks: BTreeMap::new(),
            lock_owner: BTreeMap::new(),
            round_history: Vec::new(),
            last_seq: event.seq,
        })
    }

    /// 从一个已按路由表筛好的事件序列重建；序列为空或首条不是 `delivery.created` 时返回 `None`。
    pub fn fold<'a>(
        events: impl IntoIterator<Item = &'a Event<Value>>,
    ) -> Result<Option<Self>, StateError> {
        let mut state: Option<Self> = None;
        for event in events {
            match &mut state {
                None if event.ty == EventType::DeliveryCreated => {
                    state = Some(Self::created(event)?)
                }
                None => {
                    return Err(StateError::MissingParent {
                        ty: event.ty.as_str(),
                    })
                }
                Some(state) => state.apply(event)?,
            }
        }
        Ok(state)
    }

    pub fn id(&self) -> DeliveryId {
        self.delivery.id
    }

    pub fn node(&self, id: &NodeId) -> Option<&NodeState> {
        self.workflows.values().find_map(|wf| wf.nodes.get(id))
    }

    fn node_mut(&mut self, id: &NodeId) -> Option<&mut NodeState> {
        self.workflows
            .values_mut()
            .find_map(|wf| wf.nodes.get_mut(id))
    }

    fn attempt_mut(&mut self, id: &AttemptId) -> Option<&mut AttemptState> {
        self.workflows
            .values_mut()
            .flat_map(|wf| wf.nodes.values_mut())
            .find_map(|node| node.attempts.iter_mut().find(|a| a.id == *id))
    }

    fn outbox_mut(&mut self, action_id: &ActionId) -> Option<&mut OutboxState> {
        self.outbox
            .values_mut()
            .find(|row| row.action_id == *action_id)
    }

    /// 由已提交事件重建的锁令牌（repo.md §7.6）：持有者为 engine 且租约未过期时可直接续租或释放。
    pub fn lock_token(&self, worktree_id: &WorktreeId) -> Option<LockToken> {
        self.locks.get(worktree_id).map(|lock| LockToken {
            worktree_id: *worktree_id,
            holder: lock.holder,
            delivery_id: self.delivery.id,
            fence: lock.fence,
            lease_until: lock.lease_until,
        })
    }

    /// 应用一条已归属本交付单元的事件。路由表里"其余"的事件只推进 `last_seq`。
    pub fn apply(&mut self, event: &Event<Value>) -> Result<(), StateError> {
        let ty = event.ty.as_str();
        match event.ty {
            ty_ if DeliveryRow::folds(ty_) => self.delivery.apply(event)?,
            EventType::WorkflowCreated => {
                let data: WorkflowCreatedData = decode(event, ty)?;
                let id: WorkflowId = subject(event, ty)?;
                let mut workflow = WorkflowState {
                    id,
                    stage: data.stage,
                    graph_rev: data.graph_rev,
                    status: WorkflowStatus::Running,
                    template: data.template,
                    nodes: BTreeMap::new(),
                    round: 0,
                    created_seq: event.seq,
                };
                add_nodes(&mut workflow, &data.nodes, ty)?;
                self.workflows.insert(id, workflow);
            }
            EventType::WorkflowRevised => {
                let data: WorkflowRevisedData = decode(event, ty)?;
                let id: WorkflowId = subject(event, ty)?;
                let workflow = self
                    .workflows
                    .get_mut(&id)
                    .ok_or(StateError::MissingParent { ty })?;
                workflow.graph_rev = data.graph_rev;
                workflow.round = data.round;
                add_nodes(workflow, &data.added, ty)?;
            }
            EventType::WorkflowFinished => {
                let data: WorkflowFinishedData = decode(event, ty)?;
                let id: WorkflowId = subject(event, ty)?;
                self.workflows
                    .get_mut(&id)
                    .ok_or(StateError::MissingParent { ty })?
                    .status = data.status;
            }
            EventType::NodeQueued
            | EventType::NodeStarted
            | EventType::NodeFinished
            | EventType::NodeSuperseded => {
                let data: NodeEventData = decode(event, ty)?;
                let node = self
                    .node_mut(&data.node_id)
                    .ok_or(StateError::MissingParent { ty })?;
                node.status = data.status;
                node.verdict = data.verdict;
                node.superseded_by = data.superseded_by;
            }
            EventType::NodeRetryRequested => {
                let data: NodeRetryRequestedData = decode(event, ty)?;
                let node = self
                    .node_mut(&data.node_id)
                    .ok_or(StateError::MissingParent { ty })?;
                node.status = NodeStatus::Queued;
                node.attempts_at_retry = node.attempts.len() as u32;
            }
            EventType::AttemptStarted => {
                let data: AttemptStartedData = decode(event, ty)?;
                let node = self
                    .node_mut(&data.node_id)
                    .ok_or(StateError::MissingParent { ty })?;
                let after_retry_request = node.attempts.len() as u32 - node.attempts_at_retry + 1;
                node.attempts.push(AttemptState {
                    id: data.attempt_id,
                    n: data.n,
                    exec: node.exec,
                    status: AttemptStatus::Running,
                    base_sha: data.base_sha,
                    tokens_reserved: data.tokens_total,
                    wall_time_s: data.wall_time_s,
                    config_rev: data.config_rev,
                    started_at: event.ts,
                    finished_at: None,
                    failure_class: None,
                    failure_detail: None,
                    result_ref: None,
                    result_tree: None,
                    result_commit: None,
                    accepted_open_pr: None,
                    after_retry_request,
                });
            }
            EventType::AttemptFinished => {
                let data: AttemptFinishedData = decode(event, ty)?;
                let id: AttemptId = subject(event, ty)?;
                let failure_detail = data
                    .failure_detail
                    .as_deref()
                    .map(FailureDetail::parse)
                    .transpose()
                    .map_err(|_| StateError::Data {
                        ty,
                        source: gantry_core::events::SchemaError::shape(
                            "/data/failure_detail",
                            "invalid failure_detail",
                        ),
                    })?;
                let attempt = self
                    .attempt_mut(&id)
                    .ok_or(StateError::MissingParent { ty })?;
                attempt.status = data.status;
                attempt.finished_at = Some(event.ts);
                attempt.failure_class = data.failure_class;
                attempt.failure_detail = failure_detail;
                attempt.result_ref = data.result_ref;
                attempt.result_tree = data.result_tree;
                attempt.result_commit = data.result_commit;
                attempt.accepted_open_pr = data.accepted_open_pr;
                // repo 为已通过校验的补丁生成的最新本地提交（§7.2 Evidence）。
                if data.status == AttemptStatus::Succeeded {
                    if let (Some(tree), Some(commit)) = (data.result_tree, data.result_commit) {
                        self.evidence.candidate_tree = Some(tree);
                        self.evidence.candidate_head = Some(commit);
                    }
                }
            }
            EventType::PatchValidated => {
                let data: PatchValidatedData = decode(event, ty)?;
                if data.verdict == PatchVerdict::Allow {
                    self.evidence.patch_sha256 = Some(data.sha256);
                }
            }
            EventType::BudgetCharged => {
                let data: BudgetChargedData = decode(event, ty)?;
                let used = &mut self.ledger.used;
                match data.dimension {
                    BudgetChargedDimension::TokensByTier => {
                        let tier = data.tier.ok_or(StateError::MissingParent { ty })?;
                        used.tokens_by_tier.insert(tier, data.used_after);
                    }
                    BudgetChargedDimension::Nodes => used.nodes = data.used_after,
                    BudgetChargedDimension::Attempts => used.attempts = data.used_after,
                    BudgetChargedDimension::WallTime => used.wall_time = data.used_after,
                    BudgetChargedDimension::Rounds => used.rounds = data.used_after,
                    BudgetChargedDimension::ForgeWrites => {
                        used.forge_writes = data.used_after;
                        if let BudgetChargeSource::OutboxAction { action_id } = data.source {
                            if let Some(row) = self.outbox_mut(&action_id) {
                                row.charged = true;
                            }
                        }
                    }
                }
            }
            EventType::BudgetExhausted => {
                let data: BudgetExhaustedData = decode(event, ty)?;
                self.ledger.exhausted.push((data.dimension, event.seq));
            }
            EventType::InboxRaised => {
                let data: InboxRaisedData = decode(event, ty)?;
                let item = open_inbox(&data, event);
                for superseded in &data.supersedes {
                    self.open_inbox.remove(superseded);
                }
                self.open_inbox.insert(item.id, item);
            }
            EventType::InboxResolved => {
                let data: InboxResolvedData = decode(event, ty)?;
                self.open_inbox.remove(&data.inbox_id);
            }
            // dismiss 只表示"不再提醒"，不解除阻塞（§7.11，OQ-13）。
            EventType::InboxDismissed => {}
            EventType::OutboxEnqueued => {
                let data: OutboxEnqueuedData = decode(event, ty)?;
                let action_id: ActionId = subject(event, ty)?;
                self.outbox.insert(
                    data.action.idempotency_key.clone(),
                    OutboxState {
                        action_id,
                        kind: data.action.kind,
                        epoch: data.action.epoch,
                        status: gantry_core::entities::OutboxStatus::Queued,
                        failure_code: None,
                        charged: false,
                    },
                );
            }
            EventType::OutboxExecuting => {
                self.outbox_status(event, ty, gantry_core::entities::OutboxStatus::Executing)?;
            }
            EventType::OutboxSucceeded => {
                let data: OutboxSucceededData = decode(event, ty)?;
                let row =
                    self.outbox_status(event, ty, gantry_core::entities::OutboxStatus::Succeeded)?;
                if row.kind == ForgeActionKind::CreatePr {
                    if let Some(pr) = data.github_ref.pr {
                        self.pr = Some(pr);
                    }
                }
            }
            EventType::OutboxFailed => {
                let data: OutboxFailedData = decode(event, ty)?;
                let action_id: ActionId = subject(event, ty)?;
                let row = self
                    .outbox_mut(&action_id)
                    .ok_or(StateError::MissingParent { ty })?;
                row.status = gantry_core::entities::OutboxStatus::Failed;
                row.failure_code = Some(data.code);
            }
            EventType::OutboxCancelled => {
                self.outbox_status(event, ty, gantry_core::entities::OutboxStatus::Cancelled)?;
            }
            EventType::ApprovalRequested => {
                let data: gantry_core::events::ApprovalRequestedData = decode(event, ty)?;
                let id: ApprovalId = subject(event, ty)?;
                self.approvals.insert(
                    id,
                    ApprovalLite {
                        action: data.action,
                        subject_digest: data.subject_digest,
                        status: ApprovalStatus::Pending,
                    },
                );
            }
            EventType::ApprovalGranted
            | EventType::ApprovalConsumed
            | EventType::ApprovalInvalidated
            | EventType::ApprovalExpired
            | EventType::ApprovalDenied => {
                let id: ApprovalId = subject(event, ty)?;
                let status = match event.ty {
                    EventType::ApprovalGranted => ApprovalStatus::Granted,
                    EventType::ApprovalConsumed => ApprovalStatus::Consumed,
                    EventType::ApprovalInvalidated => ApprovalStatus::Invalidated,
                    EventType::ApprovalExpired => ApprovalStatus::Expired,
                    _ => ApprovalStatus::Denied,
                };
                self.approvals
                    .get_mut(&id)
                    .ok_or(StateError::MissingParent { ty })?
                    .status = status;
            }
            EventType::WorktreeCreated => {
                let data: WorktreeCreatedData = decode(event, ty)?;
                self.worktree = Some(WorktreeInfo {
                    id: data.worktree_id,
                    branch: data.branch,
                    base_ref: data.base_ref,
                    base_sha: data.base_sha,
                });
            }
            EventType::WorktreeLocked => {
                let data: WorktreeLockedData = decode(event, ty)?;
                self.locks.insert(
                    data.worktree_id,
                    LockLite {
                        holder: data.holder,
                        lease_until: data.lease_until,
                        fence: event.seq,
                    },
                );
            }
            EventType::WorktreeUnlocked => {
                let data: WorktreeUnlockedData = decode(event, ty)?;
                self.locks.remove(&data.worktree_id);
            }
            EventType::WorktreeRemoved => {
                let data: WorktreeRemovedData = decode(event, ty)?;
                self.locks.remove(&data.worktree_id);
                if self.worktree.as_ref().map(|w| w.id) == Some(data.worktree_id) {
                    self.worktree = None;
                }
            }
            _ => {}
        }
        self.last_seq = event.seq;
        Ok(())
    }

    fn outbox_status(
        &mut self,
        event: &Event<Value>,
        ty: &'static str,
        status: gantry_core::entities::OutboxStatus,
    ) -> Result<&OutboxState, StateError> {
        let action_id: ActionId = subject(event, ty)?;
        let row = self
            .outbox_mut(&action_id)
            .ok_or(StateError::MissingParent { ty })?;
        row.status = status;
        Ok(row)
    }
}

fn add_nodes(
    workflow: &mut WorkflowState,
    descriptors: &[NodeDescriptor],
    ty: &'static str,
) -> Result<(), StateError> {
    // 先登记本批节点，`depends_on` 可以引用同批中排在后面的键。
    let mut by_key: BTreeMap<String, NodeId> = workflow
        .nodes
        .values()
        .map(|node| (node.key.clone(), node.id))
        .collect();
    for descriptor in descriptors {
        by_key.insert(descriptor.key.clone(), descriptor.node_id);
    }
    for descriptor in descriptors {
        let depends_on = descriptor
            .depends_on
            .iter()
            .map(|key| {
                by_key
                    .get(key)
                    .copied()
                    .ok_or(StateError::MissingParent { ty })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (tier, exec) = role_profile(&descriptor.role);
        workflow.nodes.insert(
            descriptor.node_id,
            NodeState {
                id: descriptor.node_id,
                key: descriptor.key.clone(),
                role: descriptor.role.clone(),
                tier,
                exec,
                depends_on,
                required: descriptor.required,
                critical: descriptor.critical,
                status: NodeStatus::Pending,
                verdict: None,
                superseded_by: None,
                attempts: Vec::new(),
                attempts_at_retry: 0,
            },
        );
    }
    Ok(())
}

fn open_inbox(data: &InboxRaisedData, event: &Event<Value>) -> OpenInbox {
    let item = &data.item;
    let raised_by = match event.actor.kind {
        ActorKind::Engine => RaisedBy::Engine,
        ActorKind::Guardian => RaisedBy::Guardian,
        _ => RaisedBy::Other,
    };
    let mut open = OpenInbox {
        id: item.id,
        kind: item.kind.clone(),
        reason_code: None,
        dedup_key: DedupKey::new(item.dedup_key.clone()),
        raised_by,
        effect: Blocking::None,
        node_id: None,
        attempt_id: None,
        approval_id: None,
        target_level: None,
        options: Vec::new(),
        raised_seq: event.seq,
    };
    let options = |options: &[gantry_core::entities::InboxOption]| {
        options
            .iter()
            .map(|o| OptionId::new(o.id.clone()))
            .collect()
    };
    match &item.payload {
        InboxPayload::ApprovalNeeded { approval_id } => open.approval_id = Some(*approval_id),
        InboxPayload::DecisionNeeded {
            reason_code,
            options: opts,
            node_id,
            ..
        } => {
            open.reason_code = Some(reason_code.clone());
            open.options = options(opts);
            open.node_id = *node_id;
        }
        InboxPayload::SpecNeeded { .. } => {}
        InboxPayload::BudgetExhausted { options: opts, .. } => open.options = options(opts),
        InboxPayload::Escalation {
            reason_code,
            node_id,
            attempt_id,
            target_level,
            options: opts,
            ..
        } => {
            open.reason_code = Some(reason_code.clone());
            open.node_id = *node_id;
            open.attempt_id = *attempt_id;
            open.target_level = *target_level;
            open.options = opts.as_deref().map(options).unwrap_or_default();
        }
        InboxPayload::SecurityFinding { attempt_id, .. } => open.attempt_id = *attempt_id,
    }
    // §7.11 表：blocker 进入 waiting_human，判定 BLOCKED（gate_blocked）进入 blocked；其余不阻塞。
    open.effect = match item.severity {
        Severity::Blocker
            if open.reason_code.as_ref().map(ReasonCode::as_str) == Some("gate_blocked") =>
        {
            Blocking::Blocked
        }
        Severity::Blocker => Blocking::WaitingHuman,
        _ => Blocking::None,
    };
    open
}

// ---------------------------------------------------------------------------
// 由状态推出的前置条件（engine.md §7.14.5）。只读状态，不看时钟以外的环境。
// ---------------------------------------------------------------------------

impl DeliveryState {
    /// 派发总闸：阶段非终态 ∧ `stage_status = running` ∧ 无未关闭的 `budget_exhausted` 上报。
    pub fn may_dispatch(&self) -> bool {
        !self.delivery.stage.is_terminal()
            && self.stage_status() == StageStatus::Running
            && !self
                .open_inbox
                .values()
                .any(|item| item.kind == InboxKind::BudgetExhausted)
    }

    /// 有效的 `stage_status`（§7.6）：暂停优先；否则引擎创建的未决阻塞项决定 `blocked` /
    /// `waiting_human`；都没有时取 `delivery.*` 折叠出的值。`dismiss` 不解除阻塞（§7.11）。
    pub fn stage_status(&self) -> StageStatus {
        let row = self.delivery.stage_status;
        if matches!(row, StageStatus::Paused | StageStatus::Done) {
            return row;
        }
        let engine_items = self
            .open_inbox
            .values()
            .filter(|item| item.raised_by == RaisedBy::Engine);
        let mut waiting = false;
        for item in engine_items {
            match item.effect {
                Blocking::Blocked => return StageStatus::Blocked,
                Blocking::WaitingHuman => waiting = true,
                Blocking::None => {}
            }
        }
        if waiting {
            StageStatus::WaitingHuman
        } else {
            row
        }
    }

    /// 有 `attempt.started` 而无 `attempt.finished` 的尝试（`RunAttempt { resume: true }` 的对象）。
    pub fn unfinished_attempts(&self) -> impl Iterator<Item = (&NodeState, &AttemptState)> {
        self.workflows
            .values()
            .flat_map(|wf| wf.nodes.values())
            .flat_map(|node| node.attempts.iter().map(move |a| (node, a)))
            .filter(|(_, attempt)| attempt.finished_at.is_none())
    }

    /// 自 `stage_entered` 以来是否已建过该阶段的 Workflow（`workflow.created` 行）。
    pub fn workflow_built_in_stage(&self) -> bool {
        let entered = self.delivery.stage_entered.seq;
        self.workflows
            .values()
            .any(|wf| wf.stage == Some(self.delivery.stage) && wf.created_seq > entered)
    }
}

/// 下一次自动重试的最早时间 = `finished_at` + 退避(n)；全抖动以 `sha256(attempt_id)` 为种子，
/// 确定性推出，不存（§7.14.4）。尝试未终结返回 `None`。
pub fn retry_not_before(
    attempt: &AttemptState,
    consts: &crate::decide::EngineConsts,
) -> Option<Timestamp> {
    let finished = attempt.finished_at?;
    let shift = attempt.n.saturating_sub(1).min(32);
    let backoff = consts
        .retry_backoff_base_ms
        .saturating_mul(1_i64 << shift)
        .min(consts.retry_backoff_cap_ms)
        .max(0);
    let digest = sha256(attempt.id.to_string().as_bytes());
    let mut seed = [0u8; 8];
    seed.copy_from_slice(&digest.as_bytes()[..8]);
    let jitter = (u64::from_be_bytes(seed) % (backoff as u64 + 1)) as i64;
    Some(Timestamp::from_millis(
        finished.as_millis().saturating_add(jitter),
    ))
}
