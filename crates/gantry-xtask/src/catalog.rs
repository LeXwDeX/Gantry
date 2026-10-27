//! 目录：哪些文件从哪些真实 Rust 类型生成。
//!
//! 这里只有映射与元数据；形状一律由 [`schema::for_type`] 或 `core` 已有的
//! `resource_schema` 产生。未定义形状的事件与字段显式拒绝，不生成宽松对象。

use gantry_core::egress::{
    ActivateSecretResponse, CloseCallResponse, ConfigApplyResponse, ConfigView, EndpointsResponse,
    FinalUsage, IssueJobTokenRequest, IssueJobTokenResponse, IssueSvcTokenRequest,
    IssueSvcTokenResponse, PresentRequest, PresentResponse, ProblemDetails, RevokeSvcTokenRequest,
    RevokeSvcTokenResponse, StageSecretRequest, StageSecretResponse, StagedSecretList,
    StatusResponse, UsagePage,
};
use gantry_core::entities::{
    Approval, Attempt, BudgetLedger, Conversation, Delivery, Device, ForgeEventKind, InboxItem,
    Message, Node, OutboxAction, Project, Sandbox, SettingsResource, Workflow, Worktree,
};
use gantry_core::events::{
    ApprovalConsumedData, ApprovalDeniedData, ApprovalExpiredData, ApprovalGrantedData,
    ApprovalInvalidatedData, ApprovalRequestedData, AttemptFinishedData, AttemptProgressData,
    AttemptStartedData, BudgetChargedData, BudgetExhaustedData, ConversationEventData,
    DeliveryAbandonedData, DeliveryCreatedData, DeliveryLevelChangedData, DeliveryPausedData,
    DeliveryResumedData, DeliveryStageChangedData, DevicePairedData, DeviceRevokedData,
    DeviceUpdatedData, Event, EventType, ForgeSyncedData, GuardianEscalatedData,
    GuardianHealRequestedData, GuardianTriagedData, InboxDismissedData, InboxRaisedData,
    InboxResolvedData, NodeEventData, NodeRetryRequestedData, OutboxCancelledData,
    OutboxEnqueuedData, OutboxExecutingData, OutboxFailedData, OutboxSucceededData,
    PatchRejectedData, PatchValidatedData, PolicyDeniedData, ProjectEventData, SandboxCreatedData,
    SandboxDestroyedData, SettingsUpdatedData, WorkflowCreatedData, WorkflowFinishedData,
    WorkflowRevisedData, WorktreeCreatedData, WorktreeLockedData, WorktreeRemovedData,
    WorktreeUnlockedData,
};
use gantry_core::forge::{
    AddLabelsPayload, CloseIssuePayload, CreateCommentPayload, CreateIssuePayload, CreatePrPayload,
    DriftPayload, ForgeEventEnvelope, MergePrPayload, PushBranchPayload, ReplyReviewThreadPayload,
    RequestReviewersPayload, RerunFailedJobsPayload, ResolveReviewThreadPayload,
    UpdateIssuePayload, UpdatePrPayload, UpsertStatusCommentPayload,
};
use gantry_core::ports::OutboxEnqueueEnvelope;
use gantry_core::settings;
use gantry_core::worker::{
    JobSpec, ProgressEvent, Proposals, ResultBundle, ResultJson, StructuredOutput,
};
use serde_json::Value;

use crate::schema;

pub struct Artifact {
    pub path: String,
    pub value: Value,
    /// OpenAPI `components.schemas` 中对应的实体名；仅实体目录使用。
    pub openapi: Option<&'static str>,
}

impl Artifact {
    fn typed<T: ?Sized + schemars::JsonSchema>(
        path: impl Into<String>,
        source: &'static str,
    ) -> Self {
        let mut value = schema::for_type::<T>();
        schema::annotate(
            &mut value,
            "x-gantry-source",
            Value::String(source.to_owned()),
        );
        Self {
            path: path.into(),
            value,
            openapi: None,
        }
    }

    fn entity<T: ?Sized + schemars::JsonSchema>(
        path: &str,
        openapi: &'static str,
        source: &'static str,
    ) -> Self {
        let mut artifact = Self::typed::<T>(format!("schemas/entities/{path}"), source);
        artifact.openapi = Some(openapi);
        artifact
    }
}

pub fn artifacts() -> Vec<Artifact> {
    let mut all = Vec::new();
    all.extend(entities());
    all.extend(events());
    all.extend(forge());
    all.extend(worker());
    all.extend(config());
    all.extend(egress());
    all.sort_by(|a, b| a.path.cmp(&b.path));
    all
}

