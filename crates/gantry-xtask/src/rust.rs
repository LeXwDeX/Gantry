//! 夹具注解的 Rust 校验注册表。
//!
//! 注解里的 `rust` 是封闭的字符串；未知名称直接失败，避免拼错后静默跳过。
//! `event:<type>` 走 `events::validate_event_data`（含 SecretRef 与 kind 一致性检查）。

use gantry_core::egress::{
    ActivateSecretResponse, ConfigApplyResponse, ConfigView, EndpointsResponse, FinalUsage,
    IssueJobTokenRequest, IssueJobTokenResponse, IssueSvcTokenRequest, IssueSvcTokenResponse,
    PresentRequest, PresentResponse, ProblemDetails, RevokeSvcTokenResponse, StageSecretResponse,
    StagedSecretList, StatusResponse, UsagePage,
};
use gantry_core::entities::{
    Approval, Attempt, BudgetLedger, Conversation, Delivery, Device, InboxItem, Message, Node,
    OutboxAction, Project, Workflow, Worktree,
};
use gantry_core::events::{validate_event_data, EventType};
use gantry_core::forge::{
    AddLabelsPayload, CloseIssuePayload, CreateCommentPayload, CreateIssuePayload, CreatePrPayload,
    ForgeEventEnvelope, MergePrPayload, PushBranchPayload, ReplyReviewThreadPayload,
    RequestReviewersPayload, RerunFailedJobsPayload, ResolveReviewThreadPayload,
    UpdateIssuePayload, UpdatePrPayload, UpsertStatusCommentPayload,
};
use gantry_core::ports::OutboxEnqueueEnvelope;
use gantry_core::settings::{
    EgressPatch, GithubPatch, GuardianPatch, LevelsPatch, ModelsPatch, NotifyPatch, PoliciesPatch,
    SandboxPatch,
};
use gantry_core::worker::{JobSpec, ProgressEvent, Proposals, ResultJson, StructuredOutput};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn check_event_data(ty: &str, data: &Value) -> Result<(), String> {
    let ty: EventType = ty
        .parse()
        .map_err(|_| format!("unknown event type `{ty}`"))?;
    validate_event_data(&ty, data).map_err(|error| error.to_string())
}

pub fn check_annotation(kind: &str, data: &Value) -> Result<(), String> {
    if let Some(ty) = kind.strip_prefix("event:") {
        return check_event_data(ty, data);
    }
    match kind {
        "none" => Ok(()),
        "worker::JobSpec" => roundtrip::<JobSpec>(data),
        "worker::ResultJson" => roundtrip::<ResultJson>(data),
        "worker::Proposals" => roundtrip::<Proposals>(data),
        "worker::StructuredOutput" => roundtrip::<StructuredOutput>(data),
        "worker::ProgressEvent" => roundtrip::<ProgressEvent>(data),
        "forge::PushBranchPayload" => roundtrip::<PushBranchPayload>(data),
        "forge::CreatePrPayload" => roundtrip::<CreatePrPayload>(data),
        "forge::UpdatePrPayload" => roundtrip::<UpdatePrPayload>(data),
        "forge::UpsertStatusCommentPayload" => roundtrip::<UpsertStatusCommentPayload>(data),
        "forge::CreateCommentPayload" => roundtrip::<CreateCommentPayload>(data),
        "forge::ReplyReviewThreadPayload" => roundtrip::<ReplyReviewThreadPayload>(data),
        "forge::ResolveReviewThreadPayload" => roundtrip::<ResolveReviewThreadPayload>(data),
        "forge::CreateIssuePayload" => roundtrip::<CreateIssuePayload>(data),
        "forge::UpdateIssuePayload" => roundtrip::<UpdateIssuePayload>(data),
        "forge::CloseIssuePayload" => roundtrip::<CloseIssuePayload>(data),
        "forge::AddLabelsPayload" => roundtrip::<AddLabelsPayload>(data),
        "forge::RequestReviewersPayload" => roundtrip::<RequestReviewersPayload>(data),
        "forge::MergePrPayload" => roundtrip::<MergePrPayload>(data),
        "forge::RerunFailedJobsPayload" => roundtrip::<RerunFailedJobsPayload>(data),
        "forge::ForgeEventEnvelope" => checked_roundtrip::<ForgeEventEnvelope>(data, |event| {
            event.validate().map_err(|error| error.to_string())
        }),
        "forge::OutboxEnqueueEnvelope" => roundtrip::<OutboxEnqueueEnvelope>(data),
        "config::AggregateDocument" => deserialize_only::<AggregateDocument>(data),
        "config::ProjectOverride" => deserialize_only::<ProjectOverride>(data),
        "egress::IssueJobTokenRequest" => roundtrip::<IssueJobTokenRequest>(data),
        "egress::IssueJobTokenResponse" => roundtrip::<IssueJobTokenResponse>(data),
        "egress::FinalUsage" => roundtrip::<FinalUsage>(data),
        "egress::IssueSvcTokenRequest" => roundtrip::<IssueSvcTokenRequest>(data),
        "egress::IssueSvcTokenResponse" => roundtrip::<IssueSvcTokenResponse>(data),
        "egress::RevokeSvcTokenResponse" => roundtrip::<RevokeSvcTokenResponse>(data),
        "egress::StageSecretResponse" => roundtrip::<StageSecretResponse>(data),
        "egress::ActivateSecretResponse" => roundtrip::<ActivateSecretResponse>(data),
        "egress::StagedSecretList" => roundtrip::<StagedSecretList>(data),
        "egress::PresentRequest" => roundtrip::<PresentRequest>(data),
        "egress::PresentResponse" => roundtrip::<PresentResponse>(data),
        "egress::UsagePage" => roundtrip::<UsagePage>(data),
        "egress::ConfigApplyResponse" => roundtrip::<ConfigApplyResponse>(data),
        "egress::ConfigView" => roundtrip::<ConfigView>(data),
        "egress::StatusResponse" => roundtrip::<StatusResponse>(data),
        "egress::EndpointsResponse" => roundtrip::<EndpointsResponse>(data),
        "egress::ProblemDetails" => roundtrip::<ProblemDetails>(data),
        "entities::Project" => roundtrip::<Project>(data),
        "entities::Delivery" => roundtrip::<Delivery>(data),
        "entities::Workflow" => roundtrip::<Workflow>(data),
        "entities::Node" => roundtrip::<Node>(data),
        "entities::Attempt" => roundtrip::<Attempt>(data),
        "entities::Worktree" => roundtrip::<Worktree>(data),
        "entities::Message" => roundtrip::<Message>(data),
        "entities::Conversation" => roundtrip::<Conversation>(data),
        "entities::Device" => roundtrip::<Device>(data),
        "entities::InboxItem" => roundtrip::<InboxItem>(data),
        "entities::Approval" => checked_roundtrip::<Approval>(data, |approval| {
            approval.validate().map_err(|error| error.to_string())
        }),
        "entities::BudgetLedger" => roundtrip::<BudgetLedger>(data),
        "entities::OutboxAction" => roundtrip::<OutboxAction>(data),
        other => Err(format!("unknown rust annotation `{other}`")),
    }
}

