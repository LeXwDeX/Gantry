//! 事件信封、类型目录与 `data` 校验（data-model §5；core §6.5、§7.5）。
//!
//! 本模块只接受**权威契约明确写出字段**的 `data`。`core` §7.5 标为缺口的
//! 事件保留名称，但校验返回 [`SchemaErrorCode::ContractUndefined`] 并带被拒绝的
//! schema 标识，**不返回成功**；清单与依据见 `docs/m0-contract-gaps.md`。
//!
//! 已定义形状的 `data` 使用 `deny_unknown_fields` 的强类型结构；沿用实体字段的
//! 事件（`message.posted`、`inbox.raised`）在解码前显式拒绝未知顶层键。

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::canon::{GitOid, IdempotencyKey, RepoSlug, Sha256Digest, SubjectDigest};
use crate::egress::LedgerRef;
use crate::entities::{
    Actor, ApprovalAction, ApprovalTarget, BudgetDimension, BudgetLimits, CheckSummary,
    DeliveryOrigin, DevicePlatform, DeviceRole, ForgeActionKind, ForgeEventKind, GithubRef,
    InboxItem, IssueRef, Message, NodeStatus, PolicyActionId, Priority, ReviewState,
    SettingsResource, SettingsScope, Stage, StageStatus, Tier, TokenUsage, TokenWeights, Verdict,
    WorkflowStatus,
};
use crate::forge::{
    ActionPayload, CommentTarget, CommentTargetType, ForgeEventSource, IssueStateReason, MergeMode,
    PrState,
};
use crate::ids::*;
use crate::level::{Effort, Level};
use crate::settings::KeyPath;
use crate::text::UntrustedText;
use crate::time::Timestamp;
use crate::worker::{
    PatchFile, PatchReason, PatchRejectReason, PatchVerdict, ProgressStage, ToolPhase,
};

pub use crate::entities::{ActorKind, Seq};
pub use crate::forge::{ForgeEventEnvelope, ForgeOrigin};
pub use crate::settings::GuardianTriageClass;

// ---------------------------------------------------------------------------
// 事件信封
// ---------------------------------------------------------------------------

/// data-model §5.1 信封。字段名逐字一致；`type` 用 `ty` 承载。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event<D = Value> {
    pub id: EventId,
    pub seq: Seq,
    pub ts: Timestamp,
    #[serde(rename = "type")]
    pub ty: EventType,
    pub subject: Subject,
    pub project_id: Option<ProjectId>,
    pub actor: Actor,
    #[serde(default)]
    pub causation_id: Option<EventId>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    pub data: D,
}

/// store 追加前的事件；`id` / `seq` / `ts` 由 store 补。
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    pub ty: EventType,
    pub subject: Subject,
    pub project_id: Option<ProjectId>,
    pub actor: Actor,
    pub causation_id: Option<EventId>,
    pub correlation_id: Option<String>,
    pub data: Value,
    pub dedup_key: Option<String>,
}

impl NewEvent {
    /// 形状校验 + settings 的 subject / project_id / scope 一致性。
    pub fn validate(&self) -> Result<(), SchemaError> {
        validate_event_data(&self.ty, &self.data)?;
        if self.ty == EventType::SettingsUpdated {
            validate_settings_subject(self.subject.as_str(), self.project_id.as_ref(), &self.data)?;
        }
        Ok(())
    }
}

fn validate_settings_subject(
    subject: &str,
    project_id: Option<&ProjectId>,
    data: &Value,
) -> Result<(), SchemaError> {
    let scope = data.get("scope").and_then(Value::as_str);
    if subject == "settings" {
        if project_id.is_some() {
            return Err(SchemaError::consistency(
                "/project_id",
                "global settings.updated must have project_id = null",
            ));
        }
        if scope != Some("global") {
            return Err(SchemaError::consistency(
                "/scope",
                "global settings.updated must have scope = global",
            ));
        }
        return Ok(());
    }
    let Some(project_id) = project_id else {
        return Err(SchemaError::consistency(
            "/project_id",
            "project settings.updated must carry project_id",
        ));
    };
    if subject != project_id.to_string() {
        return Err(SchemaError::consistency(
            "/project_id",
            "settings.updated subject must equal project_id",
        ));
    }
    if scope != Some("project") {
        return Err(SchemaError::consistency(
            "/scope",
            "project settings.updated must have scope = project",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Actor / Subject
// ---------------------------------------------------------------------------

/// `subject`：实体 ID（`<prefix>_<ULID>`）或全局设置的字面量 `settings`。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Subject(String);

impl Subject {
    pub fn parse(value: &str) -> Result<Self, SubjectError> {
        if value == "settings" {
            return Ok(Self(value.to_owned()));
        }
        let (prefix, ulid) = value.split_once('_').ok_or(SubjectError::Invalid)?;
        if !SUBJECT_PREFIXES.contains(&prefix) {
            return Err(SubjectError::Invalid);
        }
        ulid.parse::<Ulid>().map_err(|_| SubjectError::Invalid)?;
        Ok(Self(value.to_owned()))
    }

    pub fn settings() -> Self {
        Self("settings".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> SubjectKind {
        if self.0 == "settings" {
            return SubjectKind::Settings;
        }
        match self.0.split_once('_').map(|(prefix, _)| prefix) {
            Some("prj") => SubjectKind::Project,
            Some("dlv") => SubjectKind::Delivery,
            Some("wfl") => SubjectKind::Workflow,
            Some("nod") => SubjectKind::Node,
            Some("att") => SubjectKind::Attempt,
            Some("wtr") => SubjectKind::Worktree,
            Some("sbx") => SubjectKind::Sandbox,
            Some("cnv") => SubjectKind::Conversation,
            Some("msg") => SubjectKind::Message,
            Some("inb") => SubjectKind::Inbox,
            Some("apr") => SubjectKind::Approval,
            Some("act") => SubjectKind::Action,
            Some("dev") => SubjectKind::Device,
            Some("evt") => SubjectKind::Event,
            Some("bdg") => SubjectKind::Budget,
            _ => SubjectKind::Undefined,
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Subject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Subject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Subject {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Subject".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(settings|(prj|dlv|wfl|nod|att|wtr|sbx|cnv|msg|inb|apr|act|dev|evt|bdg)_[0-7][0-9A-HJKMNP-TV-Z]{25})$",
        })
    }
}

const SUBJECT_PREFIXES: &[&str] = &[
    "prj", "dlv", "wfl", "nod", "att", "wtr", "sbx", "cnv", "msg", "inb", "apr", "act", "dev",
    "evt", "bdg",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SubjectError {
    #[error("subject must be \"settings\" or a prefixed ULID")]
    Invalid,
}

/// catalog 中的 `subject` 元数据；取值未定的类型标 [`SubjectKind::Undefined`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    Project,
    Delivery,
    Workflow,
    Node,
    Attempt,
    Worktree,
    Sandbox,
    Conversation,
    Message,
    Inbox,
    Approval,
    Action,
    Device,
    Budget,
    Event,
    Settings,
    ProjectOrSettings,
    Undefined,
}

// ---------------------------------------------------------------------------
// EventType
// ---------------------------------------------------------------------------

/// 闭集；`as_str` / `FromStr` 与 data-model §5.2 逐字一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventType {
    ProjectAdded,
    ProjectUpdated,
    SettingsUpdated,
    ConversationCreated,
    DeliveryCreated,
    DeliveryStageChanged,
    DeliveryLevelChanged,
    DeliveryAbandoned,
    DeliveryPaused,
    DeliveryResumed,
    WorkflowCreated,
    WorkflowRevised,
    WorkflowFinished,
    NodeQueued,
    NodeStarted,
    NodeFinished,
    NodeSuperseded,
    NodeRetryRequested,
    AttemptStarted,
    AttemptFinished,
    AttemptProgress,
    PatchValidated,
    PatchRejected,
    SandboxCreated,
    SandboxDestroyed,
    WorktreeCreated,
    WorktreeLocked,
    WorktreeUnlocked,
    WorktreeRemoved,
    MessagePosted,
    InboxRaised,
    InboxResolved,
    InboxDismissed,
    GuardianTriaged,
    GuardianHealRequested,
    GuardianEscalated,
    ApprovalRequested,
    ApprovalGranted,
    ApprovalConsumed,
    ApprovalInvalidated,
    ApprovalExpired,
    ApprovalDenied,
    OutboxEnqueued,
    OutboxExecuting,
    OutboxSucceeded,
    OutboxFailed,
    OutboxCancelled,
    Forge(ForgeEventKind),
    ForgeSynced,
    ForgeDriftDetected,
    BudgetCharged,
    BudgetExhausted,
    BudgetLimitRaised,
    PolicyDenied,
    DevicePaired,
    DeviceUpdated,
    DeviceRevoked,
}

impl EventType {
    pub const ALL: [EventType; 79] = [
        EventType::ProjectAdded,
        EventType::ProjectUpdated,
        EventType::SettingsUpdated,
        EventType::ConversationCreated,
        EventType::DeliveryCreated,
        EventType::DeliveryStageChanged,
        EventType::DeliveryLevelChanged,
        EventType::DeliveryAbandoned,
        EventType::DeliveryPaused,
        EventType::DeliveryResumed,
        EventType::WorkflowCreated,
        EventType::WorkflowRevised,
        EventType::WorkflowFinished,
        EventType::NodeQueued,
        EventType::NodeStarted,
        EventType::NodeFinished,
        EventType::NodeSuperseded,
        EventType::NodeRetryRequested,
        EventType::AttemptStarted,
        EventType::AttemptFinished,
        EventType::AttemptProgress,
        EventType::PatchValidated,
        EventType::PatchRejected,
        EventType::SandboxCreated,
        EventType::SandboxDestroyed,
        EventType::WorktreeCreated,
        EventType::WorktreeLocked,
        EventType::WorktreeUnlocked,
        EventType::WorktreeRemoved,
        EventType::MessagePosted,
        EventType::InboxRaised,
        EventType::InboxResolved,
        EventType::InboxDismissed,
        EventType::GuardianTriaged,
        EventType::GuardianHealRequested,
        EventType::GuardianEscalated,
        EventType::ApprovalRequested,
        EventType::ApprovalGranted,
        EventType::ApprovalConsumed,
        EventType::ApprovalInvalidated,
        EventType::ApprovalExpired,
        EventType::ApprovalDenied,
        EventType::OutboxEnqueued,
        EventType::OutboxExecuting,
        EventType::OutboxSucceeded,
        EventType::OutboxFailed,
        EventType::OutboxCancelled,
        EventType::Forge(ForgeEventKind::PrOpened),
        EventType::Forge(ForgeEventKind::PrReopened),
        EventType::Forge(ForgeEventKind::PrSynchronized),
        EventType::Forge(ForgeEventKind::PrUpdated),
        EventType::Forge(ForgeEventKind::PrClosed),
        EventType::Forge(ForgeEventKind::PrMerged),
        EventType::Forge(ForgeEventKind::BaseAdvanced),
        EventType::Forge(ForgeEventKind::PrEnqueued),
        EventType::Forge(ForgeEventKind::PrDequeued),
        EventType::Forge(ForgeEventKind::MergeGroupChecksRequested),
        EventType::Forge(ForgeEventKind::MergeGroupDestroyed),
        EventType::Forge(ForgeEventKind::CheckSuiteCompleted),
        EventType::Forge(ForgeEventKind::CheckRunCompleted),
        EventType::Forge(ForgeEventKind::ReviewSubmitted),
        EventType::Forge(ForgeEventKind::ReviewDismissed),
        EventType::Forge(ForgeEventKind::ReviewComment),
        EventType::Forge(ForgeEventKind::ReviewThreadChanged),
        EventType::Forge(ForgeEventKind::IssueComment),
        EventType::Forge(ForgeEventKind::IssueClosed),
        EventType::Forge(ForgeEventKind::IssueReopened),
        EventType::Forge(ForgeEventKind::IssueUpdated),
        EventType::Forge(ForgeEventKind::InstallationChanged),
        EventType::Forge(ForgeEventKind::RepoChanged),
        EventType::ForgeSynced,
        EventType::ForgeDriftDetected,
        EventType::BudgetCharged,
        EventType::BudgetExhausted,
        EventType::BudgetLimitRaised,
        EventType::PolicyDenied,
        EventType::DevicePaired,
        EventType::DeviceUpdated,
        EventType::DeviceRevoked,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventType::ProjectAdded => "project.added",
            EventType::ProjectUpdated => "project.updated",
            EventType::SettingsUpdated => "settings.updated",
            EventType::ConversationCreated => "conversation.created",
            EventType::DeliveryCreated => "delivery.created",
            EventType::DeliveryStageChanged => "delivery.stage_changed",
            EventType::DeliveryLevelChanged => "delivery.level_changed",
            EventType::DeliveryAbandoned => "delivery.abandoned",
            EventType::DeliveryPaused => "delivery.paused",
            EventType::DeliveryResumed => "delivery.resumed",
            EventType::WorkflowCreated => "workflow.created",
            EventType::WorkflowRevised => "workflow.revised",
            EventType::WorkflowFinished => "workflow.finished",
            EventType::NodeQueued => "node.queued",
            EventType::NodeStarted => "node.started",
            EventType::NodeFinished => "node.finished",
            EventType::NodeSuperseded => "node.superseded",
            EventType::NodeRetryRequested => "node.retry_requested",
            EventType::AttemptStarted => "attempt.started",
            EventType::AttemptFinished => "attempt.finished",
            EventType::AttemptProgress => "attempt.progress",
            EventType::PatchValidated => "patch.validated",
            EventType::PatchRejected => "patch.rejected",
            EventType::SandboxCreated => "sandbox.created",
            EventType::SandboxDestroyed => "sandbox.destroyed",
            EventType::WorktreeCreated => "worktree.created",
            EventType::WorktreeLocked => "worktree.locked",
            EventType::WorktreeUnlocked => "worktree.unlocked",
            EventType::WorktreeRemoved => "worktree.removed",
            EventType::MessagePosted => "message.posted",
            EventType::InboxRaised => "inbox.raised",
            EventType::InboxResolved => "inbox.resolved",
            EventType::InboxDismissed => "inbox.dismissed",
            EventType::GuardianTriaged => "guardian.triaged",
            EventType::GuardianHealRequested => "guardian.heal_requested",
            EventType::GuardianEscalated => "guardian.escalated",
            EventType::ApprovalRequested => "approval.requested",
            EventType::ApprovalGranted => "approval.granted",
            EventType::ApprovalConsumed => "approval.consumed",
            EventType::ApprovalInvalidated => "approval.invalidated",
            EventType::ApprovalExpired => "approval.expired",
            EventType::ApprovalDenied => "approval.denied",
            EventType::OutboxEnqueued => "outbox.enqueued",
            EventType::OutboxExecuting => "outbox.executing",
            EventType::OutboxSucceeded => "outbox.succeeded",
            EventType::OutboxFailed => "outbox.failed",
            EventType::OutboxCancelled => "outbox.cancelled",
            EventType::Forge(kind) => forge_name(kind),
            EventType::ForgeSynced => "forge.synced",
            EventType::ForgeDriftDetected => "forge.drift_detected",
            EventType::BudgetCharged => "budget.charged",
            EventType::BudgetExhausted => "budget.exhausted",
            EventType::BudgetLimitRaised => "budget.limit_raised",
            EventType::PolicyDenied => "policy.denied",
            EventType::DevicePaired => "device.paired",
            EventType::DeviceUpdated => "device.updated",
            EventType::DeviceRevoked => "device.revoked",
        }
    }

    pub fn is_forge(self) -> bool {
        matches!(self, EventType::Forge(_))
    }
}

/// `forge.<kind>` 的名称是拼接结果；catalog 用 `'static` 字符串，因此预先列 23 条。
fn forge_name(kind: ForgeEventKind) -> &'static str {
    match kind {
        ForgeEventKind::PrOpened => "forge.pr_opened",
        ForgeEventKind::PrReopened => "forge.pr_reopened",
        ForgeEventKind::PrSynchronized => "forge.pr_synchronized",
        ForgeEventKind::PrUpdated => "forge.pr_updated",
        ForgeEventKind::PrClosed => "forge.pr_closed",
        ForgeEventKind::PrMerged => "forge.pr_merged",
        ForgeEventKind::BaseAdvanced => "forge.base_advanced",
        ForgeEventKind::PrEnqueued => "forge.pr_enqueued",
        ForgeEventKind::PrDequeued => "forge.pr_dequeued",
        ForgeEventKind::MergeGroupChecksRequested => "forge.merge_group_checks_requested",
        ForgeEventKind::MergeGroupDestroyed => "forge.merge_group_destroyed",
        ForgeEventKind::CheckSuiteCompleted => "forge.check_suite_completed",
        ForgeEventKind::CheckRunCompleted => "forge.check_run_completed",
        ForgeEventKind::ReviewSubmitted => "forge.review_submitted",
        ForgeEventKind::ReviewDismissed => "forge.review_dismissed",
        ForgeEventKind::ReviewComment => "forge.review_comment",
        ForgeEventKind::ReviewThreadChanged => "forge.review_thread_changed",
        ForgeEventKind::IssueComment => "forge.issue_comment",
        ForgeEventKind::IssueClosed => "forge.issue_closed",
        ForgeEventKind::IssueReopened => "forge.issue_reopened",
        ForgeEventKind::IssueUpdated => "forge.issue_updated",
        ForgeEventKind::InstallationChanged => "forge.installation_changed",
        ForgeEventKind::RepoChanged => "forge.repo_changed",
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventType {
    type Err = EventTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = value.strip_prefix("forge.") {
            if let Some(kind) = ForgeEventKind::parse_opt(rest) {
                return Ok(EventType::Forge(kind));
            }
        }
        EventType::ALL
            .into_iter()
            .find(|ty| !ty.is_forge() && ty.as_str() == value)
            .ok_or(EventTypeError::Unknown)
    }
}

impl Serialize for EventType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for EventType {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EventType".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        let names: Vec<&str> = EventType::ALL.iter().map(|ty| ty.as_str()).collect();
        schemars::json_schema!({
            "type": "string",
            "enum": names,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EventTypeError {
    #[error("unknown event type")]
    Unknown,
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct EventTypeInfo {
    pub name: &'static str,
    pub emitter: &'static [&'static str],
    pub subject: SubjectKind,
}

macro_rules! info {
    ($name:literal, [$($emitter:literal),*], $subject:ident) => {
        EventTypeInfo {
            name: $name,
            emitter: &[$($emitter),*],
            subject: SubjectKind::$subject,
        }
    };
}

/// data-model §5.2 的全部类型（55 个固定 + 23 个 `forge.<kind>`）。
pub const EVENT_TYPES: &[EventTypeInfo] = &[
    info!("project.added", ["api"], Project),
    info!("project.updated", ["forge", "api"], Project),
    info!("settings.updated", ["api"], ProjectOrSettings),
    info!("conversation.created", ["conversation"], Conversation),
    info!("delivery.created", ["engine"], Delivery),
    info!("delivery.stage_changed", ["engine"], Delivery),
    info!("delivery.level_changed", ["engine", "api"], Delivery),
    info!("delivery.abandoned", ["engine", "api"], Delivery),
    info!("delivery.paused", ["engine", "api"], Delivery),
    info!("delivery.resumed", ["engine", "api"], Delivery),
    info!("workflow.created", ["engine"], Workflow),
    info!("workflow.revised", ["engine"], Workflow),
    info!("workflow.finished", ["engine"], Workflow),
    info!("node.queued", ["engine"], Node),
    info!("node.started", ["engine"], Node),
    info!("node.finished", ["engine"], Node),
    info!("node.superseded", ["engine"], Node),
    info!("node.retry_requested", ["engine", "api"], Node),
    info!("attempt.started", ["engine"], Attempt),
    info!("attempt.finished", ["engine"], Attempt),
    info!("attempt.progress", ["sandbox", "engine"], Attempt),
    info!("patch.validated", ["repo"], Undefined),
    info!("patch.rejected", ["repo"], Undefined),
    info!("sandbox.created", ["sandbox"], Sandbox),
    info!("sandbox.destroyed", ["sandbox"], Sandbox),
    info!("worktree.created", ["repo"], Worktree),
    info!("worktree.locked", ["repo", "api"], Worktree),
    info!("worktree.unlocked", ["repo", "api"], Worktree),
    info!("worktree.removed", ["repo"], Worktree),
    info!("message.posted", ["conversation"], Message),
    info!("inbox.raised", ["notify"], Inbox),
    info!("inbox.resolved", ["notify"], Inbox),
    info!("inbox.dismissed", ["notify"], Inbox),
    info!("guardian.triaged", ["guardian"], Undefined),
    info!("guardian.heal_requested", ["guardian"], Undefined),
    info!("guardian.escalated", ["guardian"], Undefined),
    info!("approval.requested", ["policy", "api"], Approval),
    info!("approval.granted", ["policy"], Approval),
    info!("approval.consumed", ["policy"], Approval),
    info!("approval.invalidated", ["policy"], Approval),
    info!("approval.expired", ["policy"], Approval),
    info!("approval.denied", ["api", "policy"], Approval),
    info!("outbox.enqueued", ["forge"], Action),
    info!("outbox.executing", ["forge"], Action),
    info!("outbox.succeeded", ["forge"], Action),
    info!("outbox.failed", ["forge"], Action),
    info!("outbox.cancelled", ["forge"], Action),
    info!("forge.pr_opened", ["forge"], Undefined),
    info!("forge.pr_reopened", ["forge"], Undefined),
    info!("forge.pr_synchronized", ["forge"], Undefined),
    info!("forge.pr_updated", ["forge"], Undefined),
    info!("forge.pr_closed", ["forge"], Undefined),
    info!("forge.pr_merged", ["forge"], Undefined),
    info!("forge.base_advanced", ["forge"], Undefined),
    info!("forge.pr_enqueued", ["forge"], Undefined),
    info!("forge.pr_dequeued", ["forge"], Undefined),
    info!("forge.merge_group_checks_requested", ["forge"], Undefined),
    info!("forge.merge_group_destroyed", ["forge"], Undefined),
    info!("forge.check_suite_completed", ["forge"], Undefined),
    info!("forge.check_run_completed", ["forge"], Undefined),
    info!("forge.review_submitted", ["forge"], Undefined),
    info!("forge.review_dismissed", ["forge"], Undefined),
    info!("forge.review_comment", ["forge"], Undefined),
    info!("forge.review_thread_changed", ["forge"], Undefined),
    info!("forge.issue_comment", ["forge"], Undefined),
    info!("forge.issue_closed", ["forge"], Undefined),
    info!("forge.issue_reopened", ["forge"], Undefined),
    info!("forge.issue_updated", ["forge"], Undefined),
    info!("forge.installation_changed", ["forge"], Undefined),
    info!("forge.repo_changed", ["forge"], Undefined),
    info!("forge.synced", ["forge"], Undefined),
    info!("forge.drift_detected", ["forge"], Undefined),
    info!("budget.charged", ["engine", "models"], Undefined),
    info!("budget.exhausted", ["engine", "models"], Undefined),
    info!("budget.limit_raised", ["engine"], Undefined),
    info!("policy.denied", ["policy"], Undefined),
    info!("device.paired", ["pki"], Device),
    info!("device.updated", ["pki"], Device),
    info!("device.revoked", ["pki"], Device),
];

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaErrorCode {
    /// `data` 不是对象、类型不符等。
    InvalidData,
    /// 字段缺失 / 多余 / 枚举值未知。
    Shape,
    /// 权威契约尚未定义该事件的数据形状；M0 拒绝。
    ContractUndefined,
    /// `settings.updated` 的 SecretRef 位置带 `value`。
    SecretValueForbidden,
    /// 字段之间的一致性规则不成立。
    Consistency,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code:?} at {pointer}: {message}")]
pub struct SchemaError {
    pub code: SchemaErrorCode,
    pub pointer: String,
    pub message: String,
    /// 被拒绝的 schema 标识；`ContractUndefined` 必填。
    pub schema: Option<String>,
}

impl SchemaError {
    pub fn new(
        code: SchemaErrorCode,
        pointer: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            pointer: pointer.into(),
            message: message.into(),
            schema: None,
        }
    }

    pub fn shape(pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(SchemaErrorCode::Shape, pointer, message)
    }

    pub fn consistency(pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(SchemaErrorCode::Consistency, pointer, message)
    }

    pub fn undefined(ty: EventType) -> Self {
        Self {
            code: SchemaErrorCode::ContractUndefined,
            pointer: String::new(),
            message: format!(
                "{} has no contract-defined data shape; M0 rejects it (see docs/m0-contract-gaps.md)",
                ty
            ),
            schema: Some(format!("schemas/events/{}.json", ty)),
        }
    }
}

fn from_serde(error: serde_json::Error, data: &Value) -> SchemaError {
    let message = error.to_string();
    let pointer = serde_pointer(&message, data);
    let code = if message.contains("unknown field")
        || message.contains("missing field")
        || message.contains("unknown variant")
        || message.contains("expected one of")
    {
        SchemaErrorCode::Shape
    } else {
        SchemaErrorCode::InvalidData
    };
    SchemaError::new(code, pointer, message)
}

/// 尽力从 serde 错误消息恢复 JSON Pointer；恢复不了就用根。
fn serde_pointer(message: &str, data: &Value) -> String {
    for marker in ["unknown field `", "missing field `"] {
        if let Some(start) = message.find(marker) {
            let rest = &message[start + marker.len()..];
            if let Some(field) = rest.split('`').next() {
                if let Some(path) = find_key_path(data, field) {
                    return format!("{path}/{field}");
                }
                return format!("/{field}");
            }
        }
    }
    if let Some(start) = message.find("unknown variant `") {
        let rest = &message[start + "unknown variant `".len()..];
        if let Some(variant) = rest.split('`').next() {
            if let Some(path) = find_string(data, variant) {
                return path;
            }
        }
    }
    if let Some(start) = message.find("invalid value: string \"") {
        let rest = &message[start + "invalid value: string \"".len()..];
        if let Some(value) = rest.split('"').next() {
            if let Some(path) = find_string(data, value) {
                return path;
            }
        }
    }
    String::new()
}

fn find_key_path(value: &Value, key: &str) -> Option<String> {
    fn walk(value: &Value, key: &str, pointer: &mut String, depth: u32) -> bool {
        if depth > 32 {
            return false;
        }
        match value {
            Value::Array(items) => items.iter().enumerate().any(|(index, item)| {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&index.to_string());
                let found = walk(item, key, pointer, depth + 1);
                if !found {
                    pointer.truncate(len);
                }
                found
            }),
            Value::Object(map) => {
                if map.contains_key(key) {
                    return true;
                }
                map.iter().any(|(name, item)| {
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&escape_pointer(name));
                    let found = walk(item, key, pointer, depth + 1);
                    if !found {
                        pointer.truncate(len);
                    }
                    found
                })
            }
            _ => false,
        }
    }
    let mut pointer = String::new();
    if walk(value, key, &mut pointer, 0) {
        Some(pointer)
    } else {
        None
    }
}

fn find_string(value: &Value, needle: &str) -> Option<String> {
    fn walk(value: &Value, needle: &str, pointer: &mut String, depth: u32) -> bool {
        if depth > 32 {
            return false;
        }
        match value {
            Value::String(text) if text == needle => true,
            Value::Array(items) => items.iter().enumerate().any(|(index, item)| {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&index.to_string());
                let found = walk(item, needle, pointer, depth + 1);
                if !found {
                    pointer.truncate(len);
                }
                found
            }),
            Value::Object(map) => map.iter().any(|(key, item)| {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&escape_pointer(key));
                let found = walk(item, needle, pointer, depth + 1);
                if !found {
                    pointer.truncate(len);
                }
                found
            }),
            _ => false,
        }
    }
    let mut pointer = String::new();
    if walk(value, needle, &mut pointer, 0) {
        Some(pointer)
    } else {
        None
    }
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn deny_keys(data: &Value, allowed: &[&str]) -> Result<(), SchemaError> {
    let Some(map) = data.as_object() else {
        return Err(SchemaError::new(
            SchemaErrorCode::InvalidData,
            "",
            "data must be a JSON object",
        ));
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(SchemaError::shape(
                format!("/{}", escape_pointer(key)),
                format!("unknown field `{key}`"),
            ));
        }
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(data: &Value) -> Result<T, SchemaError> {
    serde_json::from_value(data.clone()).map_err(|error| from_serde(error, data))
}

fn pointer_of(base: &str, key: &str) -> String {
    if base.is_empty() {
        format!("/{key}")
    } else {
        format!("{base}/{key}")
    }
}

fn object_at<'a>(
    value: &'a Value,
    pointer: &str,
) -> Result<&'a serde_json::Map<String, Value>, SchemaError> {
    value.as_object().ok_or_else(|| {
        SchemaError::new(
            SchemaErrorCode::InvalidData,
            pointer,
            "value must be a JSON object",
        )
    })
}

fn require_keys_at(value: &Value, pointer: &str, keys: &[&str]) -> Result<(), SchemaError> {
    let map = object_at(value, pointer)?;
    for key in keys {
        if !map.contains_key(*key) {
            return Err(SchemaError::shape(
                pointer_of(pointer, key),
                format!("missing field `{key}`"),
            ));
        }
    }
    Ok(())
}

fn deny_keys_at(value: &Value, pointer: &str, allowed: &[&str]) -> Result<(), SchemaError> {
    let map = object_at(value, pointer)?;
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(SchemaError::shape(
                pointer_of(pointer, &escape_pointer(key)),
                format!("unknown field `{key}`"),
            ));
        }
    }
    Ok(())
}

