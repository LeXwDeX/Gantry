//! `FakePolicy` 的审批执行安全回归。
//!
//! 覆盖：校验先于状态（head 变化的已消费重放不得返回 AlreadyConsumed）、
//! PushBranch 的 force/前缀动作、subject 与远端前置、未定审批形状、三分支
//! 负例（ConsumedByOther / NotGranted / Expired）、approval.consumed 事件，
//! 以及消费重放不受过期影响、pending / granted 的惰性过期。

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, RepoSlug, Sha256Digest, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind,
    Seq, SettingsResource, SettingsScope,
};
use gantry_core::events::{ApprovalRequestedData, EventType};
use gantry_core::ids::{Act, ActionId, Att, AttemptId, Evt, Id, IdKind, Ulid};
use gantry_core::ports::{
    ActorCtx, ApprovalSpec, ExecutionCheck, ExecutionPermit, GateError, InvalidateReason,
    MismatchReason, PatchApprovalCheck, PatchPermit, PolicyError, PolicyPort, RuleId,
};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_core::worker::RepoPath;
use gantry_fakes::policy::FakePolicy;
use gantry_store::{EventFilter, Store, StoreDeps};

const MS: u64 = 1_700_000_000_000;

fn ts() -> Timestamp {
    Timestamp::from_millis(MS as i64)
}

fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS, n).expect("test id parts")
}

fn oid(seed: &str) -> GitOid {
    let hex = sha256(seed.as_bytes()).to_hex();
    GitOid::parse(&hex[..40]).expect("test git oid")
}

fn sha(seed: &str) -> Sha256Digest {
    sha256(seed.as_bytes())
}

fn slug() -> RepoSlug {
    RepoSlug::parse("acme/widgets").expect("test repo slug")
}

fn actor() -> Actor {
    Actor::new(ActorKind::Engine, "test")
}

fn empty_store() -> Store {
    let clock = std::sync::Arc::new(FakeClock::new(MS as i64));
    let ids = std::sync::Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        std::sync::Arc::new(SeqEntropy::new(41)),
    ));
    Store::open_in_memory_sync(StoreDeps { clock, ids }).expect("in-memory store")
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: actor(),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(MS, 1).expect("test request id"),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn merge_spec(head: GitOid) -> ApprovalSpec {
    ApprovalSpec {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        target: ApprovalTarget::Pr {
            repo: slug(),
            number: 7,
        },
        subject_digest: SubjectDigest::git(head),
        delivery_id: None,
        ttl: Duration::from_secs(60),
    }
}

async fn merge_approval(policy: &FakePolicy, head: GitOid) -> Approval {
    let approval = policy
        .request_approval(merge_spec(head), ctx())
        .await
        .expect("merge approval");
    policy.grant(&approval.id).expect("grant");
    approval
}

fn patch_spec(attempt: AttemptId, patch: Sha256Digest) -> ApprovalSpec {
    ApprovalSpec {
        action: ApprovalAction::PatchApplyProtected,
        target: ApprovalTarget::Patch {
            repo: slug(),
            attempt_id: attempt,
            rules: vec!["protected".into()],
            paths: vec!["src/lib.rs".into()],
        },
        subject_digest: SubjectDigest::sha256(patch),
        delivery_id: None,
        ttl: Duration::from_secs(60),
    }
}

fn patch_check_for(attempt: AttemptId, patch: Sha256Digest) -> PatchApprovalCheck {
    PatchApprovalCheck {
        repo: slug(),
        attempt_id: attempt,
        patch_sha256: patch,
        rules: vec![RuleId::new("protected")],
        paths: vec![RepoPath::parse("src/lib.rs").expect("test path")],
    }
}

async fn granted_patch_approval(
    policy: &FakePolicy,
    attempt: AttemptId,
    patch: Sha256Digest,
) -> Approval {
    let approval = policy
        .request_approval(patch_spec(attempt, patch), ctx())
        .await
        .expect("patch approval");
    policy.grant(&approval.id).expect("grant patch");
    approval
}

fn expired_at() -> Timestamp {
    Timestamp::from_millis(MS as i64 + 61_000)
}

