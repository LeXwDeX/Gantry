//! 经 `PolicyPort` 的补丁判定审计、proposals 判定、路径规则端口与 M1 配置隔离
//! （policy.md §12.2「补丁策略」「proposals」「配置与过渡」的 M1 条目）。

mod support;

use std::collections::{BTreeMap, HashMap};

use gantry_core::entities::{
    Actor, ActorKind, PolicyActionId, Seq, SettingsResource, SettingsScope,
};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{Att, Dlv};
use gantry_core::ports::{
    ActionDecision, AddedLineSource, DenyReason, Disposition, PolicyError, PolicyPort,
    ProposalContext, ProposalDecision, ThreadNodeId,
};
use gantry_core::settings::{builtin_resolved, Policies, ResolvedSettings};
use gantry_core::worker::{
    EscalateInboxKind, EscalateParams, FileMode, FileStatus, OpenPrParams, PatchFacts, PatchFile,
    PatchVerdict, PathConstraints, Proposal, ProposalParams, RepoPath, ResolveReviewThreadParams,
};
use gantry_policy::{PolicyBuild, PolicyRules};
use support::*;

#[derive(Default)]
struct Lines(BTreeMap<String, Vec<String>>);

impl AddedLineSource for Lines {
    fn added_lines<'a>(
        &'a self,
        path: &'a RepoPath,
    ) -> Box<dyn Iterator<Item = std::io::Result<String>> + 'a> {
        let lines = self.0.get(path.as_str()).cloned().unwrap_or_default();
        Box::new(lines.into_iter().map(Ok))
    }
}

fn file(path: &str) -> PatchFile {
    PatchFile {
        path: RepoPath::parse(path).expect("path"),
        status: FileStatus::Modified,
        old_mode: Some(FileMode::Mode100644),
        new_mode: Some(FileMode::Mode100644),
        added: Some(1),
        deleted: Some(0),
        is_binary: false,
        is_symlink: false,
        is_submodule: false,
        size_after: Some(10),
    }
}

fn facts(paths: &[&str]) -> PatchFacts {
    PatchFacts {
        project_id: project_id(),
        delivery_id: id::<Dlv>(3),
        attempt_id: id::<Att>(4),
        patch_sha256: sha("patch"),
        patch_bytes: 100,
        applies_cleanly: true,
        files: paths.iter().map(|path| file(path)).collect(),
        total_changed_lines: paths.len() as u64,
        job_paths: PathConstraints {
            allow: vec!["**".into()],
            deny: Vec::new(),
            approval_required: Vec::new(),
        },
    }
}

fn builtin_policies() -> Policies {
    match builtin_resolved(SettingsResource::Policies) {
        ResolvedSettings::Policies(policies) => policies,
        other => panic!("unexpected resource {:?}", other.resource()),
    }
}

// ---------------------------------------------------------------------------
// evaluate_patch 的审计
// ---------------------------------------------------------------------------

#[tokio::test]
async fn denied_patch_appends_one_policy_denied_event() {
    let h = Harness::new();
    let patch = facts(&["Cargo.lock", ".github/workflows/ci.yml"]);
    for _ in 0..3 {
        let verdict = h
            .engine
            .evaluate_patch(&patch, &Lines::default(), &ctx())
            .await
            .expect("evaluate");
        assert_eq!(verdict.verdict, PatchVerdict::Deny);
        let report = verdict.report.expect("report");
        assert_eq!(report.payload_kind.as_deref(), Some("manual_action"));
    }
    let events = h.events(EventType::PolicyDenied).await;
    assert_eq!(events.len(), 1, "dedup_key keeps one record per verdict");
    let data = &events[0].data;
    assert_eq!(data["origin"], "patch");
    assert_eq!(data["target"]["repo"], "acme/widgets");
    assert_eq!(data["config_rev"], 0);
    let codes: Vec<&str> = data["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|reason| reason["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        vec![
            "policy.path_protected:lockfiles",
            "policy.path_protected:workflows"
        ]
    );
}

