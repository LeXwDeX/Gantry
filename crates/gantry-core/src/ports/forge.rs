//! Forge 读端口、Outbox 写端口与 `PrRefresher`。
//!
//! 签名以 `docs/modules/forge.md` §6 为准。所有写方法额外接收 [`ActorCtx`]，
//! 这是 P-78 的统一要求；文档中显式列出的字段保持不变。
//!
//! 契约缺口：`CiLogQuery`、`CiLog`、`ChangedFile`、`ReviewThreadView`、
//! `Annotation` 与 `CancelReason` 的形状在规格书中只有名字或注释，本文件给出
//! 能表达既有语义的最小类型，并在各类型上标注缺口。

use std::time::Duration;

use async_trait::async_trait;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canon::{GitOid, IdempotencyKey, RepoSlug};
use crate::entities::{
    tagged_payload_schema, ForgeActionKind, IssueRef, OutboxAction, OutboxActionError,
    OutboxStatus, Priority, PullRequestRef,
};
use crate::forge::{ActionPayload, ForgeActor, PrHead};
use crate::ids::{ActionId, ApprovalId, DeliveryId, EventId, ProjectId};
use crate::text::UntrustedText;
use crate::time::Timestamp;
use crate::worker::{FileStatus, RepoPath};

use super::context::{ActorCtx, ModuleError, StoreError, TxHandle};

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

/// `engine` / `guardian` 看到的写入口。唯一的 GitHub 写入口是 `enqueue*`。
#[async_trait]
pub trait OutboxPort: Send + Sync {
    /// 在调用方的事务里入队，使“状态变化 + 事件 + 入队”原子。
    async fn enqueue_in(
        &self,
        tx: &mut dyn TxHandle,
        req: EnqueueRequest,
        ctx: &ActorCtx,
    ) -> Result<EnqueueOutcome, EnqueueError>;

    /// 自开事务入队的便利封装。
    async fn enqueue(
        &self,
        req: EnqueueRequest,
        ctx: &ActorCtx,
    ) -> Result<EnqueueOutcome, EnqueueError>;

    /// 取消非终态动作；`executing` 中的动作不可取消。
    async fn cancel(
        &self,
        id: &ActionId,
        reason: CancelReason,
        ctx: &ActorCtx,
    ) -> Result<CancelOutcome, ForgeError>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnqueueRequest {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub payload: ActionPayload,
    pub requires_approval: Option<ApprovalId>,
    pub depends_on: Vec<ActionId>,
    pub priority: Priority,
    pub not_before: Option<Timestamp>,
    pub epoch: u32,
    pub enqueued_by: EnqueuedBy,
}

/// 入队模块。规格书写 `{ module: "engine" | "guardian" }`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EnqueuedByModule {
    Engine,
    Guardian,
}

impl EnqueuedByModule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Guardian => "guardian",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnqueuedBy {
    pub module: EnqueuedByModule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
}

/// forge-actions §2.1 的入队信封。
///
/// 它描述“入队时”的完整字段，因此带 `idempotency_key` 与 `enqueued_by`，但没有
/// `OutboxAction` 的执行字段（`status`、`attempts`、`lease_until` 等）。
/// `EnqueueRequest` 仍是端口入参，不因本类型改变签名或增加字段。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawOutboxEnqueueEnvelope")]
pub struct OutboxEnqueueEnvelope {
    pub id: ActionId,
    pub idempotency_key: IdempotencyKey,
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub kind: ForgeActionKind,
    pub payload: ActionPayload,
    pub requires_approval: Option<ApprovalId>,
    pub depends_on: Vec<ActionId>,
    pub priority: Priority,
    pub not_before: Option<Timestamp>,
    pub epoch: u32,
    pub enqueued_by: EnqueuedBy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutboxEnqueueEnvelope {
    id: ActionId,
    idempotency_key: IdempotencyKey,
    project_id: ProjectId,
    delivery_id: DeliveryId,
    kind: ForgeActionKind,
    payload: Value,
    requires_approval: Option<ApprovalId>,
    depends_on: Vec<ActionId>,
    priority: Priority,
    not_before: Option<Timestamp>,
    epoch: u32,
    enqueued_by: EnqueuedBy,
}

impl TryFrom<RawOutboxEnqueueEnvelope> for OutboxEnqueueEnvelope {
    type Error = OutboxActionError;