async fn branch_approval(
    policy: &FakePolicy,
    action: ApprovalAction,
    branch: &str,
    local: GitOid,
    remote: Option<GitOid>,
) -> Approval {
    let approval = policy
        .request_approval(
            ApprovalSpec {
                action,
                target: ApprovalTarget::Branch {
                    repo: slug(),
                    branch: branch.into(),
                    expected_remote_oid: remote,
                },
                subject_digest: SubjectDigest::git(local),
                delivery_id: None,
                ttl: Duration::from_secs(60),
            },
            ctx(),
        )
        .await
        .expect("branch approval");
    policy.grant(&approval.id).expect("grant");
    approval
}

fn branch_check(
    branch: &str,
    force: bool,
    local: GitOid,
    remote: Option<GitOid>,
) -> ExecutionCheck {
    ExecutionCheck::PushBranch {
        repo: slug(),
        branch: branch.into(),
        force,
        local_head_oid: local,
        expected_remote_oid: remote,
        validation_ref: Some(sha("patch")),
    }
}

fn merge_check(expected: GitOid, live: GitOid) -> ExecutionCheck {
    ExecutionCheck::MergePr {
        repo: slug(),
        number: 7,
        expected_head_sha: expected,
        live_head_sha: live,
    }
}

fn assert_approval_invalid(error: GateError, expected: MismatchReason) {
    match error {
        GateError::ApprovalInvalid { reason } => assert_eq!(reason, expected),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn merge_head_changed_after_consumption_is_not_already_consumed() {
    let policy = FakePolicy::new(ts());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;
    let act: ActionId = id::<Act>(40);
    assert_eq!(
        policy
            .consume_for_execution(&approval.id, &merge_check(head, head), &act, &ctx())
            .await
            .expect("consume"),
        ExecutionPermit::Consumed
    );

    // 已消费后 live head 变化：必须先报 HeadChanged，而不是 AlreadyConsumed。
    let error = policy
        .consume_for_execution(&approval.id, &merge_check(head, oid("moved")), &act, &ctx())
        .await
        .unwrap_err();
    assert!(matches!(error, GateError::HeadChanged { .. }));
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Consumed
    );
}

#[tokio::test]
async fn push_branch_checks_force_action_subject_and_remote() {
    let policy = FakePolicy::new(ts());
    let local = oid("local");
    let remote = oid("remote");

    let force = branch_approval(
        &policy,
        ApprovalAction::ForcePush,
        "gantry/1-fix",
        local,
        Some(remote),
    )
    .await;
    let act: ActionId = id::<Act>(41);
    assert_eq!(
        policy
            .consume_for_execution(
                &force.id,
                &branch_check("gantry/1-fix", true, local, Some(remote)),
                &act,
                &ctx(),
            )
            .await
            .expect("force push consume"),
        ExecutionPermit::Consumed
    );
    assert_eq!(
        policy
            .consume_for_execution(
                &force.id,
                &branch_check("gantry/1-fix", true, local, Some(remote)),
                &act,
                &ctx(),
            )
            .await
            .expect("same action replay"),
        ExecutionPermit::AlreadyConsumedBySameAction
    );

    // 形状表没有 Forge(PushBranch)：普通前缀内 push 不能申请审批，入口 fail-closed。
    let plain = policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::Forge(ForgeActionKind::PushBranch),
                target: ApprovalTarget::Branch {
                    repo: slug(),
                    branch: "gantry/2-fix".into(),
                    expected_remote_oid: None,
                },
                subject_digest: SubjectDigest::git(local),
                delivery_id: None,
                ttl: Duration::from_secs(60),
            },
            ctx(),
        )
        .await;
    assert!(
        matches!(plain, Err(PolicyError::InvalidRequest { .. })),
        "plain push approval must fail closed"
    );
    // 已消费的 force 审批也不能授权普通 push：动作不匹配先于状态判断。
    let error = policy
        .consume_for_execution(
            &force.id,
            &branch_check("gantry/1-fix", false, local, Some(remote)),
            &id::<Act>(42),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::ActionMismatch);

    let force_action = branch_approval(
        &policy,
        ApprovalAction::ForcePush,
        "gantry/3-fix",
        local,
        None,
    )
    .await;
    let error = policy
        .consume_for_execution(
            &force_action.id,
            &branch_check("gantry/3-fix", false, local, None),
            &id::<Act>(43),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::ActionMismatch);

    let outside = branch_approval(
        &policy,
        ApprovalAction::PushOutsidePrefix,
        "feature/x",
        local,
        None,
    )
    .await;
    assert_eq!(
        policy
            .consume_for_execution(
                &outside.id,
                &branch_check("feature/x", false, local, None),
                &id::<Act>(44),
                &ctx(),
            )
            .await
            .expect("outside prefix consume"),
        ExecutionPermit::Consumed
    );

    let subject_mismatch = branch_approval(
        &policy,
        ApprovalAction::PushOutsidePrefix,
        "feature/y",
        local,
        None,
    )
    .await;
    let error = policy
        .consume_for_execution(
            &subject_mismatch.id,
            &branch_check("feature/y", false, oid("other"), None),
            &id::<Act>(45),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::DigestMismatch);

    let remote_mismatch = branch_approval(
        &policy,
        ApprovalAction::ForcePush,
        "gantry/4-fix",
        local,
        Some(remote),
    )
    .await;
    let error = policy
        .consume_for_execution(
            &remote_mismatch.id,
            &branch_check("gantry/4-fix", true, local, None),
            &id::<Act>(46),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::DigestMismatch);

    // validation_ref 在 ApprovalTarget 里没有字段承载；不绑定也不虚构检查。
    let unbound = branch_approval(
        &policy,
        ApprovalAction::PushOutsidePrefix,
        "feature/z",
        local,
        None,
    )
    .await;
    let mut check = branch_check("feature/z", false, local, None);
    if let ExecutionCheck::PushBranch { validation_ref, .. } = &mut check {
        *validation_ref = Some(sha("another-patch"));
    }
    assert_eq!(
        policy
            .consume_for_execution(&unbound.id, &check, &id::<Act>(47), &ctx())
            .await
            .expect("validation_ref is not bound by the approval target"),
        ExecutionPermit::Consumed
    );
}

