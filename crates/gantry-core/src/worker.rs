//! Worker 协议类型（`gantry-worker/1`）：JobSpec、result.json、进度事件、proposals、
//! 各 role 的 structured_output、ResultBundle、ValidatedPatch 与 PatchFacts。
//!
//! 权威来源：`docs/interfaces/worker-protocol.md` §4–§8，`docs/modules/repo.md` §6.3。
//! 沙盒交回的一切都是不可信输入；本模块只定义形状与本地一致性校验，不做 IO。

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::canon::{GitOid, RepoSlug, Sha256Digest};
use crate::entities::{
    role_profile, Exec, FailureClass, FailureDetail, Role, Seq, Tier, TokenUsage, Verdict,
};
use crate::ids::{AttemptId, DeliveryId, NodeId, ProjectId, WorkflowId};
use crate::level::{Effort, Level};
use crate::time::Timestamp;

pub const PROTOCOL_VERSION: &str = "gantry-worker/1";
pub const PROGRESS_EVENT_VERSION: u8 = 1;

/// 按完整 ID 返回内置结构化输出 schema 的原始字节。
pub fn output_schema(id: &str) -> Option<&'static [u8]> {
    match id {
        "gantry.worker.output.plan/1" => Some(include_bytes!(
            "../../../schemas/worker/output.plan.schema.json"
        )),
        "gantry.worker.output.build/1" => Some(include_bytes!(
            "../../../schemas/worker/output.build.schema.json"
        )),
        "gantry.worker.output.verify/1" => Some(include_bytes!(
            "../../../schemas/worker/output.verify.schema.json"
        )),
        "gantry.worker.output.review/1" => Some(include_bytes!(
            "../../../schemas/worker/output.review.schema.json"
        )),
        "gantry.worker.output.reasoner/1" => Some(include_bytes!(
            "../../../schemas/worker/output.reasoner.schema.json"
        )),
        "gantry.worker.output.arbiter/1" => Some(include_bytes!(
            "../../../schemas/worker/output.arbiter.schema.json"
        )),
        "gantry.worker.output.answer/1" => Some(include_bytes!(
            "../../../schemas/worker/output.answer.schema.json"
        )),
        "gantry.worker.output.summarize/1" => Some(include_bytes!(
            "../../../schemas/worker/output.summarize.schema.json"
        )),
        "gantry.worker.output.admission/1" => Some(include_bytes!(
            "../../../schemas/worker/output.admission.schema.json"
        )),
        "gantry.worker.output.intent/1" => Some(include_bytes!(
            "../../../schemas/worker/output.intent.schema.json"
        )),
        "gantry.worker.output.classify/1" => Some(include_bytes!(
            "../../../schemas/worker/output.classify.schema.json"
        )),
        _ => None,
    }
}

/// `protocol` 字段固定 `gantry-worker/1`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerProtocol;

impl WorkerProtocol {
    pub const VALUE: &'static str = PROTOCOL_VERSION;
}

impl Serialize for WorkerProtocol {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(Self::VALUE)
    }
}

impl<'de> Deserialize<'de> for WorkerProtocol {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == Self::VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(
                "unsupported worker protocol version",
            ))
        }
    }
}

impl JsonSchema for WorkerProtocol {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "WorkerProtocol".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "const": PROTOCOL_VERSION,
        })
    }
}

/// agent 标识；新增适配层只追加取值（worker-protocol §10.3）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AgentId {
    ClaudeCode,
    Other(String),
}

impl AgentId {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Other(value) => value,
        }
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentId {
    type Err = WorkerError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "claude-code" {
            return Ok(Self::ClaudeCode);
        }
        if value.is_empty() || value.len() > 64 {
            return Err(WorkerError::InvalidAgentId);
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
        {
            return Err(WorkerError::InvalidAgentId);
        }
        Ok(Self::Other(value.to_owned()))
    }
}

impl Serialize for AgentId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for AgentId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AgentId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[a-z][a-z0-9_-]{0,63}$",
        })
    }
}