#[tokio::test]
async fn allowed_patch_writes_nothing_and_secrets_never_reach_events() {
    let h = Harness::new();
    let verdict = h
        .engine
        .evaluate_patch(&facts(&["src/lib.rs"]), &Lines::default(), &ctx())
        .await
        .expect("evaluate");
    assert_eq!(verdict.verdict, PatchVerdict::Allow);
    assert!(h.events(EventType::PolicyDenied).await.is_empty());

    let secret = format!("ghp_{}", "s".repeat(36));
    let mut lines = Lines::default();
    lines.0.insert("src/lib.rs".into(), vec![secret.clone()]);
    let verdict = h
        .engine
        .evaluate_patch(&facts(&["src/lib.rs"]), &lines, &ctx())
        .await
        .expect("evaluate");
    assert_eq!(verdict.verdict, PatchVerdict::Deny);
    let events = h.events(EventType::PolicyDenied).await;
    assert_eq!(events.len(), 1);
    assert!(!events[0].data.to_string().contains(&secret));
}

#[tokio::test]
async fn stricter_rules_apply_to_the_next_evaluation() {
    let h = Harness::new();
    let patch = facts(&["src/lib.rs"]);
    let verdict = h
        .engine
        .evaluate_patch(&patch, &Lines::default(), &ctx())
        .await
        .expect("evaluate");
    assert_eq!(verdict.verdict, PatchVerdict::Allow);

    let mut policies = builtin_policies();
    policies.allow_globs = vec!["docs/**".into()];
    h.engine
        .replace_rules(PolicyRules::load(&policies, Seq::new(11).expect("seq")).expect("valid"));
    let verdict = h
        .engine
        .evaluate_patch(&patch, &Lines::default(), &ctx())
        .await
        .expect("evaluate");
    assert_eq!(verdict.verdict, PatchVerdict::Deny);
    let events = h.events(EventType::PolicyDenied).await;
    assert_eq!(events[0].data["config_rev"], 11);
}

#[tokio::test]
async fn denied_patch_without_audit_target_is_an_error_not_a_pass() {
    let h = Harness::with_failing_forge();
    let result = h
        .engine
        .evaluate_patch(&facts(&["Cargo.lock"]), &Lines::default(), &ctx())
        .await;
    assert!(matches!(
        result,
        Err(PolicyError::ApprovalUnavailable { .. })
    ));
}

// ---------------------------------------------------------------------------
// proposals
// ---------------------------------------------------------------------------

fn proposal(id: &str, params: ProposalParams, target: Option<&str>) -> Proposal {
    Proposal {
        id: id.into(),
        kind: params.kind(),
        target: target.map(str::to_owned),
        params,
        rationale: "because".into(),
    }
}

fn open_pr(id: &str) -> Proposal {
    proposal(
        id,
        ProposalParams::OpenPr(OpenPrParams {
            title: "t".into(),
            body: "b".into(),
            draft: false,
        }),
        None,
    )
}

fn escalate(id: &str) -> Proposal {
    proposal(
        id,
        ProposalParams::Escalate(EscalateParams {
            inbox_kind: EscalateInboxKind::DecisionNeeded,
            message: "help".into(),
        }),
        None,
    )
}

fn proposal_ctx(patch: Option<PatchVerdict>) -> ProposalContext {
    ProposalContext {
        project_id: project_id(),
        delivery_id: id::<Dlv>(3),
        attempt_id: id::<Att>(4),
        patch,
        thread_has_human_comment: HashMap::new(),
    }
}

#[tokio::test]
async fn open_pr_follows_the_patch_verdict() {
    let h = Harness::new();
    let cases = [
        (
            Some(PatchVerdict::Allow),
            ProposalDecision::Accepted {
                proposal_id: "p".into(),
                action_id: Some(PolicyActionId::CreatePr),
            },
        ),
        (
            Some(PatchVerdict::NeedsApproval),
            ProposalDecision::Held {
                proposal_id: "p".into(),
            },
        ),
        (
            Some(PatchVerdict::Deny),
            ProposalDecision::Denied {
                proposal_id: "p".into(),
                reason: DenyReason::PatchDenied,
            },
        ),
        (
            None,
            ProposalDecision::Denied {
                proposal_id: "p".into(),
                reason: DenyReason::NoValidatedPatch,
            },
        ),
    ];
    for (patch, expected) in cases {
        let decisions = h
            .engine
            .decide_proposals(proposal_ctx(patch), &[open_pr("p")], &ctx())
            .await
            .expect("decide");
        assert_eq!(decisions, vec![expected], "{patch:?}");
    }
    assert_eq!(h.events(EventType::PolicyDenied).await.len(), 2);
}