#[tokio::test]
async fn other_execution_check_rejects_non_forge_approvals() {
    let policy = FakePolicy::new(ts());
    let approval = policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::SettingsUpdate,
                target: ApprovalTarget::Settings {
                    scope: SettingsScope::Global,
                    project_id: None,
                    resource: SettingsResource::Policies,
                    base_rev: Seq::ZERO,
                },
                subject_digest: SubjectDigest::sha256(sha("settings")),
                delivery_id: None,
                ttl: Duration::from_secs(60),
            },
            ctx(),
        )
        .await
        .expect("settings approval");
    policy.grant(&approval.id).expect("grant");

    // APPROVAL_SHAPES 只覆盖条件伪动作与内部动作，没有 Forge(*) 形状；
    // Other 的成功路径不可达，检查必须对非 Forge 的审批 fail-closed。
    let error = policy
        .consume_for_execution(
            &approval.id,
            &ExecutionCheck::Other {
                kind: ForgeActionKind::CreateComment,
                target: serde_json::to_string(&ApprovalTarget::Pr {
                    repo: slug(),
                    number: 7,
                })
                .unwrap(),
            },
            &id::<Act>(48),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::ActionMismatch);
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Granted
    );
}

#[tokio::test]
async fn execution_consumption_reports_consumed_by_other_and_not_granted() {
    let policy = FakePolicy::new(ts());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;
    let first: ActionId = id::<Act>(50);
    let second: ActionId = id::<Act>(51);
    policy
        .consume_for_execution(&approval.id, &merge_check(head, head), &first, &ctx())
        .await
        .expect("first consume");
    let error = policy
        .consume_for_execution(&approval.id, &merge_check(head, head), &second, &ctx())
        .await
        .unwrap_err();
    assert_eq!(error, GateError::ConsumedByOther { by: first });

    let pending = policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::Forge(ForgeActionKind::MergePr),
                target: ApprovalTarget::Pr {
                    repo: slug(),
                    number: 8,
                },
                subject_digest: SubjectDigest::git(head),
                delivery_id: None,
                ttl: Duration::from_secs(60),
            },
            ctx(),
        )
        .await
        .expect("pending approval");
    let error = policy
        .consume_for_execution(
            &pending.id,
            &ExecutionCheck::MergePr {
                repo: slug(),
                number: 8,
                expected_head_sha: head,
                live_head_sha: head,
            },
            &id::<Act>(52),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::NotGranted);
}

#[tokio::test]
async fn expired_approval_is_rejected() {
    let policy = FakePolicy::new(ts());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;
    policy.set_now(Timestamp::from_millis(MS as i64 + 61_000));
    let error = policy
        .consume_for_execution(
            &approval.id,
            &merge_check(head, head),
            &id::<Act>(53),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::Expired);
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Expired
    );
}

