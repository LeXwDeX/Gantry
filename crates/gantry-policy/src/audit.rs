//! `policy.denied` 的构造与去重键（policy.md §7.3 第 8 步、§7.4；event-data §2.2）。
//!
//! 事件 `data` 只含结构化字段与截断后的不可信路径，不含补丁内容或密钥明文。
//! `dedup_key` 覆盖判定的全部输入与 `config_rev`：同一判定反复调用只留一条，
//! 规则变化后（新 `config_rev`）再拒绝会留下新记录。

use gantry_core::canon::{sha256, RepoSlug, Sha256Digest};
use gantry_core::entities::{ForgeActionKind, PolicyActionId, Seq};
use gantry_core::events::{
    EventType, NewEvent, PolicyDeniedData, PolicyDeniedReason, PolicyOrigin, PolicyProposalKind,
    PolicyReasonCode, PolicySubject, PolicyTarget, Subject,
};
use gantry_core::ids::{EventId, ProjectId};
use gantry_core::ports::{DenyReason, PatchPolicyVerdict};
use gantry_core::text::UntrustedText;
use gantry_core::worker::{PatchFacts, ProposalKind};

use crate::approvals::policy_actor;

/// 写入事件的补丁路径上限（policy.md §7.4：建议 512 字节）。
pub const MAX_EVENT_PATH_BYTES: usize = 512;
const PATCH_PATH_SOURCE: &str = "sandbox:patch_path";

pub(crate) fn reason_code(reason: &DenyReason) -> PolicyReasonCode {
    match reason {
        DenyReason::ActionDenied => PolicyReasonCode::ActionDenied,
        DenyReason::ApprovalDenied => PolicyReasonCode::ApprovalDenied,
        DenyReason::ApprovalConsumed => PolicyReasonCode::ApprovalConsumed,
        DenyReason::HeadMismatch => PolicyReasonCode::HeadMismatch,
        DenyReason::NoValidatedPatch => PolicyReasonCode::NoValidatedPatch,
        DenyReason::PatchDenied => PolicyReasonCode::PatchDenied,
        DenyReason::MultipleApprovalsRequired => PolicyReasonCode::MultipleApprovalsRequired,
        DenyReason::ApprovalShapeUndefined => PolicyReasonCode::ApprovalShapeUndefined,
        DenyReason::ApprovalChainUnavailable => PolicyReasonCode::ApprovalChainUnavailable,
        // DenyReason 是 non_exhaustive；新增的原因在补上映射前按链路不可用记账。
        _ => PolicyReasonCode::ApprovalChainUnavailable,
    }
}

/// 动作判定的拒绝记录。`target` 为 `None` 时不记事件（见 `PolicyEngine::decide_action`）。
pub(crate) struct ActionDenial {
    pub project_id: ProjectId,
    pub subject: Subject,
    pub action_id: PolicyActionId,
    pub kind: ForgeActionKind,
    pub target: PolicyTarget,
    pub reason: PolicyReasonCode,
    pub subject_digest: Option<PolicySubject>,
    pub config_rev: Seq,
}

impl ActionDenial {
    pub fn event(self, causation_id: Option<EventId>) -> NewEvent {
        let data = PolicyDeniedData {
            origin: PolicyOrigin::Action,
            action_id: Some(self.action_id),
            kind: Some(self.kind),
            target: Some(self.target),
            reason_code: Some(self.reason),
            reasons: None,
            proposal_id: None,
            proposal_kind: None,
            target_handle: None,
            subject: self.subject_digest,
            config_rev: self.config_rev.as_u64(),
        };
        build(self.project_id, self.subject, data, causation_id)
    }
}

