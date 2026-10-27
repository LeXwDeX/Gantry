//! data-model §1–§5 的实体、枚举、审批形状与预算台账算术。
//! 字段名与枚举值以 `docs/data-model.md` 为准；实体输出容忍未知字段。

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::canon::{GitOid, IdempotencyKey, RepoSlug, Sha256Digest, SubjectDigest, SubjectKind};
use crate::forge::ActionPayload;
use crate::ids::*;
use crate::level::Level;
use crate::time::{Date, Timestamp};

// ---------------------------------------------------------------------------
// 基础：事件序号、Actor、发起方
// ---------------------------------------------------------------------------

/// 全局事件游标；1..=2^53−1。配置 rev 允许 0，用 [`Seq::from_wire`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

impl Seq {
    pub const ZERO: Seq = Seq(0);
    pub const MAX: u64 = 9_007_199_254_740_991;

    pub fn new(value: u64) -> Result<Self, SeqError> {
        if value == 0 {
            return Err(SeqError::Zero);
        }
        if value > Self::MAX {
            return Err(SeqError::TooLarge);
        }
        Ok(Self(value))
    }

    pub fn from_wire(value: u64) -> Result<Self, SeqError> {
        if value > Self::MAX {
            return Err(SeqError::TooLarge);
        }
        Ok(Self(value))
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl Serialize for Seq {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for Seq {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        Self::from_wire(value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Seq {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Seq".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "integer",
            "minimum": 0,
            "maximum": 9_007_199_254_740_991u64,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SeqError {
    #[error("event seq must be at least 1")]
    Zero,
    #[error("seq exceeds 2^53-1")]
    TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    Engine,
    Guardian,
    User,
    Device,
    Forge,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
}

impl Actor {
    pub fn new(kind: ActorKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// 项目与交付
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub id: ProjectId,
    pub repo: RepoSlug,
    pub installation_id: i64,
    pub bot_user_id: i64,
    pub default_branch: String,
    /// 至少一项（与 `project.added` / `updated` 的校验一致，#70）。
    #[schemars(length(min = 1))]
    pub allowed_merge_methods: Vec<MergeMethod>,
    pub merge_queue_enabled: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Spec,
    Plan,
    Implement,
    Propose,
    Review,
    Merge,
    Closed,
    Abandoned,
}

impl Stage {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Abandoned)
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Spec => "spec",
            Self::Plan => "plan",
            Self::Implement => "implement",
            Self::Propose => "propose",
            Self::Review => "review",
            Self::Merge => "merge",
            Self::Closed => "closed",
            Self::Abandoned => "abandoned",
        };
        f.write_str(name)
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Running,
    WaitingHuman,
    Blocked,
    Paused,
    Done,
}

impl fmt::Display for StageStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Running => "running",
            Self::WaitingHuman => "waiting_human",
            Self::Blocked => "blocked",
            Self::Paused => "paused",
            Self::Done => "done",
        };
        f.write_str(name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOrigin {
    Conversation,
    GithubLabel,
    GithubCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueRef {
    pub repo: RepoSlug,
    pub number: u64,
    pub node_id: String,
    pub title: crate::text::UntrustedText,
    pub state: IssueState,
    #[serde(default)]
    pub state_reason: Option<String>,
    pub synced_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckSummary {
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    pub required_ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequestRef {
    pub repo: RepoSlug,
    pub number: u64,
    pub node_id: String,
    pub title: crate::text::UntrustedText,
    pub state: PullRequestState,
    pub head_sha: GitOid,
    pub base_ref: String,
    pub base_sha: GitOid,
    pub draft: bool,
    pub mergeable_state: String,
    pub in_merge_queue: bool,
    pub checks: CheckSummary,
    pub review_state: ReviewState,
    pub synced_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    None,
    Approved,
    ChangesRequested,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Delivery {
    pub id: DeliveryId,
    pub project_id: ProjectId,
    pub title: String,
    pub level_cap: Level,
    pub level_effective: Level,
    pub stage: Stage,
    pub stage_status: StageStatus,
    pub issues: Vec<IssueRef>,
    #[serde(default)]
    pub pr: Option<PullRequestRef>,
    #[serde(default)]
    pub worktree_id: Option<WorktreeId>,
    #[serde(default)]
    pub parent_id: Option<DeliveryId>,
    pub depends_on: Vec<DeliveryId>,
    pub origin: DeliveryOrigin,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

// ---------------------------------------------------------------------------
// 图、节点与尝试
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStatus {
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Workflow {
    pub id: WorkflowId,
    #[serde(default)]
    pub delivery_id: Option<DeliveryId>,
    #[serde(default)]
    pub conversation_id: Option<ConversationId>,
    #[serde(default)]
    pub stage: Option<Stage>,
    pub graph_rev: u32,
    pub status: WorkflowStatus,
    pub template: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Workflow {
    /// `delivery_id` 与 `conversation_id` 恰有一个非空；临时图的 `stage` 为空。
    pub fn validate(&self) -> Result<(), EntityError> {
        match (&self.delivery_id, &self.conversation_id) {
            (Some(_), None) => Ok(()),
            (None, Some(_)) if self.stage.is_none() => Ok(()),
            (None, Some(_)) => Err(EntityError::ConversationWorkflowHasStage),
            _ => Err(EntityError::WorkflowSubject),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    pub id: NodeId,
    pub workflow_id: WorkflowId,
    pub role: Role,
    pub tier: Tier,
    pub exec: Exec,
    pub depends_on: Vec<NodeId>,
    pub required: bool,
    pub status: NodeStatus,
    #[serde(default)]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub superseded_by: Option<NodeId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Node {
    pub fn validate(&self) -> Result<(), EntityError> {
        let (tier, exec) = role_profile(&self.role);
        if tier != self.tier || exec != self.exec {
            return Err(EntityError::RoleProfileMismatch);
        }
        if matches!(self.role, Role::Reasoner) && self.verdict.is_some() {
            return Err(EntityError::ReasonerHasVerdict);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Plan,
    Build,
    Verify,
    Single,
    Review(String),
    Reasoner,
    Arbiter,
    Summarize,
    Admission,
    Heal,
    Answer,
    Intent,
    Classify,
}

impl Role {
    pub fn as_str(&self) -> String {
        match self {
            Self::Plan => "plan".into(),
            Self::Build => "build".into(),
            Self::Verify => "verify".into(),
            Self::Single => "single".into(),
            Self::Review(view) => format!("review:{view}"),
            Self::Reasoner => "reasoner".into(),
            Self::Arbiter => "arbiter".into(),
            Self::Summarize => "summarize".into(),
            Self::Admission => "admission".into(),
            Self::Heal => "heal".into(),
            Self::Answer => "answer".into(),
            Self::Intent => "intent".into(),
            Self::Classify => "classify".into(),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl FromStr for Role {
    type Err = EntityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "plan" => Ok(Self::Plan),
            "build" => Ok(Self::Build),
            "verify" => Ok(Self::Verify),
            "single" => Ok(Self::Single),
            "reasoner" => Ok(Self::Reasoner),
            "arbiter" => Ok(Self::Arbiter),
            "summarize" => Ok(Self::Summarize),
            "admission" => Ok(Self::Admission),
            "heal" => Ok(Self::Heal),
            "answer" => Ok(Self::Answer),
            "intent" => Ok(Self::Intent),
            "classify" => Ok(Self::Classify),
            other => {
                let view = other
                    .strip_prefix("review:")
                    .ok_or(EntityError::UnknownRole)?;
                let mut chars = view.chars();
                let first = chars.next().ok_or(EntityError::UnknownRole)?;
                if !first.is_ascii_lowercase()
                    || !chars.all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
                    })
                {
                    return Err(EntityError::UnknownRole);
                }
                Ok(Self::Review(view.to_owned()))
            }
        }
    }
}

impl Serialize for Role {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Role {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Role".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(plan|build|verify|single|reasoner|arbiter|summarize|admission|heal|answer|intent|classify|review:[a-z][a-z0-9_-]*)$",
        })
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Utility,
    Planner,
    Worker,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Utility => "utility",
            Self::Planner => "planner",
            Self::Worker => "worker",
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Exec {
    Sandbox,
    SandboxRo,
    Direct,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum Verdict {
    #[serde(rename = "ACCEPT")]
    Accept,
    #[serde(rename = "REVISE")]
    Revise,
    #[serde(rename = "REJECT")]
    Reject,
    #[serde(rename = "BLOCKED")]
    Blocked,
}

/// architecture §9 的 role → (tier, exec) 固定映射。
pub fn role_profile(role: &Role) -> (Tier, Exec) {
    match role {
        Role::Plan => (Tier::Planner, Exec::SandboxRo),
        Role::Build | Role::Verify | Role::Single | Role::Heal => (Tier::Worker, Exec::Sandbox),
        Role::Review(_) | Role::Reasoner | Role::Arbiter | Role::Answer => {
            (Tier::Worker, Exec::SandboxRo)
        }
        Role::Summarize | Role::Admission | Role::Intent => (Tier::Planner, Exec::Direct),
        Role::Classify => (Tier::Utility, Exec::Direct),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Pending,
    Queued,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
    Superseded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttemptStatus {
    Preparing,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Timeout,
    ExecFailed,
    VerdictFail,
    PolicyDenied,
    Cancelled,
    BudgetExhausted,
    InfraError,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FailureDetail(String);

impl FailureDetail {
    pub fn parse(value: &str) -> Result<Self, EntityError> {
        if value.is_empty() || value.len() > 200 {
            return Err(EntityError::InvalidFailureDetail);
        }
        if !value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b':' | b'-')
        }) {
            return Err(EntityError::InvalidFailureDetail);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FailureDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for FailureDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FailureDetail").field(&self.0).finish()
    }
}

impl Serialize for FailureDetail {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FailureDetail {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for FailureDetail {
    type Err = EntityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl JsonSchema for FailureDetail {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FailureDetail".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[a-z0-9_.:-]{1,200}$",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub requests: u64,
}

impl TokenUsage {
    pub fn new(
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        requests: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            requests,
        }
    }

    /// 加权 token；四类 token 按权重求和后四舍五入（P-70）。
    pub fn weighted(&self, weights: &TokenWeights) -> u64 {
        weights.weighted(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attempt {
    pub id: AttemptId,
    pub node_id: NodeId,
    pub n: u32,
    #[serde(default)]
    pub sandbox_id: Option<SandboxId>,
    pub status: AttemptStatus,
    #[serde(default)]
    pub started_at: Option<Timestamp>,
    #[serde(default)]
    pub ended_at: Option<Timestamp>,
    #[serde(default)]
    pub result_ref: Option<Sha256Digest>,
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    #[serde(default)]
    pub failure_detail: Option<FailureDetail>,
    pub usage: TokenUsage,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

// ---------------------------------------------------------------------------
// 工作树与沙盒
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LockHolder {
    Engine,
    Guardian,
    Human,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeLock {
    pub holder: LockHolder,
    pub delivery_id: DeliveryId,
    pub acquired_at: Timestamp,
    pub lease_until: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Worktree {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub branch: String,
    pub base_ref: String,
    pub path: String,
    #[serde(default)]
    pub lock: Option<WorktreeLock>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxTrust {
    Trusted,
    Untrusted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxStatus {
    Created,
    Running,
    Stopped,
    Destroyed,
    Lost,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxLimits {
    pub cpu_cores: f64,
    pub mem_mb: u64,
    pub disk_mb: u64,
    pub pids: u32,
    pub wall_time_s: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeSnapshot {
    pub mirror: String,
    pub base_sha: GitOid,
    #[serde(default)]
    pub pr_base_sha: Option<GitOid>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Sandbox {
    pub id: SandboxId,
    pub driver: String,
    pub trust: SandboxTrust,
    pub image_digest: Sha256Digest,
    pub limits: SandboxLimits,
    pub status: SandboxStatus,
    pub worktree_snapshot: WorktreeSnapshot,
    pub egress_profile: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

// ---------------------------------------------------------------------------
// 会话、消息与卡片
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Conversation {
    pub id: ConversationId,
    pub project_id: ProjectId,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Agent,
    Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Planner,
    Worker,
    Utility,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MessageTone {
    Info,
    Ok,
    Run,
    Warn,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    ApprovalNeeded,
    DecisionNeeded,
    SpecNeeded,
    BudgetExhausted,
    Escalation,
    SecurityFinding,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Blocker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InboxStatus {
    Open,
    Resolved,
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ref {
    Issue { repo: RepoSlug, number: u64 },
    Pr { repo: RepoSlug, number: u64 },
    Worktree { id: WorktreeId },
    Delivery { id: DeliveryId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InboxOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub primary: Option<bool>,
}

impl InboxOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>, primary: bool) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            primary: Some(primary),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SummaryItem {
    pub text: crate::text::UntrustedText,
    #[serde(rename = "ref")]
    #[serde(default)]
    pub reference: Option<Ref>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Card {
    Plan {
        delivery_id: DeliveryId,
        workflow_id: WorkflowId,
        summary: crate::text::UntrustedText,
    },
    Approval {
        approval_id: ApprovalId,
    },
    Escalation {
        inbox_id: InboxId,
    },
    Choice {
        inbox_id: InboxId,
        options: Vec<InboxOption>,
    },
    Summary {
        delivery_id: Option<DeliveryId>,
        items: Vec<SummaryItem>,
    },
    /// 未知 kind：保留原始 JSON，客户端降级显示。
    Unknown(Value),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum KnownCard {
    Plan {
        delivery_id: DeliveryId,
        workflow_id: WorkflowId,
        summary: crate::text::UntrustedText,
    },
    Approval {
        approval_id: ApprovalId,
    },
    Escalation {
        inbox_id: InboxId,
    },
    Choice {
        inbox_id: InboxId,
        options: Vec<InboxOption>,
    },
    Summary {
        #[serde(default)]
        delivery_id: Option<DeliveryId>,
        items: Vec<SummaryItem>,
    },
}

impl Card {
    fn known(&self) -> Option<KnownCard> {
        match self {
            Self::Plan {
                delivery_id,
                workflow_id,
                summary,
            } => Some(KnownCard::Plan {
                delivery_id: *delivery_id,
                workflow_id: *workflow_id,
                summary: summary.clone(),
            }),
            Self::Approval { approval_id } => Some(KnownCard::Approval {
                approval_id: *approval_id,
            }),
            Self::Escalation { inbox_id } => Some(KnownCard::Escalation {
                inbox_id: *inbox_id,
            }),
            Self::Choice { inbox_id, options } => Some(KnownCard::Choice {
                inbox_id: *inbox_id,
                options: options.clone(),
            }),
            Self::Summary { delivery_id, items } => Some(KnownCard::Summary {
                delivery_id: *delivery_id,
                items: items.clone(),
            }),
            Self::Unknown(_) => None,
        }
    }

    fn from_known(known: KnownCard) -> Self {
        match known {
            KnownCard::Plan {
                delivery_id,
                workflow_id,
                summary,
            } => Self::Plan {
                delivery_id,
                workflow_id,
                summary,
            },
            KnownCard::Approval { approval_id } => Self::Approval { approval_id },
            KnownCard::Escalation { inbox_id } => Self::Escalation { inbox_id },
            KnownCard::Choice { inbox_id, options } => Self::Choice { inbox_id, options },
            KnownCard::Summary { delivery_id, items } => Self::Summary { delivery_id, items },
        }
    }
}

impl Serialize for Card {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.known() {
            Some(known) => known.serialize(serializer),
            None => match self {
                Self::Unknown(value) => value.serialize(serializer),
                _ => unreachable!(),
            },
        }
    }
}

impl<'de> Deserialize<'de> for Card {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let kind = match value.get("kind") {
            Some(Value::String(kind)) => kind.as_str(),
            Some(_) => return Err(serde::de::Error::custom("card `kind` must be a string")),
            None => return Err(serde::de::Error::missing_field("kind")),
        };
        if matches!(
            kind,
            "plan" | "approval" | "escalation" | "choice" | "summary"
        ) {
            reject_unknown_card_fields(kind, &value).map_err(serde::de::Error::custom)?;
            let known: KnownCard =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            Ok(Self::from_known(known))
        } else {
            Ok(Self::Unknown(value))
        }
    }
}

fn reject_unknown_card_fields(kind: &str, value: &Value) -> Result<(), String> {
    let allowed: &[&str] = match kind {
        "plan" => &["kind", "delivery_id", "workflow_id", "summary"],
        "approval" => &["kind", "approval_id"],
        "escalation" => &["kind", "inbox_id"],
        "choice" => &["kind", "inbox_id", "options"],
        "summary" => &["kind", "delivery_id", "items"],
        _ => return Ok(()),
    };
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown field `{key}` for card kind `{kind}`"));
        }
    }
    Ok(())
}

impl JsonSchema for Card {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Card".into()
    }

    /// 已知 kind 必须是严格形状；未知 kind 保留任意字段供客户端降级显示。
    /// 未知分支显式排除已定义 kind，因此已知 kind 的坏形状不会落进降级分支。
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let known = generator.subschema_for::<KnownCard>().to_value();
        if let Some(definition) = generator.definitions_mut().get_mut("KnownCard") {
            if let Some(branches) = definition.get_mut("oneOf").and_then(Value::as_array_mut) {
                for branch in branches {
                    branch["additionalProperties"] = Value::Bool(false);
                }
            }
        }
        let unknown = serde_json::json!({
            "type": "object",
            "required": ["kind"],
            "properties": {
                "kind": {
                    "type": "string",
                    "not": { "enum": ["plan", "approval", "escalation", "choice", "summary"] }
                }
            }
        });
        let mut schema = serde_json::Map::new();
        schema.insert("oneOf".to_owned(), serde_json::json!([known, unknown]));
        Schema::from(schema)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub role: MessageRole,
    #[serde(default)]
    pub agent_kind: Option<AgentKind>,
    pub text: String,
    #[serde(default)]
    pub tone: Option<MessageTone>,
    #[serde(default)]
    pub level_cap: Option<Level>,
    pub refs: Vec<Ref>,
    pub cards: Vec<Card>,
    #[serde(default)]
    pub delivery_id: Option<DeliveryId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

// ---------------------------------------------------------------------------
// 上报项
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InboxItem {
    pub id: InboxId,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub delivery_id: Option<DeliveryId>,
    pub kind: InboxKind,
    pub severity: Severity,
    pub summary: String,
    pub payload: InboxPayload,
    pub status: InboxStatus,
    pub dedup_key: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl InboxItem {
    pub fn validate(&self) -> Result<(), EntityError> {
        if self.kind != self.payload.kind() {
            return Err(EntityError::InboxPayloadKindMismatch);
        }
        if let InboxPayload::Escalation {
            reason_code,
            action_id,
            ..
        } = &self.payload
        {
            if reason_code.is_outbox_failed() != action_id.is_some() {
                return Err(EntityError::EscalationActionIdMismatch);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum InboxPayload {
    ApprovalNeeded {
        approval_id: ApprovalId,
    },
    DecisionNeeded {
        kind: DecisionKind,
        reason_code: ReasonCode,
        options: Vec<InboxOption>,
        #[serde(default)]
        node_id: Option<NodeId>,
        #[serde(default)]
        patch: Option<PatchRef>,
    },
    SpecNeeded {
        missing: Vec<MissingAspect>,
    },
    BudgetExhausted {
        dimension: BudgetDimension,
        limit: u64,
        used: u64,
        options: Vec<InboxOption>,
    },
    Escalation {
        reason_code: ReasonCode,
        #[serde(default)]
        node_id: Option<NodeId>,
        #[serde(default)]
        attempt_id: Option<AttemptId>,
        #[serde(default)]
        target_level: Option<Level>,
        /// 终态失败的 Outbox 动作；`reason_code = outbox_failed:<code>` 时必填，其余不得出现（#67）。
        #[serde(default)]
        action_id: Option<ActionId>,
        evidence: Vec<EvidenceRef>,
        #[serde(default)]
        options: Option<Vec<InboxOption>>,
    },
    SecurityFinding {
        source: Ref,
        finding: SecurityFinding,
        #[serde(default)]
        attempt_id: Option<AttemptId>,
    },
}

impl InboxPayload {
    pub fn kind(&self) -> InboxKind {
        match self {
            Self::ApprovalNeeded { .. } => InboxKind::ApprovalNeeded,
            Self::DecisionNeeded { .. } => InboxKind::DecisionNeeded,
            Self::SpecNeeded { .. } => InboxKind::SpecNeeded,
            Self::BudgetExhausted { .. } => InboxKind::BudgetExhausted,
            Self::Escalation { .. } => InboxKind::Escalation,
            Self::SecurityFinding { .. } => InboxKind::SecurityFinding,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    GateReject,
    ManualAction,
    ReviewDispute,
    Other,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ReasonCode(String);

impl ReasonCode {
    pub fn parse(value: &str) -> Result<Self, EntityError> {
        if value.is_empty() || value.len() > 200 {
            return Err(EntityError::InvalidReasonCode);
        }
        if !value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b':' | b'-')
        }) {
            return Err(EntityError::InvalidReasonCode);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `outbox_failed:<code>`：Outbox 动作终态失败的上报码族（engine.md §9）。
    pub fn is_outbox_failed(&self) -> bool {
        self.0
            .strip_prefix(OUTBOX_FAILED_PREFIX)
            .is_some_and(|code| !code.is_empty())
    }
}

pub const OUTBOX_FAILED_PREFIX: &str = "outbox_failed:";

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ReasonCode").field(&self.0).finish()
    }
}

impl Serialize for ReasonCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ReasonCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for ReasonCode {
    type Err = EntityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl JsonSchema for ReasonCode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ReasonCode".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[a-z0-9_.:-]{1,200}$",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchRef {
    #[serde(rename = "ref")]
    pub reference: Sha256Digest,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MissingAspect {
    pub aspect: String,
    pub question: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecurityFinding {
    pub severity: Severity,
    pub category: String,
    pub title: crate::text::UntrustedText,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceKind {
    Result,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum EvidenceRef {
    Ref(Ref),
    Result {
        kind: EvidenceKind,
        #[serde(rename = "ref")]
        reference: Sha256Digest,
    },
}

// ---------------------------------------------------------------------------
// 审批
// ---------------------------------------------------------------------------

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ForgeActionKind {
    PushBranch,
    CreatePr,
    UpdatePr,
    UpsertStatusComment,
    CreateComment,
    ReplyReviewThread,
    ResolveReviewThread,
    CreateIssue,
    UpdateIssue,
    CloseIssue,
    AddLabels,
    RequestReviewers,
    MergePr,
    RerunFailedJobs,
}

impl ForgeActionKind {
    pub const ALL: [Self; 14] = [
        Self::PushBranch,
        Self::CreatePr,
        Self::UpdatePr,
        Self::UpsertStatusComment,
        Self::CreateComment,
        Self::ReplyReviewThread,
        Self::ResolveReviewThread,
        Self::CreateIssue,
        Self::UpdateIssue,
        Self::CloseIssue,
        Self::AddLabels,
        Self::RequestReviewers,
        Self::MergePr,
        Self::RerunFailedJobs,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PushBranch => "push_branch",
            Self::CreatePr => "create_pr",
            Self::UpdatePr => "update_pr",
            Self::UpsertStatusComment => "upsert_status_comment",
            Self::CreateComment => "create_comment",
            Self::ReplyReviewThread => "reply_review_thread",
            Self::ResolveReviewThread => "resolve_review_thread",
            Self::CreateIssue => "create_issue",
            Self::UpdateIssue => "update_issue",
            Self::CloseIssue => "close_issue",
            Self::AddLabels => "add_labels",
            Self::RequestReviewers => "request_reviewers",
            Self::MergePr => "merge_pr",
            Self::RerunFailedJobs => "rerun_failed_jobs",
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PolicyActionId {
    PushBranch,
    CreatePr,
    UpdatePr,
    UpsertStatusComment,
    CreateComment,
    ReplyReviewThread,
    ResolveReviewThread,
    CreateIssue,
    UpdateIssue,
    CloseIssue,
    AddLabels,
    RequestReviewers,
    MergePr,
    RerunFailedJobs,
    ForcePush,
    PushOutsidePrefix,
    ResolveReviewThreadHuman,
    DeleteBranch,
    PublishRelease,
    ChangeRepoSettings,
}

impl PolicyActionId {
    pub const ALL: [Self; 20] = [
        Self::PushBranch,
        Self::CreatePr,
        Self::UpdatePr,
        Self::UpsertStatusComment,
        Self::CreateComment,
        Self::ReplyReviewThread,
        Self::ResolveReviewThread,
        Self::CreateIssue,
        Self::UpdateIssue,
        Self::CloseIssue,
        Self::AddLabels,
        Self::RequestReviewers,
        Self::MergePr,
        Self::RerunFailedJobs,
        Self::ForcePush,
        Self::PushOutsidePrefix,
        Self::ResolveReviewThreadHuman,
        Self::DeleteBranch,
        Self::PublishRelease,
        Self::ChangeRepoSettings,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PushBranch => "push_branch",
            Self::CreatePr => "create_pr",
            Self::UpdatePr => "update_pr",
            Self::UpsertStatusComment => "upsert_status_comment",
            Self::CreateComment => "create_comment",
            Self::ReplyReviewThread => "reply_review_thread",
            Self::ResolveReviewThread => "resolve_review_thread",
            Self::CreateIssue => "create_issue",
            Self::UpdateIssue => "update_issue",
            Self::CloseIssue => "close_issue",
            Self::AddLabels => "add_labels",
            Self::RequestReviewers => "request_reviewers",
            Self::MergePr => "merge_pr",
            Self::RerunFailedJobs => "rerun_failed_jobs",
            Self::ForcePush => "force_push",
            Self::PushOutsidePrefix => "push_outside_prefix",
            Self::ResolveReviewThreadHuman => "resolve_review_thread_human",
            Self::DeleteBranch => "delete_branch",
            Self::PublishRelease => "publish_release",
            Self::ChangeRepoSettings => "change_repo_settings",
        }
    }

    /// 前七个危险动作不允许 `auto`（P-42）。
    pub fn is_dangerous(self) -> bool {
        matches!(
            self,
            Self::MergePr
                | Self::ForcePush
                | Self::PushOutsidePrefix
                | Self::ResolveReviewThreadHuman
                | Self::DeleteBranch
                | Self::PublishRelease
                | Self::ChangeRepoSettings
        )
    }

    pub fn allows_auto(self) -> bool {
        !self.is_dangerous()
    }
}

impl PolicyActionId {
    pub fn parse_opt(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.as_str() == value)
    }
}

impl fmt::Display for PolicyActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PolicyActionId {
    type Err = EntityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_opt(s).ok_or(EntityError::UnknownPolicyAction)
    }
}

impl From<ForgeActionKind> for PolicyActionId {
    fn from(value: ForgeActionKind) -> Self {
        match value {
            ForgeActionKind::PushBranch => Self::PushBranch,
            ForgeActionKind::CreatePr => Self::CreatePr,
            ForgeActionKind::UpdatePr => Self::UpdatePr,
            ForgeActionKind::UpsertStatusComment => Self::UpsertStatusComment,
            ForgeActionKind::CreateComment => Self::CreateComment,
            ForgeActionKind::ReplyReviewThread => Self::ReplyReviewThread,
            ForgeActionKind::ResolveReviewThread => Self::ResolveReviewThread,
            ForgeActionKind::CreateIssue => Self::CreateIssue,
            ForgeActionKind::UpdateIssue => Self::UpdateIssue,
            ForgeActionKind::CloseIssue => Self::CloseIssue,
            ForgeActionKind::AddLabels => Self::AddLabels,
            ForgeActionKind::RequestReviewers => Self::RequestReviewers,
            ForgeActionKind::MergePr => Self::MergePr,
            ForgeActionKind::RerunFailedJobs => Self::RerunFailedJobs,
        }
    }
}

/// `Approval.action`：Forge 动作 ∪ 七个非 Forge 动作（P-54）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApprovalAction {
    Forge(ForgeActionKind),
    SettingsUpdate,
    DeviceRoleChange,
    DeviceRevoke,
    ForcePush,
    PushOutsidePrefix,
    ResolveReviewThreadHuman,
    PatchApplyProtected,
}

impl ApprovalAction {
    pub const ALL: [Self; 21] = [
        Self::Forge(ForgeActionKind::PushBranch),
        Self::Forge(ForgeActionKind::CreatePr),
        Self::Forge(ForgeActionKind::UpdatePr),
        Self::Forge(ForgeActionKind::UpsertStatusComment),
        Self::Forge(ForgeActionKind::CreateComment),
        Self::Forge(ForgeActionKind::ReplyReviewThread),
        Self::Forge(ForgeActionKind::ResolveReviewThread),
        Self::Forge(ForgeActionKind::CreateIssue),
        Self::Forge(ForgeActionKind::UpdateIssue),
        Self::Forge(ForgeActionKind::CloseIssue),
        Self::Forge(ForgeActionKind::AddLabels),
        Self::Forge(ForgeActionKind::RequestReviewers),
        Self::Forge(ForgeActionKind::MergePr),
        Self::Forge(ForgeActionKind::RerunFailedJobs),
        Self::ForcePush,
        Self::PushOutsidePrefix,
        Self::ResolveReviewThreadHuman,
        Self::SettingsUpdate,
        Self::DeviceRoleChange,
        Self::DeviceRevoke,
        Self::PatchApplyProtected,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forge(kind) => kind.as_str(),
            Self::SettingsUpdate => "settings_update",
            Self::DeviceRoleChange => "device_role_change",
            Self::DeviceRevoke => "device_revoke",
            Self::ForcePush => "force_push",
            Self::PushOutsidePrefix => "push_outside_prefix",
            Self::ResolveReviewThreadHuman => "resolve_review_thread_human",
            Self::PatchApplyProtected => "patch_apply_protected",
        }
    }

    /// 未在 data-model 形状表定义的动作返回 `None`，调用方必须 fail-closed。
    pub fn shape(self) -> Option<&'static ApprovalShape> {
        APPROVAL_SHAPES.iter().find(|shape| shape.action == self)
    }
}

impl fmt::Display for ApprovalAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApprovalAction {
    type Err = EntityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        for kind in ForgeActionKind::ALL {
            if kind.as_str() == value {
                return Ok(Self::Forge(kind));
            }
        }
        match value {
            "settings_update" => Ok(Self::SettingsUpdate),
            "device_role_change" => Ok(Self::DeviceRoleChange),
            "device_revoke" => Ok(Self::DeviceRevoke),
            "force_push" => Ok(Self::ForcePush),
            "push_outside_prefix" => Ok(Self::PushOutsidePrefix),
            "resolve_review_thread_human" => Ok(Self::ResolveReviewThreadHuman),
            "patch_apply_protected" => Ok(Self::PatchApplyProtected),
            _ => Err(EntityError::UnknownApprovalAction),
        }
    }
}

impl Serialize for ApprovalAction {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApprovalAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ApprovalAction {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ApprovalAction".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": [
                "push_branch", "create_pr", "update_pr", "upsert_status_comment", "create_comment",
                "reply_review_thread", "resolve_review_thread", "create_issue", "update_issue",
                "close_issue", "add_labels", "request_reviewers", "merge_pr", "rerun_failed_jobs",
                "force_push", "push_outside_prefix", "resolve_review_thread_human",
                "settings_update", "device_role_change", "device_revoke", "patch_apply_protected"
            ]
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SettingsScope {
    Global,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SettingsResource {
    Models,
    Levels,
    Policies,
    Guardian,
    Sandbox,
    Egress,
    Notify,
    Github,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalTargetKind {
    Pr,
    Branch,
    ReviewThread,
    Patch,
    Settings,
    Device,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[schemars(extend("allOf" = [
    {
        "if": { "properties": { "kind": { "const": "settings" } }, "required": ["kind"] },
        "then": { "oneOf": [
            {
                "properties": { "scope": { "const": "global" }, "project_id": { "type": "null" } },
                "required": ["scope"]
            },
            {
                "properties": { "scope": { "const": "project" }, "project_id": { "not": { "type": "null" } } },
                "required": ["scope", "project_id"]
            }
        ] }
    }
]))]
pub enum ApprovalTarget {
    Pr {
        repo: RepoSlug,
        number: u64,
    },
    Branch {
        repo: RepoSlug,
        branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_remote_oid: Option<GitOid>,
    },
    ReviewThread {
        repo: RepoSlug,
        number: u64,
        thread_node_id: String,
    },
    Patch {
        repo: RepoSlug,
        attempt_id: AttemptId,
        rules: Vec<String>,
        paths: Vec<String>,
    },
    Settings {
        scope: SettingsScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
        resource: SettingsResource,
        base_rev: Seq,
    },
    Device {
        id: DeviceId,
    },
}

impl ApprovalTarget {
    /// 运行时形状：settings target 的 `project_id` 必须与 `scope` 一致。
    /// global 省略 `project_id`；project 必须带非空 `project_id`。
    pub fn validate(&self) -> Result<(), ApprovalError> {
        if let Self::Settings {
            scope, project_id, ..
        } = self
        {
            match (scope, project_id) {
                (SettingsScope::Global, None) | (SettingsScope::Project, Some(_)) => {}
                _ => return Err(ApprovalError::SettingsScopeMismatch),
            }
        }
        Ok(())
    }

    pub fn kind(&self) -> ApprovalTargetKind {
        match self {
            Self::Pr { .. } => ApprovalTargetKind::Pr,
            Self::Branch { .. } => ApprovalTargetKind::Branch,
            Self::ReviewThread { .. } => ApprovalTargetKind::ReviewThread,
            Self::Patch { .. } => ApprovalTargetKind::Patch,
            Self::Settings { .. } => ApprovalTargetKind::Settings,
            Self::Device { .. } => ApprovalTargetKind::Device,
        }
    }
}

impl ApprovalTargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::Branch => "branch",
            Self::ReviewThread => "review_thread",
            Self::Patch => "patch",
            Self::Settings => "settings",
            Self::Device => "device",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalShape {
    pub action: ApprovalAction,
    pub target_kind: ApprovalTargetKind,
    pub subject_kind: SubjectKind,
}

/// data-model"审批动作的 target 与 subject_digest"表的逐行实现。
pub const APPROVAL_SHAPES: &[ApprovalShape] = &[
    ApprovalShape {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        target_kind: ApprovalTargetKind::Pr,
        subject_kind: SubjectKind::GitOid,
    },
    ApprovalShape {
        action: ApprovalAction::ForcePush,
        target_kind: ApprovalTargetKind::Branch,
        subject_kind: SubjectKind::GitOid,
    },
    ApprovalShape {
        action: ApprovalAction::PushOutsidePrefix,
        target_kind: ApprovalTargetKind::Branch,
        subject_kind: SubjectKind::GitOid,
    },
    ApprovalShape {
        action: ApprovalAction::ResolveReviewThreadHuman,
        target_kind: ApprovalTargetKind::ReviewThread,
        subject_kind: SubjectKind::Sha256,
    },
    ApprovalShape {
        action: ApprovalAction::PatchApplyProtected,
        target_kind: ApprovalTargetKind::Patch,
        subject_kind: SubjectKind::Sha256,
    },
    ApprovalShape {
        action: ApprovalAction::SettingsUpdate,
        target_kind: ApprovalTargetKind::Settings,
        subject_kind: SubjectKind::Sha256,
    },
    ApprovalShape {
        action: ApprovalAction::DeviceRoleChange,
        target_kind: ApprovalTargetKind::Device,
        subject_kind: SubjectKind::Sha256,
    },
    ApprovalShape {
        action: ApprovalAction::DeviceRevoke,
        target_kind: ApprovalTargetKind::Device,
        subject_kind: SubjectKind::Sha256,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    Pending,
    Granted,
    Consumed,
    Invalidated,
    Expired,
    Denied,
}

/// 只用于生成 JSON Schema 的字段镜像；语义与 [`Approval`] 相同。
#[derive(JsonSchema)]
#[allow(dead_code)]
struct ApprovalFields {
    id: ApprovalId,
    #[serde(default)]
    delivery_id: Option<DeliveryId>,
    action: ApprovalAction,
    target: ApprovalTarget,
    subject_digest: SubjectDigest,
    #[serde(default)]
    device_id: Option<DeviceId>,
    #[serde(default)]
    signature: Option<String>,
    status: ApprovalStatus,
    expires_at: Timestamp,
    #[serde(default)]
    consumed_by: Option<String>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// 把一个只用于生成 schema 的字段镜像内联进当前根 schema；定义随后从 `$defs` 移除。
pub(crate) fn inline_fields<T: JsonSchema>(
    generator: &mut schemars::SchemaGenerator,
) -> serde_json::Map<String, Value> {
    let name = T::schema_name().to_string();
    generator.subschema_for::<T>();
    generator
        .definitions_mut()
        .remove(&name)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// `APPROVAL_SHAPES` 的 schema 投影：每个合法动作一支，联动 target.kind 与 subject 摘要格式。
pub(crate) fn approval_shape_branches() -> Vec<Value> {
    APPROVAL_SHAPES
        .iter()
        .map(|shape| {
            let subject_pattern = match shape.subject_kind {
                SubjectKind::GitOid => "^[0-9a-f]{40}$",
                SubjectKind::Sha256 => "^sha256:[0-9a-f]{64}$",
            };
            serde_json::json!({
                "properties": {
                    "action": { "const": shape.action.as_str() },
                    "target": {
                        "properties": { "kind": { "const": shape.target_kind.as_str() } },
                        "required": ["kind"]
                    },
                    "subject_digest": { "type": "string", "pattern": subject_pattern }
                },
                "required": ["action", "target", "subject_digest"]
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId,
    #[serde(default)]
    pub delivery_id: Option<DeliveryId>,
    pub action: ApprovalAction,
    pub target: ApprovalTarget,
    pub subject_digest: SubjectDigest,
    #[serde(default)]
    pub device_id: Option<DeviceId>,
    #[serde(default)]
    pub signature: Option<String>,
    pub status: ApprovalStatus,
    pub expires_at: Timestamp,
    #[serde(default)]
    pub consumed_by: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl JsonSchema for Approval {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Approval".into()
    }

    /// 字段形状来自镜像类型；`oneOf` 只接受 `APPROVAL_SHAPES` 定义的动作，
    /// 并联动 target.kind 与 subject_digest。未定义形状的动作 fail-closed。
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let mut fields = inline_fields::<ApprovalFields>(generator);
        fields.remove("description");
        fields.insert("title".to_owned(), Value::String("Approval".to_owned()));
        fields.insert("oneOf".to_owned(), Value::Array(approval_shape_branches()));
        Schema::from(fields)
    }
}

impl Approval {
    /// 动作必须有定义形状，target 与 subject 必须匹配；否则 fail-closed。
    pub fn validate(&self) -> Result<(), ApprovalError> {
        let shape = self.action.shape().ok_or(ApprovalError::ShapeUndefined)?;
        if shape.target_kind != self.target.kind() {
            return Err(ApprovalError::TargetMismatch);
        }
        self.target.validate()?;
        if shape.subject_kind != self.subject_digest.kind() {
            return Err(ApprovalError::SubjectMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval action has no defined shape")]
    ShapeUndefined,
    #[error("approval target does not match the action shape")]
    TargetMismatch,
    #[error("approval subject digest does not match the action shape")]
    SubjectMismatch,
    #[error("settings approval target project_id does not match its scope")]
    SettingsScopeMismatch,
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    High,
    Normal,
    Low,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutboxError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExpectedEffect {
    pub event: String,
    #[serde(rename = "match")]
    pub match_: BTreeMap<String, Value>,
    #[serde(default)]
    pub matched_webhook_deliveries: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GithubBot {
    pub github_id: i64,
    pub login: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GithubPrRef {
    pub number: u64,
    pub node_id: String,
    pub head_sha: GitOid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GithubRef {
    #[serde(default)]
    pub bot: Option<GithubBot>,
    #[serde(default)]
    pub pr: Option<GithubPrRef>,
    #[serde(default)]
    pub marker: Option<String>,
    #[serde(default)]
    pub recorded_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutboxStatus {
    Queued,
    Executing,
    Succeeded,
    Failed,
    Cancelled,
}

/// 把 `kind` 常量与对应的 payload schema 组合成顶层 `oneOf`。
///
/// 顶层不设 `additionalProperties: false`：实体输出仍容忍未知字段。每个分支要求
/// `kind` 常量与 payload 子 schema 同时成立，因此 kind/payload 不匹配会被拒绝。
pub(crate) fn tagged_payload_schema(
    generator: &mut schemars::SchemaGenerator,
    base: Schema,
    description: &str,
) -> Schema {
    let base = serde_json::to_value(&base).expect("schema is serializable");
    let variants: Vec<Value> = ActionPayload::schema_variants(generator)
        .into_iter()
        .map(|(kind, payload)| {
            let payload = serde_json::to_value(&payload).expect("schema is serializable");
            serde_json::json!({
                "allOf": [
                    base.clone(),
                    {
                        "type": "object",
                        "properties": {
                            "kind": { "const": kind },
                            "payload": payload,
                        },
                        "required": ["kind", "payload"],
                    }
                ]
            })
        })
        .collect();
    Schema::try_from(serde_json::json!({
        "oneOf": variants,
        "description": description,
    }))
    .expect("generated schema is valid")
}

/// 仅用于生成 `OutboxAction` schema 的公共字段；不参与 serde。
#[derive(JsonSchema)]
#[allow(dead_code)]
struct OutboxActionBase {
    id: ActionId,
    project_id: ProjectId,
    delivery_id: DeliveryId,
    idempotency_key: IdempotencyKey,
    requires_approval: Option<ApprovalId>,
    depends_on: Vec<ActionId>,
    priority: Priority,
    not_before: Option<Timestamp>,
    epoch: u32,
    status: OutboxStatus,
    attempts: u32,
    lease_until: Option<Timestamp>,
    last_error: Option<OutboxError>,
    expected_effects: Vec<ExpectedEffect>,
    github_ref: Option<GithubRef>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl JsonSchema for OutboxAction {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "OutboxAction".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let base = generator.subschema_for::<OutboxActionBase>();
        tagged_payload_schema(
            generator,
            base,
            "待执行的 GitHub 写动作；`kind` 与 `payload` 在线上并列，`payload` 按 `kind` 强类型解码。",
        )
    }
}

/// 待执行的 GitHub 写动作；`kind` 与 `payload` 在线上并列，`payload` 按 `kind` 强类型解码。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawOutboxAction")]
pub struct OutboxAction {
    pub id: ActionId,
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub idempotency_key: IdempotencyKey,
    pub kind: ForgeActionKind,
    /// 线上仍是裸 payload 对象；解码按 `kind` 选择强类型变体，未知字段被拒绝。
    pub payload: ActionPayload,
    #[serde(default)]
    pub requires_approval: Option<ApprovalId>,
    pub depends_on: Vec<ActionId>,
    pub priority: Priority,
    #[serde(default)]
    pub not_before: Option<Timestamp>,
    pub epoch: u32,
    pub status: OutboxStatus,
    pub attempts: u32,
    #[serde(default)]
    pub lease_until: Option<Timestamp>,
    #[serde(default)]
    pub last_error: Option<OutboxError>,
    pub expected_effects: Vec<ExpectedEffect>,
    #[serde(default)]
    pub github_ref: Option<GithubRef>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

// 与 `OutboxAction` 同形，但 `payload` 先保持原始 JSON 以便按 `kind` 严格解码。
#[derive(Deserialize)]
struct RawOutboxAction {
    id: ActionId,
    project_id: ProjectId,
    delivery_id: DeliveryId,
    idempotency_key: IdempotencyKey,
    kind: ForgeActionKind,
    payload: Value,
    #[serde(default)]
    requires_approval: Option<ApprovalId>,
    depends_on: Vec<ActionId>,
    priority: Priority,
    #[serde(default)]
    not_before: Option<Timestamp>,
    epoch: u32,
    status: OutboxStatus,
    attempts: u32,
    #[serde(default)]
    lease_until: Option<Timestamp>,
    #[serde(default)]
    last_error: Option<OutboxError>,
    expected_effects: Vec<ExpectedEffect>,
    #[serde(default)]
    github_ref: Option<GithubRef>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum OutboxActionError {
    #[error("invalid {kind:?} payload: {source}")]
    Payload {
        kind: ForgeActionKind,
        source: serde_json::Error,
    },
    #[error("outbox payload kind does not match action kind")]
    KindMismatch,
}

impl OutboxAction {
    /// 新建排队中的动作；`kind` 始终由 payload 派生，避免不一致。
    pub fn new(
        id: ActionId,
        project_id: ProjectId,
        delivery_id: DeliveryId,
        idempotency_key: IdempotencyKey,
        payload: ActionPayload,
        created_at: Timestamp,
    ) -> Self {
        Self {
            id,
            project_id,
            delivery_id,
            idempotency_key,
            kind: payload.kind(),
            payload,
            requires_approval: None,
            depends_on: Vec::new(),
            priority: Priority::Normal,
            not_before: None,
            epoch: 0,
            status: OutboxStatus::Queued,
            attempts: 0,
            lease_until: None,
            last_error: None,
            expected_effects: Vec::new(),
            github_ref: None,
            created_at,
            updated_at: created_at,
        }
    }

    pub fn validate(&self) -> Result<(), OutboxActionError> {
        if self.payload.kind() != self.kind {
            return Err(OutboxActionError::KindMismatch);
        }
        Ok(())
    }
}

impl TryFrom<RawOutboxAction> for OutboxAction {
    type Error = OutboxActionError;

    fn try_from(raw: RawOutboxAction) -> Result<Self, Self::Error> {
        let payload = ActionPayload::from_value(raw.kind, raw.payload).map_err(|source| {
            OutboxActionError::Payload {
                kind: raw.kind,
                source,
            }
        })?;
        let action = Self {
            id: raw.id,
            project_id: raw.project_id,
            delivery_id: raw.delivery_id,
            idempotency_key: raw.idempotency_key,
            kind: raw.kind,
            payload,
            requires_approval: raw.requires_approval,
            depends_on: raw.depends_on,
            priority: raw.priority,
            not_before: raw.not_before,
            epoch: raw.epoch,
            status: raw.status,
            attempts: raw.attempts,
            lease_until: raw.lease_until,
            last_error: raw.last_error,
            expected_effects: raw.expected_effects,
            github_ref: raw.github_ref,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        };
        action.validate()?;
        Ok(action)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForgeEventKind {
    PrOpened,
    PrReopened,
    PrSynchronized,
    PrUpdated,
    PrClosed,
    PrMerged,
    BaseAdvanced,
    PrEnqueued,
    PrDequeued,
    MergeGroupChecksRequested,
    MergeGroupDestroyed,
    CheckSuiteCompleted,
    CheckRunCompleted,
    ReviewSubmitted,
    ReviewDismissed,
    ReviewComment,
    ReviewThreadChanged,
    IssueComment,
    IssueClosed,
    IssueReopened,
    IssueUpdated,
    InstallationChanged,
    RepoChanged,
}

impl ForgeEventKind {
    pub fn parse_opt(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    pub const ALL: [Self; 23] = [
        Self::PrOpened,
        Self::PrReopened,
        Self::PrSynchronized,
        Self::PrUpdated,
        Self::PrClosed,
        Self::PrMerged,
        Self::BaseAdvanced,
        Self::PrEnqueued,
        Self::PrDequeued,
        Self::MergeGroupChecksRequested,
        Self::MergeGroupDestroyed,
        Self::CheckSuiteCompleted,
        Self::CheckRunCompleted,
        Self::ReviewSubmitted,
        Self::ReviewDismissed,
        Self::ReviewComment,
        Self::ReviewThreadChanged,
        Self::IssueComment,
        Self::IssueClosed,
        Self::IssueReopened,
        Self::IssueUpdated,
        Self::InstallationChanged,
        Self::RepoChanged,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrOpened => "pr_opened",
            Self::PrReopened => "pr_reopened",
            Self::PrSynchronized => "pr_synchronized",
            Self::PrUpdated => "pr_updated",
            Self::PrClosed => "pr_closed",
            Self::PrMerged => "pr_merged",
            Self::BaseAdvanced => "base_advanced",
            Self::PrEnqueued => "pr_enqueued",
            Self::PrDequeued => "pr_dequeued",
            Self::MergeGroupChecksRequested => "merge_group_checks_requested",
            Self::MergeGroupDestroyed => "merge_group_destroyed",
            Self::CheckSuiteCompleted => "check_suite_completed",
            Self::CheckRunCompleted => "check_run_completed",
            Self::ReviewSubmitted => "review_submitted",
            Self::ReviewDismissed => "review_dismissed",
            Self::ReviewComment => "review_comment",
            Self::ReviewThreadChanged => "review_thread_changed",
            Self::IssueComment => "issue_comment",
            Self::IssueClosed => "issue_closed",
            Self::IssueReopened => "issue_reopened",
            Self::IssueUpdated => "issue_updated",
            Self::InstallationChanged => "installation_changed",
            Self::RepoChanged => "repo_changed",
        }
    }
}

impl fmt::Display for ForgeEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ForgeEventKind {
    type Err = EntityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_opt(s).ok_or(EntityError::UnknownForgeEventKind)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ForgeOrigin {
    #[serde(rename = "self")]
    Own,
    #[serde(rename = "self_unattributed")]
    OwnUnattributed,
    #[serde(rename = "external")]
    External,
}

// ---------------------------------------------------------------------------
// 设备
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DevicePlatform {
    Windows,
    Macos,
    Ios,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum DeviceRole {
    Viewer,
    Operator,
    Approver,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    Active,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub platform: DevicePlatform,
    pub cert_serial: String,
    pub pubkey_fpr: Sha256Digest,
    #[serde(default)]
    pub approval_pubkey: Option<String>,
    #[serde(default)]
    pub approval_pubkey_fpr: Option<Sha256Digest>,
    pub role: DeviceRole,
    pub status: DeviceStatus,
    #[serde(default)]
    pub last_seen: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

// ---------------------------------------------------------------------------
// 预算台账
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokenWeights {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Default for TokenWeights {
    fn default() -> Self {
        Self {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: 1.25,
        }
    }
}

impl TokenWeights {
    pub fn weighted(&self, usage: &TokenUsage) -> u64 {
        let terms = [
            (usage.input_tokens, self.input),
            (usage.output_tokens, self.output),
            (usage.cache_read_tokens, self.cache_read),
            (usage.cache_write_tokens, self.cache_write),
        ];
        // 正常范围内仍按总和四舍五入；只有总和溢出时才逐项饱和，避免 inf 归零。
        let total: f64 = terms
            .iter()
            .map(|(tokens, weight)| *tokens as f64 * *weight)
            .sum();
        if total.is_finite() && total <= u64::MAX as f64 {
            return if total <= 0.0 {
                0
            } else {
                total.round() as u64
            };
        }
        terms.iter().fold(0u64, |acc, (tokens, weight)| {
            let term = *tokens as f64 * *weight;
            let term = if term.is_nan() || term <= 0.0 {
                0
            } else if term >= u64::MAX as f64 {
                u64::MAX
            } else {
                term.round() as u64
            };
            acc.saturating_add(term)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BudgetSubject {
    Delivery,
    Conversation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetDimension {
    TokensByTier(Tier),
    Nodes,
    Rounds,
    WallTime,
    Concurrency,
    ForgeWrites,
}

impl BudgetDimension {
    pub fn tokens(tier: Tier) -> Self {
        Self::TokensByTier(tier)
    }

    pub fn as_str(self) -> String {
        match self {
            Self::TokensByTier(tier) => format!("tokens_by_tier.{}", tier.as_str()),
            Self::Nodes => "nodes".into(),
            Self::Rounds => "rounds".into(),
            Self::WallTime => "wall_time".into(),
            Self::Concurrency => "concurrency".into(),
            Self::ForgeWrites => "forge_writes".into(),
        }
    }

    pub fn tier(self) -> Option<Tier> {
        match self {
            Self::TokensByTier(tier) => Some(tier),
            _ => None,
        }
    }
}

impl fmt::Display for BudgetDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl Serialize for BudgetDimension {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for BudgetDimension {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "nodes" => Ok(Self::Nodes),
            "rounds" => Ok(Self::Rounds),
            "wall_time" => Ok(Self::WallTime),
            "concurrency" => Ok(Self::Concurrency),
            "forge_writes" => Ok(Self::ForgeWrites),
            other => {
                let tier = other
                    .strip_prefix("tokens_by_tier.")
                    .ok_or_else(|| serde::de::Error::custom("unknown budget dimension"))?;
                let tier = match tier {
                    "utility" => Tier::Utility,
                    "planner" => Tier::Planner,
                    "worker" => Tier::Worker,
                    _ => return Err(serde::de::Error::custom("unknown budget tier")),
                };
                Ok(Self::TokensByTier(tier))
            }
        }
    }
}

impl JsonSchema for BudgetDimension {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BudgetDimension".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": ["tokens_by_tier.utility", "tokens_by_tier.planner", "tokens_by_tier.worker",
                     "nodes", "rounds", "wall_time", "concurrency", "forge_writes"]
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetLimits {
    pub tokens_by_tier: BTreeMap<Tier, u64>,
    /// 会话台账的每日上限（`levels.none.daily_tokens`）；交付台账为 `null`。
    #[serde(default)]
    pub tokens: Option<u64>,
    pub nodes: u64,
    pub rounds: u64,
    pub wall_time: u64,
    pub concurrency: u64,
    pub forge_writes: u64,
}

impl BudgetLimits {
    pub fn delivery(
        tokens_by_tier: BTreeMap<Tier, u64>,
        nodes: u64,
        rounds: u64,
        wall_time: u64,
        concurrency: u64,
        forge_writes: u64,
    ) -> Self {
        Self {
            tokens_by_tier,
            tokens: None,
            nodes,
            rounds,
            wall_time,
            concurrency,
            forge_writes,
        }
    }

    pub fn conversation(daily_tokens: u64) -> Self {
        Self {
            tokens_by_tier: BTreeMap::new(),
            tokens: Some(daily_tokens),
            nodes: 0,
            rounds: 0,
            wall_time: 0,
            concurrency: 0,
            forge_writes: 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetUsed {
    pub tokens_by_tier: BTreeMap<Tier, u64>,
    pub nodes: u64,
    pub attempts: u64,
    pub rounds: u64,
    pub wall_time: u64,
    pub forge_writes: u64,
}

impl BudgetUsed {
    pub fn total_tokens(&self) -> u64 {
        self.tokens_by_tier
            .values()
            .fold(0u64, |acc, value| acc.saturating_add(*value))
    }

    fn add_tokens(&mut self, tier: Tier, amount: u64) {
        let entry = self.tokens_by_tier.entry(tier).or_default();
        *entry = entry.saturating_add(amount);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetDelta {
    pub tokens: TokenUsage,
    pub tier: Tier,
    pub nodes: u32,
    pub attempts: u32,
    pub rounds: u32,
    pub wall_time_s: u64,
    pub forge_writes: u32,
}

impl Default for BudgetDelta {
    fn default() -> Self {
        Self {
            tokens: TokenUsage::default(),
            tier: Tier::Utility,
            nodes: 0,
            attempts: 0,
            rounds: 0,
            wall_time_s: 0,
            forge_writes: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerSubject {
    Delivery(DeliveryId),
    Conversation {
        conversation_id: ConversationId,
        day: Date,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetLedger {
    pub id: BudgetId,
    /// 线上形态为 `"delivery"` / `"conversation"` 字符串。
    pub subject: BudgetSubject,
    #[serde(default)]
    pub delivery_id: Option<DeliveryId>,
    #[serde(default)]
    pub conversation_id: Option<ConversationId>,
    pub project_id: ProjectId,
    #[serde(default)]
    pub day: Option<Date>,
    #[serde(default)]
    pub template: Option<String>,
    pub limits: BudgetLimits,
    pub used: BudgetUsed,
    #[serde(default)]
    pub config_rev: Option<Seq>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl BudgetLedger {
    /// P-74：交付台账用 `dlv_…`；会话台账用 `cnv_…:YYYY-MM-DD`。
    pub fn ledger_id(&self) -> String {
        match self.subject {
            BudgetSubject::Delivery => self
                .delivery_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            BudgetSubject::Conversation => match (&self.conversation_id, &self.day) {
                (Some(id), Some(day)) => format!("{id}:{day}"),
                _ => String::new(),
            },
        }
    }

    pub fn subject(&self) -> Option<LedgerSubject> {
        match self.subject {
            BudgetSubject::Delivery => self.delivery_id.map(LedgerSubject::Delivery),
            BudgetSubject::Conversation => match (self.conversation_id, self.day) {
                (Some(conversation_id), Some(day)) => Some(LedgerSubject::Conversation {
                    conversation_id,
                    day,
                }),
                _ => None,
            },
        }
    }

    pub fn validate(&self) -> Result<(), EntityError> {
        match self.subject {
            BudgetSubject::Delivery => {
                if self.delivery_id.is_none()
                    || self.conversation_id.is_some()
                    || self.day.is_some()
                    || self.limits.tokens.is_some()
                {
                    return Err(EntityError::BudgetSubject);
                }
            }
            BudgetSubject::Conversation => {
                if self.conversation_id.is_none()
                    || self.delivery_id.is_some()
                    || self.day.is_none()
                    || self.template.is_some()
                {
                    return Err(EntityError::BudgetSubject);
                }
                if self.limits.tokens.is_none() {
                    return Err(EntityError::BudgetSubject);
                }
            }
        }
        Ok(())
    }

    /// 累计一次用量，返回本次新变为"已耗尽"的维度。
    pub fn apply(&mut self, delta: &BudgetDelta, weights: &TokenWeights) -> Vec<BudgetDimension> {
        let dimensions = self.relevant_dimensions(delta.tier);
        let before: Vec<bool> = dimensions
            .iter()
            .map(|d| self.is_exhausted(*d, weights))
            .collect();
        let weighted = delta.tokens.weighted(weights);
        self.used.add_tokens(delta.tier, weighted);
        if self.subject == BudgetSubject::Delivery {
            self.used.nodes = self.used.nodes.saturating_add(delta.nodes as u64);
            self.used.attempts = self.used.attempts.saturating_add(delta.attempts as u64);
            self.used.rounds = self.used.rounds.saturating_add(delta.rounds as u64);
            self.used.wall_time = self.used.wall_time.saturating_add(delta.wall_time_s);
            self.used.forge_writes = self
                .used
                .forge_writes
                .saturating_add(delta.forge_writes as u64);
        }
        dimensions
            .iter()
            .zip(before)
            .filter(|(dimension, was_exhausted)| {
                !*was_exhausted && self.is_exhausted(**dimension, weights)
            })
            .map(|(dimension, _)| *dimension)
            .collect()
    }

    /// `used + delta > limit` 的维度。
    pub fn would_exceed(
        &self,
        delta: &BudgetDelta,
        weights: &TokenWeights,
    ) -> Vec<BudgetDimension> {
        let weighted = delta.tokens.weighted(weights);
        let mut result = Vec::new();
        for dimension in self.relevant_dimensions(delta.tier) {
            let exceeds = match dimension {
                BudgetDimension::TokensByTier(tier) => {
                    let used = if self.subject == BudgetSubject::Conversation {
                        self.used.total_tokens()
                    } else {
                        self.used.tokens_by_tier.get(&tier).copied().unwrap_or(0)
                    };
                    let planned =
                        if self.subject == BudgetSubject::Conversation || tier == delta.tier {
                            used.saturating_add(weighted)
                        } else {
                            used
                        };
                    planned > self.token_limit(tier)
                }
                BudgetDimension::Nodes => {
                    self.used.nodes.saturating_add(delta.nodes as u64) > self.limits.nodes
                }
                BudgetDimension::Rounds => {
                    self.used.rounds.saturating_add(delta.rounds as u64) > self.limits.rounds
                }
                BudgetDimension::WallTime => {
                    self.used.wall_time.saturating_add(delta.wall_time_s) > self.limits.wall_time
                }
                BudgetDimension::Concurrency => false,
                BudgetDimension::ForgeWrites => {
                    self.used
                        .forge_writes
                        .saturating_add(delta.forge_writes as u64)
                        > self.limits.forge_writes
                }
            };
            if exceeds {
                result.push(dimension);
            }
        }
        result
    }

    pub fn is_exhausted(&self, dimension: BudgetDimension, _weights: &TokenWeights) -> bool {
        match dimension {
            BudgetDimension::TokensByTier(tier) => {
                let used = if self.subject == BudgetSubject::Conversation {
                    self.used.total_tokens()
                } else {
                    self.used.tokens_by_tier.get(&tier).copied().unwrap_or(0)
                };
                used >= self.token_limit(tier)
            }
            BudgetDimension::Nodes => {
                self.subject == BudgetSubject::Delivery && self.used.nodes >= self.limits.nodes
            }
            BudgetDimension::Rounds => {
                self.subject == BudgetSubject::Delivery && self.used.rounds >= self.limits.rounds
            }
            BudgetDimension::WallTime => {
                self.subject == BudgetSubject::Delivery
                    && self.used.wall_time >= self.limits.wall_time
            }
            BudgetDimension::Concurrency => false,
            BudgetDimension::ForgeWrites => {
                self.subject == BudgetSubject::Delivery
                    && self.used.forge_writes >= self.limits.forge_writes
            }
        }
    }

    fn token_limit(&self, tier: Tier) -> u64 {
        if self.subject == BudgetSubject::Conversation {
            self.limits.tokens.unwrap_or(0)
        } else {
            self.limits.tokens_by_tier.get(&tier).copied().unwrap_or(0)
        }
    }

    fn relevant_dimensions(&self, tier: Tier) -> Vec<BudgetDimension> {
        let tokens = [
            BudgetDimension::TokensByTier(Tier::Utility),
            BudgetDimension::TokensByTier(Tier::Planner),
            BudgetDimension::TokensByTier(Tier::Worker),
        ];
        if self.subject == BudgetSubject::Conversation {
            vec![BudgetDimension::TokensByTier(tier)]
        } else {
            let mut dimensions = tokens.to_vec();
            dimensions.extend([
                BudgetDimension::Nodes,
                BudgetDimension::Rounds,
                BudgetDimension::WallTime,
                BudgetDimension::ForgeWrites,
            ]);
            dimensions
        }
    }
}

// ---------------------------------------------------------------------------
// 实体错误
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntityError {
    #[error("workflow must have exactly one of delivery_id or conversation_id")]
    WorkflowSubject,
    #[error("conversation workflow must have stage = null")]
    ConversationWorkflowHasStage,
    #[error("node role/tier/exec do not match the fixed role profile")]
    RoleProfileMismatch,
    #[error("reasoner is not a gate node and must not carry a verdict")]
    ReasonerHasVerdict,
    #[error("unknown node role")]
    UnknownRole,
    #[error("failure_detail must be a non-empty lowercase subcode")]
    InvalidFailureDetail,
    #[error("reason_code must be a non-empty lowercase code")]
    InvalidReasonCode,
    #[error("unknown approval action")]
    UnknownApprovalAction,
    #[error("unknown policy action id")]
    UnknownPolicyAction,
    #[error("unknown forge event kind")]
    UnknownForgeEventKind,
    #[error("inbox payload does not match inbox kind")]
    InboxPayloadKindMismatch,
    #[error(
        "escalation action_id must be present exactly when reason_code is outbox_failed:<code>"
    )]
    EscalationActionIdMismatch,
    #[error("budget ledger subject and id columns are inconsistent")]
    BudgetSubject,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_review_roundtrip_and_profile() {
        let role: Role = "review:security".parse().unwrap();
        assert_eq!(role.to_string(), "review:security");
        assert_eq!(role_profile(&role), (Tier::Worker, Exec::SandboxRo));
        assert!("review:Security".parse::<Role>().is_err());
    }

    #[test]
    fn approval_action_shape_fail_closed() {
        assert!(ApprovalAction::Forge(ForgeActionKind::MergePr)
            .shape()
            .is_some());
        assert!(ApprovalAction::Forge(ForgeActionKind::CreatePr)
            .shape()
            .is_none());
        assert_eq!(ApprovalAction::ALL.len(), 21);
        assert_eq!(ForgeActionKind::ALL.len(), 14);
        assert_eq!(PolicyActionId::ALL.len(), 20);
        assert_eq!(ForgeEventKind::ALL.len(), 23);
        assert!(!PolicyActionId::MergePr.allows_auto());
        assert!(PolicyActionId::CreatePr.allows_auto());
    }

    #[test]
    fn card_unknown_is_preserved() {
        let raw = serde_json::json!({"kind":"future_card","x":1});
        let card: Card = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(card).unwrap(), raw);
    }

    #[test]
    fn budget_arithmetic_and_exhaustion() {
        let mut ledger = BudgetLedger {
            id: "bdg_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap(),
            subject: BudgetSubject::Delivery,
            delivery_id: Some("dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM".parse().unwrap()),
            conversation_id: None,
            project_id: "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK".parse().unwrap(),
            day: None,
            template: Some("low.single".into()),
            limits: BudgetLimits::delivery(BTreeMap::from([(Tier::Worker, 300)]), 2, 1, 100, 1, 2),
            used: BudgetUsed::default(),
            config_rev: Some(Seq::new(1).unwrap()),
            created_at: Timestamp::from_millis(0),
            updated_at: Timestamp::from_millis(0),
        };
        let weights = TokenWeights::default();
        let delta = BudgetDelta {
            tokens: TokenUsage::new(100, 10, 0, 0, 1),
            tier: Tier::Worker,
            nodes: 1,
            attempts: 1,
            rounds: 0,
            wall_time_s: 30,
            forge_writes: 1,
        };
        assert!(ledger.apply(&delta, &weights).is_empty());
        assert_eq!(ledger.used.tokens_by_tier.get(&Tier::Worker), Some(&150));
        assert!(!ledger.is_exhausted(BudgetDimension::Nodes, &weights));
        let more = BudgetDelta {
            nodes: 1,
            ..delta.clone()
        };
        assert_eq!(
            ledger.apply(&more, &weights),
            vec![
                BudgetDimension::TokensByTier(Tier::Worker),
                BudgetDimension::Nodes,
                BudgetDimension::ForgeWrites,
            ]
        );
    }

    #[test]
    fn budget_boundary_does_not_exceed_but_is_exhausted() {
        let ledger = BudgetLedger {
            id: "bdg_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap(),
            subject: BudgetSubject::Conversation,
            delivery_id: None,
            conversation_id: Some("cnv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap()),
            project_id: "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK".parse().unwrap(),
            day: Some("2026-10-24".parse().unwrap()),
            template: None,
            limits: BudgetLimits::conversation(100),
            used: BudgetUsed::default(),
            config_rev: None,
            created_at: Timestamp::from_millis(0),
            updated_at: Timestamp::from_millis(0),
        };
        let weights = TokenWeights::default();
        let delta = BudgetDelta {
            tokens: TokenUsage::new(80, 4, 0, 0, 1),
            tier: Tier::Utility,
            ..BudgetDelta::default()
        };
        assert!(ledger.would_exceed(&delta, &weights).is_empty());
        let mut ledger = ledger;
        assert_eq!(
            ledger.apply(&delta, &weights),
            vec![BudgetDimension::TokensByTier(Tier::Utility)]
        );
        assert!(ledger.is_exhausted(BudgetDimension::TokensByTier(Tier::Utility), &weights));
        assert_eq!(
            ledger.ledger_id(),
            "cnv_01JB7M4P8R2X6V9N3Q5T7W1YZB:2026-10-24"
        );
    }
}