#[tokio::test]
async fn consumed_execution_replay_survives_expiry_but_validates_first() {
    let policy = FakePolicy::new(ts());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;
    let act: ActionId = id::<Act>(70);
    policy
        .consume_for_execution(&approval.id, &merge_check(head, head), &act, &ctx())
        .await
        .expect("consume");
    policy.set_now(expired_at());

    // 同一动作重放：过期不改变已消费事实。
    assert_eq!(
        policy
            .consume_for_execution(&approval.id, &merge_check(head, head), &act, &ctx())
            .await
            .expect("replay after expiry"),
        ExecutionPermit::AlreadyConsumedBySameAction
    );
    assert_eq!(
        policy
            .consume_for_execution(
                &approval.id,
                &merge_check(head, head),
                &id::<Act>(71),
                &ctx()
            )
            .await
            .unwrap_err(),
        GateError::ConsumedByOther { by: act }
    );
    // head 校验仍先于 consumed 分支。
    let error = policy
        .consume_for_execution(&approval.id, &merge_check(head, oid("moved")), &act, &ctx())
        .await
        .unwrap_err();
    assert!(matches!(error, GateError::HeadChanged { .. }));
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Consumed
    );
}

#[tokio::test]
async fn consumed_patch_replay_survives_expiry_but_validates_first() {
    let policy = FakePolicy::new(ts());
    let attempt = id::<Att>(72);
    let patch = sha("patch-expiry");
    let approval = granted_patch_approval(&policy, attempt, patch).await;

    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(
                &mut tx,
                &approval.id,
                &patch_check_for(attempt, patch),
                &ctx(),
            )
            .await
            .expect("consume patch"),
        PatchPermit::Consumed
    );
    tx.commit().await.expect("commit patch consume");
    policy.set_now(expired_at());

    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(
                &mut tx,
                &approval.id,
                &patch_check_for(attempt, patch),
                &ctx(),
            )
            .await
            .expect("same attempt replay after expiry"),
        PatchPermit::AlreadyConsumedBySameAttempt
    );
    tx.rollback().await.expect("rollback replay");

    let mut tx = policy.begin_write();
    let error = policy
        .consume_patch_approval(
            &mut tx,
            &approval.id,
            &patch_check_for(attempt, sha("other-patch")),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::DigestMismatch);
    tx.rollback().await.expect("rollback digest mismatch");

    let mut tx = policy.begin_write();
    let mismatched_rules = PatchApprovalCheck {
        rules: vec![RuleId::new("other-rule")],
        ..patch_check_for(attempt, patch)
    };
    let error = policy
        .consume_patch_approval(&mut tx, &approval.id, &mismatched_rules, &ctx())
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::TargetMismatch);
    tx.rollback().await.expect("rollback target mismatch");
}

#[tokio::test]
async fn request_approval_appends_requested_event_and_keeps_pending_dedup() {
    let store = empty_store();
    let policy = FakePolicy::new_in(store.clone());
    policy.set_config_rev(Seq::new(4).unwrap());
    let head = oid("head");
    let request_ctx = ActorCtx {
        actor: Actor::new(ActorKind::Guardian, "guardian-test"),
        causation_id: Some(id::<Evt>(77)),
        ..ctx()
    };

    let spec = merge_spec(head);
    let first = policy
        .request_approval(spec.clone(), request_ctx.clone())
        .await
        .unwrap();
    let duplicate = policy
        .request_approval(spec.clone(), request_ctx.clone())
        .await
        .unwrap();
    assert_eq!(first.id, duplicate.id);

    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    let event = &page.events[0];
    assert_eq!(event.ty, EventType::ApprovalRequested);
    assert_eq!(event.subject.as_str(), first.id.to_string());
    assert_eq!(event.project_id, None);
    assert_eq!(event.actor, request_ctx.actor);
    assert_eq!(event.causation_id, request_ctx.causation_id);
    let data: ApprovalRequestedData = serde_json::from_value(event.data.clone()).unwrap();
    assert_eq!(data.action, spec.action);
    assert_eq!(data.target, spec.target);
    assert_eq!(data.subject_digest, spec.subject_digest);
    assert_eq!(data.expires_at, first.expires_at);
    assert_eq!(data.config_rev, Seq::new(4).unwrap());
    assert_eq!(data.requested_by, request_ctx.actor);

    // Validation failure must leave neither an approval nor an event behind.
    let invalid = ApprovalSpec {
        action: ApprovalAction::Forge(ForgeActionKind::PushBranch),
        target: ApprovalTarget::Branch {
            repo: slug(),
            branch: "gantry/2-fix".into(),
            expected_remote_oid: None,
        },
        subject_digest: SubjectDigest::git(head),
        delivery_id: None,
        ttl: Duration::from_secs(60),
    };
    assert!(matches!(
        policy.request_approval(invalid, ctx()).await,
        Err(PolicyError::InvalidRequest { .. })
    ));
    assert_eq!(policy.approvals_len(), 1);
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
}