fn entities() -> Vec<Artifact> {
    vec![
        Artifact::entity::<Project>("project.json", "Project", "entities::Project"),
        Artifact::entity::<Delivery>("delivery.json", "Delivery", "entities::Delivery"),
        Artifact::entity::<Workflow>("workflow.json", "Workflow", "entities::Workflow"),
        Artifact::entity::<Node>("node.json", "Node", "entities::Node"),
        Artifact::entity::<Attempt>("attempt.json", "Attempt", "entities::Attempt"),
        Artifact::entity::<Worktree>("worktree.json", "Worktree", "entities::Worktree"),
        Artifact::entity::<Message>("message.json", "Message", "entities::Message"),
        Artifact::entity::<Conversation>(
            "conversation.json",
            "Conversation",
            "entities::Conversation",
        ),
        Artifact::entity::<Device>("device.json", "Device", "entities::Device"),
        Artifact::entity::<InboxItem>("inbox-item.json", "InboxItem", "entities::InboxItem"),
        Artifact::entity::<Approval>("approval.json", "Approval", "entities::Approval"),
        Artifact::entity::<BudgetLedger>(
            "budget-ledger.json",
            "BudgetLedger",
            "entities::BudgetLedger",
        ),
        {
            let mut artifact =
                Artifact::entity::<Sandbox>("sandbox.json", "Sandbox", "entities::Sandbox");
            artifact.openapi = None;
            schema::annotate(
                &mut artifact.value,
                "x-gantry-api",
                Value::String("not exposed by client API".to_owned()),
            );
            artifact
        },
        envelope(),
        Artifact::typed::<OutboxAction>(
            "schemas/forge/outbox-action.json",
            "entities::OutboxAction",
        ),
    ]
}

/// 事件信封。`data` 是多态槽位：每类事件的约束在 `schemas/events/<type>.json`，
/// 信封本身不能把 `data` 收窄成某个具体类型。
fn envelope() -> Artifact {
    let mut artifact = Artifact::typed::<Event<Value>>(
        "schemas/entities/event-envelope.json",
        "events::Event<serde_json::Value>",
    );
    artifact.openapi = Some("EventEnvelope");
    schema::annotate(
        &mut artifact.value,
        "x-gantry-data-schema",
        Value::String("schemas/events/<type>.json".to_owned()),
    );
    artifact
}

fn event_artifact(ty: EventType, data: Value) -> Artifact {
    let mut value = data;
    schema::finish(&mut value);
    schema::annotate(
        &mut value,
        "x-gantry-event-type",
        Value::String(ty.as_str().to_owned()),
    );
    schema::annotate(
        &mut value,
        "x-gantry-envelope",
        Value::String("schemas/entities/event-envelope.json".to_owned()),
    );
    Artifact {
        path: format!("schemas/events/{}.json", ty.as_str()),
        value,
        openapi: None,
    }
}

fn events() -> Vec<Artifact> {
    EventType::ALL
        .into_iter()
        .map(|ty| match event_data_schema(ty) {
            Some(value) => event_artifact(ty, value),
            None => event_artifact(ty, rejection(ty)),
        })
        .collect()
}

/// 拒绝型 schema：`not: {}` 对任何实例都为 false。
fn rejection(ty: EventType) -> Value {
    serde_json::json!({
        "title": format!("{} data (contract undefined)", ty),
        "not": {},
        "x-gantry-unresolved": {
            "reason": "core §7.5 / docs/m0-contract-gaps.md 未定义该事件的 data 形状；M0 显式拒绝，不猜测字段",
            "source": "docs/m0-contract-gaps.md",
            "rejects_all_data": true,
        },
    })
}