fn reject_null_keys_at(value: &Value, pointer: &str, keys: &[&str]) -> Result<(), SchemaError> {
    let map = object_at(value, pointer)?;
    for key in keys {
        if map.get(*key).is_some_and(Value::is_null) {
            return Err(SchemaError::shape(
                pointer_of(pointer, key),
                format!("field `{key}` must be omitted, not null"),
            ));
        }
    }
    Ok(())
}

fn nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

fn require_non_empty(value: &str, pointer: &str) -> Result<(), SchemaError> {
    if value.is_empty() {
        return Err(SchemaError::shape(pointer, "value must not be empty"));
    }
    Ok(())
}

fn reject_plain_text(value: &str, pointer: &str, limit: usize) -> Result<(), SchemaError> {
    if value.chars().count() > limit {
        return Err(SchemaError::shape(
            pointer,
            format!("text must be at most {limit} Unicode scalar values"),
        ));
    }
    for ch in value.chars() {
        let blocked = ch == '\u{1b}'
            || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            || (ch.is_control() && ch != '\n' && ch != '\t');
        if blocked {
            return Err(SchemaError::shape(
                pointer,
                "text must not contain ANSI, control, or bidi characters",
            ));
        }
    }
    Ok(())
}

fn ensure_sorted_unique<T: Ord>(values: &[T], pointer: &str) -> Result<(), SchemaError> {
    for pair in values.windows(2) {
        if pair[0] >= pair[1] {
            return Err(SchemaError::consistency(
                pointer,
                "values must be sorted and unique",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// EventData
// ---------------------------------------------------------------------------

/// 可由 [`decode_event_data`] 解码的 data 结构。
pub trait EventData: Sized + Serialize + DeserializeOwned {
    /// 该结构允许的事件类型；`node.*` 等共享结构有多个。
    const TYPES: &'static [EventType];
    fn validate(&self) -> Result<(), SchemaError> {
        Ok(())
    }
}

/// 解码并要求 `D::TYPES` 与 `ev.ty` 相符。
pub fn decode_event_data<D: EventData>(ev: &Event<Value>) -> Result<D, SchemaError> {
    if !D::TYPES.contains(&ev.ty) {
        return Err(SchemaError::consistency(
            "/type",
            format!(
                "{} cannot decode event type {}",
                std::any::type_name::<D>(),
                ev.ty
            ),
        ));
    }
    let value: D = decode(&ev.data)?;
    value.validate()?;
    Ok(value)
}

// ---------------------------------------------------------------------------
// data 结构
// ---------------------------------------------------------------------------

/// `project.added` / `project.updated`：data-model §3 的实体字段。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectEventData {
    pub id: ProjectId,
    pub repo: RepoSlug,
    pub installation_id: i64,
    pub bot_user_id: i64,
    pub default_branch: String,
    pub allowed_merge_methods: Vec<crate::entities::MergeMethod>,
    pub merge_queue_enabled: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl EventData for ProjectEventData {
    const TYPES: &'static [EventType] = &[EventType::ProjectAdded, EventType::ProjectUpdated];
}

/// `conversation.created`：data-model §3 的 `Conversation`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationEventData {
    pub id: ConversationId,
    pub project_id: ProjectId,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl EventData for ConversationEventData {
    const TYPES: &'static [EventType] = &[EventType::ConversationCreated];
}

/// `settings.updated`（config.md §2.3）。`value` 是已剥离 secret 的稀疏文档；
/// 项目 `DELETE` 时为 `null`，全局写入不允许 `null`。字段必须存在：反序列化
/// 区分“缺失”（拒绝）与显式 `null`（仅项目 DELETE 允许）。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SettingsUpdatedData {
    pub scope: SettingsScope,
    pub resource: SettingsResource,
    pub schema_version: u32,
    pub base_rev: Seq,
    pub value: Option<Value>,
    pub relaxes: Vec<KeyPath>,
    pub secret_changes: Vec<SecretChange>,
}

/// 字段出现时返回 `Some(value)`（显式 `null` 为 `Some(None)`）；缺失时由
/// `#[serde(default)]` 得到 `None`。普通 `Option<Option<T>>` 会把两者都折叠成
/// `None`，无法拒绝缺失的 `value`。
fn deserialize_present_value<'de, D>(deserializer: D) -> Result<Option<Option<Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::<Value>::deserialize(deserializer)?))
}

/// 区分“字段缺失”（`None`）与显式 `null`（`Some(None)`）。
fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

impl<'de> Deserialize<'de> for SettingsUpdatedData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            scope: SettingsScope,
            resource: SettingsResource,
            schema_version: u32,
            base_rev: Seq,
            #[serde(default, deserialize_with = "deserialize_present_value")]
            value: Option<Option<Value>>,
            relaxes: Vec<KeyPath>,
            secret_changes: Vec<SecretChange>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let value = wire
            .value
            .ok_or_else(|| serde::de::Error::missing_field("value"))?;
        Ok(Self {
            scope: wire.scope,
            resource: wire.resource,
            schema_version: wire.schema_version,
            base_rev: wire.base_rev,
            value,
            relaxes: wire.relaxes,
            secret_changes: wire.secret_changes,
        })
    }
}

/// 只用于生成 JSON Schema 的字段镜像；语义与 [`SettingsUpdatedData`] 相同。
#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct SettingsUpdatedFields {
    scope: SettingsScope,
    resource: SettingsResource,
    schema_version: u32,
    base_rev: Seq,
    value: Option<Value>,
    relaxes: Vec<KeyPath>,
    secret_changes: Vec<SecretChange>,
}

const SETTINGS_RESOURCES: [SettingsResource; 8] = [
    SettingsResource::Models,
    SettingsResource::Levels,
    SettingsResource::Policies,
    SettingsResource::Guardian,
    SettingsResource::Sandbox,
    SettingsResource::Egress,
    SettingsResource::Notify,
    SettingsResource::Github,
];

impl JsonSchema for SettingsUpdatedData {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SettingsUpdatedData".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let mut fields = crate::entities::inline_fields::<SettingsUpdatedFields>(generator);
        fields.remove("description");
        fields.insert(
            "description".to_owned(),
            Value::String(
                "`settings.updated`（config.md §2.3）。`value` 是已剥离 secret 的稀疏文档且必须存在；全局写入不得为 `null`，项目 `DELETE` 时为 `null`。"
                    .to_owned(),
            ),
        );
        fields.insert(
            "title".to_owned(),
            Value::String("SettingsUpdatedData".to_owned()),
        );
        // `value` 不能是可选属性：缺失必须拒绝，global 必须是文档，只有
        // project DELETE 的显式 `null` 合法。顶层只声明字段存在，具体形状由
        // scope × resource 的 16 个分支约束。
        if let Some(properties) = fields.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert("value".to_owned(), Value::Object(Default::default()));
        }
        let required = fields
            .entry("required".to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(required) = required.as_array_mut() {
            if !required.iter().any(|field| field == "value") {
                required.push(Value::String("value".to_owned()));
            }
        }
        let mut branches = Vec::new();
        for resource in SETTINGS_RESOURCES {
            let mut value_schema = crate::settings::resource_schema(resource).to_value();
            merge_resource_definitions(generator, &value_schema);
            if let Some(object) = value_schema.as_object_mut() {
                object.remove("$defs");
            }
            let key = resource_key(resource);
            branches.push(serde_json::json!({
                "properties": {
                    "scope": { "const": "global" },
                    "resource": { "const": key },
                    "value": value_schema
                },
                "required": ["scope", "resource", "value"]
            }));
            let project_value = serde_json::json!({
                "anyOf": [value_schema, { "type": "null" }]
            });
            branches.push(serde_json::json!({
                "properties": {
                    "scope": { "const": "project" },
                    "resource": { "const": key },
                    "value": project_value
                },
                "required": ["scope", "resource", "value"]
            }));
        }
        for definition in generator.definitions_mut().values_mut() {
            if definition.get("x-gantry-secret") == Some(&Value::Bool(true)) {
                if let Some(properties) = definition
                    .get_mut("properties")
                    .and_then(Value::as_object_mut)
                {
                    // SecretRef 的读形状只有 secret_ref / present；事件日志不允许任何 value，
                    // 包括 null。写入侧 SecretRefWrite 的 schema 由同一批资源类型派生。
                    properties.insert("value".to_owned(), Value::Bool(false));
                }
            }
        }
        fields.insert("oneOf".to_owned(), Value::Array(branches));
        Schema::from(fields)
    }
}

fn merge_resource_definitions(generator: &mut schemars::SchemaGenerator, schema: &Value) {
    let Some(definitions) = schema.get("$defs").and_then(Value::as_object) else {
        return;
    };
    let target = generator.definitions_mut();
    for (name, definition) in definitions {
        match target.get(name) {
            None => {
                target.insert(name.clone(), definition.clone());
            }
            Some(existing) => assert_eq!(
                existing, definition,
                "settings resource schema definition `{name}` conflicts across resources"
            ),
        }
    }
}

impl SettingsUpdatedData {
    /// `value` 是 resource 内部文档；非 `null` 时按该 resource 的写形状解析，
    /// 拒绝未知字段、错误类型与嵌套 `null`。根 `null` 只在项目 DELETE 合法；
    /// 全局写入的 `null` 拒绝。
    fn check_value(&self) -> Result<(), SchemaError> {
        if self.value.as_ref().is_none_or(Value::is_null) {
            return match self.scope {
                SettingsScope::Global => Err(SchemaError::shape(
                    "/value",
                    "global settings.updated requires a non-null value",
                )),
                SettingsScope::Project => Ok(()),
            };
        }
        let value = self
            .value
            .as_ref()
            .expect("check_value handled null and absent values");
        if let Err(pointer) = crate::settings::reject_null_storage(value) {
            return Err(SchemaError::shape(
                pointer,
                "null is not allowed in a stored settings document; remove the key to clear it",
            ));
        }
        crate::settings::parse_patch(self.resource, value)
            .map_err(|error| from_serde(error, value))?;
        Ok(())
    }

    fn check_secret_values(&self) -> Result<(), SchemaError> {
        let Some(value) = &self.value else {
            return Ok(());
        };
        for pointer in secret_positions(self.resource, value) {
            let pointer_text = pointer_string(&pointer);
            let Some(object) = pointer_value(value, &pointer).and_then(Value::as_object) else {
                return Err(SchemaError::shape(
                    pointer_text,
                    "SecretRef must be an object",
                ));
            };
            if object.contains_key("value") {
                return Err(SchemaError::new(
                    SchemaErrorCode::SecretValueForbidden,
                    format!("{pointer_text}/value"),
                    "SecretRef positions must not carry \"value\"; secret values never enter the event log",
                ));
            }
            match object.get("secret_ref") {
                Some(Value::String(name)) if name.parse::<crate::secret::SecretName>().is_ok() => {}
                Some(Value::String(_)) => {
                    return Err(SchemaError::shape(
                        format!("{pointer_text}/secret_ref"),
                        "secret_ref must match ^[a-z0-9][a-z0-9._-]{0,63}$",
                    ))
                }
                Some(_) => {
                    return Err(SchemaError::shape(
                        format!("{pointer_text}/secret_ref"),
                        "secret_ref must be a string",
                    ))
                }
                None => {
                    return Err(SchemaError::shape(
                        format!("{pointer_text}/secret_ref"),
                        "secret_ref is required",
                    ))
                }
            }
            if object
                .get("present")
                .is_some_and(|present| !present.is_boolean())
            {
                return Err(SchemaError::shape(
                    format!("{pointer_text}/present"),
                    "present must be a boolean",
                ));
            }
        }
        Ok(())
    }
}

impl EventData for SettingsUpdatedData {
    const TYPES: &'static [EventType] = &[EventType::SettingsUpdated];

