//! Forge 契约类型：14 个 Outbox 动作 payload、`ForgeEvent` 信封与 23 个事件 `data`。
//!
//! 权威来源：`docs/interfaces/forge-actions.md` §2–§6，以及 `docs/modules/core.md` §6.6。
//! 动作 payload 拒绝未知字段；事件 `data` 来自 GitHub，按前向兼容容忍未知字段。
//! 未定字段（`pr_updated` / `issue_updated` 的“新值”形状）不在本模块猜测，见模块测试的缺口记录。

use std::fmt;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::canon::{GitOid, RepoSlug, Sha256Digest};
use crate::entities::{IssueState, MergeMethod};
use crate::ids::{ActionId, DeliveryId, ProjectId, WorktreeId};
use crate::text::UntrustedText;
use crate::time::Timestamp;

mod key;

pub use key::{derive_target_intent, KeyError, TargetIntent};

pub use crate::entities::{
    ForgeActionKind, ForgeEventKind, ForgeOrigin, ReviewState as ReviewSnapshotState,
};

// ---------------------------------------------------------------------------
// 通用事件构件
// ---------------------------------------------------------------------------

/// GitHub 来源文本的线上类型：JSON 中是字符串，Rust 中是 [`UntrustedText`]。
///
/// `core` §8 要求 GitHub 文本字段用 `UntrustedText` 区分类型；`forge-actions` §6.2
/// 规定线上是字符串。这里保留两层：类型不与 `String` 互换，序列化仍是字符串。
#[derive(Clone, PartialEq, Eq)]
pub struct ForgeText(UntrustedText);

impl ForgeText {
    pub fn new(text: impl Into<String>, source: impl Into<String>, truncated: bool) -> Self {
        Self(UntrustedText::new(text.into(), source.into(), truncated))
    }

    pub fn from_wire(text: String) -> Self {
        Self(UntrustedText::new(text, "forge:event".into(), false))
    }

    pub fn as_untrusted(&self) -> &UntrustedText {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_untrusted_str()
    }

    pub fn source(&self) -> &str {
        self.0.source()
    }

    pub fn truncated(&self) -> bool {
        self.0.truncated()
    }
}

impl fmt::Debug for ForgeText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgeText")
            .field("source", &self.0.source())
            .field("bytes", &self.0.as_untrusted_str().len())
            .field("truncated", &self.0.truncated())
            .finish()
    }
}

impl Serialize for ForgeText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ForgeText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(Self::from_wire(text))
    }
}

impl JsonSchema for ForgeText {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ForgeText".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ForgeActorKind {
    User,
    Bot,
    App,
    Unknown,
}

/// GitHub 侧 actor；全部是外部数据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForgeActor {
    pub login: String,
    pub github_id: u64,
    #[serde(rename = "type")]
    pub kind: ForgeActorKind,
    #[serde(default)]
    pub association: Option<String>,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ForgeTargetType {
    Pr,
    Issue,
    Branch,
    Installation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForgeEventTarget {
    #[serde(rename = "type")]
    pub kind: ForgeTargetType,
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub node_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ForgeEventSource {
    Webhook,
    Reconcile,
}

/// `schema` 固定值 `forge.event.v1`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForgeEventSchema;

impl ForgeEventSchema {
    pub const VALUE: &'static str = "forge.event.v1";
}

impl Serialize for ForgeEventSchema {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(Self::VALUE)
    }
}

impl<'de> Deserialize<'de> for ForgeEventSchema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == Self::VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("unsupported forge event schema"))
        }
    }
}

impl JsonSchema for ForgeEventSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ForgeEventSchema".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "const": "forge.event.v1",
        })
    }
}

