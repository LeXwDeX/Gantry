//! 事件信封、类型目录与 `data` 校验（data-model §5；core §6.5、§7.5）。
//!
//! 本模块只接受**权威契约明确写出字段**的 `data`。`core` §7.5 标为缺口的
//! 事件保留名称，但校验返回 [`SchemaErrorCode::ContractUndefined`] 并带被拒绝的
//! schema 标识，**不返回成功**；清单与依据见 `docs/m0-contract-gaps.md`。
//!
//! 已定义形状的 `data` 使用 `deny_unknown_fields` 的强类型结构；沿用实体字段的
//! 事件（`message.posted`、`inbox.raised`）在解码前显式拒绝未知顶层键。

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::canon::{GitOid, RepoSlug, Sha256Digest, SubjectDigest};
use crate::entities::{
    Actor, ApprovalAction, ApprovalTarget, BudgetDimension, BudgetLimits, DeliveryOrigin,
    DevicePlatform, DeviceRole, ForgeEventKind, GithubRef, InboxItem, IssueRef, Message,
    NodeStatus, SettingsResource, SettingsScope, Stage, StageStatus, TokenUsage, Verdict,
    WorkflowStatus,
};
use crate::ids::*;
use crate::level::{Effort, Level};
use crate::settings::KeyPath;
use crate::time::Timestamp;
use crate::worker::{PatchFile, PatchReason, PatchRejectReason, PatchVerdict};

pub use crate::entities::{ActorKind, Seq};
pub use crate::forge::{ForgeEventEnvelope, ForgeOrigin};

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
    /// 形状校验 + settings 的 subject / project_id 一致性。
    pub fn validate(&self) -> Result<(), SchemaError> {
        validate_event_data(&self.ty, &self.data)?;
        if self.ty == EventType::SettingsUpdated {
            validate_settings_subject(self.subject.as_str(), self.project_id.as_ref())?;
        }
        Ok(())
    }
}

fn validate_settings_subject(
    subject: &str,
    project_id: Option<&ProjectId>,
) -> Result<(), SchemaError> {
    if subject == "settings" {
        if project_id.is_some() {
            return Err(SchemaError::consistency(
                "/project_id",
                "global settings.updated must have project_id = null",
            ));
        }
        return Ok(());
    }
    if subject.starts_with("prj_") {
        if project_id.is_none() {
            return Err(SchemaError::consistency(
                "/project_id",
                "project settings.updated must carry project_id",
            ));
        }
        return Ok(());
    }
    Err(SchemaError::consistency(
        "/subject",
        "settings.updated subject must be \"settings\" or a project id",
    ))
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
            "pattern": "^(settings|(prj|dlv|wfl|nod|att|wtr|sbx|cnv|msg|inb|apr|act|dev|evt|bdg)_[0-9A-HJKMNP-TV-Z]{26})$",
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
    PolicyDenied,
    DevicePaired,
    DeviceUpdated,
    DeviceRevoked,
}

impl EventType {
    pub const ALL: [EventType; 78] = [
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
/// 项目 `DELETE` 时为 `null`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdatedData {
    pub scope: SettingsScope,
    pub resource: SettingsResource,
    pub schema_version: u32,
    pub base_rev: Seq,
    #[serde(default)]
    pub value: Option<Value>,
    pub relaxes: Vec<KeyPath>,
    pub secret_changes: Vec<SecretChange>,
}

impl SettingsUpdatedData {
    fn check_secret_values(&self) -> Result<(), SchemaError> {
        let Some(value) = &self.value else {
            return Ok(());
        };
        for pointer in secret_positions(value) {
            let target = pointer_value(value, &pointer);
            if let Some(map) = target.and_then(Value::as_object) {
                if map.contains_key("value") {
                    return Err(SchemaError::new(
                        SchemaErrorCode::SecretValueForbidden,
                        pointer.join("/"),
                        "SecretRef positions must not carry \"value\"; secret values never enter the event log",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl EventData for SettingsUpdatedData {
    const TYPES: &'static [EventType] = &[EventType::SettingsUpdated];

    fn validate(&self) -> Result<(), SchemaError> {
        self.check_secret_values()
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
fn secret_positions(value: &Value) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    let Some(root) = value.as_object() else {
        return found;
    };
    if let Some(models) = root.get("models").and_then(Value::as_object) {
        if let Some(providers) = models.get("providers").and_then(Value::as_object) {
            for id in providers.keys() {
                found.push(vec![
                    "models".into(),
                    "providers".into(),
                    id.clone(),
                    "credential".into(),
                ]);
            }
        }
    }
    if let Some(egress) = root.get("egress").and_then(Value::as_object) {
        if let Some(injection) = egress.get("injection").and_then(Value::as_object) {
            for id in injection.keys() {
                found.push(vec![
                    "egress".into(),
                    "injection".into(),
                    id.clone(),
                    "credential".into(),
                ]);
            }
        }
    }
    if let Some(github) = root.get("github").and_then(Value::as_object) {
        if let Some(app) = github.get("app").and_then(Value::as_object) {
            for key in ["private_key", "webhook_secret", "webhook_secret_previous"] {
                if app.contains_key(key) {
                    found.push(vec!["github".into(), "app".into(), key.into()]);
                }
            }
        }
    }
    found
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

/// `delivery.level_changed`（engine.md §11）：`effective` 只升（P-57）。
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
    #[serde(default)]
    pub served_by: Option<ServedBy>,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequestedData {
    pub action: ApprovalAction,
    pub target: ApprovalTarget,
    pub subject_digest: SubjectDigest,
    pub expires_at: Timestamp,
    pub config_rev: Seq,
    pub requested_by: Actor,
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
        EventType::OutboxSucceeded => decode::<OutboxSucceededData>(data)?.validate(),
        EventType::OutboxFailed => decode::<OutboxFailedData>(data)?.validate(),
        EventType::BudgetExhausted => decode::<BudgetExhaustedData>(data)?.validate(),
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
        EventType::AttemptProgress
        | EventType::GuardianTriaged
        | EventType::GuardianHealRequested
        | EventType::GuardianEscalated
        | EventType::ApprovalInvalidated
        | EventType::ApprovalExpired
        | EventType::ApprovalDenied
        | EventType::OutboxEnqueued
        | EventType::OutboxExecuting
        | EventType::OutboxCancelled
        | EventType::ForgeSynced
        | EventType::ForgeDriftDetected
        | EventType::BudgetCharged
        | EventType::PolicyDenied => Err(SchemaError::undefined(*ty)),
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
        validate_event_data(&self.ty, &self.data)
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
        assert_eq!(EVENT_TYPES.len(), 78);
        assert_eq!(EventType::ALL.len(), 78);
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