    fn validate(&self) -> Result<(), SchemaError> {
        self.check_secret_values()?;
        self.check_value()
    }
}

/// `secret_changes[]` 的元素：只有名字、操作与版本号（无值、无哈希）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretChange {
    #[serde(rename = "ref")]
    pub secret_ref: crate::secret::SecretName,
    pub op: SecretChangeOp,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretChangeOp {
    Set,
    Rotate,
    Delete,
}

/// 配置文档里的 SecretRef 位置（config.md §2.4）。
///
/// `value` 是 resource 内部文档（如 `models` 的 `providers.*.credential`），
/// 因此按 `resource` 的真实形状定位，而不是在 `value` 里再找 resource 前缀。
/// 同时兼容历史 / 夹具里仍带 resource 前缀的形状：只有顶层键恰好等于该
/// resource 名时才检查，避免按任意键名扫描业务文本。
fn secret_positions(resource: SettingsResource, value: &Value) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    collect_secret_positions(resource, value, &mut found);
    let prefix = resource_key(resource);
    if let Some(nested) = value.as_object().and_then(|root| root.get(prefix)) {
        let mut nested_found = Vec::new();
        collect_secret_positions(resource, nested, &mut nested_found);
        for mut pointer in nested_found {
            pointer.insert(0, prefix.to_owned());
            found.push(pointer);
        }
    }
    found
}

fn collect_secret_positions(
    resource: SettingsResource,
    value: &Value,
    found: &mut Vec<Vec<String>>,
) {
    let Some(root) = value.as_object() else {
        return;
    };
    match resource {
        SettingsResource::Models => {
            if let Some(providers) = root.get("providers").and_then(Value::as_object) {
                for id in providers.keys() {
                    found.push(vec!["providers".into(), id.clone(), "credential".into()]);
                }
            }
        }
        SettingsResource::Egress => {
            if let Some(injections) = root.get("injection").and_then(Value::as_object) {
                for id in injections.keys() {
                    found.push(vec!["injection".into(), id.clone(), "credential".into()]);
                }
            }
        }
        SettingsResource::Github => {
            if let Some(app) = root.get("app").and_then(Value::as_object) {
                for key in ["private_key", "webhook_secret", "webhook_secret_previous"] {
                    if app.contains_key(key) {
                        found.push(vec!["app".into(), key.into()]);
                    }
                }
            }
        }
        _ => {}
    }
}

fn resource_key(resource: SettingsResource) -> &'static str {
    match resource {
        SettingsResource::Models => "models",
        SettingsResource::Levels => "levels",
        SettingsResource::Policies => "policies",
        SettingsResource::Guardian => "guardian",
        SettingsResource::Sandbox => "sandbox",
        SettingsResource::Egress => "egress",
        SettingsResource::Notify => "notify",
        SettingsResource::Github => "github",
    }
}

fn pointer_string(pointer: &[String]) -> String {
    pointer
        .iter()
        .map(|part| format!("/{}", escape_pointer(part)))
        .collect()
}

fn pointer_value<'a>(value: &'a Value, pointer: &[String]) -> Option<&'a Value> {
    let mut current = value;
    for part in pointer {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

/// `message.posted`：完整 `Message`（api.md §4.5）。
impl EventData for Message {
    const TYPES: &'static [EventType] = &[EventType::MessagePosted];
}

// `Card` / `Ref` 已在实体层定义；这里只需要键表用于未知字段检查。
const MESSAGE_KEYS: &[&str] = &[
    "id",
    "conversation_id",
    "role",
    "agent_kind",
    "tone",
    "text",
    "level_cap",
    "refs",
    "cards",
    "delivery_id",
    "created_at",
    "updated_at",
];

// ---------------------------------------------------------------------------
// delivery
// ---------------------------------------------------------------------------

/// `delivery.created`（engine.md §11 的最小字段；core §7.5 仍记录完整交付单元缺口）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryCreatedData {
    pub title: String,
    pub level_cap: Level,
    pub level_effective: Level,
    pub origin: DeliveryOrigin,
    #[serde(default)]
    pub source_message_id: Option<MessageId>,
    pub issues: Vec<IssueRef>,
    pub budget: DeliveryBudget,
    pub config_rev: Seq,
}

impl EventData for DeliveryCreatedData {
    const TYPES: &'static [EventType] = &[EventType::DeliveryCreated];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.level_cap == Level::None {
            return Err(SchemaError::consistency(
                "/level_cap",
                "Level::None is not a valid delivery cap",
            ));
        }
        if self.level_effective > self.level_cap {
            return Err(SchemaError::consistency(
                "/level_effective",
                "level_effective must not exceed level_cap",
            ));
        }
        Ok(())
    }
}

/// `delivery.created.data.budget`：台账创建时固化的模板与限额（P-57）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryBudget {
    pub template: String,
    pub limits: BudgetLimits,
}

/// `delivery.stage_changed`：`from` / `to` 来自 api.md §5.1；
/// `reason` / `stage_status` 来自 engine.md §11。data-model §5.2 规定 data
/// 必须带新 `stage_status`；core §7.5 的 `trigger` 只是建议，不在 M0 数据形状中。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryStageChangedData {
    pub from: Stage,
    pub to: Stage,
    #[serde(default)]
    pub reason: Option<String>,
    pub stage_status: StageStatus,
}

impl EventData for DeliveryStageChangedData {
    const TYPES: &'static [EventType] = &[EventType::DeliveryStageChanged];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LevelChangeKind {
    Cap,
    Effective,
}

/// `delivery.level_changed`（engine.md §11）：`kind = effective` 只升（P-57）；`kind = cap`
/// 可升可降，应用时派生 `level_effective = min(原值, to)`，不另写 `effective` 下降事件（#48）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryLevelChangedData {
    pub from: Level,
    pub to: Level,
    pub kind: LevelChangeKind,
    pub reason: String,
}

impl EventData for DeliveryLevelChangedData {
    const TYPES: &'static [EventType] = &[EventType::DeliveryLevelChanged];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.from == self.to {
            return Err(SchemaError::consistency(
                "/to",
                "level_changed must change the value",
            ));
        }
        if self.kind == LevelChangeKind::Effective && self.to < self.from {
            return Err(SchemaError::consistency(
                "/to",
                "level_effective only rises (P-57)",
            ));
        }
        Ok(())
    }
}

/// `delivery.abandoned`：`reason`（api.md §4.4，≤ 500 字符）、`by`（engine.md §11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryAbandonedData {
    #[serde(default)]
    pub reason: Option<String>,
    pub by: String,
}

impl EventData for DeliveryAbandonedData {
    const TYPES: &'static [EventType] = &[EventType::DeliveryAbandoned];

    fn validate(&self) -> Result<(), SchemaError> {
        if let Some(reason) = &self.reason {
            if reason.chars().count() > 500 {
                return Err(SchemaError::shape(
                    "/reason",
                    "reason must be at most 500 characters",
                ));
            }
        }
        Ok(())
    }
}

/// `delivery.paused` / `delivery.resumed`（engine.md §11；P-56）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryPausedData {
    pub by: String,
    pub status_before: StageStatus,
}

impl EventData for DeliveryPausedData {
    const TYPES: &'static [EventType] = &[EventType::DeliveryPaused];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryResumedData {
    pub by: String,
    pub status_after: StageStatus,
}

impl EventData for DeliveryResumedData {
    const TYPES: &'static [EventType] = &[EventType::DeliveryResumed];
}

// ---------------------------------------------------------------------------
// workflow / node / attempt
// ---------------------------------------------------------------------------

/// `workflow.created` / `revised` 里的节点描述（engine.md §11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeDescriptor {
    pub node_id: NodeId,
    pub key: String,
    pub role: crate::entities::Role,
    pub depends_on: Vec<String>,
    pub required: bool,
    pub critical: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCreatedData {
    #[serde(default)]
    pub stage: Option<Stage>,
    pub template: String,
    pub graph_rev: u32,
    pub nodes: Vec<NodeDescriptor>,
    #[serde(default)]
    pub delivery_id: Option<DeliveryId>,
    #[serde(default)]
    pub conversation_id: Option<ConversationId>,
}

impl EventData for WorkflowCreatedData {
    const TYPES: &'static [EventType] = &[EventType::WorkflowCreated];

    fn validate(&self) -> Result<(), SchemaError> {
        match (&self.delivery_id, &self.conversation_id) {
            (Some(_), None) => Ok(()),
            (None, Some(_)) => Ok(()),
            (Some(_), Some(_)) => Err(SchemaError::consistency(
                "/conversation_id",
                "delivery_id and conversation_id are mutually exclusive",
            )),
            (None, None) => Err(SchemaError::consistency(
                "/delivery_id",
                "one of delivery_id or conversation_id is required (P-58)",
            )),
        }
    }
}

/// `workflow.revised`（engine.md §11）：`graph_rev` 单调递增，`added[]` 复用节点描述，
/// `superseded[]` 是被替换节点 ID（文档写“被替换节点”，未给元素类型）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRevisedData {
    pub graph_rev: u32,
    pub reason: String,
    pub round: u32,
    pub added: Vec<NodeDescriptor>,
    pub superseded: Vec<NodeId>,
}

impl EventData for WorkflowRevisedData {
    const TYPES: &'static [EventType] = &[EventType::WorkflowRevised];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.graph_rev < 2 {
            return Err(SchemaError::shape(
                "/graph_rev",
                "revised workflows start at graph_rev 2",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFinishedData {
    pub status: WorkflowStatus,
}

impl EventData for WorkflowFinishedData {
    const TYPES: &'static [EventType] = &[EventType::WorkflowFinished];

    fn validate(&self) -> Result<(), SchemaError> {
        if matches!(
            self.status,
            WorkflowStatus::Running | WorkflowStatus::Paused
        ) {
            return Err(SchemaError::consistency(
                "/status",
                "workflow.finished requires a terminal status",
            ));
        }
        Ok(())
    }
}

/// `node.queued` / `started` / `finished` / `superseded`（engine.md §11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeEventData {
    pub node_id: NodeId,
    pub status: NodeStatus,
    #[serde(default)]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub superseded_by: Option<NodeId>,
}

impl EventData for NodeEventData {
    const TYPES: &'static [EventType] = &[
        EventType::NodeQueued,
        EventType::NodeStarted,
        EventType::NodeFinished,
        EventType::NodeSuperseded,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeRetryRequestedData {
    pub node_id: NodeId,
    pub by: String,
}

impl EventData for NodeRetryRequestedData {
    const TYPES: &'static [EventType] = &[EventType::NodeRetryRequested];
}

/// `attempt.started`（engine.md §11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttemptStartedData {
    pub attempt_id: AttemptId,
    pub node_id: NodeId,
    pub n: u32,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Effort,
    pub config_rev: Seq,
    pub base_sha: GitOid,
    pub tokens_total: u64,
    pub wall_time_s: u64,
}

impl EventData for AttemptStartedData {
    const TYPES: &'static [EventType] = &[EventType::AttemptStarted];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.n == 0 {
            return Err(SchemaError::shape("/n", "attempt number starts at 1"));
        }
        Ok(())
    }
}

/// `attempt.finished`（engine.md §11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttemptFinishedData {
    pub status: crate::entities::AttemptStatus,
    #[serde(default)]
    pub result_ref: Option<Sha256Digest>,
    #[serde(default)]
    pub failure_class: Option<crate::entities::FailureClass>,
    #[serde(default)]
    pub failure_detail: Option<String>,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    #[serde(default)]
    pub result_tree: Option<GitOid>,
    /// 本地提交的 OID（engine.md §7.14.6 E1）：`push_branch.local_head_oid` 由它从日志重建。
    /// 与 `result_tree` 同时出现；`status = succeeded` 且有 `result_tree` 时必填。
    #[serde(default)]
    pub result_commit: Option<GitOid>,
    #[serde(default)]
    pub served_by: Option<ServedBy>,
    /// 被 `policy` 接受的 `open_pr` 提议的 `proposal_id`（engine.md §7.14.6 E5）。
    /// 执行器据 `result_ref` 从 CAS 取出提议原文填入 `create_pr` 意图，所以必须有 `result_ref`。
    #[serde(default)]
    pub accepted_open_pr: Option<String>,
}

impl EventData for AttemptFinishedData {
    const TYPES: &'static [EventType] = &[EventType::AttemptFinished];

    fn validate(&self) -> Result<(), SchemaError> {
        use crate::entities::{AttemptStatus, FailureClass};
        match self.status {
            AttemptStatus::Succeeded => {
                if self.failure_class.is_some() {
                    return Err(SchemaError::consistency(
                        "/failure_class",
                        "succeeded attempts must not carry failure_class",
                    ));
                }
                if self.result_tree.is_some() && self.result_commit.is_none() {
                    return Err(SchemaError::consistency(
                        "/result_commit",
                        "succeeded attempts with result_tree require result_commit",
                    ));
                }
            }
            AttemptStatus::Failed | AttemptStatus::Cancelled => {
                if self.failure_class.is_none() {
                    return Err(SchemaError::consistency(
                        "/failure_class",
                        "terminal failure requires failure_class",
                    ));
                }
                if self.status == AttemptStatus::Cancelled
                    && self.failure_class != Some(FailureClass::Cancelled)
                {
                    return Err(SchemaError::consistency(
                        "/failure_class",
                        "cancelled attempts require failure_class = cancelled",
                    ));
                }
            }
            _ => {
                return Err(SchemaError::consistency(
                    "/status",
                    "attempt.finished requires a terminal status",
                ))
            }
        }
        if self.result_commit.is_some() && self.result_tree.is_none() {
            return Err(SchemaError::consistency(
                "/result_tree",
                "result_commit requires result_tree",
            ));
        }
        if let Some(proposal_id) = &self.accepted_open_pr {
            require_non_empty(proposal_id, "/accepted_open_pr")?;
            if self.result_ref.is_none() {
                return Err(SchemaError::consistency(
                    "/result_ref",
                    "accepted_open_pr requires result_ref",
                ));
            }
        }
        Ok(())
    }
}

/// 实际使用的 provider / model / effort（models.md `ServedBy` 的文档化字段）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServedBy {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<Effort>,
}

// ---------------------------------------------------------------------------
// patch
// ---------------------------------------------------------------------------

/// `patch.validated`（repo.md §11）。复用 worker 协议的 `ValidatedPatch` 组成字段。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchValidatedData {
    pub job_id: AttemptId,
    pub sha256: Sha256Digest,
    pub base_sha: GitOid,
    pub result_tree: GitOid,
    pub verdict: PatchVerdict,
    pub files: Vec<PatchFile>,
    pub reasons: Vec<PatchReason>,
    pub config_rev: Seq,
}

impl EventData for PatchValidatedData {
    const TYPES: &'static [EventType] = &[EventType::PatchValidated];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.verdict == PatchVerdict::Deny {
            return Err(SchemaError::consistency(
                "/verdict",
                "patch.validated carries allow / needs_approval; deny uses patch.rejected",
            ));
        }
        Ok(())
    }
}

/// `patch.rejected`（repo.md §11）。机械拒绝只有 `reason`；`policy_denied` 另带
/// `reasons[]` 与 `config_rev`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchRejectedData {
    pub job_id: AttemptId,
    pub sha256: Sha256Digest,
    pub reason: PatchRejectReason,
    pub detail: String,
    #[serde(default)]
    pub reasons: Vec<PatchReason>,
    #[serde(default)]
    pub config_rev: Option<Seq>,
}