#[tokio::test]
async fn expired_pending_is_not_granted_and_request_creates_new() {
    let store = empty_store();
    let policy = FakePolicy::new_in(store.clone());
    let head = oid("head");
    let spec = merge_spec(head);
    let first = policy
        .request_approval(spec.clone(), ctx())
        .await
        .expect("pending approval");
    let duplicate = policy
        .request_approval(spec.clone(), ctx())
        .await
        .expect("duplicate while pending");
    assert_eq!(duplicate.id, first.id);
    policy.set_now(expired_at());

    // grant 自己先做惰性过期，不把过期的 pending 升为 granted。
    let error = policy.grant(&first.id).unwrap_err();
    assert!(matches!(error, PolicyError::Conflict { .. }));
    assert_eq!(
        policy.approval(&first.id).unwrap().status,
        ApprovalStatus::Expired
    );

    // 过期行不再算“已存在的同主体 pending”，必须新建。
    let replacement = policy
        .request_approval(spec, ctx())
        .await
        .expect("replacement approval");
    assert_ne!(replacement.id, first.id);
    assert_eq!(replacement.status, ApprovalStatus::Pending);
    assert_eq!(
        policy.approval(&first.id).unwrap().status,
        ApprovalStatus::Expired
    );

    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    assert!(
        page.events
            .iter()
            .all(|event| event.ty == EventType::ApprovalRequested),
        "lazy expiry must not fabricate events"
    );
}

#[tokio::test]
async fn expired_granted_is_hidden_from_live_and_not_invalidated() {
    let store = empty_store();
    let policy = FakePolicy::new_in(store.clone());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;
    policy.set_now(expired_at());

    assert!(policy
        .live_approvals_for_pr(&slug(), 7)
        .await
        .expect("live approvals")
        .is_empty());
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Expired
    );

    let error = policy
        .invalidate(&approval.id, InvalidateReason::DeliveryAbandoned, &ctx())
        .await
        .unwrap_err();
    assert!(matches!(error, PolicyError::Conflict { .. }));
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Expired
    );

    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].ty, EventType::ApprovalRequested);
}

#[tokio::test]
async fn approval_consumed_event_records_action_and_subject() {
    let store = empty_store();
    let policy = FakePolicy::new_in(store.clone());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;
    let act: ActionId = id::<Act>(60);
    assert_eq!(
        policy
            .consume_for_execution(&approval.id, &merge_check(head, head), &act, &ctx())
            .await
            .expect("consume"),
        ExecutionPermit::Consumed
    );

    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[0].ty, EventType::ApprovalRequested);
    let event = &page.events[1];
    assert_eq!(event.ty, EventType::ApprovalConsumed);
    assert_eq!(event.subject.as_str(), approval.id.to_string());
    assert_eq!(event.data["approval_id"], serde_json::json!(approval.id));
    assert_eq!(
        event.data["consumed_by"],
        serde_json::json!(act.to_string())
    );
    assert_eq!(
        event.data["subject_digest"],
        serde_json::json!(SubjectDigest::git(head))
    );

    // 重放不追加第二个事件。
    assert_eq!(
        policy
            .consume_for_execution(&approval.id, &merge_check(head, head), &act, &ctx())
            .await
            .expect("replay"),
        ExecutionPermit::AlreadyConsumedBySameAction
    );
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
}

#[tokio::test]
async fn action_target_and_digest_mismatches_are_rejected() {
    let policy = FakePolicy::new(ts());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;
    let act: ActionId = id::<Act>(61);

    let error = policy
        .consume_for_execution(
            &approval.id,
            &ExecutionCheck::PushBranch {
                repo: slug(),
                branch: "gantry/1-fix".into(),
                force: false,
                local_head_oid: head,
                expected_remote_oid: None,
                validation_ref: None,
            },
            &act,
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::ActionMismatch);

    let error = policy
        .consume_for_execution(
            &approval.id,
            &ExecutionCheck::MergePr {
                repo: slug(),
                number: 9,
                expected_head_sha: head,
                live_head_sha: head,
            },
            &act,
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::TargetMismatch);

    let error = policy
        .consume_for_execution(
            &approval.id,
            &merge_check(oid("other"), oid("other")),
            &act,
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::DigestMismatch);
}