pub(crate) fn patch_event(
    facts: &PatchFacts,
    repo: RepoSlug,
    verdict: &PatchPolicyVerdict,
    causation_id: Option<EventId>,
) -> NewEvent {
    let mut paths: Vec<&str> = verdict
        .reasons
        .iter()
        .filter_map(|reason| reason.path.as_ref().map(|path| path.as_str()))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    let mut rules: Vec<String> = verdict
        .reasons
        .iter()
        .filter_map(|reason| reason.rule_id.clone())
        .collect();
    rules.sort();
    rules.dedup();
    let data = PolicyDeniedData {
        origin: PolicyOrigin::Patch,
        action_id: None,
        kind: None,
        target: Some(PolicyTarget::Patch {
            repo,
            paths: paths.iter().map(|path| untrusted_path(path)).collect(),
            path_digest: digest_lines(paths.iter().copied()),
            rules,
        }),
        reason_code: None,
        reasons: Some(
            verdict
                .reasons
                .iter()
                .map(|reason| PolicyDeniedReason {
                    code: reason.code.as_str().to_owned(),
                    path: reason
                        .path
                        .as_ref()
                        .map(|path| untrusted_path(path.as_str())),
                    rule_id: reason.rule_id.clone(),
                })
                .collect(),
        ),
        proposal_id: None,
        proposal_kind: None,
        target_handle: None,
        subject: Some(PolicySubject::Patch(facts.patch_sha256)),
        config_rev: verdict.config_rev.as_u64(),
    };
    let subject =
        Subject::parse(&facts.attempt_id.to_string()).expect("attempt ids are valid subjects");
    build(facts.project_id, subject, data, causation_id)
}

pub(crate) struct ProposalDenial<'a> {
    pub project_id: ProjectId,
    pub subject: Subject,
    pub proposal_id: &'a str,
    pub kind: ProposalKind,
    pub target_handle: Option<&'a str>,
    pub reason: PolicyReasonCode,
    pub config_rev: Seq,
}

impl ProposalDenial<'_> {
    pub fn event(self, causation_id: Option<EventId>) -> NewEvent {
        let data = PolicyDeniedData {
            origin: PolicyOrigin::Proposal,
            action_id: None,
            kind: None,
            target: None,
            reason_code: Some(self.reason),
            reasons: None,
            proposal_id: Some(self.proposal_id.to_owned()),
            proposal_kind: Some(match self.kind {
                ProposalKind::OpenPr => PolicyProposalKind::OpenPr,
                ProposalKind::ReplyReviewThread => PolicyProposalKind::ReplyReviewThread,
                ProposalKind::ResolveReviewThread => PolicyProposalKind::ResolveReviewThread,
                ProposalKind::Escalate => PolicyProposalKind::Escalate,
            }),
            target_handle: self.target_handle.map(str::to_owned),
            subject: None,
            config_rev: self.config_rev.as_u64(),
        };
        build(self.project_id, self.subject, data, causation_id)
    }
}

fn build(
    project_id: ProjectId,
    subject: Subject,
    data: PolicyDeniedData,
    causation_id: Option<EventId>,
) -> NewEvent {
    let mut data = serde_json::to_value(&data).expect("policy.denied data serializes");
    // 契约要求可选字段缺省时省略，不写 null。
    strip_nulls(&mut data);
    // 去重键 = 事件主体 + 完整 data 的摘要；data 已含 config_rev 与全部判定输入。
    let key_input = format!("{project_id}\n{}\n{data}", subject.as_str());
    NewEvent {
        ty: EventType::PolicyDenied,
        subject,
        project_id: Some(project_id),
        actor: policy_actor(),
        causation_id,
        correlation_id: None,
        data,
        dedup_key: Some(format!(
            "policy.denied:{}",
            sha256(key_input.as_bytes()).to_hex()
        )),
    }
}

fn strip_nulls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, item| !item.is_null());
            map.values_mut().for_each(strip_nulls);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// 不可信路径按字节截断到字符边界。
fn untrusted_path(path: &str) -> UntrustedText {
    let mut end = path.len().min(MAX_EVENT_PATH_BYTES);
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    UntrustedText::new(
        path[..end].to_owned(),
        PATCH_PATH_SOURCE.to_owned(),
        end < path.len(),
    )
}

fn digest_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Sha256Digest {
    let mut joined = String::new();
    for line in lines {
        joined.push_str(line);
        joined.push('\n');
    }
    sha256(joined.as_bytes())
}
