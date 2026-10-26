//! 设置资源、稀疏写入类型与解析后的强类型文档。
//!
//! 这里刻意区分两类类型：
//! - `*Patch`：PUT / 稀疏存储的形态，所有字段可选；
//! - 无后缀类型：内置默认与全局 + 项目合并后的完整形态。
//!
//! 所有已定义字段都使用具体类型或枚举，不用 `serde_json::Value` 抹平协议。
//! `Value` 只出现在秘密值、schema 生成和对外聚合结果中。

use std::collections::BTreeMap;

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::entities::{InboxKind, PolicyActionId, TokenWeights};
use crate::level::{Effort, Level};
use crate::secret::{SecretName, SecretWrite};

// ---------------------------------------------------------------------------
// 凭据引用
// ---------------------------------------------------------------------------

/// 服务端存储的模型凭据类型；第一版只有 API key（OQ-4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    ApiKey,
}

/// 只读引用，供默认值、有效值与事件文档使用。
///
/// 序列化永不输出 `value`，也不输出 `present`（存储形态只有引用）。
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRefDoc {
    pub secret_ref: SecretName,
    #[serde(default)]
    pub present: Option<bool>,
}

impl Serialize for SecretRefDoc {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SecretRefDoc", 1)?;
        state.serialize_field("secret_ref", &self.secret_ref)?;
        state.end()
    }
}

impl JsonSchema for SecretRefDoc {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretRef".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": false,
            "required": ["secret_ref"],
            "properties": {
                "secret_ref": {
                    "type": "string",
                    "pattern": "^[a-z0-9][a-z0-9._-]{0,63}$"
                },
                "present": { "type": "boolean", "readOnly": true }
            },
            "x-gantry-secret": true,
        })
    }
}

/// 写入形态：`secret_ref` 必填；`present` 接受并忽略；`value` 省略 = 保留，
/// 字符串 = 写入 / 轮换，`null` 由 [`SecretWrite`] 反序列化拒绝。
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRefInput {
    pub secret_ref: SecretName,
    #[serde(default)]
    pub present: Option<bool>,
    #[serde(default)]
    pub value: SecretWrite,
}

impl JsonSchema for SecretRefInput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretRefWrite".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": false,
            "required": ["secret_ref"],
            "properties": {
                "secret_ref": {
                    "type": "string",
                    "pattern": "^[a-z0-9][a-z0-9._-]{0,63}$"
                },
                "present": { "type": "boolean", "readOnly": true },
                "value": { "type": "string", "writeOnly": true }
            },
            "x-gantry-secret": true,
        })
    }
}

/// 模型凭据：`{type, secret_ref}`，写入时另可带 `value`。
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCredentialPatch {
    #[serde(rename = "type")]
    pub credential_type: CredentialType,
    pub secret_ref: SecretName,
    #[serde(default)]
    pub present: Option<bool>,
    #[serde(default)]
    pub value: SecretWrite,
}

