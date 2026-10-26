//! `DeliveryControl`、`HealPort`、`ClassifyPort` 及其请求 / 响应类型。
//!
//! `DeliveryControl` 的签名以 `docs/modules/api.md` §6.3 与 `engine.md` §6
//! 为准；守护端口以 `engine.md` §6 与 `guardian.md` §6 为准。所有会写事件或
//! 创建图的端口方法都接收 [`ActorCtx`]。
//!
//! 契约缺口：`AnswerRequest`、`HealTicket`、`CiClassifyRequest`、
//! `FindingClassifyRequest`、`FindingTriage`、`ValidatedProposal` 的完整字段
//! 在文档中未定义；本模块给出最小形状并显式标注。

use async_trait::async_trait;

use crate::canon::{GitOid, Sha256Digest};
use crate::entities::{BudgetDimension, Delivery, DeliveryOrigin, FailureClass, Node, Seq};
use crate::ids::{AttemptId, ConversationId, DeliveryId, MessageId, NodeId, ProjectId, WorkflowId};
use crate::level::Level;
use crate::text::UntrustedText;
use crate::worker::{CiClassification, Confidence, FindingSeverity, PatchVerdict, Proposal};

use super::context::{ActorCtx, Health, ModuleError, StoreError};
use super::forge::ThreadNodeId;
use super::models::Material;

// ---------------------------------------------------------------------------
// 通用类型
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleStatus {
    pub applied_rev: Seq,
    pub health: Health,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemplateCatalog {
    pub templates: Vec<String>,
}

/// 创建交付单元的命令。当前任务只冻结端口类型，`conversation` 的 `EngineApi`
/// 仍在本 crate 内消费它。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDelivery {
    pub project_id: ProjectId,
    pub title: String,
    pub level_cap: Level,
    pub refs: Vec<crate::entities::Ref>,
    pub origin: DeliveryOrigin,
    pub source_message_id: MessageId,
    pub user_text: String,
    pub ctx: ActorCtx,
}