/// 与 `events::validate_event_data` 的分派保持一致：`None` 表示契约未定义。
fn event_data_schema(ty: EventType) -> Option<Value> {
    use EventType as E;
    Some(match ty {
        E::ProjectAdded | E::ProjectUpdated => schema::for_type::<ProjectEventData>(),
        E::ConversationCreated => schema::for_type::<ConversationEventData>(),
        E::SettingsUpdated => {
            let mut value = schema::for_type::<SettingsUpdatedData>();
            schema::annotate(
                &mut value,
                "x-gantry-validated-by",
                Value::String("events::validate_event_data（value 必填、scope 约束、SecretRef 位置禁止 value）".to_owned()),
            );
            value
        }
        E::MessagePosted => schema::for_type::<Message>(),
        E::DeliveryCreated => schema::for_type::<DeliveryCreatedData>(),
        E::DeliveryStageChanged => schema::for_type::<DeliveryStageChangedData>(),
        E::DeliveryLevelChanged => schema::for_type::<DeliveryLevelChangedData>(),
        E::DeliveryAbandoned => schema::for_type::<DeliveryAbandonedData>(),
        E::DeliveryPaused => schema::for_type::<DeliveryPausedData>(),
        E::DeliveryResumed => schema::for_type::<DeliveryResumedData>(),
        E::WorkflowCreated => schema::for_type::<WorkflowCreatedData>(),
        E::WorkflowRevised => schema::for_type::<WorkflowRevisedData>(),
        E::WorkflowFinished => schema::for_type::<WorkflowFinishedData>(),
        E::NodeQueued | E::NodeStarted | E::NodeFinished | E::NodeSuperseded => {
            schema::for_type::<NodeEventData>()
        }
        E::NodeRetryRequested => schema::for_type::<NodeRetryRequestedData>(),
        E::AttemptStarted => schema::for_type::<AttemptStartedData>(),
        E::AttemptFinished => schema::for_type::<AttemptFinishedData>(),
        E::AttemptProgress => schema::for_type::<AttemptProgressData>(),
        E::GuardianTriaged => schema::for_type::<GuardianTriagedData>(),
        E::GuardianHealRequested => schema::for_type::<GuardianHealRequestedData>(),
        E::GuardianEscalated => schema::for_type::<GuardianEscalatedData>(),
        E::PatchValidated => schema::for_type::<PatchValidatedData>(),
        E::PatchRejected => schema::for_type::<PatchRejectedData>(),
        E::SandboxCreated => schema::for_type::<SandboxCreatedData>(),
        E::SandboxDestroyed => schema::for_type::<SandboxDestroyedData>(),
        E::WorktreeCreated => schema::for_type::<WorktreeCreatedData>(),
        E::WorktreeLocked => schema::for_type::<WorktreeLockedData>(),
        E::WorktreeUnlocked => schema::for_type::<WorktreeUnlockedData>(),
        E::WorktreeRemoved => schema::for_type::<WorktreeRemovedData>(),
        E::InboxRaised => schema::for_type::<InboxRaisedData>(),
        E::InboxResolved => schema::for_type::<InboxResolvedData>(),
        E::InboxDismissed => schema::for_type::<InboxDismissedData>(),
        E::ApprovalRequested => schema::for_type::<ApprovalRequestedData>(),
        E::ApprovalGranted => schema::for_type::<ApprovalGrantedData>(),
        E::ApprovalConsumed => schema::for_type::<ApprovalConsumedData>(),
        E::ApprovalInvalidated => schema::for_type::<ApprovalInvalidatedData>(),
        E::ApprovalExpired => schema::for_type::<ApprovalExpiredData>(),
        E::ApprovalDenied => schema::for_type::<ApprovalDeniedData>(),
        E::OutboxEnqueued => schema::for_type::<OutboxEnqueuedData>(),
        E::OutboxExecuting => schema::for_type::<OutboxExecutingData>(),
        E::OutboxCancelled => schema::for_type::<OutboxCancelledData>(),
        E::OutboxSucceeded => schema::for_type::<OutboxSucceededData>(),
        E::OutboxFailed => schema::for_type::<OutboxFailedData>(),
        E::ForgeSynced => schema::for_type::<ForgeSyncedData>(),
        E::BudgetCharged => schema::for_type::<BudgetChargedData>(),
        E::PolicyDenied => schema::for_type::<PolicyDeniedData>(),
        E::BudgetExhausted => schema::for_type::<BudgetExhaustedData>(),
        E::DevicePaired => schema::for_type::<DevicePairedData>(),
        E::DeviceUpdated => schema::for_type::<DeviceUpdatedData>(),
        E::DeviceRevoked => schema::for_type::<DeviceRevokedData>(),
        E::Forge(kind) => {
            let mut value = forge_event_schema(kind);
            schema::annotate(
                &mut value,
                "x-gantry-projection",
                serde_json::json!({
                    "field": "kind",
                    "const": kind.as_str(),
                    "source": "forge::ForgeEventEnvelope",
                }),
            );
            value
        }
        E::ForgeDriftDetected => schema::for_type::<DriftPayload>(),
    })
}

/// 从完整信封中选出指定 kind 的分支，并把该分支的 `kind`/`data` 约束提到根。
/// 根上的 `origin=self → caused_by_action` 条件继续保留。
fn forge_event_schema(kind: ForgeEventKind) -> Value {
    let mut value = schema::for_type::<ForgeEventEnvelope>();
    let branch = value
        .get("oneOf")
        .and_then(Value::as_array)
        .and_then(|branches| {
            branches.iter().find(|branch| {
                branch
                    .pointer("/properties/kind/const")
                    .and_then(Value::as_str)
                    == Some(kind.as_str())
            })
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "ForgeEventEnvelope has no branch for kind `{}`",
                kind.as_str()
            )
        });
    if let Some(root_properties) = value.get_mut("properties").and_then(Value::as_object_mut) {
        if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
            for (name, schema) in branch_properties {
                root_properties.insert(name.clone(), schema.clone());
            }
        }
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("oneOf");
    }
    value
}

