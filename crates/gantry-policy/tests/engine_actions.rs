//! policy.md §12.2「动作判定」「消费」「状态机与失效」的 M1 条目，以及与
//! `FakePolicy` 共享的一致性用例。

mod support;

use std::time::Duration;

use gantry_core::canon::SubjectDigest;
use gantry_core::entities::{
    ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind, InboxKind, PolicyActionId, Seq,
};
use gantry_core::events::EventType;
use gantry_core::ids::{Act, Att, Dlv};
use gantry_core::ports::{
    ActionDecision, ActionFacts, ActionRequest, DenyReason, ExecutionCheck, ExecutionPermit,
    GateError, InvalidateReason, MismatchReason, PolicyError, PolicyPort,
};
use gantry_core::settings::{
    builtin_resolved, Disposition as SettingsDisposition, Policies, ResolvedSettings,
    SettingsResource,
};
use gantry_fakes::policy::{action_conformance, execution_conformance, FakePolicy};
use gantry_policy::{PolicyBuild, PolicyRules};
use support::*;

fn request(kind: ForgeActionKind, facts: ActionFacts) -> ActionRequest {
    ActionRequest {
        project_id: project_id(),
        delivery_id: Some(id::<Dlv>(3)),
        kind,
        facts,
        requested_by: ctx().actor,
        epoch: 1,
    }
}

fn merge(head: &str) -> ActionRequest {
    request(
        ForgeActionKind::MergePr,
        ActionFacts::MergePr {
            repo: slug(),
            number: 7,
            expected_head_sha: oid(head),
        },
    )
}

fn push(branch: &str, force: bool) -> ActionRequest {
    request(
        ForgeActionKind::PushBranch,
        ActionFacts::PushBranch {
            repo: slug(),
            branch: branch.into(),
            branch_prefix: "gantry/".into(),
            force,
            local_head_oid: oid("local"),
            expected_remote_oid: Some(oid("remote")),
            expected_tree_oid: Some(oid("tree")),
            validation_ref: Some(sha("patch")),
        },
    )
}

fn merge_check(expected: &str, live: &str) -> ExecutionCheck {
    ExecutionCheck::MergePr {
        repo: slug(),
        number: 7,
        expected_head_sha: oid(expected),
        live_head_sha: oid(live),
    }
}

fn policies_with(id: PolicyActionId, disposition: SettingsDisposition) -> PolicyRules {
    let mut policies: Policies = match builtin_resolved(SettingsResource::Policies) {
        ResolvedSettings::Policies(policies) => policies,
        other => panic!("unexpected resource {:?}", other.resource()),
    };
    policies.actions.insert(id, disposition);
    PolicyRules::load(&policies, Seq::new(5).expect("seq")).expect("valid rules")
}

async fn needs_approval(h: &Harness, req: ActionRequest) -> gantry_core::entities::Approval {
    let ActionDecision::NeedsApproval { spec, pending, .. } =
        h.engine.decide_action(req, &ctx()).await.expect("decide")
    else {
        panic!("expected NeedsApproval");
    };
    assert_eq!(pending, None);
    h.engine
        .request_approval(spec, ctx())
        .await
        .expect("request approval")
}