/// None 档问答的请求。规格书未列出字段；这里保留调用方必须提供的项目、
/// 会话、可信用户文本与引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerRequest {
    pub project_id: ProjectId,
    pub conversation_id: ConversationId,
    pub text: String,
    pub refs: Vec<crate::entities::Ref>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerTicket {
    pub workflow_id: WorkflowId,
    pub conversation_id: ConversationId,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    #[error("engine is not configured for the requested level")]
    NotConfigured,
    #[error("invalid engine state: {message}")]
    InvalidState { message: String },
    #[error("validation failed at {pointer}: {message}")]
    Validation { pointer: String, message: String },
    #[error("delivery or node not found")]
    NotFound,
    #[error("budget exhausted: {dimension:?}")]
    BudgetExhausted { dimension: BudgetDimension },
    #[error("forbidden")]
    Forbidden,
    #[error("conflict: {message}")]
    Conflict { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

// ---------------------------------------------------------------------------
// DeliveryControl
// ---------------------------------------------------------------------------

/// PATCH / abandon / pause / resume / retry 的业务部分。
#[async_trait]
pub trait DeliveryControl: Send + Sync {
    /// 调档：只能收紧，不改变预算模板（P-57）。
    async fn set_level_cap(
        &self,
        ctx: ActorCtx,
        id: DeliveryId,
        cap: Level,
    ) -> Result<Delivery, ModuleError>;

    /// 任何非终态阶段可用（P-55）。
    async fn abandon(
        &self,
        ctx: ActorCtx,
        id: DeliveryId,
        reason: Option<String>,
    ) -> Result<Delivery, ModuleError>;

    async fn pause(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError>;

    async fn resume(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError>;

    /// 仅接受 `failed` 节点；所属交付单元已终结 → `InvalidState`。
    async fn retry_node(&self, ctx: ActorCtx, id: NodeId) -> Result<Node, ModuleError>;
}

// ---------------------------------------------------------------------------
// HealPort
// ---------------------------------------------------------------------------

/// 愈合轮幂等键。规格书只给类型名，没有定义前缀 ID；使用不透明 newtype。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoundId(String);

impl RoundId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RoundId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrRef {
    pub repo: crate::canon::RepoSlug,
    pub number: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealTask {
    CiFailure,
    BotFinding,
    MechanicalConflict,
    NitBatch,
}

/// 问题指纹不含行号 / 时间戳 / 哈希值等易变部分（guardian.md §7.6）。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemSpec {
    pub fingerprint: Fingerprint,
    pub kind: String,
    pub material_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob(String);

impl Glob {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Glob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadTarget {
    pub thread_handle: String,
    pub thread_node_id: ThreadNodeId,
}

/// 已校验的 proposal。规格书没有额外字段；这里复用 worker 的 `Proposal`，
/// 校验结论由 `ResultRef` 与 `ProposalDisposition` 另行表达。
pub type ValidatedProposal = Proposal;

pub type ResultRef = Sha256Digest;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealRequest {
    pub round_id: RoundId,
    pub delivery_id: DeliveryId,
    pub pr: PrRef,
    pub head_sha: GitOid,
    pub task: HealTask,
    pub problems: Vec<ProblemSpec>,
    pub scope_allow: Vec<Glob>,
    pub targets: Vec<ThreadTarget>,
    pub materials: Vec<Material>,
    pub config_rev: u64,
}

/// `request_heal` 的句柄。字段来自规格书的“派一个 heal 节点”语义；未在文档
/// 冻结的字段保持 `Option`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealTicket {
    pub round_id: RoundId,
    pub delivery_id: DeliveryId,
    pub workflow_id: Option<WorkflowId>,
    pub node_id: Option<NodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelWhy {
    Frozen,
    HeadMoved,
    HeadMovedExternally,
    Superseded,
    DeliveryClosed,
    Shutdown,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealOutcome {
    Patched {
        commit: GitOid,
        tree: GitOid,
        patch_sha256: Sha256Digest,
        verdict: PatchVerdict,
        files: Vec<crate::worker::RepoPath>,
        proposals: Vec<ValidatedProposal>,
        result_ref: ResultRef,
    },
    ProposalsOnly {
        proposals: Vec<ValidatedProposal>,
        result_ref: ResultRef,
    },
    NoChange {
        reason: UntrustedText,
        result_ref: ResultRef,
    },
    Denied {
        detail: String,
        result_ref: ResultRef,
    },
    Failed {
        failure_class: FailureClass,
        detail: String,
        attempt_id: AttemptId,
    },
    Cancelled,
    Superseded {
        why: String,
    },
}

#[async_trait]
pub trait HealPort: Send + Sync {
    async fn request_heal(
        &self,
        req: HealRequest,
        ctx: &ActorCtx,
    ) -> Result<HealTicket, EngineError>;

    async fn outcome(&self, ticket: &HealTicket) -> Result<Option<HealOutcome>, EngineError>;

    async fn cancel(
        &self,
        ticket: &HealTicket,
        why: CancelWhy,
        ctx: &ActorCtx,
    ) -> Result<(), EngineError>;
}

// ---------------------------------------------------------------------------
// ClassifyPort
// ---------------------------------------------------------------------------

/// `classify_ci` 的输入。规格书没有字段表；这里携带项目的归属与不可信材料。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiClassifyRequest {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub attempt_id: AttemptId,
    pub check_run_id: Option<u64>,
    pub materials: Vec<Material>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindingClassifyRequest {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub attempt_id: AttemptId,
    pub thread_node_id: Option<ThreadNodeId>,
    pub materials: Vec<Material>,
}

/// `FindingTriage` 的枚举字段。规格书使用 `security`、`design_or_api`、
/// `actionable`、`confidence`、`severity`；这里逐项保留。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindingTriage {
    pub security: bool,
    pub design_or_api: bool,
    pub actionable: bool,
    pub confidence: Confidence,
    pub severity: Option<FindingSeverity>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClassifyError {
    #[error("classify request is invalid: {message}")]
    InvalidRequest { message: String },
    #[error("classify model is unavailable: {message}")]
    Unavailable { message: String },
    #[error("classify output does not match the schema: {message}")]
    OutputInvalid { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[async_trait]
pub trait ClassifyPort: Send + Sync {
    async fn classify_ci(
        &self,
        req: CiClassifyRequest,
        ctx: &ActorCtx,
    ) -> Result<CiClassification, ClassifyError>;

    async fn classify_finding(
        &self,
        req: FindingClassifyRequest,
        ctx: &ActorCtx,
    ) -> Result<FindingTriage, ClassifyError>;
}