// ---------------------------------------------------------------------------
// JobSpec（worker-protocol §4）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobWorkflow {
    pub id: WorkflowId,
    pub graph_rev: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobNode {
    pub id: NodeId,
    pub role: Role,
    pub exec: Exec,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobAttempt {
    pub n: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreviousAttempt {
    pub attempt_id: AttemptId,
    pub failure_class: FailureClass,
    #[serde(default)]
    pub failure_detail: Option<FailureDetail>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobContext {
    pub level_effective: Level,
    pub revision_round: u32,
    #[serde(default)]
    pub previous_attempts: Vec<PreviousAttempt>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobExecution {
    pub agent: AgentId,
    pub tier: Tier,
    pub model: String,
    pub reasoning_effort: Effort,
    /// agent 专有、不透明；适配层按白名单校验，只能收紧 constraints。
    #[serde(default)]
    pub agent_options: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobTask {
    pub title: String,
    pub instructions: String,
    #[serde(default)]
    pub output_language: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobWorkspace {
    pub path: String,
    pub repo: RepoSlug,
    pub base_sha: GitOid,
    #[serde(default)]
    pub pr_base_sha: Option<GitOid>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobIsolation {
    /// 32 位十六进制；每个 job 随机。
    pub nonce: String,
    pub begin: String,
    pub end: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaterialKind {
    IssueBody,
    IssueComment,
    PrBody,
    PrComment,
    ReviewComment,
    ReviewThread,
    CiLog,
    CommitMessage,
    DependencyDoc,
    FileExcerpt,
    UpstreamOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MaterialSource {
    pub repo: RepoSlug,
    pub number: u64,
    #[serde(default)]
    pub item_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobMaterial {
    pub id: String,
    pub kind: MaterialKind,
    pub source: MaterialSource,
    pub path: String,
    pub sha256: Sha256Digest,
    pub bytes: u64,
    pub truncated: bool,
    pub original_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpstreamRef {
    pub node_id: NodeId,
    pub role: Role,
    pub attempt_id: AttemptId,
    pub dir: String,
    pub files: Vec<String>,
    #[serde(rename = "ref")]
    pub reference: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    ReviewThread,
    Issue,
    Pr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobTarget {
    pub handle: String,
    pub kind: TargetKind,
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolConstraints {
    pub edit: bool,
    pub shell: bool,
}

/// JobSpec 的路径规则快照；同时是 `PatchFacts.job_paths` 的类型。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathConstraints {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    #[serde(default)]
    pub approval_required: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobLimits {
    pub wall_time_s: u64,
    pub idle_timeout_s: u64,
    pub max_turns: u32,
    pub tokens_total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchLimits {
    pub max_bytes: u64,
    pub max_files: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobConstraints {
    pub tools: ToolConstraints,
    pub paths: PathConstraints,
    pub limits: JobLimits,
    /// `output.patch` 不是 `forbidden` 时必填。
    #[serde(default)]
    pub patch: Option<PatchLimits>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PatchMode {
    Required,
    Optional,
    Forbidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportMode {
    Required,
    Optional,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StructuredSchema {
    /// `gantry.worker.output.<schema>/<major>`。
    pub id: String,
    pub path: String,
    pub sha256: Sha256Digest,
}

impl StructuredSchema {
    pub fn validate(&self) -> Result<(), WorkerError> {
        let Some(rest) = self.id.strip_prefix("gantry.worker.output.") else {
            return Err(WorkerError::InvalidStructuredSchemaId);
        };
        let Some((name, major)) = rest.rsplit_once('/') else {
            return Err(WorkerError::InvalidStructuredSchemaId);
        };
        if name.is_empty() || major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
            return Err(WorkerError::InvalidStructuredSchemaId);
        }
        Ok(())
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    OpenPr,
    ReplyReviewThread,
    ResolveReviewThread,
    Escalate,
}

impl ProposalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenPr => "open_pr",
            Self::ReplyReviewThread => "reply_review_thread",
            Self::ResolveReviewThread => "resolve_review_thread",
            Self::Escalate => "escalate",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalsMode {
    Forbidden,
    Allowed {
        allowed_kinds: Vec<ProposalKind>,
        max: u32,
    },
}

impl Serialize for ProposalsMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            Self::Forbidden => serializer.serialize_str("forbidden"),
            Self::Allowed { allowed_kinds, max } => {
                let mut state = serializer.serialize_struct("ProposalsMode", 2)?;
                state.serialize_field("allowed_kinds", allowed_kinds)?;
                state.serialize_field("max", max)?;
                state.end()
            }
        }
    }
}

impl JsonSchema for ProposalsMode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProposalsMode".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let kinds = generator.subschema_for::<ProposalKind>();
        schemars::json_schema!({
            "oneOf": [
                {
                    "type": "string",
                    "const": "forbidden"
                },
                {
                    "type": "object",
                    "properties": {
                        "allowed_kinds": {
                            "type": "array",
                            "items": kinds
                        },
                        "max": {
                            "type": "integer",
                            "format": "uint32",
                            "minimum": 0
                        }
                    },
                    "required": ["allowed_kinds", "max"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

impl<'de> Deserialize<'de> for ProposalsMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) if text == "forbidden" => Ok(Self::Forbidden),
            other => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Allowed {
                    allowed_kinds: Vec<ProposalKind>,
                    max: u32,
                }
                let allowed: Allowed =
                    serde_json::from_value(other).map_err(serde::de::Error::custom)?;
                Ok(Self::Allowed {
                    allowed_kinds: allowed.allowed_kinds,
                    max: allowed.max,
                })
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobOutput {
    pub structured_schema: StructuredSchema,
    pub patch: PatchMode,
    pub report: ReportMode,
    pub proposals: ProposalsMode,
}

/// `/gantry/job.json`。顶层与多数对象容忍未知字段；`constraints.*` 与 `output.*` 严格
/// （worker-protocol §4.2 的兼容规则）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobSpec {
    pub protocol: WorkerProtocol,
    pub job_id: AttemptId,
    pub created_at: Timestamp,
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub workflow: JobWorkflow,
    pub node: JobNode,
    pub attempt: JobAttempt,
    pub context: JobContext,
    pub execution: JobExecution,
    pub task: JobTask,
    pub workspace: JobWorkspace,
    pub isolation: JobIsolation,
    #[serde(default)]
    pub materials: Vec<JobMaterial>,
    #[serde(default)]
    pub upstream: Vec<UpstreamRef>,
    #[serde(default)]
    pub targets: Vec<JobTarget>,
    pub constraints: JobConstraints,
    pub output: JobOutput,
}

impl JobSpec {
    /// 本地一致性：role / exec 固定搭配、沙盒只读时禁止补丁、`direct` 不进沙盒。
    pub fn validate(&self) -> Result<(), WorkerError> {
        let (expected_tier, expected_exec) = role_profile(&self.node.role);
        if expected_tier != self.execution.tier {
            return Err(WorkerError::RoleProfileMismatch);
        }
        if expected_exec != self.node.exec {
            return Err(WorkerError::RoleProfileMismatch);
        }
        if self.node.exec == Exec::Direct {
            return Err(WorkerError::DirectRoleNotSandboxed);
        }
        if self.node.exec == Exec::SandboxRo {
            if self.output.patch != PatchMode::Forbidden {
                return Err(WorkerError::ReadOnlyJobWithPatch);
            }
            if self.constraints.tools.edit {
                return Err(WorkerError::ReadOnlyJobWithEdit);
            }
        }
        if self.output.patch != PatchMode::Forbidden && self.constraints.patch.is_none() {
            return Err(WorkerError::MissingPatchLimits);
        }
        if self.attempt.n == 0 {
            return Err(WorkerError::InvalidAttemptNumber);
        }
        if self.isolation.nonce.len() != 32
            || !self
                .isolation
                .nonce
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(WorkerError::InvalidIsolationNonce);
        }
        if self.workspace.path != "/gantry/work" {
            return Err(WorkerError::InvalidWorkspacePath);
        }
        self.output.structured_schema.validate()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// result.json（worker-protocol §5.2）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultErrorKind {
    TaskFailed,
    AgentError,
    MaxTurns,
    Budget,
    OutputInvalid,
    BadJob,
    Aborted,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultError {
    pub kind: ResultErrorKind,
    pub message: String,
    #[serde(default)]
    pub retryable_hint: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactRef {
    pub path: String,
    pub sha256: Sha256Digest,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchArtifact {
    pub path: String,
    pub sha256: Sha256Digest,
    pub bytes: u64,
    pub base_sha: GitOid,
    pub files_changed: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultArtifacts {
    #[serde(default)]
    pub patch: Option<PatchArtifact>,
    #[serde(default)]
    pub report: Option<ArtifactRef>,
    #[serde(default)]
    pub proposals: Option<ArtifactRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InjectionFlag {
    pub material_id: String,
    pub note: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultFlags {
    #[serde(default)]
    pub suspected_injection: Vec<InjectionFlag>,
}

/// worker 自报用量；只用于展示，不记账。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageReported {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub turns: u64,
    pub tool_calls: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentReport {
    pub kind: AgentId,
    pub version: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResultJson {
    pub protocol: WorkerProtocol,
    pub job_id: AttemptId,
    pub job_sha256: Sha256Digest,
    pub status: ResultStatus,
    pub summary: String,
    #[serde(default)]
    pub error: Option<ResultError>,
    #[serde(default)]
    pub structured_output: Option<StructuredOutput>,
    pub artifacts: ResultArtifacts,
    #[serde(default)]
    pub flags: ResultFlags,
    pub usage_reported: UsageReported,
    pub agent: AgentReport,
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
}

impl ResultJson {
    /// `succeeded` 必有 `structured_output`；`failed` / `cancelled` 必有 `error`。
    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.summary.chars().count() > 1000 {
            return Err(WorkerError::SummaryTooLong);
        }
        match self.status {
            ResultStatus::Succeeded => {
                if self.structured_output.is_none() {
                    return Err(WorkerError::MissingStructuredOutput);
                }
            }
            ResultStatus::Failed | ResultStatus::Cancelled => {
                if self.error.is_none() {
                    return Err(WorkerError::MissingError);
                }
            }
        }
        Ok(())
    }

    pub fn structured_output_kind(&self) -> Option<StructuredOutputKind> {
        self.structured_output.as_ref().map(StructuredOutput::kind)
    }
}

// ---------------------------------------------------------------------------
// structured_output（worker-protocol §6）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputKind {
    Plan,
    Build,
    Verify,
    Review,
    Reasoner,
    Arbiter,
    Admission,
    Summarize,
    Intent,
    Answer,
    Classify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockingReasonCode {
    MissingInfo,
    EnvUnavailable,
    ConflictingInputs,
    OutOfScope,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlockingReason {
    pub code: BlockingReasonCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Blocker,
    Major,
    Minor,
    Nit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    Security,
    Correctness,
    Reliability,
    Performance,
    Maintainability,
    Test,
    Docs,
    Style,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FindingSide {
    Head,
    Base,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FindingLocation {
    pub path: String,
    #[serde(default)]
    pub line_start: Option<u32>,
    #[serde(default)]
    pub line_end: Option<u32>,
    pub side: FindingSide,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    pub id: String,
    pub severity: FindingSeverity,
    pub category: FindingCategory,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub location: Option<FindingLocation>,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub suggested_fix: Option<String>,
    pub confidence: Confidence,
    pub in_scope: bool,
}

/// gate 公共字段（`verify` / `review` / `arbiter`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GateFields {
    pub verdict: Verdict,
    pub rationale: String,
    #[serde(default)]
    pub blocking_reasons: Vec<BlockingReason>,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanRiskSeverity {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub verification: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanRisk {
    pub severity: PlanRiskSeverity,
    pub description: String,
    pub mitigation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlockingQuestion {
    pub question: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanOutput {
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub risks: Vec<PlanRisk>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub blocking_questions: Vec<BlockingQuestion>,
    #[serde(default)]
    pub touched_paths_estimate: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Done,
    Partial,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepStatusEntry {
    pub step_id: String,
    pub status: StepStatus,
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Pass,
    Fail,
    Error,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckRunEntry {
    pub command: String,
    pub exit_code: i32,
    pub outcome: CheckOutcome,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BuildOutput {
    pub changes_summary: String,
    #[serde(default)]
    pub steps_status: Vec<StepStatusEntry>,
    #[serde(default)]
    pub checks_run: Vec<CheckRunEntry>,
    #[serde(default)]
    pub known_issues: Vec<String>,
    #[serde(default)]
    pub no_change_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerifyCheck {
    pub name: String,
    pub command: String,
    pub exit_code: i32,
    pub outcome: CheckOutcome,
    pub duration_s: u64,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanCoverage {
    pub step_id: String,
    pub satisfied: bool,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerifyOutput {
    #[serde(flatten)]
    pub gate: GateFields,
    pub checks: Vec<VerifyCheck>,
    #[serde(default)]
    pub plan_coverage: Vec<PlanCoverage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewOutput {
    pub perspective: String,
    #[serde(flatten)]
    pub gate: GateFields,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PredictedOutcome {
    Ok,
    Fail,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailurePoint {
    pub location: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReasonerScenario {
    pub id: String,
    pub description: String,
    pub path: Vec<String>,
    pub predicted_outcome: PredictedOutcome,
    pub likelihood: Confidence,
    #[serde(default)]
    pub failure_points: Vec<FailurePoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanAdjustment {
    pub step_id: String,
    pub change: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReasonerOutput {
    pub scenarios: Vec<ReasonerScenario>,
    #[serde(default)]
    pub plan_adjustments: Vec<PlanAdjustment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FindingRef {
    pub node_id: NodeId,
    pub finding_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArbiterRuling {
    Upheld,
    Dismissed,
    Downgraded,
    Duplicate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArbiterRulingEntry {
    pub finding_ref: FindingRef,
    pub ruling: ArbiterRuling,
    pub severity_final: FindingSeverity,
    #[serde(default)]
    pub duplicate_of: Option<FindingRef>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredChange {
    pub id: String,
    pub description: String,
    pub finding_refs: Vec<FindingRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EscalationFlag {
    pub needed: bool,
    #[serde(default)]
    pub reason_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArbiterOutput {
    #[serde(flatten)]
    pub gate: GateFields,
    pub round: u32,
    pub rulings: Vec<ArbiterRulingEntry>,
    #[serde(default)]
    pub required_changes: Vec<RequiredChange>,
    pub pr_summary: String,
    pub escalate: EscalationFlag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissingAspect {
    AcceptanceCriteria,
    Scope,
    Constraints,
    ReproSteps,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MissingItem {
    pub aspect: MissingAspect,
    pub question: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RefinedSpec {
    pub title: String,
    pub body_markdown: String,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdmissionOutput {
    pub verdict: Verdict,
    #[serde(default)]
    pub blocking_reasons: Vec<BlockingReason>,
    #[serde(default)]
    pub missing: Vec<MissingItem>,
    #[serde(default)]
    pub refined_spec: Option<RefinedSpec>,
    #[serde(default)]
    pub risk_flags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SummaryFinding {
    #[serde(flatten)]
    pub finding: Finding,
    pub source_refs: Vec<FindingRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SummarizeOutput {
    pub text: String,
    #[serde(default)]
    pub findings: Vec<SummaryFinding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    Task,
    Query,
    Control,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IntentOutput {
    pub intent: IntentKind,
    #[serde(default)]
    pub delivery_id: Option<DeliveryId>,
    pub needs_code: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnswerRef {
    pub path: String,
    #[serde(default)]
    pub line_start: Option<u32>,
    #[serde(default)]
    pub line_end: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnswerOutput {
    pub answer_markdown: String,
    #[serde(default)]
    pub refs: Vec<AnswerRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyTarget {
    CiClassification,
    EventLine,
    StatusAnswer,
    CommitMessage,
    StatusChecklist,
    Notification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CiCategory {
    Lint,
    Type,
    Test,
    Build,
    Infra,
    Flaky,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CiClassification {
    pub category: CiCategory,
    pub in_pr_scope: bool,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClassifyOutput {
    pub target: ClassifyTarget,
    pub text: String,
    #[serde(default)]
    pub classification: Option<CiClassification>,
}

/// `structured_output.kind` 与 `node.role` 对应（`review:<视角>` 对应 `review`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructuredOutput {
    Plan(PlanOutput),
    Build(BuildOutput),
    Verify(VerifyOutput),
    Review(ReviewOutput),
    Reasoner(ReasonerOutput),
    Arbiter(ArbiterOutput),
    Admission(AdmissionOutput),
    Summarize(SummarizeOutput),
    Intent(IntentOutput),
    Answer(AnswerOutput),
    Classify(ClassifyOutput),
}

impl StructuredOutput {
    pub fn kind(&self) -> StructuredOutputKind {
        match self {
            Self::Plan(_) => StructuredOutputKind::Plan,
            Self::Build(_) => StructuredOutputKind::Build,
            Self::Verify(_) => StructuredOutputKind::Verify,
            Self::Review(_) => StructuredOutputKind::Review,
            Self::Reasoner(_) => StructuredOutputKind::Reasoner,
            Self::Arbiter(_) => StructuredOutputKind::Arbiter,
            Self::Admission(_) => StructuredOutputKind::Admission,
            Self::Summarize(_) => StructuredOutputKind::Summarize,
            Self::Intent(_) => StructuredOutputKind::Intent,
            Self::Answer(_) => StructuredOutputKind::Answer,
            Self::Classify(_) => StructuredOutputKind::Classify,
        }
    }

    /// `role` → 期望的 `structured_output.kind`（worker-protocol §4.3）。
    pub fn expected_for_role(role: &Role) -> StructuredOutputKind {
        match role {
            Role::Plan => StructuredOutputKind::Plan,
            Role::Build | Role::Single | Role::Heal => StructuredOutputKind::Build,
            Role::Verify => StructuredOutputKind::Verify,
            Role::Review(_) => StructuredOutputKind::Review,
            Role::Reasoner => StructuredOutputKind::Reasoner,
            Role::Arbiter => StructuredOutputKind::Arbiter,
            Role::Admission => StructuredOutputKind::Admission,
            Role::Summarize => StructuredOutputKind::Summarize,
            Role::Intent => StructuredOutputKind::Intent,
            Role::Answer => StructuredOutputKind::Answer,
            Role::Classify => StructuredOutputKind::Classify,
        }
    }

    /// gate 一致性规则（worker-protocol §6.1）；违反即 schema 无效。
    ///
    /// `findings[]` 对 `review` / `verify` 必填、对 `arbiter` 可选；因此 arbiter 的
    /// REVISE / REJECT 以 `rulings` 非空为据（§6.3 的 arbiter 示例即如此）。
    pub fn validate(&self) -> Result<(), WorkerError> {
        match self {
            Self::Verify(output) => validate_gate(&output.gate, true),
            Self::Review(output) => validate_gate(&output.gate, true),
            Self::Arbiter(output) => {
                if matches!(output.gate.verdict, Verdict::Revise | Verdict::Reject)
                    && output.rulings.is_empty()
                {
                    return Err(WorkerError::GateInconsistent);
                }
                validate_gate(&output.gate, false)
            }
            Self::Admission(output) => {
                if output.verdict == Verdict::Blocked && output.blocking_reasons.is_empty() {
                    return Err(WorkerError::GateInconsistent);
                }
                if output.verdict == Verdict::Revise && output.missing.is_empty() {
                    return Err(WorkerError::GateInconsistent);
                }
                Ok(())
            }
            Self::Classify(output) => {
                if output.target == ClassifyTarget::CiClassification
                    && output.classification.is_none()
                {
                    return Err(WorkerError::ClassifyMissingClassification);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// proposals.json（worker-protocol §5.4）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EscalateInboxKind {
    DecisionNeeded,
    SpecNeeded,
    SecurityFinding,
    Escalation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenPrParams {
    pub title: String,
    pub body: String,
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyReviewThreadParams {
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveReviewThreadParams {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalateParams {
    pub inbox_kind: EscalateInboxKind,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalParams {
    OpenPr(OpenPrParams),
    ReplyReviewThread(ReplyReviewThreadParams),
    ResolveReviewThread(ResolveReviewThreadParams),
    Escalate(EscalateParams),
}

impl ProposalParams {
    pub fn kind(&self) -> ProposalKind {
        match self {
            Self::OpenPr(_) => ProposalKind::OpenPr,
            Self::ReplyReviewThread(_) => ProposalKind::ReplyReviewThread,
            Self::ResolveReviewThread(_) => ProposalKind::ResolveReviewThread,
            Self::Escalate(_) => ProposalKind::Escalate,
        }
    }

    fn from_value(kind: ProposalKind, value: Value) -> Result<Self, serde_json::Error> {
        Ok(match kind {
            ProposalKind::OpenPr => Self::OpenPr(serde_json::from_value(value)?),
            ProposalKind::ReplyReviewThread => {
                Self::ReplyReviewThread(serde_json::from_value(value)?)
            }
            ProposalKind::ResolveReviewThread => {
                Self::ResolveReviewThread(serde_json::from_value(value)?)
            }
            ProposalKind::Escalate => Self::Escalate(serde_json::from_value(value)?),
        })
    }

    fn to_value(&self) -> Result<Value, serde_json::Error> {
        match self {
            Self::OpenPr(params) => serde_json::to_value(params),
            Self::ReplyReviewThread(params) => serde_json::to_value(params),
            Self::ResolveReviewThread(params) => serde_json::to_value(params),
            Self::Escalate(params) => serde_json::to_value(params),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub id: String,
    pub kind: ProposalKind,
    pub target: Option<String>,
    pub params: ProposalParams,
    pub rationale: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProposal {
    id: String,
    kind: ProposalKind,
    target: Option<String>,
    params: Value,
    rationale: String,
}

impl Proposal {
    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.params.kind() != self.kind {
            return Err(WorkerError::ProposalKindMismatch);
        }
        if matches!(
            self.kind,
            ProposalKind::ReplyReviewThread | ProposalKind::ResolveReviewThread
        ) && self.target.is_none()
        {
            return Err(WorkerError::ProposalMissingTarget);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Proposal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawProposal::deserialize(deserializer)?;
        let params = ProposalParams::from_value(raw.kind, raw.params).map_err(|error| {
            serde::de::Error::custom(format!("invalid proposal params: {error}"))
        })?;
        Ok(Self {
            id: raw.id,
            kind: raw.kind,
            target: raw.target,
            params,
            rationale: raw.rationale,
        })
    }
}

impl Serialize for Proposal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let params = self.params.to_value().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("Proposal", 5)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("target", &self.target)?;
        state.serialize_field("params", &params)?;
        state.serialize_field("rationale", &self.rationale)?;
        state.end()
    }
}

impl JsonSchema for Proposal {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Proposal".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        fn variant(kind: ProposalKind, params: Schema) -> Schema {
            schemars::json_schema!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "kind": {"const": kind.as_str()},
                    "target": {"type": ["string", "null"]},
                    "params": params,
                    "rationale": {"type": "string"}
                },
                "required": ["id", "kind", "params", "rationale"],
                "additionalProperties": false
            })
        }

        let open_pr = generator.subschema_for::<OpenPrParams>();
        let reply = generator.subschema_for::<ReplyReviewThreadParams>();
        let resolve = generator.subschema_for::<ResolveReviewThreadParams>();
        let escalate = generator.subschema_for::<EscalateParams>();
        let variants = vec![
            variant(ProposalKind::OpenPr, open_pr),
            variant(ProposalKind::ReplyReviewThread, reply),
            variant(ProposalKind::ResolveReviewThread, resolve),
            variant(ProposalKind::Escalate, escalate),
        ];
        schemars::json_schema!({ "oneOf": variants })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Proposals {
    pub protocol: WorkerProtocol,
    pub job_id: AttemptId,
    pub proposals: Vec<Proposal>,
}

// ---------------------------------------------------------------------------
// 进度事件（worker-protocol §7.1）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStage {
    Init,
    Explore,
    Plan,
    Edit,
    Test,
    Review,
    Package,
    Finalize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Start,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressEvent {
    Hello(HelloEvent),
    Stage(StageEvent),
    Tool(ToolEvent),
    Usage(UsageEvent),
    Heartbeat(HeartbeatEvent),
    Warn(WarnEvent),
    Finish(FinishEvent),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelloEvent {
    pub v: u8,
    pub seq: u64,
    pub job_id: AttemptId,
    pub agent: AgentId,
    pub agent_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StageEvent {
    pub v: u8,
    pub seq: u64,
    pub stage: ProgressStage,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolEvent {
    pub v: u8,
    pub seq: u64,
    pub phase: ToolPhase,
    pub call_id: String,
    pub tool: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageEvent {
    pub v: u8,
    pub seq: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub turns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HeartbeatEvent {
    pub v: u8,
    pub seq: u64,
    pub in_flight: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WarnEvent {
    pub v: u8,
    pub seq: u64,
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FinishEvent {
    pub v: u8,
    pub seq: u64,
    pub status: ResultStatus,
}

impl ProgressEvent {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Hello(event) => event.seq,
            Self::Stage(event) => event.seq,
            Self::Tool(event) => event.seq,
            Self::Usage(event) => event.seq,
            Self::Heartbeat(event) => event.seq,
            Self::Warn(event) => event.seq,
            Self::Finish(event) => event.seq,
        }
    }

    pub fn version(&self) -> u8 {
        match self {
            Self::Hello(event) => event.v,
            Self::Stage(event) => event.v,
            Self::Tool(event) => event.v,
            Self::Usage(event) => event.v,
            Self::Heartbeat(event) => event.v,
            Self::Warn(event) => event.v,
            Self::Finish(event) => event.v,
        }
    }
}

impl<'de> Deserialize<'de> for ProgressEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("progress event is missing a type"))?;
        match kind {
            "hello" => serde_json::from_value(value)
                .map(Self::Hello)
                .map_err(serde::de::Error::custom),
            "stage" => serde_json::from_value(value)
                .map(Self::Stage)
                .map_err(serde::de::Error::custom),
            "tool" => serde_json::from_value(value)
                .map(Self::Tool)
                .map_err(serde::de::Error::custom),
            "usage" => serde_json::from_value(value)
                .map(Self::Usage)
                .map_err(serde::de::Error::custom),
            "heartbeat" => serde_json::from_value(value)
                .map(Self::Heartbeat)
                .map_err(serde::de::Error::custom),
            "warn" => serde_json::from_value(value)
                .map(Self::Warn)
                .map_err(serde::de::Error::custom),
            "finish" => serde_json::from_value(value)
                .map(Self::Finish)
                .map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "unknown progress event type: {other}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// ResultBundle（worker-protocol §5.2，服务端生成；#84 拍板）
// ---------------------------------------------------------------------------

pub const RESULT_BUNDLE_VERSION: u32 = 1;

/// `bundle_version` 字段固定为 1。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResultBundleVersion;

impl ResultBundleVersion {
    pub const VALUE: u32 = RESULT_BUNDLE_VERSION;
}

impl Serialize for ResultBundleVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(Self::VALUE)
    }
}

impl<'de> Deserialize<'de> for ResultBundleVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u32::deserialize(deserializer)?;
        if value == Self::VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(
                "unsupported result bundle version",
            ))
        }
    }
}

impl JsonSchema for ResultBundleVersion {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ResultBundleVersion".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "integer",
            "const": RESULT_BUNDLE_VERSION,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultTerminalCause {
    Exited,
    KilledTimer,
    KilledCancel,
    KilledRuntime,
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultTimerKind {
    Wall,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultTerminal {
    pub cause: ResultTerminalCause,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    pub oom: bool,
    #[serde(default)]
    pub timer: Option<ResultTimerKind>,
    pub cancel_requested: bool,
}

/// 内容寻址对象的引用（CAS 摘要 + 字节数）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileArtifact {
    pub sha256: Sha256Digest,
    pub bytes: u64,
}

/// `files` 的键：只允许 `out/` 白名单里的文件名（worker-protocol §5.1）。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum BundleFileName {
    #[serde(rename = "result.json")]
    ResultJson,
    #[serde(rename = "changes.patch")]
    ChangesPatch,
    #[serde(rename = "report.md")]
    ReportMd,
    #[serde(rename = "proposals.json")]
    ProposalsJson,
    #[serde(rename = "progress.jsonl")]
    ProgressJsonl,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultLogs {
    #[serde(default)]
    pub stdout_tail: Option<FileArtifact>,
    #[serde(default)]
    pub stderr_tail: Option<FileArtifact>,
}

/// Egress 计量（权威用量）加 P-70 加权值。M1 只定义形状，语义留到 M2。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BundleUsageMetered {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub requests: u64,
    #[serde(default)]
    pub weighted_tokens: Option<u64>,
}

impl BundleUsageMetered {
    pub fn token_usage(&self) -> TokenUsage {
        TokenUsage::new(
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.requests,
        )
    }
}

/// `EgressFlags` 中可入清单的部分；`upstream_error_summary` 是不可信文本，不入清单。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BundleEgressFlags {
    pub capped: bool,
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PatchVerdict {
    Allow,
    NeedsApproval,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchValidationSummary {
    pub verdict: PatchVerdict,
    /// 补丁内容摘要，等于 `files["changes.patch"].sha256`（= 补丁审批的 subject_digest，P-20）。
    pub sha256: Sha256Digest,
    /// 必须与 `attempt.finished.result_tree` 一致；事件为权威来源，由引擎写入时核对。
    #[serde(default)]
    pub result_tree: Option<GitOid>,
    pub files: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProposalDisposition {
    Accepted,
    NeedsApproval,
    Denied,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProposalDispositionEntry {
    pub id: String,
    pub kind: ProposalKind,
    pub disposition: ProposalDisposition,
    #[serde(default)]
    pub reasons: Option<Vec<String>>,
    /// 被接受提议的净化文本（引擎另存的 CAS 对象）；执行器只读它，不碰 worker 原始文件。
    #[serde(default)]
    pub text_ref: Option<FileArtifact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BundleUnexpectedArtifact {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultClassification {
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    #[serde(default)]
    pub failure_detail: Option<FailureDetail>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultValidation {
    #[serde(default)]
    pub patch: Option<PatchValidationSummary>,
    #[serde(default)]
    pub proposals: Vec<ProposalDispositionEntry>,
    /// M1 只定义形状，语义留到 M2（隔离区引用与 worker-protocol OQ-12 一起定）。
    #[serde(default)]
    pub unexpected_artifacts: Option<Vec<BundleUnexpectedArtifact>>,
}

/// 尝试结果清单：JCS 规范化后入 CAS，`Attempt.result_ref` 是规范化字节的摘要。
///
/// 不带 `project_id`：归属经 `Attempt.result_ref` 反查（Q-12 以清单为索引授权）。
/// 不含 `result_commit`：提交时间戳取自本清单的 `collected_at`，`result_commit` 只在事件里。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultBundle {
    pub bundle_version: ResultBundleVersion,
    pub job_id: AttemptId,
    pub job_sha256: Sha256Digest,
    /// 沙盒停止时间（`Lost` 时取判定丢失那条事件的时间），截到秒；重复 collect 结果相同。
    pub collected_at: Timestamp,
    pub terminal: ResultTerminal,
    #[serde(default)]
    pub files: BTreeMap<BundleFileName, FileArtifact>,
    #[serde(default)]
    pub logs: ResultLogs,
    #[serde(default)]
    pub usage_metered: Option<BundleUsageMetered>,
    #[serde(default)]
    pub usage_reported: Option<UsageReported>,
    #[serde(default)]
    pub egress: Option<BundleEgressFlags>,
    pub classification: ResultClassification,
    /// `result.json` 缺失或无效时为空。
    #[serde(default)]
    pub validation: Option<ResultValidation>,
}

impl ResultBundle {
    /// 清单本地一致性（worker-protocol §5.2）。与事件的一致性由引擎写入时核对。
    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.collected_at.as_millis() % 1000 != 0 {
            return Err(WorkerError::BundleCollectedAtNotSeconds);
        }
        if self.terminal.cause == ResultTerminalCause::Exited && self.terminal.exit_code.is_none() {
            return Err(WorkerError::BundleExitCodeMissing);
        }
        let Some(validation) = &self.validation else {
            return Ok(());
        };
        if let Some(patch) = &validation.patch {
            match self.files.get(&BundleFileName::ChangesPatch) {
                Some(file) if file.sha256 == patch.sha256 => {}
                _ => return Err(WorkerError::BundlePatchMismatch),
            }
            if patch.verdict == PatchVerdict::Deny && patch.result_tree.is_some() {
                return Err(WorkerError::BundleDeniedPatchHasTree);
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for proposal in &validation.proposals {
            if !ids.insert(proposal.id.as_str()) {
                return Err(WorkerError::BundleDuplicateProposal);
            }
            let accepted = proposal.disposition == ProposalDisposition::Accepted;
            if proposal.text_ref.is_some() && !accepted {
                return Err(WorkerError::BundleTextRefNotAccepted);
            }
            if accepted && proposal.kind == ProposalKind::OpenPr && proposal.text_ref.is_none() {
                return Err(WorkerError::BundleOpenPrTextMissing);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 补丁闸门类型（repo.md §6.3，P-69）
// ---------------------------------------------------------------------------

/// 规范化后的仓库相对路径；`repo` 负责完整 P4 检查，这里只做基础拒绝。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn parse(value: &str) -> Result<Self, PatchError> {
        if value.is_empty() || value.len() > 4096 {
            return Err(PatchError::PathLength);
        }
        if value.starts_with('/') {
            return Err(PatchError::PathAbsolute);
        }
        if value.contains('\0') || value.contains('\\') {
            return Err(PatchError::PathCharacter);
        }
        if value.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err(PatchError::PathComponent);
        }
        Ok(Self(value.to_owned()))
    }

    /// 任何组件是 `.git`（大小写变体）都拒绝。
    pub fn is_dotgit(&self) -> bool {
        self.0
            .split('/')
            .any(|part| part.eq_ignore_ascii_case(".git"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RepoPath").field(&self.0).finish()
    }
}

impl FromStr for RepoPath {
    type Err = PatchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for RepoPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for RepoPath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RepoPath".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 4096,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    ModeOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileMode {
    Mode100644,
    Mode100755,
    Mode120000,
    Mode160000,
}

impl FileMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mode100644 => "100644",
            Self::Mode100755 => "100755",
            Self::Mode120000 => "120000",
            Self::Mode160000 => "160000",
        }
    }

    pub fn parse(value: &str) -> Result<Self, PatchError> {
        match value {
            "100644" => Ok(Self::Mode100644),
            "100755" => Ok(Self::Mode100755),
            "120000" => Ok(Self::Mode120000),
            "160000" => Ok(Self::Mode160000),
            _ => Err(PatchError::InvalidMode),
        }
    }

    pub fn is_regular(self) -> bool {
        matches!(self, Self::Mode100644 | Self::Mode100755)
    }
}

impl Serialize for FileMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FileMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for FileMode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FileMode".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": ["100644", "100755", "120000", "160000"],
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchFile {
    pub path: RepoPath,
    pub status: FileStatus,
    #[serde(default)]
    pub old_mode: Option<FileMode>,
    #[serde(default)]
    pub new_mode: Option<FileMode>,
    #[serde(default)]
    pub added: Option<u32>,
    #[serde(default)]
    pub deleted: Option<u32>,
    pub is_binary: bool,
    pub is_symlink: bool,
    pub is_submodule: bool,
    #[serde(default)]
    pub size_after: Option<u64>,
}

/// `repo` 机械检查的产物，`policy` 规则判定的唯一输入。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchFacts {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub attempt_id: AttemptId,
    pub patch_sha256: Sha256Digest,
    pub patch_bytes: u64,
    pub applies_cleanly: bool,
    pub files: Vec<PatchFile>,
    pub total_changed_lines: u64,
    pub job_paths: PathConstraints,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub struct PolicyCode(String);

impl PolicyCode {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PolicyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PolicyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PolicyCode").field(&self.0).finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchReason {
    pub code: PolicyCode,
    #[serde(default)]
    pub path: Option<RepoPath>,
    #[serde(default)]
    pub rule_id: Option<String>,
}

/// `repo` 机械拒绝码；策略拒绝为 `policy_denied`（由 policy 追加 `policy.denied`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PatchRejectReason {
    DeclaredMismatch,
    NotApplicable,
    PathInvalid,
    Dotgit,
    Binary,
    Symlink,
    Submodule,
    PolicyDenied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ValidatedPatch {
    /// 补丁内容摘要 = `patch_apply_protected` 审批的 `subject_digest`。
    pub sha256: Sha256Digest,
    pub base_sha: GitOid,
    /// `deny` 时为 `None`（不写 tree 对象）。
    #[serde(default)]
    pub result_tree: Option<GitOid>,
    pub files: Vec<PatchFile>,
    pub verdict: PatchVerdict,
    pub reasons: Vec<PatchReason>,
    pub matched_approval_rules: Vec<String>,
    pub config_rev: Seq,
}

fn validate_gate(gate: &GateFields, findings_required: bool) -> Result<(), WorkerError> {
    match gate.verdict {
        Verdict::Accept => {
            if gate
                .findings
                .iter()
                .any(|finding| finding.severity == FindingSeverity::Blocker)
            {
                return Err(WorkerError::GateInconsistent);
            }
        }
        Verdict::Revise | Verdict::Reject => {
            if findings_required && gate.findings.is_empty() {
                return Err(WorkerError::GateInconsistent);
            }
        }
        Verdict::Blocked => {
            if gate.blocking_reasons.is_empty() {
                return Err(WorkerError::GateInconsistent);
            }
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("agent id must match ^[a-z][a-z0-9_-]{{0,63}}$")]
    InvalidAgentId,
    #[error("role profile does not match tier/exec")]
    RoleProfileMismatch,
    #[error("direct roles do not run in a sandbox")]
    DirectRoleNotSandboxed,
    #[error("read-only job may not produce a patch")]
    ReadOnlyJobWithPatch,
    #[error("read-only job may not enable edits")]
    ReadOnlyJobWithEdit,
    #[error("a patch-producing job requires constraints.patch")]
    MissingPatchLimits,
    #[error("attempt number must be at least 1")]
    InvalidAttemptNumber,
    #[error("isolation nonce must be 32 lowercase hex characters")]
    InvalidIsolationNonce,
    #[error("workspace path must be /gantry/work")]
    InvalidWorkspacePath,
    #[error("structured schema id must be gantry.worker.output.<schema>/<major>")]
    InvalidStructuredSchemaId,
    #[error("ci_classification output requires classification")]
    ClassifyMissingClassification,
    #[error("result summary exceeds 1000 characters")]
    SummaryTooLong,
    #[error("succeeded result requires structured_output")]
    MissingStructuredOutput,
    #[error("failed or cancelled result requires error")]
    MissingError,
    #[error("gate fields violate the consistency rules")]
    GateInconsistent,
    #[error("proposal kind does not match its params")]
    ProposalKindMismatch,
    #[error("thread proposals require a target handle")]
    ProposalMissingTarget,
    #[error("result bundle collected_at must be whole seconds")]
    BundleCollectedAtNotSeconds,
    #[error("exited terminal requires exit_code")]
    BundleExitCodeMissing,
    #[error("validation.patch requires files[\"changes.patch\"] with the same sha256")]
    BundlePatchMismatch,
    #[error("denied patch must not carry result_tree")]
    BundleDeniedPatchHasTree,
    #[error("proposal ids in validation must be unique")]
    BundleDuplicateProposal,
    #[error("text_ref is only allowed on accepted proposals")]
    BundleTextRefNotAccepted,
    #[error("accepted open_pr proposal requires text_ref")]
    BundleOpenPrTextMissing,
}

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("repo path is empty or too long")]
    PathLength,
    #[error("repo path must be relative")]
    PathAbsolute,
    #[error("repo path contains NUL or a backslash")]
    PathCharacter,
    #[error("repo path contains an empty or parent component")]
    PathComponent,
    #[error("unsupported file mode")]
    InvalidMode,
}