impl JsonSchema for ModelCredentialPatch {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ModelCredentialWrite".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": false,
            "required": ["type", "secret_ref"],
            "properties": {
                "type": { "const": "api_key" },
                "secret_ref": {
                    "type": "string",
                    "pattern": "^[a-z0-9][a-z0-9._-]{0,63}$"
                },
                "present": { "type": "boolean", "readOnly": true },
                "value": { "type": "string", "writeOnly": true }
            },
            "x-gantry-secret": true,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-gantry-secret" = true))]
pub struct ModelCredential {
    #[serde(rename = "type")]
    pub credential_type: CredentialType,
    pub secret_ref: SecretName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present: Option<bool>,
}

// ---------------------------------------------------------------------------
// models
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerAdapter {
    #[serde(rename = "claude-code")]
    ClaudeCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlannerEffort {
    None,
    Low,
    Medium,
    High,
    Max,
    ByLevel,
}

impl From<Effort> for PlannerEffort {
    fn from(value: Effort) -> Self {
        match value {
            Effort::None => Self::None,
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
            Effort::Max => Self::Max,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffortChoice {
    pub default: Effort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical: Option<Effort>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EffortByLevel {
    pub low: EffortChoice,
    pub medium: EffortChoice,
    pub high: EffortChoice,
    pub max: EffortChoice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Effort>,
}

#[derive(Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelProviderPatch {
    pub kind: Option<ProviderKind>,
    pub base_url: Option<String>,
    pub credential: Option<ModelCredentialPatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelProvider {
    pub kind: ProviderKind,
    pub base_url: String,
    pub credential: ModelCredential,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryPatch {
    pub max_attempts: Option<u64>,
    pub backoff_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    pub max_attempts: u64,
    pub backoff_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UtilityPatch {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<Effort>,
    pub fallback: Option<Vec<Fallback>>,
    pub request_timeout_s: Option<u64>,
    pub total_timeout_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Utility {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub reasoning_effort: Effort,
    pub fallback: Vec<Fallback>,
    pub request_timeout_s: u64,
    pub total_timeout_s: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlannerPatch {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<PlannerEffort>,
    pub effort_by_level: Option<EffortByLevel>,
    pub fallback: Option<Vec<Fallback>>,
    pub request_timeout_s: Option<u64>,
    pub total_timeout_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Planner {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub reasoning_effort: PlannerEffort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_by_level: Option<EffortByLevel>,
    pub fallback: Vec<Fallback>,
    pub request_timeout_s: u64,
    pub total_timeout_s: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerPatch {
    pub adapter: Option<WorkerAdapter>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<PlannerEffort>,
    pub effort_by_level: Option<EffortByLevel>,
    pub fallback: Option<Vec<Fallback>>,
    pub request_timeout_s: Option<u64>,
    pub attempt_wall_s: Option<u64>,
    pub max_turns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    pub adapter: WorkerAdapter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub reasoning_effort: PlannerEffort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_by_level: Option<EffortByLevel>,
    pub fallback: Vec<Fallback>,
    pub request_timeout_s: u64,
    pub attempt_wall_s: u64,
    pub max_turns: u64,
}

#[derive(Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelsPatch {
    pub providers: Option<BTreeMap<String, ModelProviderPatch>>,
    pub retry: Option<RetryPatch>,
    pub utility: Option<UtilityPatch>,
    pub planner: Option<PlannerPatch>,
    pub worker: Option<WorkerPatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Models {
    pub providers: BTreeMap<String, ModelProvider>,
    pub retry: Retry,
    pub utility: Utility,
    pub planner: Planner,
    pub worker: Worker,
}

// ---------------------------------------------------------------------------
// levels
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoneLevelPatch {
    pub daily_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoneLevel {
    pub daily_tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TierBudgetPatch {
    pub utility: Option<u64>,
    pub planner: Option<u64>,
    pub worker: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TierBudget {
    pub utility: u64,
    pub planner: u64,
    pub worker: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LevelBudgetPatch {
    pub tokens_by_tier: Option<TierBudgetPatch>,
    pub nodes: Option<u64>,
    pub rounds: Option<u64>,
    pub wall_time_s: Option<u64>,
    pub concurrency: Option<u64>,
    pub forge_writes: Option<u64>,
    pub forge_writes_per_hour: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LevelBudget {
    pub tokens_by_tier: TierBudget,
    pub nodes: u64,
    pub rounds: u64,
    pub wall_time_s: u64,
    pub concurrency: u64,
    pub forge_writes: u64,
    pub forge_writes_per_hour: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LevelEntryPatch {
    pub graph_template: Option<String>,
    pub budget: Option<LevelBudgetPatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LevelEntry {
    pub graph_template: String,
    pub budget: LevelBudget,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LevelsPatch {
    pub default_level: Option<Level>,
    pub token_weights: Option<TokenWeights>,
    pub none: Option<NoneLevelPatch>,
    pub low: Option<LevelEntryPatch>,
    pub medium: Option<LevelEntryPatch>,
    pub high: Option<LevelEntryPatch>,
    pub max: Option<LevelEntryPatch>,
    pub ultimate: Option<LevelEntryPatch>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Levels {
    pub default_level: Level,
    pub token_weights: TokenWeights,
    pub none: NoneLevel,
    pub low: LevelEntry,
    pub medium: LevelEntry,
    pub high: LevelEntry,
    pub max: LevelEntry,
    pub ultimate: LevelEntry,
}

// ---------------------------------------------------------------------------
// policies
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Disposition {
    Auto,
    Approve,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UnlistedDisposition {
    Approve,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathDisposition {
    Approve,
    Deny,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathRulePatch {
    pub globs: Option<Vec<String>>,
    pub disposition: Option<PathDisposition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub globs: Vec<String>,
    pub disposition: PathDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum EscalationWhen {
    Paths(WhenPaths),
    Diff(WhenDiff),
    Labels(WhenLabels),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhenPaths {
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhenDiff {
    pub diff: DiffThreshold,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhenLabels {
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffThreshold {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_gte: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_gte: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EscalationLevel {
    Medium,
    High,
    Max,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationRule {
    pub when: EscalationWhen,
    pub raise_to: EscalationLevel,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationPatch {
    pub enabled: Option<bool>,
    pub rules: Option<BTreeMap<String, EscalationRule>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Escalation {
    pub enabled: bool,
    pub rules: BTreeMap<String, EscalationRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPatch {
    pub ttl_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub ttl_s: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchLimitsPatch {
    pub max_files: Option<u64>,
    pub max_bytes: Option<u64>,
    pub max_changed_lines: Option<u64>,
    pub max_file_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchLimits {
    pub max_files: u64,
    pub max_bytes: u64,
    pub max_changed_lines: u64,
    pub max_file_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoliciesPatch {
    pub allow_globs: Option<Vec<String>>,
    pub actions: Option<BTreeMap<PolicyActionId, Disposition>>,
    pub unlisted_action: Option<UnlistedDisposition>,
    pub paths: Option<BTreeMap<String, PathRulePatch>>,
    pub escalation: Option<EscalationPatch>,
    pub approval: Option<ApprovalPatch>,
    pub patch: Option<PatchLimitsPatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    pub allow_globs: Vec<String>,
    pub actions: BTreeMap<PolicyActionId, Disposition>,
    pub unlisted_action: UnlistedDisposition,
    pub paths: BTreeMap<String, PathRule>,
    pub escalation: Escalation,
    pub approval: Approval,
    pub patch: PatchLimits,
}

// ---------------------------------------------------------------------------
// guardian
// ---------------------------------------------------------------------------

/// 守护分流类 id（config.md §6.1）。锁定类出现在 `triage` 里由 V17 拒绝；
/// V17 属于 M2，本节点只保证 id 与取值类型正确。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GuardianTriageClass {
    CiFailureInScope,
    BotFindingValid,
    BotFindingInvalid,
    Nit,
    MechanicalConflict,
    SecurityFinding,
    TouchesProtectedPath,
    ScopeExpandingFix,
    RepeatedOrNoProgress,
    BudgetExhausted,
    ApprovalNeeded,
    PushAfterApproval,
    DesignOrApiChange,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriageAction {
    SilentFix,
    SilentReply,
    Batch,
    Escalate,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoProgressPatch {
    pub same_class_repeats: Option<u64>,
    pub stalled_rounds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoProgress {
    pub same_class_repeats: u64,
    pub stalled_rounds: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusChecklistPatch {
    pub enabled: Option<bool>,
    pub min_update_interval_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusChecklist {
    pub enabled: bool,
    pub min_update_interval_s: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardianPatch {
    pub triage: Option<BTreeMap<GuardianTriageClass, TriageAction>>,
    pub no_progress: Option<NoProgressPatch>,
    pub status_checklist: Option<StatusChecklistPatch>,
    pub patrol_interval_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Guardian {
    pub triage: BTreeMap<GuardianTriageClass, TriageAction>,
    pub no_progress: NoProgress,
    pub status_checklist: StatusChecklist,
    pub patrol_interval_s: u64,
}

// ---------------------------------------------------------------------------
// sandbox
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxDriver {
    Podman,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxRuntime {
    Runsc,
    Runc,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxLimitsPatch {
    pub cpu_cores: Option<f64>,
    pub mem_mb: Option<u64>,
    pub disk_mb: Option<u64>,
    pub pids: Option<u64>,
    pub wall_time_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxLimits {
    pub cpu_cores: f64,
    pub mem_mb: u64,
    pub disk_mb: u64,
    pub pids: u64,
    pub wall_time_s: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrewarmPatch {
    pub enabled: Option<bool>,
    pub pool_size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prewarm {
    pub enabled: bool,
    pub pool_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DevcontainerMode {
    Off,
    SafeSubset,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DevcontainerPatch {
    pub mode: Option<DevcontainerMode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Devcontainer {
    pub mode: DevcontainerMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProgressIoPatch {
    pub max_line_bytes: Option<u64>,
    pub max_lines: Option<u64>,
    pub max_rate_per_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProgressIo {
    pub max_line_bytes: u64,
    pub max_lines: u64,
    pub max_rate_per_s: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIoPatch {
    pub result_max_bytes: Option<u64>,
    pub report_max_bytes: Option<u64>,
    pub proposals_max_bytes: Option<u64>,
    pub proposals_max_count: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIo {
    pub result_max_bytes: u64,
    pub report_max_bytes: u64,
    pub proposals_max_bytes: u64,
    pub proposals_max_count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialIoPatch {
    pub max_item_bytes: Option<u64>,
    pub max_total_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialIo {
    pub max_item_bytes: u64,
    pub max_total_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxIoPatch {
    pub progress: Option<ProgressIoPatch>,
    pub artifacts: Option<ArtifactIoPatch>,
    pub materials: Option<MaterialIoPatch>,
    pub logs_tail_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxIo {
    pub progress: ProgressIo,
    pub artifacts: ArtifactIo,
    pub materials: MaterialIo,
    pub logs_tail_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxPatch {
    pub driver: Option<SandboxDriver>,
    pub runtime: Option<SandboxRuntime>,
    pub base_image: Option<String>,
    pub limits: Option<SandboxLimitsPatch>,
    pub max_concurrent_sandboxes: Option<u64>,
    pub prewarm: Option<PrewarmPatch>,
    pub devcontainer: Option<DevcontainerPatch>,
    pub idle_timeout_s: Option<u64>,
    pub kill_grace_s: Option<u64>,
    pub io: Option<SandboxIoPatch>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    pub driver: SandboxDriver,
    pub runtime: SandboxRuntime,
    pub base_image: String,
    pub limits: SandboxLimits,
    pub max_concurrent_sandboxes: u64,
    pub prewarm: Prewarm,
    pub devcontainer: Devcontainer,
    pub idle_timeout_s: u64,
    pub kill_grace_s: u64,
    pub io: SandboxIo,
}

// ---------------------------------------------------------------------------
// egress
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressGroupPatch {
    pub hosts: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressGroup {
    pub hosts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressProfilePatch {
    pub groups: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressProfile {
    pub groups: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EgressInjectionKind {
    Bearer,
    Header,
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressInjectionPatch {
    pub hosts: Option<Vec<String>>,
    pub kind: Option<EgressInjectionKind>,
    pub header: Option<String>,
    pub credential: Option<SecretRefInput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressInjection {
    pub hosts: Vec<String>,
    pub kind: EgressInjectionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub credential: SecretRefDoc,
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressPatch {
    pub groups: Option<BTreeMap<String, EgressGroupPatch>>,
    pub profiles: Option<BTreeMap<String, EgressProfilePatch>>,
    pub default_profile: Option<String>,
    pub injection: Option<BTreeMap<String, EgressInjectionPatch>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Egress {
    pub groups: BTreeMap<String, EgressGroup>,
    pub profiles: BTreeMap<String, EgressProfile>,
    pub default_profile: String,
    pub injection: BTreeMap<String, EgressInjection>,
}

// ---------------------------------------------------------------------------
// notify
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotifyChannelKind {
    Desktop,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopChannelPatch {
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopChannel {
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyChannelsPatch {
    pub desktop: Option<DesktopChannelPatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyChannels {
    pub desktop: DesktopChannel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyRoute {
    pub kinds: Vec<InboxKind>,
    pub channels: Vec<NotifyChannelKind>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyPatch {
    pub channels: Option<NotifyChannelsPatch>,
    pub routes: Option<BTreeMap<String, NotifyRoute>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    pub channels: NotifyChannels,
    #[serde(default)]
    pub routes: BTreeMap<String, NotifyRoute>,
}

// ---------------------------------------------------------------------------
// github
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubAppPatch {
    pub app_id: Option<u64>,
    pub private_key: Option<SecretRefInput>,
    pub webhook_secret: Option<SecretRefInput>,
    pub webhook_secret_previous: Option<SecretRefInput>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubApp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<SecretRefDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<SecretRefDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret_previous: Option<SecretRefDoc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubPatch {
    pub app: Option<GithubAppPatch>,
    pub commit_identity: Option<CommitIdentity>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Github {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<GithubApp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_identity: Option<CommitIdentity>,
}

// ---------------------------------------------------------------------------
// 聚合
// ---------------------------------------------------------------------------

// 变体大小差异来自各 resource 的字段数；这里不做 Box，保持 API 直观。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum SettingsPatch {
    Models(ModelsPatch),
    Levels(LevelsPatch),
    Policies(PoliciesPatch),
    Guardian(GuardianPatch),
    Sandbox(SandboxPatch),
    Egress(EgressPatch),
    Notify(NotifyPatch),
    Github(GithubPatch),
}

impl SettingsPatch {
    pub fn resource(&self) -> crate::entities::SettingsResource {
        use crate::entities::SettingsResource;
        match self {
            Self::Models(_) => SettingsResource::Models,
            Self::Levels(_) => SettingsResource::Levels,
            Self::Policies(_) => SettingsResource::Policies,
            Self::Guardian(_) => SettingsResource::Guardian,
            Self::Sandbox(_) => SettingsResource::Sandbox,
            Self::Egress(_) => SettingsResource::Egress,
            Self::Notify(_) => SettingsResource::Notify,
            Self::Github(_) => SettingsResource::Github,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ResolvedSettings {
    Models(Models),
    Levels(Levels),
    Policies(Policies),
    Guardian(Guardian),
    Sandbox(Sandbox),
    Egress(Egress),
    Notify(Notify),
    Github(Github),
}

impl ResolvedSettings {
    pub fn resource(&self) -> crate::entities::SettingsResource {
        use crate::entities::SettingsResource;
        match self {
            Self::Models(_) => SettingsResource::Models,
            Self::Levels(_) => SettingsResource::Levels,
            Self::Policies(_) => SettingsResource::Policies,
            Self::Guardian(_) => SettingsResource::Guardian,
            Self::Sandbox(_) => SettingsResource::Sandbox,
            Self::Egress(_) => SettingsResource::Egress,
            Self::Notify(_) => SettingsResource::Notify,
            Self::Github(_) => SettingsResource::Github,
        }
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("resolved settings serialize")
    }
}

/// 把请求 / 存储文档解析成稀疏类型。未知字段由 `deny_unknown_fields` 拒绝。
pub fn parse_patch(
    resource: crate::entities::SettingsResource,
    doc: &Value,
) -> Result<SettingsPatch, serde_json::Error> {
    use crate::entities::SettingsResource;
    Ok(match resource {
        SettingsResource::Models => SettingsPatch::Models(serde_json::from_value(doc.clone())?),
        SettingsResource::Levels => SettingsPatch::Levels(serde_json::from_value(doc.clone())?),
        SettingsResource::Policies => SettingsPatch::Policies(serde_json::from_value(doc.clone())?),
        SettingsResource::Guardian => SettingsPatch::Guardian(serde_json::from_value(doc.clone())?),
        SettingsResource::Sandbox => SettingsPatch::Sandbox(serde_json::from_value(doc.clone())?),
        SettingsResource::Egress => SettingsPatch::Egress(serde_json::from_value(doc.clone())?),
        SettingsResource::Notify => SettingsPatch::Notify(serde_json::from_value(doc.clone())?),
        SettingsResource::Github => SettingsPatch::Github(serde_json::from_value(doc.clone())?),
    })
}

/// 把已合并的完整文档解析成解析后的类型。
pub fn parse_resolved(
    resource: crate::entities::SettingsResource,
    doc: &Value,
) -> Result<ResolvedSettings, serde_json::Error> {
    use crate::entities::SettingsResource;
    Ok(match resource {
        SettingsResource::Models => ResolvedSettings::Models(serde_json::from_value(doc.clone())?),
        SettingsResource::Levels => ResolvedSettings::Levels(serde_json::from_value(doc.clone())?),
        SettingsResource::Policies => {
            ResolvedSettings::Policies(serde_json::from_value(doc.clone())?)
        }
        SettingsResource::Guardian => {
            ResolvedSettings::Guardian(serde_json::from_value(doc.clone())?)
        }
        SettingsResource::Sandbox => {
            ResolvedSettings::Sandbox(serde_json::from_value(doc.clone())?)
        }
        SettingsResource::Egress => ResolvedSettings::Egress(serde_json::from_value(doc.clone())?),
        SettingsResource::Notify => ResolvedSettings::Notify(serde_json::from_value(doc.clone())?),
        SettingsResource::Github => ResolvedSettings::Github(serde_json::from_value(doc.clone())?),
    })
}