fn forge() -> Vec<Artifact> {
    let mut all = vec![
        payload::<PushBranchPayload>("push_branch", "PushBranchPayload"),
        payload::<CreatePrPayload>("create_pr", "CreatePrPayload"),
        payload::<UpdatePrPayload>("update_pr", "UpdatePrPayload"),
        payload::<UpsertStatusCommentPayload>(
            "upsert_status_comment",
            "UpsertStatusCommentPayload",
        ),
        payload::<CreateCommentPayload>("create_comment", "CreateCommentPayload"),
        payload::<ReplyReviewThreadPayload>("reply_review_thread", "ReplyReviewThreadPayload"),
        payload::<ResolveReviewThreadPayload>(
            "resolve_review_thread",
            "ResolveReviewThreadPayload",
        ),
        payload::<CreateIssuePayload>("create_issue", "CreateIssuePayload"),
        payload::<UpdateIssuePayload>("update_issue", "UpdateIssuePayload"),
        payload::<CloseIssuePayload>("close_issue", "CloseIssuePayload"),
        payload::<AddLabelsPayload>("add_labels", "AddLabelsPayload"),
        payload::<RequestReviewersPayload>("request_reviewers", "RequestReviewersPayload"),
        payload::<MergePrPayload>("merge_pr", "MergePrPayload"),
        payload::<RerunFailedJobsPayload>("rerun_failed_jobs", "RerunFailedJobsPayload"),
    ];
    all.push(Artifact::typed::<OutboxEnqueueEnvelope>(
        "schemas/forge/outbox-enqueue-envelope.json",
        "ports::OutboxEnqueueEnvelope",
    ));
    let mut event_schema = schema::for_type::<ForgeEventEnvelope>();
    schema::annotate(
        &mut event_schema,
        "x-gantry-schema-id",
        Value::String("forge.event.v1".to_owned()),
    );
    all.push(Artifact {
        path: "schemas/forge/forge.event.v1.json".to_owned(),
        value: event_schema,
        openapi: None,
    });
    all.push(Artifact::typed::<DriftPayload>(
        "schemas/forge/drift.json",
        "forge::DriftPayload",
    ));
    all
}

fn payload<T: ?Sized + schemars::JsonSchema>(kind: &str, source: &'static str) -> Artifact {
    let mut artifact = Artifact::typed::<T>(format!("schemas/forge/{kind}.json"), source);
    schema::annotate(
        &mut artifact.value,
        "x-gantry-action-kind",
        Value::String(kind.to_owned()),
    );
    artifact
}

/// 每个 role 的输出 schema 从 `StructuredOutput` 的 tagged enum 中选分支。
fn output_artifact(kind: &str) -> Artifact {
    let union = schema::for_type::<StructuredOutput>();
    let definitions = union.get("$defs").cloned();
    let mut value = schema::narrow_tagged_variant(&union, "kind", kind)
        .unwrap_or_else(|| panic!("StructuredOutput has no variant `{kind}`"));
    if let (Some(definitions), Some(object)) = (definitions, value.as_object_mut()) {
        object.insert("$defs".to_owned(), definitions);
    }
    schema::finish(&mut value);
    schema::annotate(
        &mut value,
        "x-gantry-source",
        Value::String("worker::StructuredOutput".to_owned()),
    );
    schema::annotate(
        &mut value,
        "x-gantry-projection",
        serde_json::json!({
            "from": "worker::StructuredOutput",
            "tag": "kind",
            "const": kind,
        }),
    );
    Artifact {
        path: format!("schemas/worker/output.{kind}.schema.json"),
        value,
        openapi: None,
    }
}

fn worker() -> Vec<Artifact> {
    let mut all = vec![
        Artifact::typed::<JobSpec>("schemas/worker/job.schema.json", "worker::JobSpec"),
        Artifact::typed::<ResultJson>("schemas/worker/result.schema.json", "worker::ResultJson"),
        Artifact::typed::<ResultBundle>(
            "schemas/worker/result-bundle.schema.json",
            "worker::ResultBundle",
        ),
        Artifact::typed::<Proposals>("schemas/worker/proposals.schema.json", "worker::Proposals"),
        Artifact::typed::<ProgressEvent>(
            "schemas/worker/progress-event.schema.json",
            "worker::ProgressEvent",
        ),
    ];
    for kind in [
        "plan",
        "build",
        "verify",
        "review",
        "reasoner",
        "arbiter",
        "answer",
        "admission",
        "summarize",
        "intent",
        "classify",
    ] {
        all.push(output_artifact(kind));
    }
    all
}