fn roundtrip<T>(data: &Value) -> Result<(), String>
where
    T: DeserializeOwned + Serialize,
{
    checked_roundtrip::<T>(data, |_| Ok(()))
}

fn checked_roundtrip<T>(
    data: &Value,
    check: impl FnOnce(&T) -> Result<(), String>,
) -> Result<(), String>
where
    T: DeserializeOwned + Serialize,
{
    let typed: T =
        serde_json::from_value(data.clone()).map_err(|error| format!("反序列化失败: {error}"))?;
    check(&typed)?;
    let back = serde_json::to_value(&typed).map_err(|error| format!("重新序列化失败: {error}"))?;
    if &back != data {
        return Err(format!(
            "语义不相等：输入 {}，Rust 往返后 {}",
            compact(data),
            compact(&back)
        ));
    }
    Ok(())
}

/// config.md §12 的聚合文档：逐个 resource 用生产 `*Patch` 类型解码。
///
/// `*Patch` 刻意只实现 `Deserialize`（写入形态含 `SecretWrite`，不序列化凭据），
/// 因此这里只验证反序列化；字段形状由同一批类型的 `JsonSchema` 另行校验。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // 字段只用于逐 resource 反序列化校验，不在运行期读取。
struct AggregateDocument {
    models: ModelsPatch,
    levels: LevelsPatch,
    policies: PoliciesPatch,
    guardian: GuardianPatch,
    sandbox: SandboxPatch,
    egress: EgressPatch,
    notify: NotifyPatch,
    github: GithubPatch,
}

/// config.md §12.2 的项目覆盖文档；`notify` / `github` 不可覆盖。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // 字段只用于逐 resource 反序列化校验，不在运行期读取。
struct ProjectOverride {
    models: ModelsPatch,
    levels: LevelsPatch,
    policies: PoliciesPatch,
    guardian: GuardianPatch,
    sandbox: SandboxPatch,
    egress: EgressPatch,
}

fn deserialize_only<T: DeserializeOwned>(data: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(data.clone())
        .map(|_| ())
        .map_err(|error| format!("反序列化失败: {error}"))
}

fn compact(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    if text.chars().count() > 400 {
        let mut out: String = text.chars().take(400).collect();
        out.push('…');
        out
    } else {
        text
    }
}
