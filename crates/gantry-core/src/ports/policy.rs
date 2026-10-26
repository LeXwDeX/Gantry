//! `PolicyPort`：动作判定、审批、补丁规则与执行前消费。
//!
//! 签名以 `docs/modules/policy.md` §6 为准。`PolicyPort` 是跨泳道端口；同一
//! crate 内的 `ApprovalService` / `AuditLog` 不在本模块重复定义。
//!
//! 所有可能追加事件的写方法接收 [`ActorCtx`]。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::canon::{RepoSlug, Sha256Digest, SubjectDigest};
use crate::entities::{
    Actor, Approval, ApprovalAction, ApprovalTarget, InboxKind, PolicyActionId, Seq, Severity,
};
use crate::forge::ForgeActionKind;
use crate::ids::{ActionId, ApprovalId, AttemptId, DeliveryId, ProjectId};
use crate::worker::{PatchReason, PatchVerdict, Proposal, RepoPath};

use super::context::{ActorCtx, StoreError, TxHandle};
use super::forge::ThreadNodeId;
use super::repo::PathRulesSnapshot;

/// 路径规则命中的规则 id。规则 id 的封闭词表由 config 决定。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuleId(String);

impl RuleId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `policies.actions.*` 的处置，严格度 `Auto < Approve < Deny`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Disposition {
    Auto,
    Approve,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRequest {
    pub project_id: ProjectId,
    pub delivery_id: Option<DeliveryId>,
    pub kind: ForgeActionKind,
    pub facts: ActionFacts,
    pub requested_by: Actor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionFacts {
    MergePr {
        repo: RepoSlug,
        number: u64,
        expected_head_sha: crate::canon::GitOid,
    },
    PushBranch {
        repo: RepoSlug,
        branch: String,
        branch_prefix: String,
        force: bool,
        local_head_oid: crate::canon::GitOid,
        expected_remote_oid: Option<crate::canon::GitOid>,
        expected_tree_oid: Option<crate::canon::GitOid>,
        validation_ref: Option<Sha256Digest>,
    },
    ResolveReviewThread {
        repo: RepoSlug,
        thread_has_human_comment: bool,
    },
    Other {
        repo: RepoSlug,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionDecision {
    Allow {
        action_id: PolicyActionId,
        approval: Option<ApprovalId>,
        config_rev: Seq,
    },
    NeedsApproval {
        spec: ApprovalSpec,
        pending: Option<ApprovalId>,
        config_rev: Seq,
    },
    Deny {
        action_id: PolicyActionId,
        reason: DenyReason,
        report: Option<ReportHint>,
        config_rev: Seq,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalSpec {
    pub action: ApprovalAction,
    pub target: ApprovalTarget,
    pub subject_digest: SubjectDigest,
    pub delivery_id: Option<DeliveryId>,
    pub ttl: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportHint {
    pub inbox_kind: InboxKind,
    pub payload_kind: Option<String>,
    pub reason_code: crate::entities::ReasonCode,
    pub severity: Severity,
    pub attach_patch: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DenyReason {
    #[error("action denied")]
    ActionDenied,
    #[error("approval denied")]
    ApprovalDenied,
    #[error("approval already consumed")]
    ApprovalConsumed,
    #[error("head mismatch")]
    HeadMismatch,
    #[error("no validated patch")]
    NoValidatedPatch,
    #[error("patch denied")]
    PatchDenied,
    #[error("multiple approvals required")]
    MultipleApprovalsRequired,
    #[error("approval shape is undefined")]
    ApprovalShapeUndefined,
    #[error("approval chain unavailable")]
    ApprovalChainUnavailable,
}

/// 审批发起方主动作废的语义。规格书只给类型名；这里覆盖现有调用点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidateReason {
    SubjectReplaced,
    AttemptSuperseded,
    DeliveryAbandoned,
    HeadMoved,
    OwnerCancelled,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRuleHit {
    pub path: RepoPath,
    pub rule_id: Option<RuleId>,
    pub disposition: Disposition,
    pub outside_allow: bool,
}

/// 路径规则的唯一实现。guardian 与 engine 复用同一个对象。
pub trait PathRuleSet: Send + Sync {
    fn evaluate(&self, paths: &[RepoPath]) -> Vec<PathRuleHit>;
}

/// 新增行的惰性来源；`repo` 在干净检出上按需读取。
pub trait AddedLineSource: Send + Sync {
    fn added_lines<'a>(
        &'a self,
        path: &'a RepoPath,
    ) -> Box<dyn Iterator<Item = std::io::Result<String>> + 'a>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchPolicyVerdict {
    pub verdict: PatchVerdict,
    pub reasons: Vec<PatchReason>,
    pub matched_approval_rules: Vec<RuleId>,
    pub report: Option<ReportHint>,
    pub config_rev: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchApprovalCheck {
    pub repo: RepoSlug,
    pub attempt_id: AttemptId,
    pub patch_sha256: Sha256Digest,
    pub rules: Vec<RuleId>,
    pub paths: Vec<RepoPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchPermit {
    Consumed,
    AlreadyConsumedBySameAttempt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposalContext {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub attempt_id: AttemptId,
    pub patch: Option<PatchVerdict>,
    pub thread_has_human_comment: HashMap<ThreadNodeId, bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalDecision {
    Accepted {
        proposal_id: String,
        action_id: Option<PolicyActionId>,
    },
    NeedsApproval {
        proposal_id: String,
        spec: ApprovalSpec,
    },
    Held {
        proposal_id: String,
    },
    Denied {
        proposal_id: String,
        reason: DenyReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionCheck {
    MergePr {
        repo: RepoSlug,
        number: u64,
        expected_head_sha: crate::canon::GitOid,
        live_head_sha: crate::canon::GitOid,
    },
    PushBranch {
        repo: RepoSlug,
        branch: String,
        force: bool,
        local_head_oid: crate::canon::GitOid,
        expected_remote_oid: Option<crate::canon::GitOid>,
        validation_ref: Option<Sha256Digest>,
    },
    Other {
        kind: ForgeActionKind,
        target: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionPermit {
    Consumed,
    AlreadyConsumedBySameAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MismatchReason {
    #[error("approval is not granted")]
    NotGranted,
    #[error("approval already consumed")]
    Consumed,
    #[error("approval expired")]
    Expired,
    #[error("action mismatch")]
    ActionMismatch,
    #[error("target mismatch")]
    TargetMismatch,
    #[error("digest mismatch")]
    DigestMismatch,
    #[error("base revision mismatch")]
    BaseRevMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    #[error("approval invalid: {reason:?}")]
    ApprovalInvalid { reason: MismatchReason },
    #[error("head changed to {live_head_sha}")]
    HeadChanged { live_head_sha: crate::canon::GitOid },
    #[error("approval already consumed by {by}")]
    ConsumedByOther { by: ActionId },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("record not found")]
    NotFound,
    #[error("invalid request at {pointer}: {message}")]
    InvalidRequest { pointer: String, message: String },
    #[error("policy is not configured")]
    NotConfigured,
    #[error("action {action:?} is not an internal approval action")]
    NotInternalAction { action: ApprovalAction },
    #[error("approval unavailable: {message}")]
    ApprovalUnavailable { message: String },
    #[error("policy conflict: {message}")]
    Conflict { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `engine` / `guardian` / `repo` / `forge` 共用的策略端口。
#[async_trait]
pub trait PolicyPort: Send + Sync {
    /// 判定待入队的 Forge 动作；可能追加去重后的 `policy.denied`。
    async fn decide_action(
        &self,
        req: ActionRequest,
        ctx: &ActorCtx,
    ) -> Result<ActionDecision, PolicyError>;

    /// 创建或返回同主体的 pending 审批。
    async fn request_approval(
        &self,
        spec: ApprovalSpec,
        ctx: ActorCtx,
    ) -> Result<Approval, PolicyError>;

    /// 审批发起方在主体被替换时主动作废。
    async fn invalidate(
        &self,
        id: &ApprovalId,
        reason: InvalidateReason,
        ctx: &ActorCtx,
    ) -> Result<Approval, PolicyError>;

    /// 某 PR 上 pending / granted 的审批。
    async fn live_approvals_for_pr(
        &self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Vec<Approval>, PolicyError>;

    /// 逐条判定 worker proposals。
    async fn decide_proposals(
        &self,
        proposal_ctx: ProposalContext,
        items: &[Proposal],
        ctx: &ActorCtx,
    ) -> Result<Vec<ProposalDecision>, PolicyError>;

    /// 对能干净应用的补丁做规则判定。
    async fn evaluate_patch(
        &self,
        facts: &crate::worker::PatchFacts,
        added: &dyn AddedLineSource,
        ctx: &ActorCtx,
    ) -> Result<PatchPolicyVerdict, PolicyError>;

    /// held 补丁提交前核对并消费审批，在 repo 的写事务里追加事件。
    async fn consume_patch_approval(
        &self,
        tx: &mut dyn TxHandle,
        approval: &ApprovalId,
        check: &PatchApprovalCheck,
        ctx: &ActorCtx,
    ) -> Result<PatchPermit, GateError>;

    fn path_rules(&self, project: &ProjectId) -> Arc<dyn PathRuleSet>;

    fn path_rules_snapshot(&self, project: &ProjectId) -> PathRulesSnapshot;

    fn scan_text(&self, text: &str) -> Vec<SecretFinding>;

    /// `forge` 执行开始前验证并消费审批。
    async fn consume_for_execution(
        &self,
        approval: &ApprovalId,
        check: &ExecutionCheck,
        act: &ActionId,
        ctx: &ActorCtx,
    ) -> Result<ExecutionPermit, GateError>;
}

/// 密钥扫描命中只返回位置与模式名，不返回明文。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretFinding {
    pub path: Option<String>,
    pub line: Option<u32>,
    pub pattern: String,
}