impl EventData for PatchRejectedData {
    const TYPES: &'static [EventType] = &[EventType::PatchRejected];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.reason == PatchRejectReason::PolicyDenied {
            if self.reasons.is_empty() {
                return Err(SchemaError::consistency(
                    "/reasons",
                    "policy_denied requires policy reasons",
                ));
            }
            if self.config_rev.is_none() {
                return Err(SchemaError::consistency(
                    "/config_rev",
                    "policy_denied requires config_rev",
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// sandbox / worktree
// ---------------------------------------------------------------------------

/// `sandbox.created`（sandbox.md §11）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxCreatedData {
    pub sandbox_id: SandboxId,
    pub attempt_id: AttemptId,
    pub driver: String,
    pub runtime: String,
    pub image_digest: Sha256Digest,
    pub limits: crate::entities::SandboxLimits,
    pub egress_profile: String,
    pub worktree_snapshot: crate::entities::WorktreeSnapshot,
    pub trust: crate::entities::SandboxTrust,
}

impl EventData for SandboxCreatedData {
    const TYPES: &'static [EventType] = &[EventType::SandboxCreated];
}

/// `sandbox.destroyed`（sandbox.md §11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxDestroyedData {
    pub sandbox_id: SandboxId,
    pub final_status: crate::entities::SandboxStatus,
    pub reason: String,
}

impl EventData for SandboxDestroyedData {
    const TYPES: &'static [EventType] = &[EventType::SandboxDestroyed];

    fn validate(&self) -> Result<(), SchemaError> {
        use crate::entities::SandboxStatus;
        if !matches!(
            self.final_status,
            SandboxStatus::Destroyed | SandboxStatus::Lost
        ) {
            return Err(SchemaError::consistency(
                "/final_status",
                "sandbox.destroyed final_status is destroyed or lost",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeCreatedData {
    pub worktree_id: WorktreeId,
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub branch: String,
    pub base_ref: String,
    pub base_sha: GitOid,
}

impl EventData for WorktreeCreatedData {
    const TYPES: &'static [EventType] = &[EventType::WorktreeCreated];
}

/// `worktree.locked`（repo.md §11）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeLockedData {
    pub worktree_id: WorktreeId,
    pub holder: crate::entities::LockHolder,
    pub delivery_id: DeliveryId,
    pub lease_until: Timestamp,
}

impl EventData for WorktreeLockedData {
    const TYPES: &'static [EventType] = &[EventType::WorktreeLocked];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeUnlockReason {
    Released,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeUnlockedData {
    pub worktree_id: WorktreeId,
    pub reason: WorktreeUnlockReason,
}

impl EventData for WorktreeUnlockedData {
    const TYPES: &'static [EventType] = &[EventType::WorktreeUnlocked];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeRemovedData {
    pub worktree_id: WorktreeId,
    pub branch: String,
}

impl EventData for WorktreeRemovedData {
    const TYPES: &'static [EventType] = &[EventType::WorktreeRemoved];
}

// ---------------------------------------------------------------------------
// inbox
// ---------------------------------------------------------------------------

/// `inbox.raised`（notify.md §7.3）：唯一线上形状是 `{item, supersedes?}`。
/// 扁平 `InboxItem` 不是已确认协议，M0 不再兼容。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxRaisedData {
    pub item: InboxItem,
    #[serde(default)]
    pub supersedes: Vec<InboxId>,
}

impl InboxRaisedData {
    pub fn item(&self) -> &InboxItem {
        &self.item
    }
}

impl EventData for InboxRaisedData {
    const TYPES: &'static [EventType] = &[EventType::InboxRaised];

    fn validate(&self) -> Result<(), SchemaError> {
        self.item
            .validate()
            .map_err(|error| SchemaError::consistency("/item", error.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxResolvedData {
    pub inbox_id: InboxId,
    pub resolution: InboxResolution,
    #[serde(default)]
    pub option_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub message_id: Option<MessageId>,
    #[serde(default)]
    pub approval_id: Option<ApprovalId>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl EventData for InboxResolvedData {
    const TYPES: &'static [EventType] = &[EventType::InboxResolved];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.resolution == InboxResolution::Chosen && self.option_id.is_none() {
            return Err(SchemaError::consistency(
                "/option_id",
                "resolution = chosen requires option_id",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InboxResolution {
    Chosen,
    Answered,
    ApprovalSettled,
    DeliveryClosed,
    DeliveryAbandoned,
    Superseded,
    External,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxDismissedData {
    pub inbox_id: InboxId,
    #[serde(default)]
    pub note: Option<String>,
}

impl EventData for InboxDismissedData {
    const TYPES: &'static [EventType] = &[EventType::InboxDismissed];
}

// ---------------------------------------------------------------------------
// approval
// ---------------------------------------------------------------------------

/// `approval.requested`（policy.md §7.3）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequestedData {
    pub action: ApprovalAction,
    pub target: ApprovalTarget,
    pub subject_digest: SubjectDigest,
    pub expires_at: Timestamp,
    pub config_rev: Seq,
    pub requested_by: Actor,
}

/// 只用于生成 JSON Schema 的字段镜像；语义与 [`ApprovalRequestedData`] 相同。
#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ApprovalRequestedFields {
    action: ApprovalAction,
    target: ApprovalTarget,
    subject_digest: SubjectDigest,
    expires_at: Timestamp,
    config_rev: Seq,
    requested_by: Actor,
}

impl JsonSchema for ApprovalRequestedData {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ApprovalRequestedData".into()
    }

    /// 与 [`crate::entities::Approval`] 共用 `APPROVAL_SHAPES` 投影；事件 schema 不再
    /// 只校验三个字段各自的枚举/格式，而是要求 action、target.kind、subject_digest 同时符合。
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let mut fields = crate::entities::inline_fields::<ApprovalRequestedFields>(generator);
        fields.remove("description");
        fields.insert(
            "description".to_owned(),
            Value::String("`approval.requested`（policy.md §7.3）。".to_owned()),
        );
        fields.insert(
            "title".to_owned(),
            Value::String("ApprovalRequestedData".to_owned()),
        );
        fields.insert(
            "oneOf".to_owned(),
            Value::Array(crate::entities::approval_shape_branches()),
        );
        Schema::from(fields)
    }
}

impl EventData for ApprovalRequestedData {
    const TYPES: &'static [EventType] = &[EventType::ApprovalRequested];

    fn validate(&self) -> Result<(), SchemaError> {
        let shape = self
            .action
            .shape()
            .ok_or_else(|| SchemaError::consistency("/action", "approval action has no shape"))?;
        if shape.target_kind != self.target.kind() {
            return Err(SchemaError::consistency(
                "/target",
                "target does not match the approval action shape",
            ));
        }
        self.target
            .validate()
            .map_err(|error| SchemaError::consistency("/target", error.to_string()))?;
        if shape.subject_kind != self.subject_digest.kind() {
            return Err(SchemaError::consistency(
                "/subject_digest",
                "subject_digest does not match the approval action shape",
            ));
        }
        Ok(())
    }
}

/// `approval.granted`（policy.md §7.5 第 6 步）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalGrantedData {
    pub approval_id: ApprovalId,
    pub device_id: DeviceId,
    pub approval_pubkey_fpr: Sha256Digest,
    pub payload_sha256: Sha256Digest,
}

impl EventData for ApprovalGrantedData {
    const TYPES: &'static [EventType] = &[EventType::ApprovalGranted];
}

/// `approval.consumed`（policy.md §7.5；P-54）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConsumedData {
    pub approval_id: ApprovalId,
    pub consumed_by: String,
    pub subject_digest: SubjectDigest,
}

impl EventData for ApprovalConsumedData {
    const TYPES: &'static [EventType] = &[EventType::ApprovalConsumed];
}

// ---------------------------------------------------------------------------
// outbox
// ---------------------------------------------------------------------------

/// `outbox.succeeded`（core §7.5）。`github_ref` 形状见 forge-actions §4.2。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxSucceededData {
    pub github_ref: GithubRef,
}

impl EventData for OutboxSucceededData {
    const TYPES: &'static [EventType] = &[EventType::OutboxSucceeded];
}

/// `outbox.failed`（forge-actions §2.6）：只在终态发；`code` 取顶层失败码。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxFailedData {
    pub code: OutboxFailureCode,
    #[serde(default)]
    pub message: Option<String>,
}

impl EventData for OutboxFailedData {
    const TYPES: &'static [EventType] = &[EventType::OutboxFailed];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutboxFailureCode {
    RateLimited,
    Transient,
    TokenExpired,
    PreconditionFailed,
    NotMergeable,
    ValidationFailed,
    PermissionDenied,
    TargetMissing,
    ApprovalInvalid,
    SignatureRequired,
}

// ---------------------------------------------------------------------------
// budget
// ---------------------------------------------------------------------------

/// `budget.exhausted`（core §7.5：`{dimension, limit, used}`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetExhaustedData {
    pub dimension: BudgetDimension,
    pub limit: u64,
    pub used: u64,
}

impl EventData for BudgetExhaustedData {
    const TYPES: &'static [EventType] = &[EventType::BudgetExhausted];
}

/// `budget.limit_raised`（decisions D-19，#129）：用户对 `budget_exhausted` 选
/// `raise_cap` 后，引擎给耗尽维度追加一份该档模板的原值。`to = from + by`，`by > 0`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetLimitRaisedData {
    pub dimension: BudgetDimension,
    pub from: u64,
    pub to: u64,
    pub by: u64,
}

impl EventData for BudgetLimitRaisedData {
    const TYPES: &'static [EventType] = &[EventType::BudgetLimitRaised];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.by == 0 {
            return Err(SchemaError::consistency("/by", "by must be greater than 0"));
        }
        if self.from.checked_add(self.by) != Some(self.to) {
            return Err(SchemaError::consistency("/to", "to must equal from + by"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// device
// ---------------------------------------------------------------------------

/// `device.paired`（pki.md §7.4.3）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DevicePairedData {
    pub device_id: DeviceId,
    pub name: String,
    pub platform: DevicePlatform,
    pub role: DeviceRole,
    pub cert_serial: String,
    pub pubkey_fpr: Sha256Digest,
    #[serde(default)]
    pub approval_pubkey: Option<String>,
    #[serde(default)]
    pub approval_pubkey_fpr: Option<Sha256Digest>,
}

impl EventData for DevicePairedData {
    const TYPES: &'static [EventType] = &[EventType::DevicePaired];
}

/// `device.updated`（pki.md §7.5、OQ-K12）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceUpdatedData {
    #[serde(default)]
    pub device_id: Option<DeviceId>,
    pub changed: DeviceChanged,
    #[serde(default)]
    pub via: Option<String>,
    #[serde(default)]
    pub host_uid: Option<u32>,
    #[serde(default)]
    pub sudo_user: Option<String>,
}

impl EventData for DeviceUpdatedData {
    const TYPES: &'static [EventType] = &[EventType::DeviceUpdated];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.changed.is_empty() {
            return Err(SchemaError::shape(
                "/changed",
                "device.updated requires at least one changed field",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceChanged {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<RoleChange>,
    #[serde(default)]
    pub cert_serial: Option<String>,
    #[serde(default)]
    pub pubkey_fpr: Option<Sha256Digest>,
}

impl DeviceChanged {
    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.role.is_none()
            && self.cert_serial.is_none()
            && self.pubkey_fpr.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleChange {
    pub from: DeviceRole,
    pub to: DeviceRole,
}

/// `device.revoked`（pki.md §7.5）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceRevokedData {
    pub device_id: DeviceId,
    pub cert_serials: Vec<String>,
    pub remaining_approvers: u64,
    #[serde(default)]
    pub via: Option<String>,
}

impl EventData for DeviceRevokedData {
    const TYPES: &'static [EventType] = &[EventType::DeviceRevoked];
}

// ---------------------------------------------------------------------------
// attempt.progress（event-data §2.2.1）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptProgressKind {
    Stage,
    Tool,
    Usage,
}

/// `attempt.progress` 的白名单投影；`type` 决定哪些字段允许出现。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptProgressData {
    pub v: u8,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: AttemptProgressKind,
    #[serde(default)]
    pub stage: Option<ProgressStage>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub phase: Option<ToolPhase>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub turns: Option<u64>,
}

impl JsonSchema for AttemptProgressData {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AttemptProgressData".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        let stage = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "v": { "const": 1 },
                "seq": { "type": "integer", "minimum": 1 },
                "type": { "const": "stage" },
                "stage": { "enum": ["init", "explore", "plan", "edit", "test", "review", "package", "finalize"] },
                "detail": { "type": "string", "maxLength": 200 }
            },
            "required": ["v", "seq", "type", "stage", "detail"]
        });
        let tool_start = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "v": { "const": 1 },
                "seq": { "type": "integer", "minimum": 1 },
                "type": { "const": "tool" },
                "phase": { "const": "start" },
                "call_id": { "type": "string", "minLength": 1 },
                "tool": { "type": "string", "minLength": 1 },
                "summary": { "type": "string", "maxLength": 200 }
            },
            "required": ["v", "seq", "type", "phase", "call_id", "tool", "summary"]
        });
        let tool_end = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "v": { "const": 1 },
                "seq": { "type": "integer", "minimum": 1 },
                "type": { "const": "tool" },
                "phase": { "const": "end" },
                "call_id": { "type": "string", "minLength": 1 },
                "tool": { "type": "string", "minLength": 1 },
                "ok": { "type": "boolean" },
                "duration_ms": { "type": "integer", "minimum": 0 }
            },
            "required": ["v", "seq", "type", "phase", "call_id", "tool", "ok", "duration_ms"]
        });
        let usage = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "v": { "const": 1 },
                "seq": { "type": "integer", "minimum": 1 },
                "type": { "const": "usage" },
                "input_tokens": { "type": "integer", "minimum": 0 },
                "output_tokens": { "type": "integer", "minimum": 0 },
                "cache_read_tokens": { "type": "integer", "minimum": 0 },
                "cache_write_tokens": { "type": "integer", "minimum": 0 },
                "turns": { "type": "integer", "minimum": 0 }
            },
            "required": ["v", "seq", "type", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "turns"]
        });
        Schema::try_from(serde_json::json!({
            "title": "AttemptProgressData",
            "oneOf": [stage, tool_start, tool_end, usage]
        }))
        .expect("attempt.progress schema is valid")
    }
}

impl EventData for AttemptProgressData {
    const TYPES: &'static [EventType] = &[EventType::AttemptProgress];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.v != 1 {
            return Err(SchemaError::shape("/v", "attempt.progress v must be 1"));
        }
        if self.seq == 0 {
            return Err(SchemaError::shape(
                "/seq",
                "attempt.progress seq must be >= 1",
            ));
        }
        match self.kind {
            AttemptProgressKind::Stage => {
                if self.stage.is_none() {
                    return Err(SchemaError::shape("/stage", "stage is required"));
                }
                let Some(detail) = &self.detail else {
                    return Err(SchemaError::shape("/detail", "detail is required"));
                };
                reject_plain_text(detail, "/detail", 200)?;
            }
            AttemptProgressKind::Tool => {
                let phase = self
                    .phase
                    .ok_or_else(|| SchemaError::shape("/phase", "tool phase is required"))?;
                let call_id = self
                    .call_id
                    .as_deref()
                    .ok_or_else(|| SchemaError::shape("/call_id", "call_id is required"))?;
                require_non_empty(call_id, "/call_id")?;
                reject_plain_text(call_id, "/call_id", usize::MAX)?;
                let tool = self
                    .tool
                    .as_deref()
                    .ok_or_else(|| SchemaError::shape("/tool", "tool is required"))?;
                require_non_empty(tool, "/tool")?;
                reject_plain_text(tool, "/tool", usize::MAX)?;
                match phase {
                    ToolPhase::Start => {
                        let summary = self
                            .summary
                            .as_deref()
                            .ok_or_else(|| SchemaError::shape("/summary", "summary is required"))?;
                        reject_plain_text(summary, "/summary", 200)?;
                        if self.ok.is_some() || self.duration_ms.is_some() {
                            return Err(SchemaError::shape(
                                "/ok",
                                "start does not carry ok or duration_ms",
                            ));
                        }
                    }
                    ToolPhase::End => {
                        if self.ok.is_none() {
                            return Err(SchemaError::shape("/ok", "ok is required"));
                        }
                        if self.duration_ms.is_none() {
                            return Err(SchemaError::shape(
                                "/duration_ms",
                                "duration_ms is required",
                            ));
                        }
                        if self.summary.is_some() {
                            return Err(SchemaError::shape(
                                "/summary",
                                "end does not carry summary",
                            ));
                        }
                    }
                }
            }
            AttemptProgressKind::Usage => {
                for (field, value) in [
                    ("/input_tokens", self.input_tokens),
                    ("/output_tokens", self.output_tokens),
                    ("/cache_read_tokens", self.cache_read_tokens),
                    ("/cache_write_tokens", self.cache_write_tokens),
                    ("/turns", self.turns),
                ] {
                    if value.is_none() {
                        return Err(SchemaError::shape(field, "usage field is required"));
                    }
                }
            }
        }
        Ok(())
    }
}