fn deny_reason(decision: &ActionDecision) -> &DenyReason {
    match decision {
        ActionDecision::Deny { reason, .. } => reason,
        other => panic!("expected Deny, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// merge_pr
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_pr_walks_the_approval_lifecycle() {
    let h = Harness::new();

    let ActionDecision::NeedsApproval {
        spec,
        pending,
        config_rev,
    } = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide")
    else {
        panic!("merge_pr needs approval");
    };
    assert_eq!(pending, None);
    assert_eq!(config_rev, Seq::ZERO);
    assert_eq!(spec.action, ApprovalAction::Forge(ForgeActionKind::MergePr));
    assert_eq!(
        spec.target,
        ApprovalTarget::Pr {
            repo: slug(),
            number: 7
        }
    );
    assert_eq!(spec.subject_digest, SubjectDigest::git(oid("head")));
    assert_eq!(spec.ttl, Duration::from_secs(86_400));

    let approval = h
        .engine
        .request_approval(spec.clone(), ctx())
        .await
        .expect("request");
    assert_eq!(approval.status, ApprovalStatus::Pending);
    // 同主体的 pending 复用，不新建。
    let again = h.engine.request_approval(spec, ctx()).await.expect("again");
    assert_eq!(again.id, approval.id);
    assert_eq!(h.events(EventType::ApprovalRequested).await.len(), 1);

    let decision = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide");
    assert!(matches!(decision,
        ActionDecision::NeedsApproval { pending: Some(id), .. } if id == approval.id));

    h.grant(&approval).await;
    let decision = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide");
    assert_eq!(
        decision,
        ActionDecision::Allow {
            action_id: PolicyActionId::MergePr,
            approval: Some(approval.id),
            config_rev: Seq::ZERO,
        }
    );
}

#[tokio::test]
async fn merge_pr_head_mismatch_is_denied_and_audited() {
    let h = Harness::new();
    let decision = h
        .engine
        .decide_action(merge("stale"), &ctx())
        .await
        .expect("decide");
    assert_eq!(deny_reason(&decision), &DenyReason::HeadMismatch);
    let denied = h.events(EventType::PolicyDenied).await;
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].data["origin"], "action");
    assert_eq!(denied[0].data["reason_code"], "policy.head_mismatch");
    assert_eq!(denied[0].data["target"]["kind"], "pr");

    // 没有 PR 快照时同样无法核对 head。
    let h = Harness::new();
    let mut req = merge("head");
    req.facts = ActionFacts::MergePr {
        repo: slug(),
        number: 99,
        expected_head_sha: oid("head"),
    };
    let decision = h.engine.decide_action(req, &ctx()).await.expect("decide");
    assert_eq!(deny_reason(&decision), &DenyReason::HeadMismatch);
}

#[tokio::test]
async fn merge_pr_denied_and_consumed_approvals_deny() {
    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    h.deny(&approval).await;
    let decision = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide");
    assert_eq!(deny_reason(&decision), &DenyReason::ApprovalDenied);

    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    h.grant(&approval).await;
    let act = id::<Act>(1);
    h.engine
        .consume_for_execution(&approval.id, &merge_check("head", "head"), &act, &ctx())
        .await
        .expect("consume");
    // 执行终态失败不退回审批（P-54）：同主体再判定不会放行。
    let decision = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide");
    assert_eq!(deny_reason(&decision), &DenyReason::ApprovalConsumed);
}

#[tokio::test]
async fn merge_pr_configured_deny_reports_merge_denied_by_policy() {
    let h = Harness::with_rules(
        PolicyBuild::M1,
        policies_with(PolicyActionId::MergePr, SettingsDisposition::Deny),
    );
    let decision = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide");
    let ActionDecision::Deny {
        reason,
        report,
        config_rev,
        ..
    } = decision
    else {
        panic!("expected deny");
    };
    assert_eq!(reason, DenyReason::ActionDenied);
    assert_eq!(config_rev, Seq::new(5).expect("seq"));
    let report = report.expect("merge deny carries a report");
    assert_eq!(report.inbox_kind, InboxKind::Escalation);
    assert_eq!(report.reason_code.as_str(), "merge_denied_by_policy");
}

// ---------------------------------------------------------------------------
// push_branch 条件伪动作与 I8 前置
// ---------------------------------------------------------------------------

#[tokio::test]
async fn push_branch_pseudo_actions() {
    let h = Harness::new();
    h.validated_patch(sha("patch"), oid("tree")).await;

    let decision = h
        .engine
        .decide_action(push("gantry/x", false), &ctx())
        .await
        .expect("decide");
    assert_eq!(
        decision,
        ActionDecision::Allow {
            action_id: PolicyActionId::PushBranch,
            approval: None,
            config_rev: Seq::ZERO,
        }
    );

    for (branch, force, action) in [
        ("gantry/x", true, ApprovalAction::ForcePush),
        ("feature/x", false, ApprovalAction::PushOutsidePrefix),
        ("feature/x", true, ApprovalAction::ForcePush),
    ] {
        let decision = h
            .engine
            .decide_action(push(branch, force), &ctx())
            .await
            .expect("decide");
        let ActionDecision::NeedsApproval { spec, .. } = decision else {
            panic!("{branch} force={force} needs approval");
        };
        assert_eq!(spec.action, action, "{branch} force={force}");
        assert_eq!(
            spec.target,
            ApprovalTarget::Branch {
                repo: slug(),
                branch: branch.into(),
                expected_remote_oid: Some(oid("remote")),
            }
        );
        assert_eq!(spec.subject_digest, SubjectDigest::git(oid("local")));
    }
}

#[tokio::test]
async fn push_branch_without_matching_validated_patch_is_denied() {
    let h = Harness::new();
    let decision = h
        .engine
        .decide_action(push("gantry/x", false), &ctx())
        .await
        .expect("decide");
    assert_eq!(deny_reason(&decision), &DenyReason::NoValidatedPatch);

    // validation_ref 对得上、tree 对不上，同样拒绝。
    h.validated_patch(sha("patch"), oid("other-tree")).await;
    let decision = h
        .engine
        .decide_action(push("gantry/x", false), &ctx())
        .await
        .expect("decide");
    assert_eq!(deny_reason(&decision), &DenyReason::NoValidatedPatch);

    let mut req = push("gantry/x", false);
    if let ActionFacts::PushBranch { validation_ref, .. } = &mut req.facts {
        *validation_ref = None;
    }
    let decision = h.engine.decide_action(req, &ctx()).await.expect("decide");
    assert_eq!(deny_reason(&decision), &DenyReason::NoValidatedPatch);
}

// ---------------------------------------------------------------------------
// 失败即关闭与审计
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approve_without_defined_shape_is_denied_with_report() {
    let h = Harness::with_rules(
        PolicyBuild::M1,
        policies_with(PolicyActionId::CreatePr, SettingsDisposition::Approve),
    );
    let req = request(
        ForgeActionKind::CreatePr,
        ActionFacts::Other {
            target: gantry_core::events::PolicyTarget::PrCreation {
                repo: slug(),
                head: "gantry/x".into(),
                base: "main".into(),
            },
        },
    );
    let decision = h.engine.decide_action(req, &ctx()).await.expect("decide");
    let ActionDecision::Deny { reason, report, .. } = decision else {
        panic!("expected deny");
    };
    assert_eq!(reason, DenyReason::ApprovalShapeUndefined);
    assert_eq!(
        report.expect("report").reason_code.as_str(),
        "approval_shape_undefined"
    );
    assert!(h.events(EventType::ApprovalRequested).await.is_empty());
}

#[tokio::test]
async fn resolve_thread_with_human_comment_is_denied_by_default() {
    let h = Harness::new();
    let req = request(
        ForgeActionKind::ResolveReviewThread,
        ActionFacts::ResolveReviewThread {
            repo: slug(),
            number: 7,
            thread_node_id: "PRRT_x".into(),
            thread_has_human_comment: true,
        },
    );
    let decision = h.engine.decide_action(req, &ctx()).await.expect("decide");
    let ActionDecision::Deny {
        action_id, reason, ..
    } = decision
    else {
        panic!("expected deny");
    };
    assert_eq!(action_id, PolicyActionId::ResolveReviewThreadHuman);
    assert_eq!(reason, DenyReason::ActionDenied);
}

#[tokio::test]
async fn repeated_denial_is_recorded_once() {
    let h = Harness::new();
    for _ in 0..100 {
        let decision = h
            .engine
            .decide_action(merge("stale"), &ctx())
            .await
            .expect("decide");
        assert_eq!(deny_reason(&decision), &DenyReason::HeadMismatch);
    }
    assert_eq!(h.events(EventType::PolicyDenied).await.len(), 1);
}

/// 读 PR 快照失败（审批链路不可用）时 approve 按 deny 处理，绝不放行。
#[tokio::test]
async fn approval_chain_failure_fails_closed() {
    let h = Harness::with_failing_forge();
    let decision = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide");
    assert_eq!(
        deny_reason(&decision),
        &DenyReason::ApprovalChainUnavailable
    );
}

/// #66 已定"engine 发起的 Deny 必带 ReportHint"，但其余 DenyReason 在上报词表里
/// 还没有对应码（policy.md OQ-P15），拍板前没有合法取值。
#[tokio::test]
#[ignore = "待 policy.md OQ-P15 拍板：其余 DenyReason 的上报码"]
async fn every_engine_deny_carries_a_report_hint() {
    let h = Harness::new();
    let decision = h
        .engine
        .decide_action(merge("stale"), &ctx())
        .await
        .expect("decide");
    let ActionDecision::Deny { report, .. } = decision else {
        panic!("expected deny");
    };
    assert!(report.is_some());
}

// ---------------------------------------------------------------------------
// 消费
// ---------------------------------------------------------------------------

#[tokio::test]
async fn consume_for_execution_rejects_any_head_disagreement_without_transition() {
    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    h.grant(&approval).await;
    let act = id::<Act>(1);

    let live_moved = h
        .engine
        .consume_for_execution(&approval.id, &merge_check("head", "moved"), &act, &ctx())
        .await;
    assert_eq!(
        live_moved,
        Err(GateError::HeadChanged {
            live_head_sha: oid("moved")
        })
    );
    let expected_moved = h
        .engine
        .consume_for_execution(&approval.id, &merge_check("moved", "moved"), &act, &ctx())
        .await;
    assert_eq!(
        expected_moved,
        Err(GateError::ApprovalInvalid {
            reason: MismatchReason::DigestMismatch
        })
    );
    let status = h
        .engine
        .approval(&approval.id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(status.status, ApprovalStatus::Granted);
    assert!(h.events(EventType::ApprovalConsumed).await.is_empty());
}

#[tokio::test]
async fn consume_for_execution_is_once_per_action() {
    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    h.grant(&approval).await;
    let act = id::<Act>(1);
    let check = merge_check("head", "head");

    assert_eq!(
        h.engine
            .consume_for_execution(&approval.id, &check, &act, &ctx())
            .await,
        Ok(ExecutionPermit::Consumed)
    );
    assert_eq!(
        h.engine
            .consume_for_execution(&approval.id, &check, &act, &ctx())
            .await,
        Ok(ExecutionPermit::AlreadyConsumedBySameAction)
    );
    let other = id::<Act>(2);
    assert_eq!(
        h.engine
            .consume_for_execution(&approval.id, &check, &other, &ctx())
            .await,
        Err(GateError::ConsumedByOther { by: act })
    );
    let consumed = h.events(EventType::ApprovalConsumed).await;
    assert_eq!(consumed.len(), 1);
    assert_eq!(consumed[0].data["consumed_by"], act.to_string());
}

#[tokio::test]
async fn consume_rechecks_current_rules() {
    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    h.grant(&approval).await;
    h.engine.replace_rules(policies_with(
        PolicyActionId::MergePr,
        SettingsDisposition::Deny,
    ));
    let result = h
        .engine
        .consume_for_execution(
            &approval.id,
            &merge_check("head", "head"),
            &id::<Act>(1),
            &ctx(),
        )
        .await;
    assert_eq!(
        result,
        Err(GateError::ApprovalInvalid {
            reason: MismatchReason::NotGranted
        })
    );
    let status = h
        .engine
        .approval(&approval.id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(status.status, ApprovalStatus::Granted);
}

#[tokio::test]
async fn pending_or_unknown_approvals_cannot_be_consumed() {
    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    let check = merge_check("head", "head");
    assert_eq!(
        h.engine
            .consume_for_execution(&approval.id, &check, &id::<Act>(1), &ctx())
            .await,
        Err(GateError::ApprovalInvalid {
            reason: MismatchReason::NotGranted
        })
    );
    let unknown = id::<gantry_core::ids::Apr>(999);
    assert_eq!(
        h.engine
            .consume_for_execution(&unknown, &check, &id::<Act>(1), &ctx())
            .await,
        Err(GateError::ApprovalInvalid {
            reason: MismatchReason::NotGranted
        })
    );
}

// ---------------------------------------------------------------------------
// 状态机：惰性过期、作废、活跃审批
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lazy_expiry_transitions_once_on_read_and_consume() {
    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    h.grant(&approval).await;
    h.advance(Duration::from_secs(86_400));

    let check = merge_check("head", "head");
    assert_eq!(
        h.engine
            .consume_for_execution(&approval.id, &check, &id::<Act>(1), &ctx())
            .await,
        Err(GateError::ApprovalInvalid {
            reason: MismatchReason::Expired
        })
    );
    let read = h
        .engine
        .approval(&approval.id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(read.status, ApprovalStatus::Expired);
    assert_eq!(h.events(EventType::ApprovalExpired).await.len(), 1);
    // 过期后再判定：需要新审批。
    let decision = h
        .engine
        .decide_action(merge("head"), &ctx())
        .await
        .expect("decide");
    assert!(matches!(
        decision,
        ActionDecision::NeedsApproval { pending: None, .. }
    ));
}

#[tokio::test]
async fn invalidate_live_and_reject_terminal() {
    let h = Harness::new();
    let approval = needs_approval(&h, merge("head")).await;
    let invalidated = h
        .engine
        .invalidate(&approval.id, InvalidateReason::DeliveryAbandoned, &ctx())
        .await
        .expect("invalidate");
    assert_eq!(invalidated.status, ApprovalStatus::Invalidated);
    let events = h.events(EventType::ApprovalInvalidated).await;
    assert_eq!(events[0].data["reason"], "delivery_abandoned");
    assert!(matches!(
        h.engine
            .invalidate(&approval.id, InvalidateReason::OwnerCancelled, &ctx())
            .await,
        Err(PolicyError::Conflict { .. })
    ));
}

/// #89 Q2：`InvalidateReason` 带上前后值，事件 data 与之一一对应，不再降级为
/// `owner_cancelled`。
#[tokio::test]
async fn invalidate_carries_reason_fields_into_event() {
    let h = Harness::new();
    let cases = [
        InvalidateReason::HeadMoved {
            from: oid("old"),
            to: oid("new"),
        },
        InvalidateReason::PrClosed,
        InvalidateReason::PrMerged,
        InvalidateReason::AttemptSuperseded {
            from: id::<Att>(1),
            to: id::<Att>(2),
        },
    ];
    for reason in cases {
        let approval = needs_approval(&h, merge("head")).await;
        h.engine
            .invalidate(&approval.id, reason, &ctx())
            .await
            .expect("invalidate");
    }
    let events = h.events(EventType::ApprovalInvalidated).await;
    let reasons: Vec<_> = events.iter().map(|e| e.data["reason"].clone()).collect();
    assert_eq!(
        reasons,
        [
            "head_changed",
            "pr_closed",
            "pr_merged",
            "attempt_superseded"
        ]
    );
    assert_eq!(events[0].data["previous_head_sha"], oid("old").to_string());
    assert_eq!(events[0].data["current_head_sha"], oid("new").to_string());
    assert_eq!(
        events[3].data["previous_attempt_id"],
        id::<Att>(1).to_string()
    );
    assert_eq!(
        events[3].data["current_attempt_id"],
        id::<Att>(2).to_string()
    );
}

#[tokio::test]
async fn live_approvals_for_pr_lists_pending_and_granted_only() {
    let h = Harness::new();
    let pending = needs_approval(&h, merge("head")).await;
    let live = h
        .engine
        .live_approvals_for_pr(&slug(), 7)
        .await
        .expect("live");
    assert_eq!(
        live.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![pending.id]
    );
    assert!(h
        .engine
        .live_approvals_for_pr(&slug(), 8)
        .await
        .expect("live")
        .is_empty());
    h.engine
        .invalidate(&pending.id, InvalidateReason::OwnerCancelled, &ctx())
        .await
        .expect("invalidate");
    assert!(h
        .engine
        .live_approvals_for_pr(&slug(), 7)
        .await
        .expect("live")
        .is_empty());
}

// ---------------------------------------------------------------------------
// 与 FakePolicy 共享的一致性用例
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shared_conformance_holds_for_engine_and_fake() {
    let h = Harness::new();
    for req in [merge("head"), merge("stale"), push("gantry/x", false)] {
        action_conformance(&h.engine, req.clone(), &ctx())
            .await
            .expect("engine action conformance");
    }
    let approval = needs_approval(&h, merge("head")).await;
    h.grant(&approval).await;
    execution_conformance(
        &h.engine,
        &approval.id,
        &merge_check("head", "head"),
        &id::<Act>(1),
        &ctx(),
    )
    .await
    .expect("engine execution conformance");

    let fake = FakePolicy::new(ts());
    action_conformance(&fake, merge("head"), &ctx())
        .await
        .expect("fake action conformance");
    let spec = match h
        .engine
        .decide_action(merge("head2"), &ctx())
        .await
        .expect("decide")
    {
        ActionDecision::Deny { .. } => {
            // head2 与快照不符；直接用固定规格给替身。
            gantry_core::ports::ApprovalSpec {
                action: ApprovalAction::Forge(ForgeActionKind::MergePr),
                target: ApprovalTarget::Pr {
                    repo: slug(),
                    number: 7,
                },
                subject_digest: SubjectDigest::git(oid("head")),
                delivery_id: None,
                ttl: Duration::from_secs(60),
            }
        }
        other => panic!("unexpected {other:?}"),
    };
    let fake_approval = fake
        .request_approval(spec, ctx())
        .await
        .expect("fake request");
    fake.grant(&fake_approval.id).expect("fake grant");
    execution_conformance(
        &fake,
        &fake_approval.id,
        &merge_check("head", "head"),
        &id::<Act>(1),
        &ctx(),
    )
    .await
    .expect("fake execution conformance");
}