#[tokio::test]
async fn escalate_is_accepted_and_one_denial_does_not_affect_others() {
    let h = Harness::new();
    let resolve = proposal(
        "r",
        ProposalParams::ResolveReviewThread(ResolveReviewThreadParams {}),
        Some("thread-1"),
    );
    let mut ctx_human = proposal_ctx(None);
    ctx_human
        .thread_has_human_comment
        .insert(ThreadNodeId::new("thread-1"), true);
    let decisions = h
        .engine
        .decide_proposals(
            ctx_human,
            &[open_pr("o"), escalate("e"), resolve.clone()],
            &ctx(),
        )
        .await
        .expect("decide");
    assert_eq!(
        decisions,
        vec![
            ProposalDecision::Denied {
                proposal_id: "o".into(),
                reason: DenyReason::NoValidatedPatch,
            },
            ProposalDecision::Accepted {
                proposal_id: "e".into(),
                action_id: None,
            },
            ProposalDecision::Denied {
                proposal_id: "r".into(),
                reason: DenyReason::ActionDenied,
            },
        ]
    );
    let denied = h.events(EventType::PolicyDenied).await;
    assert_eq!(denied.len(), 2);
    assert!(denied
        .iter()
        .all(|event| event.data["origin"] == "proposal"));

    // 线程无人类评论：按 resolve_review_thread（auto）放行。
    let mut ctx_bot = proposal_ctx(None);
    ctx_bot
        .thread_has_human_comment
        .insert(ThreadNodeId::new("thread-1"), false);
    let decisions = h
        .engine
        .decide_proposals(ctx_bot, &[resolve], &ctx())
        .await
        .expect("decide");
    assert_eq!(
        decisions,
        vec![ProposalDecision::Accepted {
            proposal_id: "r".into(),
            action_id: Some(PolicyActionId::ResolveReviewThread),
        }]
    );
}

// ---------------------------------------------------------------------------
// 路径规则端口、扫描器与 M1 配置隔离
// ---------------------------------------------------------------------------

#[tokio::test]
async fn path_rules_and_scanner_through_the_port() {
    let h = Harness::new();
    let rules = h.engine.path_rules(&project_id());
    let hits = rules.evaluate(&[RepoPath::parse("Cargo.lock").expect("path")]);
    assert_eq!(hits[0].disposition, Disposition::Approve);
    let snapshot = h.engine.path_rules_snapshot(&project_id());
    assert!(snapshot.approval_required.is_empty());
    assert!(snapshot.deny.iter().any(|glob| glob == "Cargo.lock"));
    assert!(h.engine.scan_text("conformance-probe").is_empty());
    assert_eq!(
        h.engine.scan_text(&format!("x sk-{}", "k".repeat(30)))[0].pattern,
        "openai_api_key"
    );
}

/// M1 构建只用内置规则：事件日志里出现 `policies` 的设置变更也不生效。
#[tokio::test]
async fn m1_ignores_policies_settings_in_the_log() {
    let h = Harness::new();
    let data = serde_json::json!({
        "scope": SettingsScope::Global,
        "resource": SettingsResource::Policies,
        "schema_version": 1,
        "base_rev": 0,
        "value": { "actions": { "create_pr": "deny" } },
        "relaxes": [],
        "secret_changes": [],
    });
    let mut tx = h.store.begin_write().await.expect("begin");
    tx.append(NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::settings(),
        project_id: None,
        actor: Actor::new(ActorKind::Device, "dev_test"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    })
    .await
    .expect("append settings.updated");
    tx.commit().await.expect("commit");

    assert_eq!(h.engine.build(), PolicyBuild::M1);
    assert_eq!(h.engine.rules().config_rev, Seq::ZERO);
    let req = gantry_core::ports::ActionRequest {
        project_id: project_id(),
        delivery_id: None,
        kind: gantry_core::entities::ForgeActionKind::CreatePr,
        facts: gantry_core::ports::ActionFacts::Other { repo: slug() },
        requested_by: ctx().actor,
    };
    let decision = h.engine.decide_action(req, &ctx()).await.expect("decide");
    assert_eq!(
        decision,
        ActionDecision::Allow {
            action_id: PolicyActionId::CreatePr,
            approval: None,
            config_rev: Seq::ZERO,
        }
    );
}