#[tokio::test]
async fn expired_unconsumed_pending_and_granted_return_expired() {
    let head = oid("head");

    // pending execution approval：过期消费入口返回 Expired，而不是 NotGranted。
    let pending_policy = FakePolicy::new(ts());
    let pending = pending_policy
        .request_approval(merge_spec(head), ctx())
        .await
        .expect("pending approval");
    pending_policy.set_now(expired_at());
    let error = pending_policy
        .consume_for_execution(
            &pending.id,
            &merge_check(head, head),
            &id::<Act>(80),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::Expired);
    assert_eq!(
        pending_policy.approval(&pending.id).unwrap().status,
        ApprovalStatus::Expired
    );

    // granted patch approval：入口先 CAS 到 Expired，错误返回也要经调用方事务提交。
    let granted_policy = FakePolicy::new(ts());
    let attempt = id::<Att>(81);
    let patch = sha("expired-granted");
    let granted = granted_patch_approval(&granted_policy, attempt, patch).await;
    granted_policy.set_now(expired_at());
    let mut tx = granted_policy.begin_write();
    let error = granted_policy
        .consume_patch_approval(
            &mut tx,
            &granted.id,
            &patch_check_for(attempt, patch),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::Expired);
    tx.commit().await.expect("persist lazy expiry");
    assert_eq!(
        granted_policy.approval(&granted.id).unwrap().status,
        ApprovalStatus::Expired
    );

    // pending patch approval：同样在消费入口过期并返回 Expired。
    let patch_policy = FakePolicy::new(ts());
    let pending_attempt = id::<Att>(82);
    let pending_patch = sha("expired-pending-patch");
    let pending = patch_policy
        .request_approval(patch_spec(pending_attempt, pending_patch), ctx())
        .await
        .expect("pending patch approval");
    patch_policy.set_now(expired_at());
    let mut tx = patch_policy.begin_write();
    let error = patch_policy
        .consume_patch_approval(
            &mut tx,
            &pending.id,
            &patch_check_for(pending_attempt, pending_patch),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::Expired);
    tx.commit().await.expect("persist lazy expiry");
    assert_eq!(
        patch_policy.approval(&pending.id).unwrap().status,
        ApprovalStatus::Expired
    );
}

#[tokio::test]
async fn consumed_expired_patch_replay_checks_target_before_already_consumed() {
    let policy = FakePolicy::new(ts());
    let attempt = id::<Att>(83);
    let patch = sha("consumed-expired-target");
    let approval = granted_patch_approval(&policy, attempt, patch).await;

    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(
                &mut tx,
                &approval.id,
                &patch_check_for(attempt, patch),
                &ctx()
            )
            .await
            .expect("consume patch"),
        PatchPermit::Consumed
    );
    tx.commit().await.expect("commit patch consume");
    policy.set_now(expired_at());

    // 已消费 + 已过期：错误的 attempt 仍先报 TargetMismatch。
    let mut tx = policy.begin_write();
    let error = policy
        .consume_patch_approval(
            &mut tx,
            &approval.id,
            &patch_check_for(id::<Att>(84), patch),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_approval_invalid(error, MismatchReason::TargetMismatch);
    tx.rollback().await.expect("rollback target mismatch");

    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(
                &mut tx,
                &approval.id,
                &patch_check_for(attempt, patch),
                &ctx()
            )
            .await
            .expect("same attempt replay after expiry"),
        PatchPermit::AlreadyConsumedBySameAttempt
    );
    tx.rollback().await.expect("rollback replay");
}

#[tokio::test]
async fn read_only_queries_do_not_break_an_open_write_tx() {
    let store = empty_store();
    let policy = FakePolicy::new_in(store.clone());
    let head = oid("head");
    let approval = merge_approval(&policy, head).await;

    let tx = store.begin_write().await.expect("open write tx");
    assert!(policy.approval(&approval.id).is_some());
    assert_eq!(
        policy
            .live_approvals_for_pr(&slug(), 7)
            .await
            .expect("live approvals")
            .len(),
        1
    );
    tx.commit()
        .await
        .expect("read-only queries must not move the store revision");
}