    fn try_from(raw: RawOutboxEnqueueEnvelope) -> Result<Self, Self::Error> {
        let payload = ActionPayload::from_value(raw.kind, raw.payload).map_err(|source| {
            OutboxActionError::Payload {
                kind: raw.kind,
                source,
            }
        })?;
        Ok(Self {
            id: raw.id,
            idempotency_key: raw.idempotency_key,
            project_id: raw.project_id,
            delivery_id: raw.delivery_id,
            kind: raw.kind,
            payload,
            requires_approval: raw.requires_approval,
            depends_on: raw.depends_on,
            priority: raw.priority,
            not_before: raw.not_before,
            epoch: raw.epoch,
            enqueued_by: raw.enqueued_by,
        })
    }
}

/// 仅用于生成 `OutboxEnqueueEnvelope` schema 的公共字段；不参与 serde。
///
/// `kind`/`payload` 先用最宽类型占位，具体的 kind/payload 联动由外层 `oneOf`
/// 分支约束；`additionalProperties: false` 与 raw 解码的严格性一致。
#[derive(JsonSchema)]
#[allow(dead_code)]
#[schemars(extend("additionalProperties" = false))]
struct OutboxEnqueueEnvelopeBase {
    id: ActionId,
    idempotency_key: IdempotencyKey,
    project_id: ProjectId,
    delivery_id: DeliveryId,
    kind: ForgeActionKind,
    payload: Value,
    requires_approval: Option<ApprovalId>,
    depends_on: Vec<ActionId>,
    priority: Priority,
    not_before: Option<Timestamp>,
    epoch: u32,
    enqueued_by: EnqueuedBy,
}

impl JsonSchema for OutboxEnqueueEnvelope {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "OutboxEnqueueEnvelope".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let base = generator.subschema_for::<OutboxEnqueueEnvelopeBase>();
        tagged_payload_schema(
            generator,
            base,
            "forge-actions §2.1 的入队信封；`kind` 与 `payload` 必须联动。",
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnqueueOutcome {
    pub action: OutboxAction,
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnqueueError {
    #[error("invalid payload at {pointer}: {message}")]
    InvalidPayload { pointer: String, message: String },
    #[error("approval required: {reason}")]
    ApprovalRequired { reason: &'static str },
    #[error("permission missing for {kind:?}: {permission}")]
    PermissionMissing {
        kind: ForgeActionKind,
        permission: &'static str,
    },
    #[error("project unknown")]
    ProjectUnknown,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// 取消原因。`superseded` 来自 forge-actions §2.7 的合并；其余项覆盖引擎与
/// 守护的现有调用点。规格书没有封闭词表，因此保留 `Other` 以兼容新调用方。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Superseded,
    DeliveryAbandoned,
    ApprovalInvalidated,
    DeliveryClosed,
    Shutdown,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    AlreadyTerminal(OutboxStatus),
    NotCancellable,
}

// ---------------------------------------------------------------------------
// ForgeReader
// ---------------------------------------------------------------------------

/// 读 GitHub 的端口：所有方法都不写 GitHub；`reconcile_now` 与 `sync_mirror`
/// 只写本地快照 / 镜像，因此也接收 [`ActorCtx`] 以符合 P-78。
#[async_trait]
pub trait ForgeReader: Send + Sync {
    async fn pull_request(
        &self,
        project: &ProjectId,
        number: u64,
    ) -> Result<Option<PullRequestRef>, ForgeError>;

    async fn issue(&self, project: &ProjectId, number: u64)
        -> Result<Option<IssueRef>, ForgeError>;

    async fn project_snapshot(&self, project: &ProjectId) -> Result<ProjectSnapshot, ForgeError>;

    /// Issue / PR 正文与评论，按需拉取，不进快照、不进事件日志（P-59）。
    async fn discussion(
        &self,
        project: &ProjectId,
        target: DiscussionTarget,
        query: DiscussionQuery,
    ) -> Result<Discussion, ForgeError>;

    async fn ci_log(&self, project: &ProjectId, query: CiLogQuery) -> Result<CiLog, ForgeError>;

    async fn changed_files(
        &self,
        project: &ProjectId,
        number: u64,
        head_sha: &GitOid,
    ) -> Result<Vec<ChangedFile>, ForgeError>;

    async fn review_thread(
        &self,
        project: &ProjectId,
        thread: &ThreadNodeId,
    ) -> Result<ReviewThreadView, ForgeError>;

    async fn check_annotations(
        &self,
        project: &ProjectId,
        check_run_id: u64,
    ) -> Result<Vec<Annotation>, ForgeError>;

    async fn reconcile_now(
        &self,
        target: ReconcileTarget,
        ctx: &ActorCtx,
    ) -> Result<ReconcileReport, ForgeError>;

    async fn sync_mirror(
        &self,
        project: &ProjectId,
        want: MirrorWant,
        ctx: &ActorCtx,
    ) -> Result<(), ForgeError>;
}

/// 项目快照。规格书只给类型名；这里复用 `Project` 的实体形状并补充
/// 镜像仓状态，避免再次定义项目字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSnapshot {
    pub project: crate::entities::Project,
    pub synced_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionTarget {
    Issue(u64),
    PullRequest(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionQuery {
    pub include_body: bool,
    pub comments_since: Option<Timestamp>,
    pub max_comments: u32,
    pub max_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discussion {
    pub title: UntrustedText,
    pub body: Option<UntrustedText>,
    pub comments: Vec<DiscussionComment>,
    pub truncated: bool,
    pub fetched_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionComment {
    pub id: u64,
    pub author_login: String,
    pub author_association: Option<String>,
    pub created_at: Timestamp,
    pub body: UntrustedText,
}

/// `ci_log` 的查询。规格书只写 `CiLogQuery`，没有字段表；这里保留
/// GitHub Actions run 的最小定位信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiLogQuery {
    pub run_id: u64,
    pub run_attempt: Option<u32>,
    pub max_bytes: u32,
}

/// 不可信的 CI 日志文本。`truncated` 表示按 `max_bytes` 截断过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiLog {
    pub text: UntrustedText,
    pub truncated: bool,
    pub fetched_at: Timestamp,
}

/// 单个改动文件。字段来自 GitHub pull request files 的常用列；规格书未给
/// 字段表，故在 M0 只承诺这些列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: RepoPath,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub changes: u32,
    pub patch: Option<UntrustedText>,
}

/// review 线程的不可变句柄。GitHub 的 node id 不是 data-model 的 15 种 ID。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadNodeId(String);

impl ThreadNodeId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThreadComment {
    pub comment_id: u64,
    pub author_login: String,
    pub body: UntrustedText,
    pub is_reply: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThreadView {
    pub thread_node_id: ThreadNodeId,
    pub comments: Vec<ReviewThreadComment>,
    pub has_human_comment: bool,
    pub resolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    pub path: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub level: AnnotationLevel,
    pub message: UntrustedText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationLevel {
    Notice,
    Warning,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileTarget {
    Pr { project: ProjectId, number: u64 },
    Issue { project: ProjectId, number: u64 },
    Delivery(DeliveryId),
    Project(ProjectId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileReport {
    pub emitted: Vec<EventId>,
    pub drifts: Vec<DriftKind>,
    pub synced_at: Timestamp,
}

/// forge-actions §7.4 的 drift 目录。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    PrStateDiverged,
    HeadDiverged,
    EffectMissing,
    OrphanEffect,
    IssueStateDiverged,
    ReviewStateDiverged,
    MergeStateDiverged,
    DefaultBranchChanged,
    InstallationUnreachable,
}

impl DriftKind {
    pub const ALL: [Self; 9] = [
        Self::PrStateDiverged,
        Self::HeadDiverged,
        Self::EffectMissing,
        Self::OrphanEffect,
        Self::IssueStateDiverged,
        Self::ReviewStateDiverged,
        Self::MergeStateDiverged,
        Self::DefaultBranchChanged,
        Self::InstallationUnreachable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrStateDiverged => "pr_state_diverged",
            Self::HeadDiverged => "head_diverged",
            Self::EffectMissing => "effect_missing",
            Self::OrphanEffect => "orphan_effect",
            Self::IssueStateDiverged => "issue_state_diverged",
            Self::ReviewStateDiverged => "review_state_diverged",
            Self::MergeStateDiverged => "merge_state_diverged",
            Self::DefaultBranchChanged => "default_branch_changed",
            Self::InstallationUnreachable => "installation_unreachable",
        }
    }
}

/// 回读 PR 的合并与关闭事实；`None` 表示该路径没有来源，按契约不得合成。
///
/// 字段来自 `docs/interfaces/forge-drift.md` §4.4.1。M0 只定义类型，不改
/// `ForgeReader` 的现有快照读取签名，也不新增端口方法。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequestReadback {
    pub head_sha: GitOid,
    pub base_ref: String,
    #[serde(default)]
    pub closed_by: Option<ForgeActor>,
    #[serde(default)]
    pub merged_by: Option<ForgeActor>,
    #[serde(default)]
    pub via_merge_queue: Option<bool>,
}

/// 回读 Issue 的关闭事实；`None` 表示没有来源。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueReadback {
    #[serde(default)]
    pub closed_by: Option<ForgeActor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorWant {
    Branch(String),
    Oid(GitOid),
    All,
}

/// 端口错误：不包含 HTTP 协议；`api` 负责映射。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForgeError {
    #[error("pull request or issue not found")]
    NotFound,
    #[error("github rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("github unavailable: {message}")]
    Unavailable { message: String },
    #[error("github rejected the request: {message}")]
    GitHub { message: String },
    #[error("invalid target: {message}")]
    InvalidTarget { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

// ---------------------------------------------------------------------------
// PrRefresher
// ---------------------------------------------------------------------------

/// `api` 在审批签名前强制对账。保留 `repo` + PR number，不改成 project。
#[async_trait]
pub trait PrRefresher: Send + Sync {
    async fn refresh_pr(
        &self,
        repo: &RepoSlug,
        number: u64,
        ctx: &ActorCtx,
    ) -> Result<PrHead, ModuleError>;
}