// ---------------------------------------------------------------------------
// Outbox 动作 payload（forge-actions §3.3）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PushTransport {
    Git,
    Api,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CommentTargetType {
    Pr,
    Issue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentTarget {
    #[serde(rename = "type")]
    pub kind: CommentTargetType,
    pub number: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusCommentState {
    Pending,
    Running,
    Passed,
    Failed,
    Skipped,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusCommentItem {
    pub id: String,
    pub state: StatusCommentState,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueStateReason {
    Completed,
    NotPlanned,
}

/// `mode` 目前只定义 `auto`；其他合并队列模式见 forge-actions OQ-1，未冻结前拒绝。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    Auto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushBranchPayload {
    pub repo: RepoSlug,
    pub branch: String,
    pub base_ref: String,
    pub worktree_id: WorktreeId,
    pub local_head_oid: GitOid,
    pub expected_tree_oid: GitOid,
    #[serde(default)]
    pub expected_remote_oid: Option<GitOid>,
    pub validation_ref: Sha256Digest,
    pub transport: PushTransport,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePrPayload {
    pub repo: RepoSlug,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub closes: Vec<u64>,
    pub expected_head_oid: GitOid,
    pub maintainer_can_modify: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePrPayload {
    pub repo: RepoSlug,
    pub pull_number: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub state: Option<PrState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpsertStatusCommentPayload {
    pub repo: RepoSlug,
    pub target: CommentTarget,
    pub slot: String,
    pub revision: u32,
    pub head_sha: GitOid,
    pub title: String,
    pub items: Vec<StatusCommentItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCommentPayload {
    pub repo: RepoSlug,
    pub target: CommentTarget,
    pub client_ref: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyReviewThreadPayload {
    pub repo: RepoSlug,
    pub pull_number: u64,
    pub thread_node_id: String,
    pub in_reply_to_comment_id: u64,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveReviewThreadPayload {
    pub repo: RepoSlug,
    pub pull_number: u64,
    pub thread_node_id: String,
    pub expected_comment_count: u32,
    /// 仅用于审计，不进幂等键。
    pub reason: String,
    pub fixed_in_oid: GitOid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIssuePayload {
    pub repo: RepoSlug,
    pub client_ref: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    #[serde(default)]
    pub milestone: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssuePayload {
    pub repo: RepoSlug,
    pub number: u64,
    pub spec_block_markdown: String,
    /// `null` 表示正文里还没有 Gantry 规格块（首次写入）。
    #[serde(default)]
    pub expected_block_hash: Option<Sha256Digest>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseIssuePayload {
    pub repo: RepoSlug,
    pub number: u64,
    pub state_reason: IssueStateReason,
    #[serde(default)]
    pub require_merged_pr: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddLabelsPayload {
    pub repo: RepoSlug,
    pub number: u64,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestReviewersPayload {
    pub repo: RepoSlug,
    pub pull_number: u64,
    pub reviewers: Vec<String>,
    pub team_reviewers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergePrPayload {
    pub repo: RepoSlug,
    pub pull_number: u64,
    pub expected_head_sha: GitOid,
    pub merge_method: MergeMethod,
    pub mode: MergeMode,
    #[serde(default)]
    pub commit_title: Option<String>,
    #[serde(default)]
    pub commit_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RerunFailedJobsPayload {
    pub repo: RepoSlug,
    pub run_id: u64,
    pub expected_run_attempt: u32,
    pub head_sha: GitOid,
}

/// 与 [`ForgeActionKind`] 一一对应的强类型 payload。
///
/// 序列化为裸 payload 对象（Outbox `payload` 列不含 `kind`）；解码必须显式给出 `kind`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ActionPayload {
    PushBranch(PushBranchPayload),
    CreatePr(CreatePrPayload),
    UpdatePr(UpdatePrPayload),
    UpsertStatusComment(UpsertStatusCommentPayload),
    CreateComment(CreateCommentPayload),
    ReplyReviewThread(ReplyReviewThreadPayload),
    ResolveReviewThread(ResolveReviewThreadPayload),
    CreateIssue(CreateIssuePayload),
    UpdateIssue(UpdateIssuePayload),
    CloseIssue(CloseIssuePayload),
    AddLabels(AddLabelsPayload),
    RequestReviewers(RequestReviewersPayload),
    MergePr(MergePrPayload),
    RerunFailedJobs(RerunFailedJobsPayload),
}

impl ActionPayload {
    /// 每个动作 `kind` 的 payload 子 schema；用于生成 kind/payload 联动的 `oneOf`。
    ///
    /// 顺序与 [`ForgeActionKind::ALL`] 一致；未知字段由各 payload 自己的
    /// `additionalProperties: false` 拒绝。
    pub(crate) fn schema_variants(
        generator: &mut schemars::SchemaGenerator,
    ) -> Vec<(&'static str, schemars::Schema)> {
        vec![
            (
                "push_branch",
                generator.subschema_for::<PushBranchPayload>(),
            ),
            ("create_pr", generator.subschema_for::<CreatePrPayload>()),
            ("update_pr", generator.subschema_for::<UpdatePrPayload>()),
            (
                "upsert_status_comment",
                generator.subschema_for::<UpsertStatusCommentPayload>(),
            ),
            (
                "create_comment",
                generator.subschema_for::<CreateCommentPayload>(),
            ),
            (
                "reply_review_thread",
                generator.subschema_for::<ReplyReviewThreadPayload>(),
            ),
            (
                "resolve_review_thread",
                generator.subschema_for::<ResolveReviewThreadPayload>(),
            ),
            (
                "create_issue",
                generator.subschema_for::<CreateIssuePayload>(),
            ),
            (
                "update_issue",
                generator.subschema_for::<UpdateIssuePayload>(),
            ),
            (
                "close_issue",
                generator.subschema_for::<CloseIssuePayload>(),
            ),
            ("add_labels", generator.subschema_for::<AddLabelsPayload>()),
            (
                "request_reviewers",
                generator.subschema_for::<RequestReviewersPayload>(),
            ),
            ("merge_pr", generator.subschema_for::<MergePrPayload>()),
            (
                "rerun_failed_jobs",
                generator.subschema_for::<RerunFailedJobsPayload>(),
            ),
        ]
    }

    pub fn kind(&self) -> ForgeActionKind {
        match self {
            Self::PushBranch(_) => ForgeActionKind::PushBranch,
            Self::CreatePr(_) => ForgeActionKind::CreatePr,
            Self::UpdatePr(_) => ForgeActionKind::UpdatePr,
            Self::UpsertStatusComment(_) => ForgeActionKind::UpsertStatusComment,
            Self::CreateComment(_) => ForgeActionKind::CreateComment,
            Self::ReplyReviewThread(_) => ForgeActionKind::ReplyReviewThread,
            Self::ResolveReviewThread(_) => ForgeActionKind::ResolveReviewThread,
            Self::CreateIssue(_) => ForgeActionKind::CreateIssue,
            Self::UpdateIssue(_) => ForgeActionKind::UpdateIssue,
            Self::CloseIssue(_) => ForgeActionKind::CloseIssue,
            Self::AddLabels(_) => ForgeActionKind::AddLabels,
            Self::RequestReviewers(_) => ForgeActionKind::RequestReviewers,
            Self::MergePr(_) => ForgeActionKind::MergePr,
            Self::RerunFailedJobs(_) => ForgeActionKind::RerunFailedJobs,
        }
    }

    /// 严格解码；未知字段、缺失字段、类型不符都返回错误。
    pub fn from_value(kind: ForgeActionKind, value: Value) -> Result<Self, serde_json::Error> {
        Ok(match kind {
            ForgeActionKind::PushBranch => Self::PushBranch(serde_json::from_value(value)?),
            ForgeActionKind::CreatePr => Self::CreatePr(serde_json::from_value(value)?),
            ForgeActionKind::UpdatePr => Self::UpdatePr(serde_json::from_value(value)?),
            ForgeActionKind::UpsertStatusComment => {
                Self::UpsertStatusComment(serde_json::from_value(value)?)
            }
            ForgeActionKind::CreateComment => Self::CreateComment(serde_json::from_value(value)?),
            ForgeActionKind::ReplyReviewThread => {
                Self::ReplyReviewThread(serde_json::from_value(value)?)
            }
            ForgeActionKind::ResolveReviewThread => {
                Self::ResolveReviewThread(serde_json::from_value(value)?)
            }
            ForgeActionKind::CreateIssue => Self::CreateIssue(serde_json::from_value(value)?),
            ForgeActionKind::UpdateIssue => Self::UpdateIssue(serde_json::from_value(value)?),
            ForgeActionKind::CloseIssue => Self::CloseIssue(serde_json::from_value(value)?),
            ForgeActionKind::AddLabels => Self::AddLabels(serde_json::from_value(value)?),
            ForgeActionKind::RequestReviewers => {
                Self::RequestReviewers(serde_json::from_value(value)?)
            }
            ForgeActionKind::MergePr => Self::MergePr(serde_json::from_value(value)?),
            ForgeActionKind::RerunFailedJobs => {
                Self::RerunFailedJobs(serde_json::from_value(value)?)
            }
        })
    }

    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

// ---------------------------------------------------------------------------
// ForgeEvent 信封与 23 个 data（forge-actions §6.2–§6.3）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrHead {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: GitOid,
    pub repo: RepoSlug,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrBase {
    #[serde(rename = "ref")]
    pub ref_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrOpenedData {
    pub number: u64,
    pub node_id: String,
    pub title: ForgeText,
    pub body: ForgeText,
    pub head: PrHead,
    pub base: PrBase,
    pub draft: bool,
    pub labels: Vec<String>,
    pub author: ForgeActor,
    pub closing_issues: Vec<u64>,
    pub created_by_gantry: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrReopenedData {
    pub number: u64,
    pub head_sha: GitOid,
    pub base_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrSynchronizedData {
    pub head_sha: GitOid,
    pub previous_head_sha: GitOid,
    pub head_epoch: u64,
    #[serde(default)]
    pub forced: Option<bool>,
    pub base_ref: String,
    #[serde(default)]
    pub commits_added: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrChangedField {
    Title,
    Body,
    Base,
    Draft,
    Labels,
    Reviewers,
}

/// `changed[]` 的逐字段新值形状在 forge-actions §6.3 未定义，本类型只保留字段名；
/// 契约补齐新值形状前不猜测（见 `docs/m0-implementation.md` 的缺口处理）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrUpdatedData {
    pub changed: Vec<PrChangedField>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrClosedData {
    pub head_sha: GitOid,
    pub closed_by: ForgeActor,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PrMergedData {
    pub merge_commit_sha: GitOid,
    pub head_sha: GitOid,
    pub base_ref: String,
    pub merged_by: ForgeActor,
    pub via_merge_queue: bool,
    #[serde(default)]
    pub merge_method: Option<MergeMethod>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BaseAdvancedData {
    pub base_ref: String,
    pub before: GitOid,
    pub after: GitOid,
    pub affected_prs: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrEnqueuedData {
    pub number: u64,
    pub head_sha: GitOid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrDequeuedData {
    pub number: u64,
    pub head_sha: GitOid,
    /// 取值需核实（forge-actions §6.3），不建封闭枚举。
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MergeGroupChecksRequestedData {
    pub head_sha: GitOid,
    pub head_ref: String,
    pub base_ref: String,
    pub base_sha: GitOid,
    pub pr_numbers: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MergeGroupDestroyedData {
    pub head_sha: GitOid,
    /// 官方取值 `merged` / `dequeued` / `invalidated` 的确切字符串需核实，保留原文。
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    TimedOut,
    ActionRequired,
    Stale,
    Skipped,
    StartupFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForgeApp {
    pub id: u64,
    pub slug: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRun {
    pub run_id: u64,
    pub run_attempt: u32,
    pub name: String,
    pub path: String,
    pub conclusion: CheckConclusion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailedCheckRun {
    pub check_run_id: u64,
    pub name: String,
    pub conclusion: CheckConclusion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckSuiteCompletedData {
    pub check_suite_id: u64,
    pub head_sha: GitOid,
    pub conclusion: CheckConclusion,
    pub app: ForgeApp,
    pub pr_numbers: Vec<u64>,
    pub is_current_head: bool,
    pub workflow_runs: Vec<WorkflowRun>,
    pub failed_check_runs: Vec<FailedCheckRun>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckRunOutput {
    pub title: ForgeText,
    pub summary: ForgeText,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckRunCompletedData {
    pub check_run_id: u64,
    pub check_suite_id: u64,
    pub name: String,
    pub head_sha: GitOid,
    pub conclusion: CheckConclusion,
    pub app: ForgeApp,
    #[serde(default)]
    pub details_url: Option<String>,
    pub output: CheckRunOutput,
    pub annotations_count: u64,
    /// `required` 在 installation token 下是否可用需核实（forge-actions OQ-9）。
    #[serde(default)]
    pub required: Option<bool>,
    pub is_current_head: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewSubmittedData {
    pub review_id: u64,
    pub state: ReviewState,
    pub commit_id: GitOid,
    pub reviewer: ForgeActor,
    pub body: ForgeText,
    pub is_current_head: bool,
    pub is_bot: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewDismissedData {
    pub review_id: u64,
    pub dismissed_by: ForgeActor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiffSide {
    Head,
    Base,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewCommentData {
    pub comment_id: u64,
    pub review_id: u64,
    pub thread_node_id: String,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub side: Option<DiffSide>,
    pub commit_id: GitOid,
    pub body: ForgeText,
    pub diff_hunk: ForgeText,
    pub outdated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReviewThreadState {
    Resolved,
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewThreadChangedData {
    pub thread_node_id: String,
    pub state: ReviewThreadState,
    pub thread_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueCommentOn {
    Issue,
    PullRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IssueCommentAction {
    Created,
    Edited,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IssueCommentData {
    pub comment_id: u64,
    pub on: IssueCommentOn,
    pub number: u64,
    pub action: IssueCommentAction,
    pub body: ForgeText,
    #[serde(default)]
    pub author_association: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueClosedData {
    pub number: u64,
    /// 已知 `completed` / `not_planned`，另有取值需核实（forge-actions OQ-12），保留原文。
    #[serde(default)]
    pub state_reason: Option<String>,
    pub closed_by: ForgeActor,
    #[serde(default)]
    pub closed_via_pr: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueReopenedData {
    pub number: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueChangedField {
    Title,
    Body,
    Labels,
    Assignees,
}

/// 与 [`PrUpdatedData`] 相同：逐字段新值形状未定义，只保留字段名。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueUpdatedData {
    pub number: u64,
    pub changed: Vec<IssueChangedField>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallationAction {
    Suspend,
    Unsuspend,
    Deleted,
    NewPermissionsAccepted,
    ReposAdded,
    ReposRemoved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstallationChangedData {
    pub action: InstallationAction,
    pub repos: Vec<RepoSlug>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepoAction {
    Renamed,
    Archived,
    Transferred,
    Edited,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoChangedData {
    pub action: RepoAction,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub renamed_from: Option<RepoSlug>,
}

/// 23 个 `forge.<kind>` 的强类型 `data`。
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ForgeEventData {
    PrOpened(PrOpenedData),
    PrReopened(PrReopenedData),
    PrSynchronized(PrSynchronizedData),
    PrUpdated(PrUpdatedData),
    PrClosed(PrClosedData),
    PrMerged(PrMergedData),
    BaseAdvanced(BaseAdvancedData),
    PrEnqueued(PrEnqueuedData),
    PrDequeued(PrDequeuedData),
    MergeGroupChecksRequested(MergeGroupChecksRequestedData),
    MergeGroupDestroyed(MergeGroupDestroyedData),
    CheckSuiteCompleted(CheckSuiteCompletedData),
    CheckRunCompleted(CheckRunCompletedData),
    ReviewSubmitted(ReviewSubmittedData),
    ReviewDismissed(ReviewDismissedData),
    ReviewComment(ReviewCommentData),
    ReviewThreadChanged(ReviewThreadChangedData),
    IssueComment(IssueCommentData),
    IssueClosed(IssueClosedData),
    IssueReopened(IssueReopenedData),
    IssueUpdated(IssueUpdatedData),
    InstallationChanged(InstallationChangedData),
    RepoChanged(RepoChangedData),
}

impl ForgeEventData {
    pub fn kind(&self) -> ForgeEventKind {
        match self {
            Self::PrOpened(_) => ForgeEventKind::PrOpened,
            Self::PrReopened(_) => ForgeEventKind::PrReopened,
            Self::PrSynchronized(_) => ForgeEventKind::PrSynchronized,
            Self::PrUpdated(_) => ForgeEventKind::PrUpdated,
            Self::PrClosed(_) => ForgeEventKind::PrClosed,
            Self::PrMerged(_) => ForgeEventKind::PrMerged,
            Self::BaseAdvanced(_) => ForgeEventKind::BaseAdvanced,
            Self::PrEnqueued(_) => ForgeEventKind::PrEnqueued,
            Self::PrDequeued(_) => ForgeEventKind::PrDequeued,
            Self::MergeGroupChecksRequested(_) => ForgeEventKind::MergeGroupChecksRequested,
            Self::MergeGroupDestroyed(_) => ForgeEventKind::MergeGroupDestroyed,
            Self::CheckSuiteCompleted(_) => ForgeEventKind::CheckSuiteCompleted,
            Self::CheckRunCompleted(_) => ForgeEventKind::CheckRunCompleted,
            Self::ReviewSubmitted(_) => ForgeEventKind::ReviewSubmitted,
            Self::ReviewDismissed(_) => ForgeEventKind::ReviewDismissed,
            Self::ReviewComment(_) => ForgeEventKind::ReviewComment,
            Self::ReviewThreadChanged(_) => ForgeEventKind::ReviewThreadChanged,
            Self::IssueComment(_) => ForgeEventKind::IssueComment,
            Self::IssueClosed(_) => ForgeEventKind::IssueClosed,
            Self::IssueReopened(_) => ForgeEventKind::IssueReopened,
            Self::IssueUpdated(_) => ForgeEventKind::IssueUpdated,
            Self::InstallationChanged(_) => ForgeEventKind::InstallationChanged,
            Self::RepoChanged(_) => ForgeEventKind::RepoChanged,
        }
    }

    pub fn from_value(kind: ForgeEventKind, value: Value) -> Result<Self, serde_json::Error> {
        Ok(match kind {
            ForgeEventKind::PrOpened => Self::PrOpened(serde_json::from_value(value)?),
            ForgeEventKind::PrReopened => Self::PrReopened(serde_json::from_value(value)?),
            ForgeEventKind::PrSynchronized => Self::PrSynchronized(serde_json::from_value(value)?),
            ForgeEventKind::PrUpdated => Self::PrUpdated(serde_json::from_value(value)?),
            ForgeEventKind::PrClosed => Self::PrClosed(serde_json::from_value(value)?),
            ForgeEventKind::PrMerged => Self::PrMerged(serde_json::from_value(value)?),
            ForgeEventKind::BaseAdvanced => Self::BaseAdvanced(serde_json::from_value(value)?),
            ForgeEventKind::PrEnqueued => Self::PrEnqueued(serde_json::from_value(value)?),
            ForgeEventKind::PrDequeued => Self::PrDequeued(serde_json::from_value(value)?),
            ForgeEventKind::MergeGroupChecksRequested => {
                Self::MergeGroupChecksRequested(serde_json::from_value(value)?)
            }
            ForgeEventKind::MergeGroupDestroyed => {
                Self::MergeGroupDestroyed(serde_json::from_value(value)?)
            }
            ForgeEventKind::CheckSuiteCompleted => {
                Self::CheckSuiteCompleted(serde_json::from_value(value)?)
            }
            ForgeEventKind::CheckRunCompleted => {
                Self::CheckRunCompleted(serde_json::from_value(value)?)
            }
            ForgeEventKind::ReviewSubmitted => {
                Self::ReviewSubmitted(serde_json::from_value(value)?)
            }
            ForgeEventKind::ReviewDismissed => {
                Self::ReviewDismissed(serde_json::from_value(value)?)
            }
            ForgeEventKind::ReviewComment => Self::ReviewComment(serde_json::from_value(value)?),
            ForgeEventKind::ReviewThreadChanged => {
                Self::ReviewThreadChanged(serde_json::from_value(value)?)
            }
            ForgeEventKind::IssueComment => Self::IssueComment(serde_json::from_value(value)?),
            ForgeEventKind::IssueClosed => Self::IssueClosed(serde_json::from_value(value)?),
            ForgeEventKind::IssueReopened => Self::IssueReopened(serde_json::from_value(value)?),
            ForgeEventKind::IssueUpdated => Self::IssueUpdated(serde_json::from_value(value)?),
            ForgeEventKind::InstallationChanged => {
                Self::InstallationChanged(serde_json::from_value(value)?)
            }
            ForgeEventKind::RepoChanged => Self::RepoChanged(serde_json::from_value(value)?),
        })
    }

    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

/// `forge.<kind>` 事件的 `data`：forge-actions §6.2 信封。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForgeEventEnvelope {
    pub schema: ForgeEventSchema,
    pub kind: ForgeEventKind,
    pub dedup_key: String,
    pub source: ForgeEventSource,
    pub delivery_ids: Vec<String>,
    pub project_id: ProjectId,
    pub repo: RepoSlug,
    pub installation_id: u64,
    pub delivery_id: Option<DeliveryId>,
    pub target: ForgeEventTarget,
    pub actor: ForgeActor,
    pub origin: ForgeOrigin,
    pub caused_by_action: Option<ActionId>,
    pub occurred_at: Timestamp,
    pub observed_at: Timestamp,
    pub stale: bool,
    pub data: ForgeEventData,
}

/// 只用于生成 JSON Schema 的字段镜像；`data` 由 kind 分支收窄。
#[derive(JsonSchema)]
#[allow(dead_code)]
struct ForgeEnvelopeFields {
    schema: ForgeEventSchema,
    kind: ForgeEventKind,
    dedup_key: String,
    source: ForgeEventSource,
    delivery_ids: Vec<String>,
    project_id: ProjectId,
    repo: RepoSlug,
    installation_id: u64,
    delivery_id: Option<DeliveryId>,
    target: ForgeEventTarget,
    actor: ForgeActor,
    origin: ForgeOrigin,
    caused_by_action: Option<ActionId>,
    occurred_at: Timestamp,
    observed_at: Timestamp,
    stale: bool,
    data: Value,
}

fn forge_data_branch(kind: ForgeEventKind, data: Schema) -> Value {
    serde_json::json!({
        "properties": {
            "kind": { "const": kind.as_str() },
            "data": data.to_value()
        },
        "required": ["kind", "data"]
    })
}

impl JsonSchema for ForgeEventEnvelope {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ForgeEventEnvelope".into()
    }

    /// 每个 `kind` 一支，绑定该 kind 的强类型 `data`；`origin=self` 时必须带
    /// `caused_by_action`。未列出的 kind/data 组合不匹配任何分支。
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let mut branches = Vec::new();
        macro_rules! branch {
            ($kind:expr, $ty:ty) => {{
                let data = generator.subschema_for::<$ty>();
                branches.push(forge_data_branch($kind, data));
            }};
        }
        branch!(ForgeEventKind::PrOpened, PrOpenedData);
        branch!(ForgeEventKind::PrReopened, PrReopenedData);
        branch!(ForgeEventKind::PrSynchronized, PrSynchronizedData);
        branch!(ForgeEventKind::PrUpdated, PrUpdatedData);
        branch!(ForgeEventKind::PrClosed, PrClosedData);
        branch!(ForgeEventKind::PrMerged, PrMergedData);
        branch!(ForgeEventKind::BaseAdvanced, BaseAdvancedData);
        branch!(ForgeEventKind::PrEnqueued, PrEnqueuedData);
        branch!(ForgeEventKind::PrDequeued, PrDequeuedData);
        branch!(
            ForgeEventKind::MergeGroupChecksRequested,
            MergeGroupChecksRequestedData
        );
        branch!(ForgeEventKind::MergeGroupDestroyed, MergeGroupDestroyedData);
        branch!(ForgeEventKind::CheckSuiteCompleted, CheckSuiteCompletedData);
        branch!(ForgeEventKind::CheckRunCompleted, CheckRunCompletedData);
        branch!(ForgeEventKind::ReviewSubmitted, ReviewSubmittedData);
        branch!(ForgeEventKind::ReviewDismissed, ReviewDismissedData);
        branch!(ForgeEventKind::ReviewComment, ReviewCommentData);
        branch!(ForgeEventKind::ReviewThreadChanged, ReviewThreadChangedData);
        branch!(ForgeEventKind::IssueComment, IssueCommentData);
        branch!(ForgeEventKind::IssueClosed, IssueClosedData);
        branch!(ForgeEventKind::IssueReopened, IssueReopenedData);
        branch!(ForgeEventKind::IssueUpdated, IssueUpdatedData);
        branch!(ForgeEventKind::InstallationChanged, InstallationChangedData);
        branch!(ForgeEventKind::RepoChanged, RepoChangedData);
        let mut fields = crate::entities::inline_fields::<ForgeEnvelopeFields>(generator);
        fields.remove("description");
        fields.insert(
            "description".to_owned(),
            Value::String("`forge.<kind>` 事件的 `data`：forge-actions §6.2 信封。".to_owned()),
        );
        fields.insert(
            "title".to_owned(),
            Value::String("ForgeEventEnvelope".to_owned()),
        );
        fields.insert("oneOf".to_owned(), Value::Array(branches));
        fields.insert(
            "allOf".to_owned(),
            serde_json::json!([{
                "if": {
                    "properties": { "origin": { "const": "self" } },
                    "required": ["origin"]
                },
                "then": {
                    "required": ["caused_by_action"],
                    "properties": {
                        "caused_by_action": { "not": { "type": "null" } }
                    }
                }
            }]),
        );
        Schema::from(fields)
    }
}

impl ForgeEventEnvelope {
    /// `origin=self` 必须带 `caused_by_action`；`data` 必须与 `kind` 相符。
    pub fn validate(&self) -> Result<(), ForgeEventError> {
        if self.data.kind() != self.kind {
            return Err(ForgeEventError::KindDataMismatch);
        }
        if self.origin == ForgeOrigin::Own && self.caused_by_action.is_none() {
            return Err(ForgeEventError::MissingCausedByAction);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ForgeEventError {
    #[error("forge event data does not match its kind")]
    KindDataMismatch,
    #[error("origin=self requires caused_by_action")]
    MissingCausedByAction,
}

#[derive(Deserialize)]
struct RawForgeEventEnvelope {
    schema: ForgeEventSchema,
    kind: ForgeEventKind,
    dedup_key: String,
    source: ForgeEventSource,
    delivery_ids: Vec<String>,
    project_id: ProjectId,
    repo: RepoSlug,
    installation_id: u64,
    delivery_id: Option<DeliveryId>,
    target: ForgeEventTarget,
    actor: ForgeActor,
    origin: ForgeOrigin,
    caused_by_action: Option<ActionId>,
    occurred_at: Timestamp,
    observed_at: Timestamp,
    stale: bool,
    data: Value,
}

impl<'de> Deserialize<'de> for ForgeEventEnvelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawForgeEventEnvelope::deserialize(deserializer)?;
        let data = ForgeEventData::from_value(raw.kind, raw.data).map_err(|error| {
            serde::de::Error::custom(format!("invalid forge event data: {error}"))
        })?;
        Ok(Self {
            schema: raw.schema,
            kind: raw.kind,
            dedup_key: raw.dedup_key,
            source: raw.source,
            delivery_ids: raw.delivery_ids,
            project_id: raw.project_id,
            repo: raw.repo,
            installation_id: raw.installation_id,
            delivery_id: raw.delivery_id,
            target: raw.target,
            actor: raw.actor,
            origin: raw.origin,
            caused_by_action: raw.caused_by_action,
            occurred_at: raw.occurred_at,
            observed_at: raw.observed_at,
            stale: raw.stale,
            data,
        })
    }
}

/// 预留的 Forge 动作 id 辅助表；`PolicyActionId` 仍是策略动作的权威类型。
pub fn action_kinds() -> &'static [ForgeActionKind] {
    &ForgeActionKind::ALL
}

/// `pr_updated` / `issue_updated` 的未定新值形状记录，供 schema 生成与测试引用。
pub const UNDEFINED_CHANGED_VALUES: &str =
    "forge-actions §6.3 未定义 changed[] 的逐字段新值形状；M0 不猜测，只有字段名";

// ---------------------------------------------------------------------------
// Drift 记录（docs/interfaces/forge-drift.md §4）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriftResolution {
    GithubWins,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriftDetectedBy {
    Patrol,
    StartupSweep,
    Recovery,
    OnDemand,
    Webhook,
}

/// 合成事件的 kind；不带 `forge.` 前缀。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriftSynthesizedEvent {
    PrClosed,
    PrMerged,
    PrSynchronized,
    IssueClosed,
    IssueReopened,
    PrDequeued,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallationUnreachableCode {
    TokenExchangeFailed,
    RepoNotAccessible,
    PermissionDenied,
    TransportError,
}

/// drift 的对象身份。不能用 `ForgeEventTarget` 代替：后者没有 `repo` / `branch`，
/// 且 `number` / `node_id` 可选。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum DriftTarget {
    Pr { repo: RepoSlug, number: u64 },
    Issue { repo: RepoSlug, number: u64 },
    Branch { repo: RepoSlug, branch: String },
    Repo { repo: RepoSlug },
    Installation { installation_id: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PrExpectedState {
    Open,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PrTerminalState {
    Closed,
    Merged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrphanObject {
    Comment,
    PullRequest,
    Issue,
}

/// 必填但可为 `null` 的包装；用于区分“字段缺失”和“字段存在但值未知”。
///
/// 不能写成 `#[serde(transparent)] Option<T>`：serde 会把缺失字段也当成 `None`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequiredNullable<T> {
    Value(T),
    Null,
}

impl<T> RequiredNullable<T> {
    pub fn as_ref(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Null => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// 只接受 JSON `null` 的字段类型；字段缺失由 [`DriftPayload`] 的解码入口拒绝。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NullOnly;

impl Serialize for NullOnly {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_unit()
    }
}

impl<'de> Deserialize<'de> for NullOnly {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Option::<Value>::deserialize(deserializer)? {
            None => Ok(Self),
            Some(_) => Err(serde::de::Error::custom("expected null")),
        }
    }
}

impl JsonSchema for NullOnly {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NullOnly".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({ "type": "null" })
    }
}

/// 可空但字段必须存在的 `delivery_id`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum MaybeDeliveryId {
    Delivery(DeliveryId),
    Null,
}

impl MaybeDeliveryId {
    pub fn as_ref(&self) -> Option<&DeliveryId> {
        match self {
            Self::Delivery(id) => Some(id),
            Self::Null => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrStateExpected {
    pub state: PrExpectedState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "lowercase", deny_unknown_fields)]
pub enum PrStateActual {
    Closed {},
    Merged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merge_commit_sha: Option<GitOid>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadDivergedExpected {
    pub head_sha: GitOid,
    pub basis_action: ActionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadDivergedActual {
    pub head_sha: GitOid,
    pub pushed_by: RequiredNullable<ForgeActor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectMissingExpected {
    StatusComment {
        action_id: ActionId,
        render_digest: Sha256Digest,
    },
    Branch {
        action_id: ActionId,
    },
    PullRequest {
        action_id: ActionId,
        state: PrExpectedState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "observation", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectMissingActual {
    Missing {},
    ContentChanged { render_digest: Sha256Digest },
    StateChanged { state: PrTerminalState },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrphanEffectExpected {
    pub outbox_action: NullOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrphanEffectActual {
    pub object: OrphanObject,
    pub github_id: u64,
    pub node_id: String,
    pub marker: ForgeText,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueStateExpected {
    pub state: IssueState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<IssueStateReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "observation", rename_all = "snake_case", deny_unknown_fields)]
pub enum IssueStateActual {
    State {
        state: IssueState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_reason: Option<IssueStateReason>,
    },
    Deleted {},
    Transferred {
        to_repo: RepoSlug,
        new_number: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateExpected {
    pub review_state: ReviewSnapshotState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateActual {
    pub review_state: ReviewSnapshotState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeStateExpected {
    pub in_merge_queue: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeStateActual {
    pub in_merge_queue: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ForgeText>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultBranchChangedExpected {
    pub default_branch: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultBranchChangedActual {
    pub default_branch: String,
    /// 旧默认分支名，不是 `RepoSlug`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallationUnreachableExpected {
    pub reachable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallationUnreachableActual {
    pub reachable: bool,
    pub code: InstallationUnreachableCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrStateDivergedDrift {
    pub delivery_id: DeliveryId,
    pub target: DriftTarget,
    pub expected: PrStateExpected,
    pub actual: PrStateActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadDivergedDrift {
    pub delivery_id: DeliveryId,
    pub target: DriftTarget,
    pub expected: HeadDivergedExpected,
    pub actual: HeadDivergedActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffectMissingDrift {
    pub delivery_id: DeliveryId,
    pub target: DriftTarget,
    pub expected: EffectMissingExpected,
    pub actual: EffectMissingActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrphanEffectDrift {
    pub delivery_id: MaybeDeliveryId,
    pub target: DriftTarget,
    pub expected: OrphanEffectExpected,
    pub actual: OrphanEffectActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueStateDivergedDrift {
    pub delivery_id: DeliveryId,
    pub target: DriftTarget,
    pub expected: IssueStateExpected,
    pub actual: IssueStateActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateDivergedDrift {
    pub delivery_id: DeliveryId,
    pub target: DriftTarget,
    pub expected: ReviewStateExpected,
    pub actual: ReviewStateActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeStateDivergedDrift {
    pub delivery_id: DeliveryId,
    pub target: DriftTarget,
    pub expected: MergeStateExpected,
    pub actual: MergeStateActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultBranchChangedDrift {
    pub delivery_id: NullOnly,
    pub target: DriftTarget,
    pub expected: DefaultBranchChangedExpected,
    pub actual: DefaultBranchChangedActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallationUnreachableDrift {
    pub delivery_id: NullOnly,
    pub target: DriftTarget,
    pub expected: InstallationUnreachableExpected,
    pub actual: InstallationUnreachableActual,
    pub resolution: DriftResolution,
    pub synthesized_events: Vec<DriftSynthesizedEvent>,
    pub detected_by: DriftDetectedBy,
}

/// `forge.drift_detected.data`：`kind` 与 expected/actual 分支强绑定。
///
/// 反序列化只接受这 9 个 `kind`；未知 kind、分支错配、未知字段和缺必填都会失败。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriftPayload {
    PrStateDiverged(PrStateDivergedDrift),
    HeadDiverged(HeadDivergedDrift),
    EffectMissing(EffectMissingDrift),
    OrphanEffect(OrphanEffectDrift),
    IssueStateDiverged(IssueStateDivergedDrift),
    ReviewStateDiverged(ReviewStateDivergedDrift),
    MergeStateDiverged(MergeStateDivergedDrift),
    DefaultBranchChanged(DefaultBranchChangedDrift),
    InstallationUnreachable(InstallationUnreachableDrift),
}

impl DriftPayload {
    pub fn kind(&self) -> crate::ports::forge::DriftKind {
        match self {
            Self::PrStateDiverged(_) => crate::ports::forge::DriftKind::PrStateDiverged,
            Self::HeadDiverged(_) => crate::ports::forge::DriftKind::HeadDiverged,
            Self::EffectMissing(_) => crate::ports::forge::DriftKind::EffectMissing,
            Self::OrphanEffect(_) => crate::ports::forge::DriftKind::OrphanEffect,
            Self::IssueStateDiverged(_) => crate::ports::forge::DriftKind::IssueStateDiverged,
            Self::ReviewStateDiverged(_) => crate::ports::forge::DriftKind::ReviewStateDiverged,
            Self::MergeStateDiverged(_) => crate::ports::forge::DriftKind::MergeStateDiverged,
            Self::DefaultBranchChanged(_) => crate::ports::forge::DriftKind::DefaultBranchChanged,
            Self::InstallationUnreachable(_) => {
                crate::ports::forge::DriftKind::InstallationUnreachable
            }
        }
    }

    pub fn target(&self) -> &DriftTarget {
        match self {
            Self::PrStateDiverged(drift) => &drift.target,
            Self::HeadDiverged(drift) => &drift.target,
            Self::EffectMissing(drift) => &drift.target,
            Self::OrphanEffect(drift) => &drift.target,
            Self::IssueStateDiverged(drift) => &drift.target,
            Self::ReviewStateDiverged(drift) => &drift.target,
            Self::MergeStateDiverged(drift) => &drift.target,
            Self::DefaultBranchChanged(drift) => &drift.target,
            Self::InstallationUnreachable(drift) => &drift.target,
        }
    }

    pub fn delivery_id(&self) -> Option<&DeliveryId> {
        match self {
            Self::PrStateDiverged(drift) => Some(&drift.delivery_id),
            Self::HeadDiverged(drift) => Some(&drift.delivery_id),
            Self::EffectMissing(drift) => Some(&drift.delivery_id),
            Self::OrphanEffect(drift) => drift.delivery_id.as_ref(),
            Self::IssueStateDiverged(drift) => Some(&drift.delivery_id),
            Self::ReviewStateDiverged(drift) => Some(&drift.delivery_id),
            Self::MergeStateDiverged(drift) => Some(&drift.delivery_id),
            Self::DefaultBranchChanged(_) | Self::InstallationUnreachable(_) => None,
        }
    }

    pub fn synthesized_events(&self) -> &[DriftSynthesizedEvent] {
        match self {
            Self::PrStateDiverged(drift) => &drift.synthesized_events,
            Self::HeadDiverged(drift) => &drift.synthesized_events,
            Self::EffectMissing(drift) => &drift.synthesized_events,
            Self::OrphanEffect(drift) => &drift.synthesized_events,
            Self::IssueStateDiverged(drift) => &drift.synthesized_events,
            Self::ReviewStateDiverged(drift) => &drift.synthesized_events,
            Self::MergeStateDiverged(drift) => &drift.synthesized_events,
            Self::DefaultBranchChanged(drift) => &drift.synthesized_events,
            Self::InstallationUnreachable(drift) => &drift.synthesized_events,
        }
    }

    /// 程序化构造后的一致性校验；`from_value` 会自动调用。
    pub fn validate(&self) -> Result<(), DriftValidationError> {
        match self {
            Self::PrStateDiverged(drift) => validate_pr_state(drift),
            Self::HeadDiverged(drift) => validate_head(drift),
            Self::EffectMissing(drift) => validate_effect_missing(drift),
            Self::OrphanEffect(drift) => validate_orphan_effect(drift),
            Self::IssueStateDiverged(drift) => validate_issue_state(drift),
            Self::ReviewStateDiverged(drift) => validate_review_state(drift),
            Self::MergeStateDiverged(drift) => validate_merge_state(drift),
            Self::DefaultBranchChanged(drift) => validate_default_branch(drift),
            Self::InstallationUnreachable(drift) => validate_installation(drift),
        }
    }

    /// 严格解码；未知 kind、分支错配、未知字段和缺必填都返回错误。
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

impl<'de> Deserialize<'de> for DriftPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let Value::Object(mut object) = Value::deserialize(deserializer)? else {
            return Err(D::Error::custom("drift payload must be an object"));
        };
        let kind = object
            .remove("kind")
            .ok_or_else(|| D::Error::missing_field("kind"))?;
        let kind = kind
            .as_str()
            .ok_or_else(|| D::Error::custom("drift `kind` must be a string"))?
            .to_owned();
        if !object.contains_key("delivery_id") {
            return Err(D::Error::missing_field("delivery_id"));
        }
        let value = Value::Object(object);
        let payload = match kind.as_str() {
            "pr_state_diverged" => Self::PrStateDiverged(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "head_diverged" => Self::HeadDiverged(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "effect_missing" => Self::EffectMissing(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "orphan_effect" => Self::OrphanEffect(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "issue_state_diverged" => Self::IssueStateDiverged(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "review_state_diverged" => Self::ReviewStateDiverged(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "merge_state_diverged" => Self::MergeStateDiverged(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "default_branch_changed" => Self::DefaultBranchChanged(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            "installation_unreachable" => Self::InstallationUnreachable(
                serde_json::from_value(value)
                    .map_err(|error| D::Error::custom(error.to_string()))?,
            ),
            other => return Err(D::Error::custom(format!("unknown drift kind `{other}`"))),
        };
        payload
            .validate()
            .map_err(|error| D::Error::custom(error.to_string()))?;
        Ok(payload)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DriftValidationError {
    message: String,
}

impl DriftValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

fn check_events(
    events: &[DriftSynthesizedEvent],
    allowed: &[DriftSynthesizedEvent],
) -> Result<(), DriftValidationError> {
    if events.len() > 1 {
        return Err(DriftValidationError::new(
            "at most one synthesized event is allowed",
        ));
    }
    if let Some(event) = events.first() {
        if !allowed.contains(event) {
            return Err(DriftValidationError::new(
                "synthesized event is not allowed for this drift kind",
            ));
        }
    }
    Ok(())
}

fn require_target(
    target: &DriftTarget,
    predicate: fn(&DriftTarget) -> bool,
    message: &'static str,
) -> Result<(), DriftValidationError> {
    if predicate(target) {
        Ok(())
    } else {
        Err(DriftValidationError::new(message))
    }
}

fn validate_pr_state(drift: &PrStateDivergedDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Pr { .. }),
        "pr_state_diverged target must be pr",
    )?;
    check_events(
        &drift.synthesized_events,
        &[
            DriftSynthesizedEvent::PrClosed,
            DriftSynthesizedEvent::PrMerged,
        ],
    )?;
    match &drift.actual {
        PrStateActual::Closed {} => {
            if drift.synthesized_events.first() == Some(&DriftSynthesizedEvent::PrMerged) {
                return Err(DriftValidationError::new(
                    "closed pull request cannot synthesize pr_merged",
                ));
            }
        }
        PrStateActual::Merged { merge_commit_sha } => {
            if drift.synthesized_events.first() == Some(&DriftSynthesizedEvent::PrClosed) {
                return Err(DriftValidationError::new(
                    "merged pull request cannot synthesize pr_closed",
                ));
            }
            if drift.synthesized_events.first() == Some(&DriftSynthesizedEvent::PrMerged)
                && merge_commit_sha.is_none()
            {
                return Err(DriftValidationError::new(
                    "pr_merged requires merge_commit_sha",
                ));
            }
        }
    }
    Ok(())
}

fn validate_head(drift: &HeadDivergedDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Pr { .. }),
        "head_diverged target must be pr",
    )?;
    if drift.expected.head_sha == drift.actual.head_sha {
        return Err(DriftValidationError::new(
            "head_diverged requires different expected and actual head_sha",
        ));
    }
    check_events(
        &drift.synthesized_events,
        &[DriftSynthesizedEvent::PrSynchronized],
    )?;
    if drift.synthesized_events.first() == Some(&DriftSynthesizedEvent::PrSynchronized)
        && drift.actual.pushed_by.is_null()
    {
        return Err(DriftValidationError::new(
            "pr_synchronized requires pushed_by",
        ));
    }
    Ok(())
}

fn validate_effect_missing(drift: &EffectMissingDrift) -> Result<(), DriftValidationError> {
    check_events(&drift.synthesized_events, &[])?;
    match (&drift.expected, &drift.actual) {
        (
            EffectMissingExpected::StatusComment { render_digest, .. },
            EffectMissingActual::ContentChanged {
                render_digest: actual_digest,
            },
        ) => {
            require_target(
                &drift.target,
                |target| matches!(target, DriftTarget::Pr { .. } | DriftTarget::Issue { .. }),
                "effect_missing status_comment target must be pr or issue",
            )?;
            if render_digest == actual_digest {
                return Err(DriftValidationError::new(
                    "content_changed render_digest must differ from expected",
                ));
            }
        }
        (EffectMissingExpected::StatusComment { .. }, EffectMissingActual::Missing {}) => {
            require_target(
                &drift.target,
                |target| matches!(target, DriftTarget::Pr { .. } | DriftTarget::Issue { .. }),
                "effect_missing status_comment target must be pr or issue",
            )?;
        }
        (EffectMissingExpected::Branch { .. }, EffectMissingActual::Missing {}) => {
            require_target(
                &drift.target,
                |target| matches!(target, DriftTarget::Branch { .. }),
                "effect_missing branch target must be branch",
            )?;
        }
        (EffectMissingExpected::PullRequest { .. }, EffectMissingActual::Missing {})
        | (EffectMissingExpected::PullRequest { .. }, EffectMissingActual::StateChanged { .. }) => {
            require_target(
                &drift.target,
                |target| matches!(target, DriftTarget::Pr { .. }),
                "effect_missing pull_request target must be pr",
            )?;
        }
        _ => {
            return Err(DriftValidationError::new(
                "effect_missing actual does not match expected effect",
            ));
        }
    }
    Ok(())
}

fn validate_orphan_effect(drift: &OrphanEffectDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Pr { .. } | DriftTarget::Issue { .. }),
        "orphan_effect target must be pr or issue",
    )?;
    check_events(&drift.synthesized_events, &[])?;
    Ok(())
}

fn validate_issue_state(drift: &IssueStateDivergedDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Issue { .. }),
        "issue_state_diverged target must be issue",
    )?;
    check_events(
        &drift.synthesized_events,
        &[
            DriftSynthesizedEvent::IssueClosed,
            DriftSynthesizedEvent::IssueReopened,
        ],
    )?;
    match &drift.actual {
        IssueStateActual::State { state, .. } => {
            let expected_event = match state {
                IssueState::Closed => DriftSynthesizedEvent::IssueClosed,
                IssueState::Open => DriftSynthesizedEvent::IssueReopened,
            };
            if let Some(event) = drift.synthesized_events.first() {
                if *event != expected_event {
                    return Err(DriftValidationError::new(
                        "issue_state_diverged event does not match actual state",
                    ));
                }
            }
        }
        IssueStateActual::Deleted {} | IssueStateActual::Transferred { .. } => {
            if !drift.synthesized_events.is_empty() {
                return Err(DriftValidationError::new(
                    "deleted or transferred issue cannot synthesize an event",
                ));
            }
        }
    }
    Ok(())
}

fn validate_review_state(drift: &ReviewStateDivergedDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Pr { .. }),
        "review_state_diverged target must be pr",
    )?;
    check_events(&drift.synthesized_events, &[])?;
    Ok(())
}

fn validate_merge_state(drift: &MergeStateDivergedDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Pr { .. }),
        "merge_state_diverged target must be pr",
    )?;
    if !drift.expected.in_merge_queue {
        return Err(DriftValidationError::new(
            "merge_state_diverged expected.in_merge_queue must be true",
        ));
    }
    if drift.actual.in_merge_queue {
        return Err(DriftValidationError::new(
            "merge_state_diverged actual.in_merge_queue must be false",
        ));
    }
    check_events(
        &drift.synthesized_events,
        &[DriftSynthesizedEvent::PrDequeued],
    )?;
    Ok(())
}

fn validate_default_branch(drift: &DefaultBranchChangedDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Repo { .. }),
        "default_branch_changed target must be repo",
    )?;
    check_events(&drift.synthesized_events, &[])?;
    if drift.expected.default_branch == drift.actual.default_branch {
        return Err(DriftValidationError::new(
            "default_branch_changed requires different default_branch values",
        ));
    }
    if let Some(renamed_from) = &drift.actual.renamed_from {
        if renamed_from != &drift.expected.default_branch {
            return Err(DriftValidationError::new(
                "renamed_from must equal the previous default branch",
            ));
        }
    }
    Ok(())
}

fn validate_installation(drift: &InstallationUnreachableDrift) -> Result<(), DriftValidationError> {
    require_target(
        &drift.target,
        |target| matches!(target, DriftTarget::Installation { .. }),
        "installation_unreachable target must be installation",
    )?;
    check_events(&drift.synthesized_events, &[])?;
    if !drift.expected.reachable {
        return Err(DriftValidationError::new(
            "installation_unreachable expected.reachable must be true",
        ));
    }
    if drift.actual.reachable {
        return Err(DriftValidationError::new(
            "installation_unreachable actual.reachable must be false",
        ));
    }
    Ok(())
}