fn check_attempt_progress_shape(data: &Value) -> Result<(), SchemaError> {
    let map = object_at(data, "")?;
    let kind = map.get("type").and_then(Value::as_str);
    let allowed: &[&str] = match kind {
        Some("stage") => &["v", "seq", "type", "stage", "detail"],
        Some("tool") => &[
            "v",
            "seq",
            "type",
            "phase",
            "call_id",
            "tool",
            "summary",
            "ok",
            "duration_ms",
        ],
        Some("usage") => &[
            "v",
            "seq",
            "type",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "turns",
        ],
        _ => return Ok(()),
    };
    deny_keys_at(data, "", allowed)?;
    for key in allowed {
        if *key == "type" || *key == "v" || *key == "seq" {
            continue;
        }
        if map.get(*key).is_some_and(Value::is_null) {
            return Err(SchemaError::shape(
                pointer_of("", key),
                format!("field `{key}` must be omitted, not null"),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// guardian.*（event-data §2.2.2）
// ---------------------------------------------------------------------------

/// guardian §7.9 的 27 个上报原因码。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum GuardianReasonCode {
    #[serde(rename = "guardian.security_finding")]
    SecurityFinding,
    #[serde(rename = "guardian.protected_path")]
    ProtectedPath,
    #[serde(rename = "guardian.scope_expanding_fix")]
    ScopeExpandingFix,
    #[serde(rename = "guardian.repeated_problem")]
    RepeatedProblem,
    #[serde(rename = "guardian.no_progress")]
    NoProgress,
    #[serde(rename = "guardian.budget_exhausted")]
    BudgetExhausted,
    #[serde(rename = "guardian.workflow_run_approval")]
    WorkflowRunApproval,
    #[serde(rename = "guardian.push_after_approval")]
    PushAfterApproval,
    #[serde(rename = "guardian.design_or_api_change")]
    DesignOrApiChange,
    #[serde(rename = "guardian.mechanical_conflict")]
    MechanicalConflict,
    #[serde(rename = "guardian.ci_failure_escalated")]
    CiFailureEscalated,
    #[serde(rename = "guardian.nit_escalated")]
    NitEscalated,
    #[serde(rename = "guardian.bot_finding_escalated")]
    BotFindingEscalated,
    #[serde(rename = "guardian.ci_out_of_scope")]
    CiOutOfScope,
    #[serde(rename = "guardian.ci_low_confidence")]
    CiLowConfidence,
    #[serde(rename = "guardian.ci_startup_failure")]
    CiStartupFailure,
    #[serde(rename = "guardian.ci_timeout")]
    CiTimeout,
    #[serde(rename = "guardian.scope_unknown")]
    ScopeUnknown,
    #[serde(rename = "guardian.finding_low_confidence")]
    FindingLowConfidence,
    #[serde(rename = "guardian.human_feedback")]
    HumanFeedback,
    #[serde(rename = "guardian.dequeued")]
    Dequeued,
    #[serde(rename = "guardian.heal_failed")]
    HealFailed,
    #[serde(rename = "guardian.heal_no_change")]
    HealNoChange,
    #[serde(rename = "guardian.patch_denied")]
    PatchDenied,
    #[serde(rename = "guardian.github_write_failed")]
    GithubWriteFailed,
    #[serde(rename = "guardian.action_denied")]
    ActionDenied,
    #[serde(rename = "guardian.unknown_event")]
    UnknownEvent,
}

impl GuardianReasonCode {
    fn class(self) -> GuardianTriageClass {
        use GuardianReasonCode as R;
        use GuardianTriageClass as C;
        match self {
            R::SecurityFinding => C::SecurityFinding,
            R::ProtectedPath => C::TouchesProtectedPath,
            R::ScopeExpandingFix => C::ScopeExpandingFix,
            R::RepeatedProblem | R::NoProgress => C::RepeatedOrNoProgress,
            R::BudgetExhausted => C::BudgetExhausted,
            R::WorkflowRunApproval => C::ApprovalNeeded,
            R::PushAfterApproval => C::PushAfterApproval,
            R::DesignOrApiChange => C::DesignOrApiChange,
            R::MechanicalConflict => C::MechanicalConflict,
            R::CiFailureEscalated => C::CiFailureInScope,
            R::NitEscalated => C::Nit,
            R::BotFindingEscalated => C::BotFindingValid,
            R::CiOutOfScope
            | R::CiLowConfidence
            | R::CiStartupFailure
            | R::CiTimeout
            | R::ScopeUnknown
            | R::FindingLowConfidence
            | R::HumanFeedback
            | R::Dequeued
            | R::HealFailed
            | R::HealNoChange
            | R::PatchDenied
            | R::GithubWriteFailed
            | R::ActionDenied
            | R::UnknownEvent => C::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardianTriageAction {
    SilentFix,
    SilentReply,
    Batch,
    Defer,
    Finalize,
    Escalate,
    RoundClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardianDeferReason {
    Paused,
    LockHeld,
    RoundActive,
    Settling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardianRoundOutcome {
    Pushed,
    Replied,
    Escalated,
    NoChange,
    Failed,
    Denied,
    Superseded,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardianRoundProgress {
    Done,
    Improved,
    Stalled,
}

/// `event_ref` 的四种触发来源；普通 triage 只用 `forge` / `event`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum GuardianEventRef {
    Forge {
        kind: ForgeEventKind,
        dedup_key: String,
        #[serde(default)]
        head_sha: Option<GitOid>,
    },
    Event {
        event_id: EventId,
    },
    Attempt {
        event_id: EventId,
        attempt_id: AttemptId,
    },
    Settlement {
        head_sha: GitOid,
    },
}

impl JsonSchema for GuardianEventRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GuardianEventRef".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        let branch = |properties: Value, required: &[&str]| {
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": properties,
                "required": required,
            })
        };
        Schema::try_from(serde_json::json!({
            "title": "GuardianEventRef",
            "oneOf": [
                branch(
                    serde_json::json!({
                        "source": { "const": "forge" },
                        "kind": {
                            "type": "string",
                            "enum": ForgeEventKind::ALL.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
                        },
                        "dedup_key": { "type": "string" },
                        "head_sha": { "type": "string", "pattern": "^[0-9a-f]{40}$" }
                    }),
                    &["source", "kind", "dedup_key"],
                ),
                branch(
                    serde_json::json!({
                        "source": { "const": "event" },
                        "event_id": { "type": "string" }
                    }),
                    &["source", "event_id"],
                ),
                branch(
                    serde_json::json!({
                        "source": { "const": "attempt" },
                        "event_id": { "type": "string" },
                        "attempt_id": { "type": "string" }
                    }),
                    &["source", "event_id", "attempt_id"],
                ),
                branch(
                    serde_json::json!({
                        "source": { "const": "settlement" },
                        "head_sha": { "type": "string", "pattern": "^[0-9a-f]{40}$" }
                    }),
                    &["source", "head_sha"],
                ),
            ]
        }))
        .expect("guardian event_ref schema is valid")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardianTriagedData {
    pub event_ref: GuardianEventRef,
    pub action: GuardianTriageAction,
    #[serde(default)]
    pub class: Option<GuardianTriageClass>,
    pub delivery_id: DeliveryId,
    pub rule: String,
    pub also_matched: Vec<GuardianTriageClass>,
    pub config_rev: Seq,
    #[serde(default)]
    pub reason_code: Option<GuardianReasonCode>,
    #[serde(default)]
    pub fingerprint: Option<Sha256Digest>,
    #[serde(default)]
    pub round_id: Option<Ulid>,
    #[serde(default)]
    pub inbox_id: Option<InboxId>,
    #[serde(default)]
    pub defer_reason: Option<GuardianDeferReason>,
    pub action_ids: Vec<ActionId>,
    #[serde(default)]
    pub outcome: Option<GuardianRoundOutcome>,
    #[serde(default)]
    pub head_before: Option<GitOid>,
    #[serde(default)]
    pub head_after: Option<GitOid>,
    #[serde(default)]
    pub failing_before: Option<Vec<Sha256Digest>>,
    #[serde(default)]
    pub failing_after: Option<Vec<Sha256Digest>>,
    #[serde(default)]
    pub progress: Option<GuardianRoundProgress>,
    #[serde(default)]
    pub stalled_rounds: Option<u32>,
}

const TRIAGED_KEYS: &[&str] = &[
    "event_ref",
    "action",
    "class",
    "delivery_id",
    "rule",
    "also_matched",
    "config_rev",
    "reason_code",
    "fingerprint",
    "round_id",
    "inbox_id",
    "defer_reason",
    "action_ids",
    "outcome",
    "head_before",
    "head_after",
    "failing_before",
    "failing_after",
    "progress",
    "stalled_rounds",
];

const TRIAGED_ROUND_KEYS: &[&str] = &[
    "outcome",
    "head_before",
    "head_after",
    "failing_before",
    "failing_after",
    "progress",
    "stalled_rounds",
];

fn check_guardian_event_ref_shape(data: &Value) -> Result<(), SchemaError> {
    let map = object_at(data, "/event_ref")?;
    let Some(source) = map.get("source").and_then(Value::as_str) else {
        return Err(SchemaError::shape(
            "/event_ref/source",
            "source must be a string",
        ));
    };
    let allowed: &[&str] = match source {
        "forge" => &["source", "kind", "dedup_key", "head_sha"],
        "event" => &["source", "event_id"],
        "attempt" => &["source", "event_id", "attempt_id"],
        "settlement" => &["source", "head_sha"],
        _ => return Ok(()),
    };
    deny_keys_at(data, "/event_ref", allowed)?;
    let required: &[&str] = match source {
        "forge" => &["source", "kind", "dedup_key"],
        "event" => &["source", "event_id"],
        "attempt" => &["source", "event_id", "attempt_id"],
        "settlement" => &["source", "head_sha"],
        _ => &[],
    };
    require_keys_at(data, "/event_ref", required)?;
    reject_null_keys_at(data, "/event_ref", &["head_sha"])?;
    Ok(())
}

fn check_guardian_triaged_shape(data: &Value) -> Result<(), SchemaError> {
    object_at(data, "")?;
    deny_keys_at(data, "", TRIAGED_KEYS)?;
    require_keys_at(
        data,
        "",
        &[
            "event_ref",
            "action",
            "delivery_id",
            "rule",
            "also_matched",
            "config_rev",
            "action_ids",
        ],
    )?;
    if let Some(event_ref) = nested(data, &["event_ref"]) {
        check_guardian_event_ref_shape(event_ref)?;
    }
    reject_null_keys_at(
        data,
        "",
        &[
            "class",
            "reason_code",
            "fingerprint",
            "round_id",
            "inbox_id",
            "defer_reason",
            "outcome",
            "head_before",
            "head_after",
            "failing_before",
            "failing_after",
            "stalled_rounds",
        ],
    )?;
    let action = data
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action == "round_closed" {
        require_keys_at(
            data,
            "",
            &[
                "outcome",
                "head_before",
                "failing_before",
                "progress",
                "stalled_rounds",
            ],
        )?;
    } else {
        for key in TRIAGED_ROUND_KEYS {
            if data.get(*key).is_some() {
                return Err(SchemaError::shape(
                    format!("/{key}"),
                    "round_closed field is forbidden for this action",
                ));
            }
        }
    }
    Ok(())
}

impl EventData for GuardianTriagedData {
    const TYPES: &'static [EventType] = &[EventType::GuardianTriaged];

    fn validate(&self) -> Result<(), SchemaError> {
        require_non_empty(&self.rule, "/rule")?;
        ensure_sorted_unique(&self.action_ids, "/action_ids")?;
        let is_round_closed = self.action == GuardianTriageAction::RoundClosed;
        let event_source = match &self.event_ref {
            GuardianEventRef::Forge { .. } => "forge",
            GuardianEventRef::Event { .. } => "event",
            GuardianEventRef::Attempt { .. } => "attempt",
            GuardianEventRef::Settlement { .. } => "settlement",
        };
        if is_round_closed {
            if !matches!(event_source, "attempt" | "settlement") {
                return Err(SchemaError::consistency(
                    "/event_ref",
                    "round_closed only accepts attempt / settlement event_ref",
                ));
            }
            return self.validate_round_closed();
        }
        if !matches!(event_source, "forge" | "event") {
            return Err(SchemaError::consistency(
                "/event_ref",
                "ordinary triage only accepts forge / event event_ref",
            ));
        }
        for key in TRIAGED_ROUND_KEYS {
            let present = match *key {
                "outcome" => self.outcome.is_some(),
                "head_before" => self.head_before.is_some(),
                "head_after" => self.head_after.is_some(),
                "failing_before" => self.failing_before.is_some(),
                "failing_after" => self.failing_after.is_some(),
                "progress" => self.progress.is_some(),
                "stalled_rounds" => self.stalled_rounds.is_some(),
                _ => false,
            };
            if present {
                return Err(SchemaError::shape(
                    format!("/{key}"),
                    "round_closed field is forbidden for this action",
                ));
            }
        }
        if self.action == GuardianTriageAction::Finalize {
            if self.class.is_some() {
                return Err(SchemaError::consistency("/class", "finalize omits class"));
            }
        } else if self.class.is_none() {
            return Err(SchemaError::consistency(
                "/class",
                "class is required for this action",
            ));
        }
        if matches!(self.action, GuardianTriageAction::Finalize) && !self.also_matched.is_empty() {
            return Err(SchemaError::consistency(
                "/also_matched",
                "finalize requires an empty also_matched",
            ));
        }
        if self.round_id.is_some() && matches!(self.action, GuardianTriageAction::Batch) {
            return Err(SchemaError::consistency(
                "/round_id",
                "batch does not own a round",
            ));
        }
        if self.reason_code.is_some() && self.action != GuardianTriageAction::Escalate {
            return Err(SchemaError::consistency(
                "/reason_code",
                "reason_code is only allowed for escalate",
            ));
        }
        if self.inbox_id.is_some() && self.action != GuardianTriageAction::Escalate {
            return Err(SchemaError::consistency(
                "/inbox_id",
                "inbox_id is only allowed for escalate",
            ));
        }
        if self.fingerprint.is_some() && self.action == GuardianTriageAction::Finalize {
            return Err(SchemaError::consistency(
                "/fingerprint",
                "finalize does not carry fingerprint",
            ));
        }
        if self.defer_reason.is_some() && self.action != GuardianTriageAction::Defer {
            return Err(SchemaError::consistency(
                "/defer_reason",
                "defer_reason is only allowed for defer",
            ));
        }
        use GuardianTriageAction as A;
        use GuardianTriageClass as C;
        match self.action {
            A::SilentFix => {
                if !matches!(
                    self.class,
                    Some(C::CiFailureInScope | C::BotFindingValid | C::MechanicalConflict)
                ) {
                    return Err(SchemaError::consistency(
                        "/class",
                        "silent_fix class must be ci_failure_in_scope, bot_finding_valid, or mechanical_conflict",
                    ));
                }
                if self.round_id.is_none() {
                    return Err(SchemaError::consistency(
                        "/round_id",
                        "silent_fix opens a round and requires round_id",
                    ));
                }
            }
            A::SilentReply => {
                if self.class != Some(C::BotFindingInvalid) {
                    return Err(SchemaError::consistency(
                        "/class",
                        "silent_reply class must be bot_finding_invalid",
                    ));
                }
                if self.round_id.is_none() {
                    return Err(SchemaError::consistency(
                        "/round_id",
                        "silent_reply opens a round and requires round_id",
                    ));
                }
            }
            A::Batch => {
                if self.class != Some(C::Nit) {
                    return Err(SchemaError::consistency(
                        "/class",
                        "batch class must be nit",
                    ));
                }
            }
            A::Defer => {
                if !matches!(
                    self.class,
                    Some(
                        C::CiFailureInScope
                            | C::BotFindingValid
                            | C::BotFindingInvalid
                            | C::MechanicalConflict
                    )
                ) {
                    return Err(SchemaError::consistency(
                        "/class",
                        "defer class must be one of the four configurable classes",
                    ));
                }
                let Some(reason) = self.defer_reason else {
                    return Err(SchemaError::consistency(
                        "/defer_reason",
                        "defer requires defer_reason",
                    ));
                };
                match reason {
                    GuardianDeferReason::RoundActive => {
                        if self.round_id.is_none() {
                            return Err(SchemaError::consistency(
                                "/round_id",
                                "defer(round_active) requires round_id",
                            ));
                        }
                    }
                    GuardianDeferReason::Settling => {}
                    GuardianDeferReason::Paused | GuardianDeferReason::LockHeld => {
                        if self.round_id.is_some() {
                            return Err(SchemaError::consistency(
                                "/round_id",
                                "this defer reason does not carry round_id",
                            ));
                        }
                    }
                }
            }
            A::Finalize => {}
            A::Escalate => {
                let Some(reason) = self.reason_code else {
                    return Err(SchemaError::consistency(
                        "/reason_code",
                        "escalate requires reason_code",
                    ));
                };
                let class = self
                    .class
                    .ok_or_else(|| SchemaError::consistency("/class", "escalate requires class"))?;
                if reason.class() != class {
                    return Err(SchemaError::consistency(
                        "/reason_code",
                        "reason_code does not match the guardian class",
                    ));
                }
            }
            A::RoundClosed => unreachable!("round_closed handled above"),
        }
        Ok(())
    }
}

impl GuardianTriagedData {
    fn validate_round_closed(&self) -> Result<(), SchemaError> {
        if self.class.is_none() {
            return Err(SchemaError::consistency(
                "/class",
                "round_closed keeps the opening class",
            ));
        }
        if !self.also_matched.is_empty() {
            return Err(SchemaError::consistency(
                "/also_matched",
                "round_closed requires an empty also_matched",
            ));
        }
        if self.fingerprint.is_some() {
            return Err(SchemaError::consistency(
                "/fingerprint",
                "round_closed uses failing_before / failing_after",
            ));
        }
        if self.round_id.is_none() {
            return Err(SchemaError::consistency(
                "/round_id",
                "round_closed requires round_id",
            ));
        }
        if self.defer_reason.is_some() {
            return Err(SchemaError::consistency(
                "/defer_reason",
                "round_closed does not carry defer_reason",
            ));
        }
        let outcome = self
            .outcome
            .ok_or_else(|| SchemaError::consistency("/outcome", "round_closed requires outcome"))?;
        let head_before = self.head_before.ok_or_else(|| {
            SchemaError::consistency("/head_before", "round_closed requires head_before")
        })?;
        if self.head_after == Some(head_before) {
            return Err(SchemaError::consistency(
                "/head_after",
                "head_after must differ from head_before",
            ));
        }
        let failing_before = self.failing_before.as_ref().ok_or_else(|| {
            SchemaError::consistency("/failing_before", "round_closed requires failing_before")
        })?;
        if self.stalled_rounds.is_none() {
            return Err(SchemaError::consistency(
                "/stalled_rounds",
                "round_closed requires stalled_rounds",
            ));
        }
        let has_after = self.failing_after.is_some();
        let has_progress = self.progress.is_some();
        let reason_required = matches!(
            outcome,
            GuardianRoundOutcome::Escalated
                | GuardianRoundOutcome::NoChange
                | GuardianRoundOutcome::Failed
                | GuardianRoundOutcome::Denied
        );
        if reason_required != self.reason_code.is_some() {
            return Err(SchemaError::consistency(
                "/reason_code",
                if reason_required {
                    "this outcome requires reason_code"
                } else {
                    "this outcome forbids reason_code"
                },
            ));
        }
        if self.inbox_id.is_some()
            && !matches!(
                outcome,
                GuardianRoundOutcome::Escalated
                    | GuardianRoundOutcome::NoChange
                    | GuardianRoundOutcome::Failed
                    | GuardianRoundOutcome::Denied
            )
        {
            return Err(SchemaError::consistency(
                "/inbox_id",
                "inbox_id is only allowed for an escalated terminal outcome",
            ));
        }
        match outcome {
            GuardianRoundOutcome::Pushed => {
                if !matches!(
                    self.progress,
                    Some(
                        GuardianRoundProgress::Done
                            | GuardianRoundProgress::Improved
                            | GuardianRoundProgress::Stalled
                    )
                ) {
                    return Err(SchemaError::consistency(
                        "/progress",
                        "pushed requires progress done / improved / stalled",
                    ));
                }
                if !has_after {
                    return Err(SchemaError::consistency(
                        "/failing_after",
                        "pushed requires failing_after",
                    ));
                }
                if self.progress == Some(GuardianRoundProgress::Done)
                    && self
                        .failing_after
                        .as_ref()
                        .is_some_and(|after| !after.is_empty())
                {
                    return Err(SchemaError::consistency(
                        "/failing_after",
                        "progress=done requires an empty failing_after",
                    ));
                }
            }
            GuardianRoundOutcome::Replied => {
                if has_progress || has_after {
                    return Err(SchemaError::consistency(
                        "/progress",
                        "replied requires progress = null and no failing_after",
                    ));
                }
            }
            GuardianRoundOutcome::Escalated => match self.progress {
                Some(GuardianRoundProgress::Stalled) => {
                    if !has_after {
                        return Err(SchemaError::consistency(
                            "/failing_after",
                            "escalated with progress requires failing_after",
                        ));
                    }
                }
                None => {
                    if has_after {
                        return Err(SchemaError::consistency(
                            "/failing_after",
                            "escalated with progress = null omits failing_after",
                        ));
                    }
                }
                Some(_) => {
                    return Err(SchemaError::consistency(
                        "/progress",
                        "escalated progress must be stalled or null",
                    ));
                }
            },
            GuardianRoundOutcome::NoChange
            | GuardianRoundOutcome::Failed
            | GuardianRoundOutcome::Denied => {
                if has_progress || has_after {
                    return Err(SchemaError::consistency(
                        "/progress",
                        "this outcome requires progress = null and no failing_after",
                    ));
                }
            }
            GuardianRoundOutcome::Superseded | GuardianRoundOutcome::Cancelled => {
                if has_progress || has_after {
                    return Err(SchemaError::consistency(
                        "/progress",
                        "this outcome requires progress = null and no failing_after",
                    ));
                }
            }
        }
        let _ = failing_before;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardianHealTask {
    CiFailure,
    BotFinding,
    MechanicalConflict,
    NitBatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GuardianProblemKind {
    CiFailureInScope,
    BotFindingValid,
    BotFindingInvalid,
    MechanicalConflict,
    Nit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardianProblem {
    pub fingerprint: Sha256Digest,
    pub kind: GuardianProblemKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardianHealRequestedData {
    pub round_id: Ulid,
    pub delivery_id: DeliveryId,
    pub task: GuardianHealTask,
    pub head_sha: GitOid,
    pub problems: Vec<GuardianProblem>,
    pub batch_included: Vec<Sha256Digest>,
    pub request_digest: Sha256Digest,
    pub config_rev: Seq,
}

impl EventData for GuardianHealRequestedData {
    const TYPES: &'static [EventType] = &[EventType::GuardianHealRequested];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.problems.is_empty() {
            return Err(SchemaError::shape(
                "/problems",
                "heal_requested requires at least one problem",
            ));
        }
        let mut seen = BTreeSet::new();
        for (index, problem) in self.problems.iter().enumerate() {
            if !seen.insert(problem.fingerprint) {
                return Err(SchemaError::consistency(
                    format!("/problems/{index}/fingerprint"),
                    "fingerprints must be unique",
                ));
            }
        }
        let nit_positions: std::collections::BTreeMap<Sha256Digest, usize> = self
            .problems
            .iter()
            .enumerate()
            .filter(|(_, problem)| problem.kind == GuardianProblemKind::Nit)
            .map(|(index, problem)| (problem.fingerprint, index))
            .collect();
        let mut previous = None;
        let mut seen_batch = BTreeSet::new();
        for (index, fingerprint) in self.batch_included.iter().enumerate() {
            let Some(position) = nit_positions.get(fingerprint) else {
                return Err(SchemaError::consistency(
                    format!("/batch_included/{index}"),
                    "batch_included must be a subset of nit problems",
                ));
            };
            if !seen_batch.insert(*fingerprint) {
                return Err(SchemaError::consistency(
                    format!("/batch_included/{index}"),
                    "batch_included must be unique",
                ));
            }
            if previous.is_some_and(|previous| previous >= *position) {
                return Err(SchemaError::consistency(
                    format!("/batch_included/{index}"),
                    "batch_included follows the submitted problem order",
                ));
            }
            previous = Some(*position);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardianEscalatedData {
    pub delivery_id: DeliveryId,
    pub class: GuardianTriageClass,
    pub reason_code: GuardianReasonCode,
    #[serde(default)]
    pub inbox_id: Option<InboxId>,
    pub suppressed_duplicate: bool,
    #[serde(default)]
    pub fingerprint: Option<Sha256Digest>,
    #[serde(default)]
    pub round_id: Option<Ulid>,
}

impl EventData for GuardianEscalatedData {
    const TYPES: &'static [EventType] = &[EventType::GuardianEscalated];

    fn validate(&self) -> Result<(), SchemaError> {
        if !self.suppressed_duplicate && self.inbox_id.is_none() {
            return Err(SchemaError::consistency(
                "/inbox_id",
                "a newly raised escalation requires inbox_id",
            ));
        }
        if self.reason_code.class() != self.class {
            return Err(SchemaError::consistency(
                "/reason_code",
                "reason_code does not match the guardian class",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// approval.*（event-data §2.2.3）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ApprovalInvalidatedData {
    HeadChanged {
        previous_head_sha: GitOid,
        current_head_sha: GitOid,
    },
    PrClosed,
    PrMerged,
    DeliveryAbandoned,
    AttemptSuperseded {
        previous_attempt_id: AttemptId,
        current_attempt_id: AttemptId,
    },
    OwnerCancelled,
}

impl EventData for ApprovalInvalidatedData {
    const TYPES: &'static [EventType] = &[EventType::ApprovalInvalidated];

    fn validate(&self) -> Result<(), SchemaError> {
        match self {
            Self::HeadChanged {
                previous_head_sha,
                current_head_sha,
            } => {
                if previous_head_sha == current_head_sha {
                    return Err(SchemaError::consistency(
                        "/current_head_sha",
                        "head_changed requires different previous and current heads",
                    ));
                }
            }
            Self::AttemptSuperseded {
                previous_attempt_id,
                current_attempt_id,
            } => {
                if previous_attempt_id == current_attempt_id {
                    return Err(SchemaError::consistency(
                        "/current_attempt_id",
                        "attempt_superseded requires different attempts",
                    ));
                }
            }
            Self::PrClosed | Self::PrMerged | Self::DeliveryAbandoned | Self::OwnerCancelled => {}
        }
        Ok(())
    }
}

fn check_approval_invalidated_shape(data: &Value) -> Result<(), SchemaError> {
    let map = object_at(data, "")?;
    let Some(reason) = map.get("reason").and_then(Value::as_str) else {
        return Err(SchemaError::shape("/reason", "reason must be a string"));
    };
    let allowed: &[&str] = match reason {
        "head_changed" => &["reason", "previous_head_sha", "current_head_sha"],
        "attempt_superseded" => &["reason", "previous_attempt_id", "current_attempt_id"],
        "pr_closed" | "pr_merged" | "delivery_abandoned" | "owner_cancelled" => &["reason"],
        _ => return Ok(()),
    };
    deny_keys_at(data, "", allowed)?;
    require_keys_at(data, "", allowed)?;
    Ok(())
}

/// `approval.expired` 的 `data` 固定为 `{}`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalExpiredData {}

impl JsonSchema for ApprovalExpiredData {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ApprovalExpiredData".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "maxProperties": 0,
        })
    }
}

impl EventData for ApprovalExpiredData {
    const TYPES: &'static [EventType] = &[EventType::ApprovalExpired];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDeniedData {
    #[serde(default)]
    pub reason: Option<String>,
}

impl EventData for ApprovalDeniedData {
    const TYPES: &'static [EventType] = &[EventType::ApprovalDenied];

    fn validate(&self) -> Result<(), SchemaError> {
        if let Some(reason) = &self.reason {
            // 备注按纯文本展示，不在这里另设长度上限（受 API 64 KiB 请求体门禁）。
            reject_plain_text(reason, "/reason", usize::MAX)?;
        }
        Ok(())
    }
}

fn check_approval_denied_shape(data: &Value) -> Result<(), SchemaError> {
    deny_keys_at(data, "", &["reason"])?;
    reject_null_keys_at(data, "", &["reason"])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// outbox.*（event-data §2.2.4）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OutboxEnqueuedBy {
    pub module: String,
    pub causation_id: Option<EventId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutboxEnqueuedBy {
    module: String,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    causation_id: Option<Option<EventId>>,
}

impl<'de> Deserialize<'de> for OutboxEnqueuedBy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawOutboxEnqueuedBy::deserialize(deserializer)?;
        let causation_id = raw
            .causation_id
            .ok_or_else(|| serde::de::Error::missing_field("causation_id"))?;
        Ok(Self {
            module: raw.module,
            causation_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushBranchIntent {
    pub local_head_oid: GitOid,
    pub expected_remote_oid: Option<GitOid>,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePrIntent {
    pub base: String,
    pub head: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub closes: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePrIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PrState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpsertStatusCommentIntent {
    pub revision: u32,
    pub render_digest: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCommentIntent {
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyReviewThreadIntent {
    pub body: String,
    pub in_reply_to_comment_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveReviewThreadIntent {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIssueIntent {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssueIntent {
    pub spec_block_digest: Sha256Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseIssueIntent {
    pub state_reason: IssueStateReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_merged_pr: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddLabelsIntent {
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestReviewersIntent {
    pub reviewers: Vec<String>,
    pub team_reviewers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergePrIntent {
    pub expected_head_sha: GitOid,
    pub merge_method: crate::entities::MergeMethod,
    pub mode: MergeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RerunFailedJobsIntent {
    pub expected_run_attempt: u32,
    pub head_sha: GitOid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum OutboxIntent {
    PushBranch(PushBranchIntent),
    CreatePr(CreatePrIntent),
    UpdatePr(UpdatePrIntent),
    UpsertStatusComment(UpsertStatusCommentIntent),
    CreateComment(CreateCommentIntent),
    ReplyReviewThread(ReplyReviewThreadIntent),
    ResolveReviewThread(ResolveReviewThreadIntent),
    CreateIssue(CreateIssueIntent),
    UpdateIssue(UpdateIssueIntent),
    CloseIssue(CloseIssueIntent),
    AddLabels(AddLabelsIntent),
    RequestReviewers(RequestReviewersIntent),
    MergePr(MergePrIntent),
    RerunFailedJobs(RerunFailedJobsIntent),
}

impl OutboxIntent {
    fn from_value(kind: ForgeActionKind, value: Value) -> Result<Self, serde_json::Error> {
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
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutboxEnqueuedAction {
    pub delivery_id: DeliveryId,
    pub kind: ForgeActionKind,
    pub payload: ActionPayload,
    pub target: String,
    pub intent: OutboxIntent,
    pub idempotency_key: IdempotencyKey,
    pub requires_approval: Option<ApprovalId>,
    pub depends_on: Vec<ActionId>,
    pub priority: Priority,
    pub not_before: Option<Timestamp>,
    pub epoch: u32,
    pub coalesce_key: Option<String>,
    pub enqueued_by: OutboxEnqueuedBy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutboxEnqueuedAction {
    delivery_id: DeliveryId,
    kind: ForgeActionKind,
    payload: Value,
    target: String,
    intent: Value,
    idempotency_key: IdempotencyKey,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    requires_approval: Option<Option<ApprovalId>>,
    depends_on: Vec<ActionId>,
    priority: Priority,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    not_before: Option<Option<Timestamp>>,
    epoch: u32,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    coalesce_key: Option<Option<String>>,
    enqueued_by: OutboxEnqueuedBy,
}

impl<'de> Deserialize<'de> for OutboxEnqueuedAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawOutboxEnqueuedAction::deserialize(deserializer)?;
        let payload =
            ActionPayload::from_value(raw.kind, raw.payload).map_err(serde::de::Error::custom)?;
        if payload.kind() != raw.kind {
            return Err(serde::de::Error::custom(
                "outbox payload kind does not match action kind",
            ));
        }
        let intent =
            OutboxIntent::from_value(raw.kind, raw.intent).map_err(serde::de::Error::custom)?;
        let requires_approval = raw
            .requires_approval
            .ok_or_else(|| serde::de::Error::missing_field("requires_approval"))?;
        let not_before = raw
            .not_before
            .ok_or_else(|| serde::de::Error::missing_field("not_before"))?;
        let coalesce_key = raw
            .coalesce_key
            .ok_or_else(|| serde::de::Error::missing_field("coalesce_key"))?;
        Ok(Self {
            delivery_id: raw.delivery_id,
            kind: raw.kind,
            payload,
            target: raw.target,
            intent,
            idempotency_key: raw.idempotency_key,
            requires_approval,
            depends_on: raw.depends_on,
            priority: raw.priority,
            not_before,
            epoch: raw.epoch,
            coalesce_key,
            enqueued_by: raw.enqueued_by,
        })
    }
}

#[derive(JsonSchema)]
#[schemars(extend("additionalProperties" = false))]
#[allow(dead_code)]
struct OutboxEnqueuedActionBase {
    delivery_id: DeliveryId,
    kind: ForgeActionKind,
    payload: Value,
    target: String,
    intent: OutboxIntent,
    idempotency_key: IdempotencyKey,
    requires_approval: Option<ApprovalId>,
    depends_on: Vec<ActionId>,
    priority: Priority,
    not_before: Option<Timestamp>,
    epoch: u32,
    coalesce_key: Option<String>,
    enqueued_by: OutboxEnqueuedBy,
}

impl JsonSchema for OutboxEnqueuedAction {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "OutboxEnqueuedAction".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let base = generator.subschema_for::<OutboxEnqueuedActionBase>();
        crate::entities::tagged_payload_schema(
            generator,
            base,
            "outbox.enqueued 的不可变入队意图；`kind` 与 `payload` 必须联动。",
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxEnqueuedData {
    pub action: OutboxEnqueuedAction,
}

impl EventData for OutboxEnqueuedData {
    const TYPES: &'static [EventType] = &[EventType::OutboxEnqueued];

    fn validate(&self) -> Result<(), SchemaError> {
        self.action.validate()
    }
}

impl OutboxEnqueuedAction {
    fn expected_target(&self) -> String {
        match &self.payload {
            ActionPayload::PushBranch(payload) => format!("branch:{}", payload.branch),
            ActionPayload::CreatePr(payload) => {
                format!("pr:{}->{}", payload.head, payload.base)
            }
            ActionPayload::UpdatePr(payload) => format!("pr:{}", payload.pull_number),
            ActionPayload::UpsertStatusComment(payload) => format!(
                "comment:{}:{}:slot:{}",
                target_type(payload.target.kind),
                payload.target.number,
                payload.slot
            ),
            ActionPayload::CreateComment(payload) => format!(
                "comment:{}:{}:{}",
                target_type(payload.target.kind),
                payload.target.number,
                payload.client_ref
            ),
            ActionPayload::ReplyReviewThread(payload) => {
                format!("thread:{}", payload.thread_node_id)
            }
            ActionPayload::ResolveReviewThread(payload) => {
                format!("thread:{}:resolve", payload.thread_node_id)
            }
            ActionPayload::CreateIssue(payload) => format!("issue:new:{}", payload.client_ref),
            ActionPayload::UpdateIssue(payload) => format!("issue:{}:spec", payload.number),
            ActionPayload::CloseIssue(payload) => format!("issue:{}:close", payload.number),
            ActionPayload::AddLabels(payload) => format!("labels:{}", payload.number),
            ActionPayload::RequestReviewers(payload) => {
                format!("reviewers:{}", payload.pull_number)
            }
            ActionPayload::MergePr(payload) => format!("pr:{}:merge", payload.pull_number),
            ActionPayload::RerunFailedJobs(payload) => format!("run:{}", payload.run_id),
        }
    }

    fn validate(&self) -> Result<(), SchemaError> {
        if self.payload.kind() != self.kind {
            return Err(SchemaError::consistency(
                "/action/payload",
                "payload does not match kind",
            ));
        }
        let expected = self.expected_target();
        if self.target != expected {
            return Err(SchemaError::consistency(
                "/action/target",
                "target does not match the payload",
            ));
        }
        let mut seen = BTreeSet::new();
        for (index, id) in self.depends_on.iter().enumerate() {
            if !seen.insert(*id) {
                return Err(SchemaError::consistency(
                    format!("/action/depends_on/{index}"),
                    "depends_on must not contain duplicates",
                ));
            }
        }
        require_non_empty(&self.enqueued_by.module, "/action/enqueued_by/module")?;
        if let Some(key) = &self.coalesce_key {
            require_non_empty(key, "/action/coalesce_key")?;
            if self.kind != ForgeActionKind::UpsertStatusComment {
                return Err(SchemaError::consistency(
                    "/action/coalesce_key",
                    "only upsert_status_comment may carry coalesce_key",
                ));
            }
        }
        self.validate_intent()
    }

    fn validate_intent(&self) -> Result<(), SchemaError> {
        let mismatch =
            |pointer: &str| SchemaError::consistency(pointer, "intent does not match the payload");
        match (&self.payload, &self.intent) {
            (ActionPayload::PushBranch(payload), OutboxIntent::PushBranch(intent)) => {
                if intent.local_head_oid != payload.local_head_oid
                    || intent.expected_remote_oid != payload.expected_remote_oid
                    || intent.force != payload.force
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            (ActionPayload::CreatePr(payload), OutboxIntent::CreatePr(intent)) => {
                if intent.base != payload.base
                    || intent.head != payload.head
                    || intent.title != payload.title
                    || intent.body != payload.body
                    || intent.draft != payload.draft
                    || intent.closes != payload.closes
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            (ActionPayload::UpdatePr(payload), OutboxIntent::UpdatePr(intent)) => {
                if intent.title != payload.title
                    || intent.body != payload.body
                    || intent.base != payload.base
                    || intent.state != payload.state
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            (
                ActionPayload::UpsertStatusComment(payload),
                OutboxIntent::UpsertStatusComment(intent),
            ) => {
                if intent.revision != payload.revision {
                    return Err(SchemaError::consistency(
                        "/action/intent/revision",
                        "intent and payload revision must be equal",
                    ));
                }
            }
            (ActionPayload::CreateComment(payload), OutboxIntent::CreateComment(intent)) => {
                if intent.body != payload.body {
                    return Err(mismatch("/action/intent"));
                }
            }
            (
                ActionPayload::ReplyReviewThread(payload),
                OutboxIntent::ReplyReviewThread(intent),
            ) => {
                if intent.body != payload.body
                    || intent.in_reply_to_comment_id != payload.in_reply_to_comment_id
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            (ActionPayload::ResolveReviewThread(_), OutboxIntent::ResolveReviewThread(_)) => {}
            (ActionPayload::CreateIssue(payload), OutboxIntent::CreateIssue(intent)) => {
                if intent.title != payload.title
                    || intent.body != payload.body
                    || intent.labels != payload.labels
                    || intent.assignees != payload.assignees
                    || intent.milestone != payload.milestone
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            (ActionPayload::UpdateIssue(payload), OutboxIntent::UpdateIssue(intent)) => {
                if intent.title != payload.title || intent.labels != payload.labels {
                    return Err(mismatch("/action/intent"));
                }
            }
            (ActionPayload::CloseIssue(payload), OutboxIntent::CloseIssue(intent)) => {
                if intent.state_reason != payload.state_reason
                    || intent.require_merged_pr != payload.require_merged_pr
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            (ActionPayload::AddLabels(payload), OutboxIntent::AddLabels(intent)) => {
                if intent.labels != payload.labels {
                    return Err(mismatch("/action/intent"));
                }
                ensure_sorted_unique(&intent.labels, "/action/intent/labels")?;
            }
            (ActionPayload::RequestReviewers(payload), OutboxIntent::RequestReviewers(intent)) => {
                if intent.reviewers != payload.reviewers
                    || intent.team_reviewers != payload.team_reviewers
                {
                    return Err(mismatch("/action/intent"));
                }
                ensure_sorted_unique(&intent.reviewers, "/action/intent/reviewers")?;
                ensure_sorted_unique(&intent.team_reviewers, "/action/intent/team_reviewers")?;
            }
            (ActionPayload::MergePr(payload), OutboxIntent::MergePr(intent)) => {
                if intent.expected_head_sha != payload.expected_head_sha
                    || intent.merge_method != payload.merge_method
                    || intent.mode != payload.mode
                    || intent.commit_title != payload.commit_title
                    || intent.commit_message != payload.commit_message
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            (ActionPayload::RerunFailedJobs(payload), OutboxIntent::RerunFailedJobs(intent)) => {
                if intent.expected_run_attempt != payload.expected_run_attempt
                    || intent.head_sha != payload.head_sha
                {
                    return Err(mismatch("/action/intent"));
                }
            }
            _ => {
                return Err(SchemaError::consistency(
                    "/action/intent",
                    "intent branch does not match payload kind",
                ))
            }
        }
        Ok(())
    }
}

fn target_type(kind: CommentTargetType) -> &'static str {
    match kind {
        CommentTargetType::Pr => "pr",
        CommentTargetType::Issue => "issue",
    }
}

fn check_outbox_enqueued_shape(data: &Value) -> Result<(), SchemaError> {
    deny_keys_at(data, "", &["action"])?;
    require_keys_at(data, "", &["action"])?;
    let action = nested(data, &["action"]).expect("action is present");
    deny_keys_at(
        action,
        "/action",
        &[
            "delivery_id",
            "kind",
            "payload",
            "target",
            "intent",
            "idempotency_key",
            "requires_approval",
            "depends_on",
            "priority",
            "not_before",
            "epoch",
            "coalesce_key",
            "enqueued_by",
        ],
    )?;
    require_keys_at(
        action,
        "/action",
        &[
            "delivery_id",
            "kind",
            "payload",
            "target",
            "intent",
            "idempotency_key",
            "requires_approval",
            "depends_on",
            "priority",
            "not_before",
            "epoch",
            "coalesce_key",
            "enqueued_by",
        ],
    )?;
    action
        .get("target")
        .and_then(Value::as_str)
        .ok_or_else(|| SchemaError::shape("/action/target", "target must be a string"))?;
    if let Some(coalesce) = action.get("coalesce_key") {
        if !coalesce.is_null() {
            let Some(coalesce) = coalesce.as_str() else {
                return Err(SchemaError::shape(
                    "/action/coalesce_key",
                    "coalesce_key must be a string or null",
                ));
            };
            require_non_empty(coalesce, "/action/coalesce_key")?;
        }
    }
    if let (Some(kind), Some(intent)) = (
        action.get("kind").and_then(Value::as_str),
        action.get("intent"),
    ) {
        if intent.is_object() {
            let optional_null_fields: &[&str] = match kind {
                "update_pr" => &["title", "body", "base", "state"],
                "create_issue" => &["milestone"],
                "update_issue" => &["title", "labels"],
                "close_issue" => &["require_merged_pr"],
                "merge_pr" => &["commit_title", "commit_message"],
                _ => &[],
            };
            reject_null_keys_at(intent, "/action/intent", optional_null_fields)?;
        }
    }
    if let Some(enqueued_by) = action.get("enqueued_by") {
        deny_keys_at(
            enqueued_by,
            "/action/enqueued_by",
            &["module", "causation_id"],
        )?;
        require_keys_at(
            enqueued_by,
            "/action/enqueued_by",
            &["module", "causation_id"],
        )?;
        let module = enqueued_by
            .get("module")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SchemaError::shape("/action/enqueued_by/module", "module must be a string")
            })?;
        require_non_empty(module, "/action/enqueued_by/module")?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxExecutingData {
    pub attempts: u32,
    pub replay: bool,
}

impl EventData for OutboxExecutingData {
    const TYPES: &'static [EventType] = &[EventType::OutboxExecuting];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.attempts == 0 {
            return Err(SchemaError::shape("/attempts", "attempts must be >= 1"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutboxCancelReason {
    Superseded,
    DependencyFailed,
    ApprovalInvalid,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxCancelledData {
    pub reason: OutboxCancelReason,
}

impl EventData for OutboxCancelledData {
    const TYPES: &'static [EventType] = &[EventType::OutboxCancelled];
}

// ---------------------------------------------------------------------------
// forge.synced（event-data §2.2.5.1）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForgeSyncedTargetKind {
    Pr,
    Issue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgeSyncedTarget {
    #[serde(rename = "type")]
    pub kind: ForgeSyncedTargetKind,
    pub number: u64,
    pub node_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForgeSyncedState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum ForgeSyncedChange {
    State {
        from: ForgeSyncedState,
        to: ForgeSyncedState,
    },
    StateReason {
        from: Option<IssueStateReason>,
        to: Option<IssueStateReason>,
    },
    Title {
        from: UntrustedText,
        to: UntrustedText,
    },
    HeadSha {
        from: GitOid,
        to: GitOid,
    },
    BaseRef {
        from: String,
        to: String,
    },
    BaseSha {
        from: GitOid,
        to: GitOid,
    },
    Draft {
        from: bool,
        to: bool,
    },
    MergeableState {
        from: String,
        to: String,
    },
    InMergeQueue {
        from: bool,
        to: bool,
    },
    Checks {
        from: CheckSummary,
        to: CheckSummary,
    },
    ReviewState {
        from: ReviewState,
        to: ReviewState,
    },
}

impl ForgeSyncedChange {
    fn field(&self) -> &'static str {
        match self {
            Self::State { .. } => "state",
            Self::StateReason { .. } => "state_reason",
            Self::Title { .. } => "title",
            Self::HeadSha { .. } => "head_sha",
            Self::BaseRef { .. } => "base_ref",
            Self::BaseSha { .. } => "base_sha",
            Self::Draft { .. } => "draft",
            Self::MergeableState { .. } => "mergeable_state",
            Self::InMergeQueue { .. } => "in_merge_queue",
            Self::Checks { .. } => "checks",
            Self::ReviewState { .. } => "review_state",
        }
    }

    fn ordinal(&self) -> u8 {
        match self {
            Self::State { .. } => 0,
            Self::StateReason { .. } => 1,
            Self::Title { .. } => 2,
            Self::HeadSha { .. } => 3,
            Self::BaseRef { .. } => 4,
            Self::BaseSha { .. } => 5,
            Self::Draft { .. } => 6,
            Self::MergeableState { .. } => 7,
            Self::InMergeQueue { .. } => 8,
            Self::Checks { .. } => 9,
            Self::ReviewState { .. } => 10,
        }
    }

    fn changed(&self) -> bool {
        match self {
            Self::State { from, to } => from != to,
            Self::StateReason { from, to } => from != to,
            Self::Title { from, to } => from != to,
            Self::HeadSha { from, to } => from != to,
            Self::BaseRef { from, to } => from != to,
            Self::BaseSha { from, to } => from != to,
            Self::Draft { from, to } => from != to,
            Self::MergeableState { from, to } => from != to,
            Self::InMergeQueue { from, to } => from != to,
            Self::Checks { from, to } => from != to,
            Self::ReviewState { from, to } => from != to,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgeSyncedData {
    pub target: ForgeSyncedTarget,
    pub source: ForgeEventSource,
    pub changes: Vec<ForgeSyncedChange>,
    pub synced_at: Timestamp,
}

impl EventData for ForgeSyncedData {
    const TYPES: &'static [EventType] = &[EventType::ForgeSynced];

    fn validate(&self) -> Result<(), SchemaError> {
        if self.changes.is_empty() {
            return Err(SchemaError::shape(
                "/changes",
                "forge.synced requires at least one change",
            ));
        }
        let mut seen = BTreeSet::new();
        let mut last_ordinal = None;
        for (index, change) in self.changes.iter().enumerate() {
            let pointer = format!("/changes/{index}");
            let field = change.field();
            if !seen.insert(field) {
                return Err(SchemaError::consistency(
                    format!("{pointer}/field"),
                    "the same field may appear at most once",
                ));
            }
            let ordinal = change.ordinal();
            if last_ordinal.is_some_and(|last| last >= ordinal) {
                return Err(SchemaError::consistency(
                    format!("{pointer}/field"),
                    "changes must follow the contract field order",
                ));
            }
            last_ordinal = Some(ordinal);
            if !change.changed() {
                return Err(SchemaError::consistency(
                    format!("{pointer}/to"),
                    "from and to must differ",
                ));
            }
            match self.target.kind {
                ForgeSyncedTargetKind::Issue => {
                    if !matches!(field, "state" | "state_reason" | "title") {
                        return Err(SchemaError::consistency(
                            format!("{pointer}/field"),
                            "this field only applies to pull requests",
                        ));
                    }
                    if matches!(
                        change,
                        ForgeSyncedChange::State {
                            from: ForgeSyncedState::Merged,
                            ..
                        } | ForgeSyncedChange::State {
                            to: ForgeSyncedState::Merged,
                            ..
                        }
                    ) {
                        return Err(SchemaError::consistency(
                            format!("{pointer}/field"),
                            "issues cannot use the merged state",
                        ));
                    }
                }
                ForgeSyncedTargetKind::Pr => {
                    if field == "state_reason" {
                        return Err(SchemaError::consistency(
                            format!("{pointer}/field"),
                            "state_reason only applies to issues",
                        ));
                    }
                }
            }
            match change {
                ForgeSyncedChange::BaseRef { from, to } => {
                    require_non_empty(from, &format!("{pointer}/from"))?;
                    require_non_empty(to, &format!("{pointer}/to"))?;
                }
                ForgeSyncedChange::MergeableState { from, to } => {
                    require_non_empty(from, &format!("{pointer}/from"))?;
                    require_non_empty(to, &format!("{pointer}/to"))?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn check_forge_synced_shape(data: &Value) -> Result<(), SchemaError> {
    deny_keys_at(data, "", &["target", "source", "changes", "synced_at"])?;
    require_keys_at(data, "", &["target", "source", "changes", "synced_at"])?;
    reject_null_keys_at(data, "", &["target", "source", "changes", "synced_at"])?;
    if let Some(changes) = data.get("changes") {
        if !changes.is_array() {
            return Err(SchemaError::shape("/changes", "changes must be an array"));
        }
    }
    if let Some(target) = nested(data, &["target"]) {
        deny_keys_at(target, "/target", &["type", "number", "node_id"])?;
        require_keys_at(target, "/target", &["type", "number", "node_id"])?;
        if let Some(kind) = target.get("type").and_then(Value::as_str) {
            if !matches!(kind, "pr" | "issue") {
                return Err(SchemaError::shape(
                    "/target/type",
                    "forge.synced target type must be pr or issue",
                ));
            }
        }
        reject_null_keys_at(target, "/target", &["type", "number", "node_id"])?;
    }
    let Some(changes) = data.get("changes").and_then(Value::as_array) else {
        return Ok(());
    };
    for (index, change) in changes.iter().enumerate() {
        let pointer = format!("/changes/{index}");
        let map = object_at(change, &pointer)?;
        deny_keys_at(change, &pointer, &["field", "from", "to"])?;
        require_keys_at(change, &pointer, &["field", "from", "to"])?;
        let field = map.get("field").and_then(Value::as_str).ok_or_else(|| {
            SchemaError::shape(format!("{pointer}/field"), "field must be a string")
        })?;
        let allowed = [
            "state",
            "state_reason",
            "title",
            "head_sha",
            "base_ref",
            "base_sha",
            "draft",
            "mergeable_state",
            "in_merge_queue",
            "checks",
            "review_state",
        ];
        if !allowed.contains(&field) {
            return Err(SchemaError::shape(
                format!("{pointer}/field"),
                format!("unknown field `{field}`"),
            ));
        }
        if field != "state_reason" {
            reject_null_keys_at(change, &pointer, &["from", "to"])?;
        }
        match field {
            "title" => {
                for direction in ["from", "to"] {
                    let value = map.get(direction).expect("from/to is present");
                    let direction_pointer = format!("{pointer}/{direction}");
                    deny_keys_at(value, &direction_pointer, &["text", "source", "truncated"])?;
                    require_keys_at(value, &direction_pointer, &["text", "source", "truncated"])?;
                    reject_null_keys_at(
                        value,
                        &direction_pointer,
                        &["text", "source", "truncated"],
                    )?;
                }
            }
            "checks" => {
                for direction in ["from", "to"] {
                    let value = map.get(direction).expect("from/to is present");
                    let direction_pointer = format!("{pointer}/{direction}");
                    deny_keys_at(
                        value,
                        &direction_pointer,
                        &["passed", "failed", "pending", "required_ok"],
                    )?;
                    require_keys_at(
                        value,
                        &direction_pointer,
                        &["passed", "failed", "pending", "required_ok"],
                    )?;
                    reject_null_keys_at(
                        value,
                        &direction_pointer,
                        &["passed", "failed", "pending", "required_ok"],
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// budget.charged（event-data §2.2.6.1–§2.2.6.2）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetChargedDimension {
    TokensByTier,
    Nodes,
    Attempts,
    WallTime,
    Rounds,
    ForgeWrites,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BudgetChargeSource {
    Attempt { attempt_id: AttemptId },
    DirectCall { call_id: Ulid },
    Node { node_id: NodeId },
    ReviewRound { workflow_id: WorkflowId, round: u32 },
    HealRound { round_id: Ulid },
    OutboxAction { action_id: ActionId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WeightRev {
    pub config_rev: u64,
    pub weighted_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum WeightAudit {
    JobToken {
        config_rev: u64,
        weights: TokenWeights,
    },
    SvcRecords {
        record_count: u64,
        #[serde(default)]
        first_seq: Option<u64>,
        #[serde(default)]
        last_seq: Option<u64>,
        weight_revs: Vec<WeightRev>,
        records_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetChargedData {
    pub ledger: LedgerRef,
    pub dimension: BudgetChargedDimension,
    pub delta: u64,
    pub used_after: u64,
    pub source: BudgetChargeSource,
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    #[serde(default)]
    pub weighted_tokens: Option<u64>,
    #[serde(default)]
    pub weight_audit: Option<WeightAudit>,
}

impl EventData for BudgetChargedData {
    const TYPES: &'static [EventType] = &[EventType::BudgetCharged];

    fn validate(&self) -> Result<(), SchemaError> {
        let token = self.dimension == BudgetChargedDimension::TokensByTier;
        if token {
            if self.tier.is_none() {
                return Err(SchemaError::consistency(
                    "/tier",
                    "tokens_by_tier requires tier",
                ));
            }
            let usage = self.usage.as_ref().ok_or_else(|| {
                SchemaError::consistency("/usage", "tokens_by_tier requires usage")
            })?;
            let _ = usage;
            let weighted = self.weighted_tokens.ok_or_else(|| {
                SchemaError::consistency(
                    "/weighted_tokens",
                    "tokens_by_tier requires weighted_tokens",
                )
            })?;
            if weighted != self.delta {
                return Err(SchemaError::consistency(
                    "/weighted_tokens",
                    "weighted_tokens must equal delta",
                ));
            }
            let audit = self.weight_audit.as_ref().ok_or_else(|| {
                SchemaError::consistency("/weight_audit", "tokens_by_tier requires weight_audit")
            })?;
            self.validate_weight_audit(audit)?;
            match (&self.source, audit) {
                (BudgetChargeSource::Attempt { .. }, WeightAudit::JobToken { .. }) => {}
                (BudgetChargeSource::DirectCall { .. }, WeightAudit::SvcRecords { .. }) => {}
                (BudgetChargeSource::Attempt { .. }, WeightAudit::SvcRecords { .. }) => {
                    return Err(SchemaError::consistency(
                        "/weight_audit/basis",
                        "attempt charges use the job_token basis",
                    ))
                }
                (BudgetChargeSource::DirectCall { .. }, WeightAudit::JobToken { .. }) => {
                    return Err(SchemaError::consistency(
                        "/weight_audit/basis",
                        "direct calls use the svc_records basis",
                    ))
                }
                _ => {
                    return Err(SchemaError::consistency(
                        "/source",
                        "tokens_by_tier source must be attempt or direct_call",
                    ))
                }
            }
        } else {
            if self.tier.is_some()
                || self.usage.is_some()
                || self.weighted_tokens.is_some()
                || self.weight_audit.is_some()
            {
                return Err(SchemaError::consistency(
                    "/tier",
                    "token fields are only allowed for tokens_by_tier",
                ));
            }
        }
        match self.dimension {
            BudgetChargedDimension::TokensByTier => {}
            BudgetChargedDimension::Nodes => {
                if !matches!(self.source, BudgetChargeSource::Node { .. }) {
                    return Err(SchemaError::consistency(
                        "/source",
                        "nodes requires a node source",
                    ));
                }
                if self.delta != 1 {
                    return Err(SchemaError::consistency("/delta", "nodes delta must be 1"));
                }
            }
            BudgetChargedDimension::Attempts => {
                if !matches!(self.source, BudgetChargeSource::Attempt { .. }) {
                    return Err(SchemaError::consistency(
                        "/source",
                        "attempts requires an attempt source",
                    ));
                }
                if self.delta != 1 {
                    return Err(SchemaError::consistency(
                        "/delta",
                        "attempts delta must be 1",
                    ));
                }
            }
            BudgetChargedDimension::WallTime => {
                if !matches!(self.source, BudgetChargeSource::Attempt { .. }) {
                    return Err(SchemaError::consistency(
                        "/source",
                        "wall_time requires an attempt source",
                    ));
                }
            }
            BudgetChargedDimension::Rounds => {
                if !matches!(
                    self.source,
                    BudgetChargeSource::ReviewRound { .. } | BudgetChargeSource::HealRound { .. }
                ) {
                    return Err(SchemaError::consistency(
                        "/source",
                        "rounds requires a review_round or heal_round source",
                    ));
                }
                if let BudgetChargeSource::ReviewRound { round, .. } = self.source {
                    if round == 0 {
                        return Err(SchemaError::shape("/source/round", "round must be >= 1"));
                    }
                }
                if self.delta != 1 {
                    return Err(SchemaError::consistency("/delta", "rounds delta must be 1"));
                }
            }
            BudgetChargedDimension::ForgeWrites => {
                if !matches!(self.source, BudgetChargeSource::OutboxAction { .. }) {
                    return Err(SchemaError::consistency(
                        "/source",
                        "forge_writes requires an outbox_action source",
                    ));
                }
                if self.delta != 1 {
                    return Err(SchemaError::consistency(
                        "/delta",
                        "forge_writes delta must be 1",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl BudgetChargedData {
    fn validate_weight_audit(&self, audit: &WeightAudit) -> Result<(), SchemaError> {
        match audit {
            WeightAudit::JobToken { weights, .. } => {
                for (name, value) in [
                    ("input", weights.input),
                    ("output", weights.output),
                    ("cache_read", weights.cache_read),
                    ("cache_write", weights.cache_write),
                ] {
                    if !value.is_finite() || value <= 0.0 {
                        return Err(SchemaError::shape(
                            format!("/weight_audit/weights/{name}"),
                            "token weight must be a positive finite number",
                        ));
                    }
                }
            }
            WeightAudit::SvcRecords {
                record_count,
                first_seq,
                last_seq,
                weight_revs,
                ..
            } => {
                if weight_revs.is_empty() {
                    // 零记录且零增量的审计事件允许没有修订项；其他情况必须有
                    // 可核对的修订明细，不能用空数组冒充结算证据。
                    if !(*record_count == 0 && self.delta == 0) {
                        return Err(SchemaError::shape(
                            "/weight_audit/weight_revs",
                            "svc_records requires at least one weight revision",
                        ));
                    }
                }
                let mut seen_revs = BTreeSet::new();
                let mut sum = 0u64;
                for (index, rev) in weight_revs.iter().enumerate() {
                    if !seen_revs.insert(rev.config_rev) {
                        return Err(SchemaError::consistency(
                            format!("/weight_audit/weight_revs/{index}/config_rev"),
                            "config revisions must be merged and unique",
                        ));
                    }
                    sum = sum.saturating_add(rev.weighted_tokens);
                }
                if sum != self.delta {
                    return Err(SchemaError::consistency(
                        "/weight_audit/weight_revs",
                        "weighted_tokens must sum to delta",
                    ));
                }
                if first_seq.is_some() != last_seq.is_some() {
                    return Err(SchemaError::consistency(
                        "/weight_audit",
                        "first_seq and last_seq must appear together",
                    ));
                }
                if let (Some(first), Some(last)) = (first_seq, last_seq) {
                    if first > last {
                        return Err(SchemaError::consistency(
                            "/weight_audit/first_seq",
                            "first_seq must not exceed last_seq",
                        ));
                    }
                }
                if *record_count == 0 && (first_seq.is_some() || last_seq.is_some()) {
                    return Err(SchemaError::consistency(
                        "/weight_audit/record_count",
                        "zero records must omit first_seq and last_seq",
                    ));
                }
                if *record_count > 0 && first_seq.is_none() {
                    return Err(SchemaError::consistency(
                        "/weight_audit/first_seq",
                        "non-zero record_count requires first_seq and last_seq",
                    ));
                }
                if matches!(self.source, BudgetChargeSource::DirectCall { .. }) {
                    // B-01：direct call 必须先取得调用级终结证据；单条事件只能校验
                    // 证据形状与至少一条记录，不能替代端点契约。
                    if *record_count == 0 {
                        return Err(SchemaError::consistency(
                            "/weight_audit/record_count",
                            "direct call settlement requires at least one record (B-01)",
                        ));
                    }
                    if first_seq.is_none() {
                        return Err(SchemaError::consistency(
                            "/weight_audit/first_seq",
                            "direct call settlement requires the record range (B-01)",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn check_budget_shape(data: &Value) -> Result<(), SchemaError> {
    deny_keys_at(
        data,
        "",
        &[
            "ledger",
            "dimension",
            "delta",
            "used_after",
            "source",
            "tier",
            "usage",
            "weighted_tokens",
            "weight_audit",
        ],
    )?;
    require_keys_at(
        data,
        "",
        &["ledger", "dimension", "delta", "used_after", "source"],
    )?;
    reject_null_keys_at(
        data,
        "",
        &[
            "ledger",
            "dimension",
            "delta",
            "used_after",
            "source",
            "tier",
            "usage",
            "weighted_tokens",
            "weight_audit",
        ],
    )?;
    if let Some(ledger) = nested(data, &["ledger"]) {
        let subject = ledger.get("subject").and_then(Value::as_str);
        let allowed: &[&str] = match subject {
            Some("delivery") => &["subject", "id"],
            Some("conversation") => &["subject", "id", "day"],
            _ => &["subject", "id", "day"],
        };
        deny_keys_at(ledger, "/ledger", allowed)?;
        require_keys_at(ledger, "/ledger", allowed)?;
        reject_null_keys_at(ledger, "/ledger", allowed)?;
    }
    if let Some(usage) = data.get("usage") {
        deny_keys_at(
            usage,
            "/usage",
            &[
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "requests",
            ],
        )?;
        require_keys_at(
            usage,
            "/usage",
            &[
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "requests",
            ],
        )?;
        reject_null_keys_at(
            usage,
            "/usage",
            &[
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "requests",
            ],
        )?;
    }
    if let Some(source) = nested(data, &["source"]) {
        let kind = source
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let allowed: &[&str] = match kind {
            "attempt" => &["kind", "attempt_id"],
            "direct_call" => &["kind", "call_id"],
            "node" => &["kind", "node_id"],
            "review_round" => &["kind", "workflow_id", "round"],
            "heal_round" => &["kind", "round_id"],
            "outbox_action" => &["kind", "action_id"],
            _ => return Ok(()),
        };
        deny_keys_at(source, "/source", allowed)?;
        require_keys_at(source, "/source", allowed)?;
        reject_null_keys_at(source, "/source", allowed)?;
    }
    if let Some(audit) = nested(data, &["weight_audit"]) {
        let basis = audit
            .get("basis")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let allowed: &[&str] = match basis {
            "job_token" => &["basis", "config_rev", "weights"],
            "svc_records" => &[
                "basis",
                "record_count",
                "first_seq",
                "last_seq",
                "weight_revs",
                "records_digest",
            ],
            _ => return Ok(()),
        };
        deny_keys_at(audit, "/weight_audit", allowed)?;
        require_keys_at(
            audit,
            "/weight_audit",
            if basis == "job_token" {
                &["basis", "config_rev", "weights"]
            } else {
                &["basis", "record_count", "weight_revs", "records_digest"]
            },
        )?;
        reject_null_keys_at(
            audit,
            "/weight_audit",
            &[
                "config_rev",
                "weights",
                "record_count",
                "weight_revs",
                "records_digest",
            ],
        )?;
        if basis == "job_token" {
            if let Some(weights) = audit.get("weights") {
                deny_keys_at(
                    weights,
                    "/weight_audit/weights",
                    &["input", "output", "cache_read", "cache_write"],
                )?;
                require_keys_at(
                    weights,
                    "/weight_audit/weights",
                    &["input", "output", "cache_read", "cache_write"],
                )?;
                reject_null_keys_at(
                    weights,
                    "/weight_audit/weights",
                    &["input", "output", "cache_read", "cache_write"],
                )?;
            }
        }
        if let Some(revs) = audit.get("weight_revs").and_then(Value::as_array) {
            for (index, rev) in revs.iter().enumerate() {
                let pointer = format!("/weight_audit/weight_revs/{index}");
                deny_keys_at(rev, &pointer, &["config_rev", "weighted_tokens"])?;
                require_keys_at(rev, &pointer, &["config_rev", "weighted_tokens"])?;
                reject_null_keys_at(rev, &pointer, &["config_rev", "weighted_tokens"])?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// policy.denied（event-data §2.2.6.4）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOrigin {
    Action,
    Patch,
    Proposal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PolicyReasonCode {
    #[serde(rename = "policy.action_denied")]
    ActionDenied,
    #[serde(rename = "policy.approval_denied")]
    ApprovalDenied,
    #[serde(rename = "policy.approval_consumed")]
    ApprovalConsumed,
    #[serde(rename = "policy.head_mismatch")]
    HeadMismatch,
    #[serde(rename = "policy.no_validated_patch")]
    NoValidatedPatch,
    #[serde(rename = "policy.patch_denied")]
    PatchDenied,
    #[serde(rename = "policy.multiple_approvals_required")]
    MultipleApprovalsRequired,
    #[serde(rename = "policy.approval_shape_undefined")]
    ApprovalShapeUndefined,
    #[serde(rename = "policy.approval_chain_unavailable")]
    ApprovalChainUnavailable,
    #[serde(rename = "policy.patch_not_approved")]
    PatchNotApproved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyProposalKind {
    OpenPr,
    ReplyReviewThread,
    ResolveReviewThread,
    Escalate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyTarget {
    Pr {
        repo: RepoSlug,
        number: u64,
    },
    Branch {
        repo: RepoSlug,
        branch: String,
    },
    ReviewThread {
        repo: RepoSlug,
        number: u64,
        thread_node_id: String,
    },
    CommentTarget {
        repo: RepoSlug,
        target: CommentTarget,
        slot: String,
    },
    CommentCreation {
        repo: RepoSlug,
        target: CommentTarget,
        client_ref: String,
    },
    Issue {
        repo: RepoSlug,
        number: u64,
    },
    IssueCreation {
        repo: RepoSlug,
        client_ref: String,
    },
    Run {
        repo: RepoSlug,
        run_id: u64,
    },
    PrCreation {
        repo: RepoSlug,
        head: String,
        base: String,
    },
    Patch {
        repo: RepoSlug,
        paths: Vec<UntrustedText>,
        path_digest: Sha256Digest,
        rules: Vec<String>,
    },
}

impl PolicyTarget {
    fn kind(&self) -> &'static str {
        match self {
            Self::Pr { .. } => "pr",
            Self::Branch { .. } => "branch",
            Self::ReviewThread { .. } => "review_thread",
            Self::CommentTarget { .. } => "comment_target",
            Self::CommentCreation { .. } => "comment_creation",
            Self::Issue { .. } => "issue",
            Self::IssueCreation { .. } => "issue_creation",
            Self::Run { .. } => "run",
            Self::PrCreation { .. } => "pr_creation",
            Self::Patch { .. } => "patch",
        }
    }

    fn validate(&self, _pointer: &str) -> Result<(), SchemaError> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PolicySubject {
    Head(GitOid),
    Patch(Sha256Digest),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyDeniedReason {
    pub code: String,
    #[serde(default)]
    pub path: Option<UntrustedText>,
    #[serde(default)]
    pub rule_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyDeniedData {
    pub origin: PolicyOrigin,
    #[serde(default)]
    pub action_id: Option<PolicyActionId>,
    #[serde(default)]
    pub kind: Option<ForgeActionKind>,
    #[serde(default)]
    pub target: Option<PolicyTarget>,
    #[serde(default)]
    pub reason_code: Option<PolicyReasonCode>,
    #[serde(default)]
    pub reasons: Option<Vec<PolicyDeniedReason>>,
    #[serde(default)]
    pub proposal_id: Option<String>,
    #[serde(default)]
    pub proposal_kind: Option<PolicyProposalKind>,
    #[serde(default)]
    pub target_handle: Option<String>,
    #[serde(default)]
    pub subject: Option<PolicySubject>,
    pub config_rev: u64,
}

impl EventData for PolicyDeniedData {
    const TYPES: &'static [EventType] = &[EventType::PolicyDenied];

    fn validate(&self) -> Result<(), SchemaError> {
        match self.origin {
            PolicyOrigin::Action => {
                if self.reasons.is_some()
                    || self.proposal_id.is_some()
                    || self.proposal_kind.is_some()
                    || self.target_handle.is_some()
                {
                    return Err(SchemaError::consistency(
                        "/origin",
                        "action origin does not carry patch/proposal fields",
                    ));
                }
                let action_id = self.action_id.ok_or_else(|| {
                    SchemaError::consistency("/action_id", "action origin requires action_id")
                })?;
                let kind = self.kind.ok_or_else(|| {
                    SchemaError::consistency("/kind", "action origin requires kind")
                })?;
                let target = self.target.as_ref().ok_or_else(|| {
                    SchemaError::consistency("/target", "action origin requires target")
                })?;
                if self.reason_code.is_none() {
                    return Err(SchemaError::consistency(
                        "/reason_code",
                        "action origin requires reason_code",
                    ));
                }
                let Some((expected_kind, expected_target)) = policy_action_shape(action_id) else {
                    return Err(SchemaError::consistency(
                        "/action_id",
                        "reserved policy action has no executable event path",
                    ));
                };
                if kind != expected_kind {
                    return Err(SchemaError::consistency(
                        "/kind",
                        "kind does not match action_id",
                    ));
                }
                if target.kind() != expected_target {
                    return Err(SchemaError::consistency(
                        "/target/kind",
                        "target kind does not match action_id",
                    ));
                }
                target.validate("/target")?;
                if matches!(target, PolicyTarget::Patch { .. }) {
                    return Err(SchemaError::consistency(
                        "/target/kind",
                        "action origin cannot use the patch target",
                    ));
                }
            }
            PolicyOrigin::Patch => {
                for (present, pointer, field) in [
                    (self.action_id.is_some(), "/action_id", "action_id"),
                    (self.kind.is_some(), "/kind", "kind"),
                    (self.reason_code.is_some(), "/reason_code", "reason_code"),
                    (self.proposal_id.is_some(), "/proposal_id", "proposal_id"),
                    (
                        self.proposal_kind.is_some(),
                        "/proposal_kind",
                        "proposal_kind",
                    ),
                    (
                        self.target_handle.is_some(),
                        "/target_handle",
                        "target_handle",
                    ),
                ] {
                    if present {
                        return Err(SchemaError::consistency(
                            pointer,
                            format!("patch origin does not carry {field}"),
                        ));
                    }
                }
                let target = self.target.as_ref().ok_or_else(|| {
                    SchemaError::consistency("/target", "patch origin requires target")
                })?;
                if !matches!(target, PolicyTarget::Patch { .. }) {
                    return Err(SchemaError::consistency(
                        "/target/kind",
                        "patch origin requires a patch target",
                    ));
                }
                target.validate("/target")?;
                let reasons = self.reasons.as_ref().ok_or_else(|| {
                    SchemaError::consistency("/reasons", "patch origin requires reasons")
                })?;
                if reasons.is_empty() {
                    return Err(SchemaError::shape(
                        "/reasons",
                        "patch origin requires at least one reason",
                    ));
                }
                let Some(PolicySubject::Patch(_)) = &self.subject else {
                    return Err(SchemaError::consistency(
                        "/subject",
                        "patch origin requires a sha256 subject",
                    ));
                };
                for (index, reason) in reasons.iter().enumerate() {
                    validate_patch_reason(reason, &format!("/reasons/{index}"))?;
                }
            }
            PolicyOrigin::Proposal => {
                for (present, pointer, field) in [
                    (self.target.is_some(), "/target", "target"),
                    (self.action_id.is_some(), "/action_id", "action_id"),
                    (self.kind.is_some(), "/kind", "kind"),
                    (self.reasons.is_some(), "/reasons", "reasons"),
                ] {
                    if present {
                        return Err(SchemaError::consistency(
                            pointer,
                            format!("proposal origin does not carry {field}"),
                        ));
                    }
                }
                self.proposal_id.as_deref().ok_or_else(|| {
                    SchemaError::consistency("/proposal_id", "proposal origin requires proposal_id")
                })?;
                if self.proposal_kind.is_none() {
                    return Err(SchemaError::consistency(
                        "/proposal_kind",
                        "proposal origin requires proposal_kind",
                    ));
                }
                if self.reason_code.is_none() {
                    return Err(SchemaError::consistency(
                        "/reason_code",
                        "proposal origin requires reason_code",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn policy_action_shape(action_id: PolicyActionId) -> Option<(ForgeActionKind, &'static str)> {
    use ForgeActionKind as K;
    use PolicyActionId as A;
    Some(match action_id {
        A::MergePr => (K::MergePr, "pr"),
        A::ForcePush => (K::PushBranch, "branch"),
        A::PushOutsidePrefix => (K::PushBranch, "branch"),
        A::ResolveReviewThreadHuman => (K::ResolveReviewThread, "review_thread"),
        A::PushBranch => (K::PushBranch, "branch"),
        A::CreatePr => (K::CreatePr, "pr_creation"),
        A::UpdatePr => (K::UpdatePr, "pr"),
        A::UpsertStatusComment => (K::UpsertStatusComment, "comment_target"),
        A::CreateComment => (K::CreateComment, "comment_creation"),
        A::ReplyReviewThread => (K::ReplyReviewThread, "review_thread"),
        A::ResolveReviewThread => (K::ResolveReviewThread, "review_thread"),
        A::CreateIssue => (K::CreateIssue, "issue_creation"),
        A::UpdateIssue => (K::UpdateIssue, "issue"),
        A::CloseIssue => (K::CloseIssue, "issue"),
        A::AddLabels => (K::AddLabels, "issue"),
        A::RequestReviewers => (K::RequestReviewers, "pr"),
        A::RerunFailedJobs => (K::RerunFailedJobs, "run"),
        A::DeleteBranch | A::PublishRelease | A::ChangeRepoSettings => return None,
    })
}

fn validate_patch_reason(reason: &PolicyDeniedReason, pointer: &str) -> Result<(), SchemaError> {
    let fixed = [
        "policy.size",
        "policy.symlink",
        "policy.submodule",
        "policy.binary",
        "policy.path_outside_allow",
        "policy.path_denied",
        "policy.path_protected:workflows",
        "policy.path_protected:file_mode_change",
        "policy.secret",
        "policy.path_invalid",
        "policy.dotgit",
    ];
    if fixed.contains(&reason.code.as_str()) {
        return Ok(());
    }
    if let Some(rule) = reason.code.strip_prefix("policy.path_protected:") {
        require_non_empty(rule, &format!("{pointer}/code"))?;
        return Ok(());
    }
    Err(SchemaError::shape(
        format!("{pointer}/code"),
        "unknown patch policy reason code",
    ))
}

const POLICY_KEYS: &[&str] = &[
    "origin",
    "action_id",
    "kind",
    "target",
    "reason_code",
    "reasons",
    "proposal_id",
    "proposal_kind",
    "target_handle",
    "subject",
    "config_rev",
];

fn check_policy_target_shape(target: &Value) -> Result<(), SchemaError> {
    let map = object_at(target, "/target")?;
    let kind = map.get("kind").and_then(Value::as_str).unwrap_or_default();
    let allowed: &[&str] = match kind {
        "pr" => &["kind", "repo", "number"],
        "branch" => &["kind", "repo", "branch"],
        "review_thread" => &["kind", "repo", "number", "thread_node_id"],
        "comment_target" => &["kind", "repo", "target", "slot"],
        "comment_creation" => &["kind", "repo", "target", "client_ref"],
        "issue" => &["kind", "repo", "number"],
        "issue_creation" => &["kind", "repo", "client_ref"],
        "run" => &["kind", "repo", "run_id"],
        "pr_creation" => &["kind", "repo", "head", "base"],
        "patch" => &["kind", "repo", "paths", "path_digest", "rules"],
        _ => return Ok(()),
    };
    deny_keys_at(target, "/target", allowed)?;
    require_keys_at(target, "/target", allowed)?;
    reject_null_keys_at(target, "/target", allowed)?;
    if matches!(kind, "comment_target" | "comment_creation") {
        let comment = map.get("target").expect("target is present");
        deny_keys_at(comment, "/target/target", &["type", "number"])?;
        require_keys_at(comment, "/target/target", &["type", "number"])?;
        reject_null_keys_at(comment, "/target/target", &["type", "number"])?;
    }
    if kind == "patch" {
        if let Some(paths) = map.get("paths").and_then(Value::as_array) {
            for (index, path) in paths.iter().enumerate() {
                let pointer = format!("/target/paths/{index}");
                deny_keys_at(path, &pointer, &["text", "source", "truncated"])?;
                require_keys_at(path, &pointer, &["text", "source", "truncated"])?;
                reject_null_keys_at(path, &pointer, &["text", "source", "truncated"])?;
            }
        }
    }
    Ok(())
}

fn check_policy_shape(data: &Value) -> Result<(), SchemaError> {
    deny_keys_at(data, "", POLICY_KEYS)?;
    require_keys_at(data, "", &["origin", "config_rev"])?;
    reject_null_keys_at(
        data,
        "",
        &[
            "origin",
            "action_id",
            "kind",
            "target",
            "reason_code",
            "reasons",
            "proposal_id",
            "proposal_kind",
            "target_handle",
            "subject",
            "config_rev",
        ],
    )?;
    if let Some(target) = nested(data, &["target"]) {
        check_policy_target_shape(target)?;
    }
    if let Some(subject) = data.get("subject") {
        if serde_json::from_value::<PolicySubject>(subject.clone()).is_err() {
            return Err(SchemaError::shape(
                "/subject",
                "subject must be a GitOid or sha256 digest",
            ));
        }
    }
    if let Some(reasons) = data.get("reasons").and_then(Value::as_array) {
        for (index, reason) in reasons.iter().enumerate() {
            let pointer = format!("/reasons/{index}");
            deny_keys_at(reason, &pointer, &["code", "path", "rule_id"])?;
            require_keys_at(reason, &pointer, &["code"])?;
            reject_null_keys_at(reason, &pointer, &["code", "rule_id"])?;
            if let Some(path) = reason.get("path") {
                reject_null_keys_at(reason, &pointer, &["path"])?;
                let path_pointer = format!("{pointer}/path");
                deny_keys_at(path, &path_pointer, &["text", "source", "truncated"])?;
                require_keys_at(path, &path_pointer, &["text", "source", "truncated"])?;
                reject_null_keys_at(path, &path_pointer, &["text", "source", "truncated"])?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 形状校验
// ---------------------------------------------------------------------------

/// 形状校验：能反序列化为该类型的 data 结构，且满足一致性规则；失败返回带
/// JSON Pointer 的错误。`core` §7.5 未定义形状的事件返回 `ContractUndefined`。
pub fn validate_event_data(ty: &EventType, data: &Value) -> Result<(), SchemaError> {
    match ty {
        EventType::ProjectAdded | EventType::ProjectUpdated => {
            decode::<ProjectEventData>(data)?.validate()
        }
        EventType::ConversationCreated => decode::<ConversationEventData>(data)?.validate(),
        EventType::SettingsUpdated => {
            check_settings_shape(data)?;
            decode::<SettingsUpdatedData>(data)?.validate()
        }
        EventType::MessagePosted => {
            deny_keys(data, MESSAGE_KEYS)?;
            decode::<Message>(data).map(|_| ())
        }
        EventType::DeliveryCreated => decode::<DeliveryCreatedData>(data)?.validate(),
        EventType::DeliveryStageChanged => decode::<DeliveryStageChangedData>(data)?.validate(),
        EventType::DeliveryLevelChanged => decode::<DeliveryLevelChangedData>(data)?.validate(),
        EventType::DeliveryAbandoned => decode::<DeliveryAbandonedData>(data)?.validate(),
        EventType::DeliveryPaused => decode::<DeliveryPausedData>(data)?.validate(),
        EventType::DeliveryResumed => decode::<DeliveryResumedData>(data)?.validate(),
        EventType::WorkflowCreated => decode::<WorkflowCreatedData>(data)?.validate(),
        EventType::WorkflowRevised => decode::<WorkflowRevisedData>(data)?.validate(),
        EventType::WorkflowFinished => decode::<WorkflowFinishedData>(data)?.validate(),
        EventType::NodeQueued
        | EventType::NodeStarted
        | EventType::NodeFinished
        | EventType::NodeSuperseded => {
            let event: NodeEventData = decode(data)?;
            check_node_status(ty, &event)?;
            Ok(())
        }
        EventType::NodeRetryRequested => decode::<NodeRetryRequestedData>(data)?.validate(),
        EventType::AttemptStarted => decode::<AttemptStartedData>(data)?.validate(),
        EventType::AttemptFinished => decode::<AttemptFinishedData>(data)?.validate(),
        EventType::AttemptProgress => {
            check_attempt_progress_shape(data)?;
            decode::<AttemptProgressData>(data)?.validate()
        }
        EventType::GuardianTriaged => {
            check_guardian_triaged_shape(data)?;
            decode::<GuardianTriagedData>(data)?.validate()
        }
        EventType::GuardianHealRequested => decode::<GuardianHealRequestedData>(data)?.validate(),
        EventType::GuardianEscalated => {
            reject_null_keys_at(data, "", &["inbox_id", "fingerprint", "round_id"])?;
            decode::<GuardianEscalatedData>(data)?.validate()
        }
        EventType::PatchValidated => decode::<PatchValidatedData>(data)?.validate(),
        EventType::PatchRejected => decode::<PatchRejectedData>(data)?.validate(),
        EventType::SandboxCreated => decode::<SandboxCreatedData>(data)?.validate(),
        EventType::SandboxDestroyed => decode::<SandboxDestroyedData>(data)?.validate(),
        EventType::WorktreeCreated => decode::<WorktreeCreatedData>(data)?.validate(),
        EventType::WorktreeLocked => decode::<WorktreeLockedData>(data)?.validate(),
        EventType::WorktreeUnlocked => decode::<WorktreeUnlockedData>(data)?.validate(),
        EventType::WorktreeRemoved => decode::<WorktreeRemovedData>(data)?.validate(),
        EventType::InboxRaised => {
            check_inbox_raised_keys(data)?;
            decode::<InboxRaisedData>(data)?.validate()
        }
        EventType::InboxResolved => decode::<InboxResolvedData>(data)?.validate(),
        EventType::InboxDismissed => decode::<InboxDismissedData>(data)?.validate(),
        EventType::ApprovalRequested => decode::<ApprovalRequestedData>(data)?.validate(),
        EventType::ApprovalGranted => decode::<ApprovalGrantedData>(data)?.validate(),
        EventType::ApprovalConsumed => decode::<ApprovalConsumedData>(data)?.validate(),
        EventType::ApprovalInvalidated => {
            check_approval_invalidated_shape(data)?;
            decode::<ApprovalInvalidatedData>(data)?.validate()
        }
        EventType::ApprovalExpired => decode::<ApprovalExpiredData>(data)?.validate(),
        EventType::ApprovalDenied => {
            check_approval_denied_shape(data)?;
            decode::<ApprovalDeniedData>(data)?.validate()
        }
        EventType::OutboxEnqueued => {
            check_outbox_enqueued_shape(data)?;
            decode::<OutboxEnqueuedData>(data)?.validate()
        }
        EventType::OutboxExecuting => decode::<OutboxExecutingData>(data)?.validate(),
        EventType::OutboxCancelled => decode::<OutboxCancelledData>(data)?.validate(),
        EventType::OutboxSucceeded => decode::<OutboxSucceededData>(data)?.validate(),
        EventType::OutboxFailed => decode::<OutboxFailedData>(data)?.validate(),
        EventType::ForgeSynced => {
            check_forge_synced_shape(data)?;
            decode::<ForgeSyncedData>(data)?.validate()
        }
        EventType::BudgetCharged => {
            check_budget_shape(data)?;
            decode::<BudgetChargedData>(data)?.validate()
        }
        EventType::PolicyDenied => {
            check_policy_shape(data)?;
            decode::<PolicyDeniedData>(data)?.validate()
        }
        EventType::BudgetExhausted => decode::<BudgetExhaustedData>(data)?.validate(),
        EventType::BudgetLimitRaised => decode::<BudgetLimitRaisedData>(data)?.validate(),
        EventType::DevicePaired => decode::<DevicePairedData>(data)?.validate(),
        EventType::DeviceUpdated => decode::<DeviceUpdatedData>(data)?.validate(),
        EventType::DeviceRevoked => decode::<DeviceRevokedData>(data)?.validate(),
        EventType::Forge(kind) => {
            let envelope: ForgeEventEnvelope = decode(data)?;
            if envelope.kind != *kind {
                return Err(SchemaError::consistency(
                    "/kind",
                    "forge event data.kind does not match the event type",
                ));
            }
            envelope
                .validate()
                .map_err(|error| SchemaError::consistency("/data", error.to_string()))
        }
        EventType::ForgeDriftDetected => {
            let payload: crate::forge::DriftPayload = decode(data)?;
            payload
                .validate()
                .map_err(|error| SchemaError::consistency("/data", error.to_string()))
        }
    }
}

fn check_settings_shape(data: &Value) -> Result<(), SchemaError> {
    let Some(map) = data.as_object() else {
        return Ok(());
    };
    let Some(changes) = map.get("secret_changes").and_then(Value::as_array) else {
        return Ok(());
    };
    for (index, item) in changes.iter().enumerate() {
        let Some(object) = item.as_object() else {
            continue;
        };
        for key in object.keys() {
            if !["ref", "op", "version"].contains(&key.as_str()) {
                return Err(SchemaError::shape(
                    format!("/secret_changes/{index}/{}", escape_pointer(key)),
                    format!("unknown field `{key}`"),
                ));
            }
        }
    }
    Ok(())
}

fn check_node_status(ty: &EventType, data: &NodeEventData) -> Result<(), SchemaError> {
    use crate::entities::NodeStatus;
    let expected = match ty {
        EventType::NodeQueued => NodeStatus::Queued,
        EventType::NodeStarted => NodeStatus::Running,
        EventType::NodeFinished => {
            if matches!(
                data.status,
                NodeStatus::Succeeded
                    | NodeStatus::Failed
                    | NodeStatus::Skipped
                    | NodeStatus::Cancelled
            ) {
                return Ok(());
            }
            return Err(SchemaError::consistency(
                "/status",
                "node.finished requires a terminal status",
            ));
        }
        EventType::NodeSuperseded => NodeStatus::Superseded,
        _ => return Ok(()),
    };
    if data.status != expected {
        return Err(SchemaError::consistency(
            "/status",
            "status does not match the node event type",
        ));
    }
    Ok(())
}

fn check_inbox_raised_keys(data: &Value) -> Result<(), SchemaError> {
    let Some(map) = data.as_object() else {
        return Err(SchemaError::new(
            SchemaErrorCode::InvalidData,
            "",
            "data must be a JSON object",
        ));
    };
    for key in map.keys() {
        if key != "item" && key != "supersedes" {
            return Err(SchemaError::shape(
                format!("/{}", escape_pointer(key)),
                format!("unknown field `{key}`"),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 便捷构造
// ---------------------------------------------------------------------------

impl Event<Value> {
    pub fn validate_data(&self) -> Result<(), SchemaError> {
        validate_event_data(&self.ty, &self.data)?;
        if self.ty == EventType::SettingsUpdated {
            validate_settings_subject(self.subject.as_str(), self.project_id.as_ref(), &self.data)?;
        }
        Ok(())
    }
}

/// 供测试与构建器使用的空信封（`seq` 由 store 分配）。
impl NewEvent {
    pub fn new(
        ty: EventType,
        subject: Subject,
        project_id: Option<ProjectId>,
        actor: Actor,
        data: Value,
    ) -> Self {
        Self {
            ty,
            subject,
            project_id,
            actor,
            causation_id: None,
            correlation_id: None,
            data,
            dedup_key: None,
        }
    }

    pub fn with_dedup_key(mut self, key: impl Into<String>) -> Self {
        self.dedup_key = Some(key.into());
        self
    }
}

/// 供 catalog 测试使用：名称 → 事件类型。
pub fn event_type_by_name(name: &str) -> Option<EventType> {
    name.parse().ok()
}

/// 供 schema 生成与测试：类型在 catalog 中的元数据。
pub fn event_info(ty: EventType) -> Option<&'static EventTypeInfo> {
    EVENT_TYPES.iter().find(|info| info.name == ty.as_str())
}

/// 未定义事件集合（供文档与 schema 生成交叉检查）。
pub fn contract_undefined_types() -> Vec<EventType> {
    EventType::ALL
        .into_iter()
        .filter(|ty| {
            validate_event_data(ty, &Value::Object(Default::default())).map_err(|error| error.code)
                == Err(SchemaErrorCode::ContractUndefined)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_type_once() {
        assert_eq!(EVENT_TYPES.len(), 79);
        assert_eq!(EventType::ALL.len(), 79);
        for ty in EventType::ALL {
            assert_eq!(
                EVENT_TYPES
                    .iter()
                    .filter(|info| info.name == ty.as_str())
                    .count(),
                1,
                "missing or duplicate catalog row for {ty}"
            );
        }
    }

    #[test]
    fn node_status_is_checked_by_event_type() {
        let data = serde_json::json!({
            "node_id": "nod_01JB7M8V5X1Z3N6R9Q2K4W7YZC",
            "status": "running"
        });
        assert!(validate_event_data(&EventType::NodeStarted, &data).is_ok());
        assert!(validate_event_data(&EventType::NodeQueued, &data).is_err());
    }
}