fn config() -> Vec<Artifact> {
    [
        (SettingsResource::Models, "models"),
        (SettingsResource::Levels, "levels"),
        (SettingsResource::Policies, "policies"),
        (SettingsResource::Guardian, "guardian"),
        (SettingsResource::Sandbox, "sandbox"),
        (SettingsResource::Egress, "egress"),
        (SettingsResource::Notify, "notify"),
        (SettingsResource::Github, "github"),
    ]
    .into_iter()
    .map(|(resource, name)| {
        let mut value = settings::resource_schema(resource).to_value();
        schema::finish(&mut value);
        schema::annotate(
            &mut value,
            "x-gantry-resource",
            Value::String(name.to_owned()),
        );
        Artifact {
            path: format!("schemas/config/{name}.json"),
            value,
            openapi: None,
        }
    })
    .collect()
}

fn egress() -> Vec<Artifact> {
    let all = vec![
        Artifact::typed::<IssueJobTokenRequest>(
            "schemas/egress/issue-job-token-request.json",
            "egress::IssueJobTokenRequest",
        ),
        Artifact::typed::<IssueJobTokenResponse>(
            "schemas/egress/issue-job-token-response.json",
            "egress::IssueJobTokenResponse",
        ),
        Artifact::typed::<FinalUsage>("schemas/egress/final-usage.json", "egress::FinalUsage"),
        Artifact::typed::<IssueSvcTokenRequest>(
            "schemas/egress/issue-svc-token-request.json",
            "egress::IssueSvcTokenRequest",
        ),
        Artifact::typed::<IssueSvcTokenResponse>(
            "schemas/egress/issue-svc-token-response.json",
            "egress::IssueSvcTokenResponse",
        ),
        Artifact::typed::<RevokeSvcTokenRequest>(
            "schemas/egress/revoke-svc-token-request.json",
            "egress::RevokeSvcTokenRequest",
        ),
        Artifact::typed::<RevokeSvcTokenResponse>(
            "schemas/egress/revoke-svc-token-response.json",
            "egress::RevokeSvcTokenResponse",
        ),
        Artifact::typed::<CloseCallResponse>(
            "schemas/egress/close-call-response.json",
            "egress::CloseCallResponse",
        ),
        Artifact::typed::<StageSecretRequest>(
            "schemas/egress/stage-secret-request.json",
            "egress::StageSecretRequest",
        ),
        Artifact::typed::<StageSecretResponse>(
            "schemas/egress/stage-secret-response.json",
            "egress::StageSecretResponse",
        ),
        Artifact::typed::<ActivateSecretResponse>(
            "schemas/egress/activate-secret-response.json",
            "egress::ActivateSecretResponse",
        ),
        Artifact::typed::<StagedSecretList>(
            "schemas/egress/staged-secret-list.json",
            "egress::StagedSecretList",
        ),
        Artifact::typed::<PresentRequest>(
            "schemas/egress/present-request.json",
            "egress::PresentRequest",
        ),
        Artifact::typed::<PresentResponse>(
            "schemas/egress/present-response.json",
            "egress::PresentResponse",
        ),
        Artifact::typed::<UsagePage>("schemas/egress/usage-page.json", "egress::UsagePage"),
        Artifact::typed::<ConfigApplyResponse>(
            "schemas/egress/config-apply-response.json",
            "egress::ConfigApplyResponse",
        ),
        Artifact::typed::<StatusResponse>("schemas/egress/status.json", "egress::StatusResponse"),
        Artifact::typed::<EndpointsResponse>(
            "schemas/egress/endpoints.json",
            "egress::EndpointsResponse",
        ),
        Artifact::typed::<ProblemDetails>("schemas/egress/problem.json", "egress::ProblemDetails"),
    ];
    let mut all = all;
    let mut config_view = schema::for_type::<ConfigView>();
    schema::annotate(
        &mut config_view,
        "x-gantry-injection",
        Value::String(
            "egress-control §4.6 只给出 `injection: {}` 示例；M0 只接受空对象".to_owned(),
        ),
    );
    all.push(Artifact {
        path: "schemas/egress/config-view.json".to_owned(),
        value: config_view,
        openapi: None,
    });
    all.sort_by(|a, b| a.path.cmp(&b.path));
    all
}
