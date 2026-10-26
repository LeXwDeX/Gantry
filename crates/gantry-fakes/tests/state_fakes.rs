//! 五个 M0 状态替身的示例测试与共享一致性用例调用。
//!
//! 这些测试不接触 store、Git、GitHub 或文件系统；它们验证替身自身的事务、
//! 幂等、锁、审批消费、去重、版本与脱敏语义。

use std::any::Any;
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, IdempotencyKey, RepoSlug, Sha256Digest, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, ApprovalStatus, ApprovalTarget, CheckSummary, DecisionKind,
    ForgeActionKind, InboxItem, InboxKind, InboxOption, InboxPayload, LockHolder, MergeMethod,
    PolicyActionId, Priority, Project, PullRequestRef, PullRequestState, ReasonCode, ReviewState,
    Seq, Severity,
};
use gantry_core::forge::{ActionPayload, AddLabelsPayload};
use gantry_core::ids::{Act, Apr, Att, Dev, Dlv, Id, IdKind, Prj, Ulid};
use gantry_core::ports::{
    ActionDecision, ActionFacts, ActionRequest, ActorCtx, ApnsEnvironment, ApprovalSpec,
    BranchName, ClassifyPort, ClientProfile, CommitInfo, CommitMessage, CommitPatch,
    ConversationService, CreateWorktree, DeclaredPatch, DedupKey, DeliveryControl, DenyReason,
    Disposition, EgressControl, EnqueueError, EnqueueRequest, EnqueuedBy, EnqueuedByModule,
    ExecutionCheck, Expect, ForgeReader, GateError, HealPort, InboxPort, LockError, MirrorWant,
    ModelProvider, OptionId, OutboxPort, PatchApprovalCheck, PatchPermit, PathRuleSet, PolicyPort,
    PrRefresher, PushToken, RaiseError, RaiseOutcome, RaiseRequest, ReconcileTarget, RepoError,
    RepoService, RuleId, SafeSummary, SandboxService, SecretSink, SecretSource, SecretValue,
    SecretVersion, SecretsError, StoreError, TxHandle, ValidatePatch, WorkerAdapter, WorktreeLocks,
};
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::time::Timestamp;
use gantry_core::worker::{PathConstraints, RepoPath};
use gantry_fakes::conversation::FakeConversation;
use gantry_fakes::egress::EgressStub;
use gantry_fakes::engine::FakeEngine;
use gantry_fakes::forge::{outbox_conformance, FakeForge, FakeWebhook};
use gantry_fakes::inbox::{dedup_conformance, FakeInbox};
use gantry_fakes::models::FakeModelProvider;
use gantry_fakes::policy::{action_conformance, execution_conformance, FakePathRules, FakePolicy};
use gantry_fakes::repo::{lock_conformance, patch_idempotency_conformance, FakeRepo};
use gantry_fakes::sandbox::FakeSandboxService;
use gantry_fakes::secrets::{secrets_conformance, MemorySecretStore, MemorySecrets};
use gantry_fakes::worker::StubWorkerAdapter;

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

fn project() -> Project {
    Project {
        id: id::<Prj>(1),
        repo: slug(),
        installation_id: 42,
        bot_user_id: 9,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Merge],
        merge_queue_enabled: false,
        created_at: ts(),
        updated_at: ts(),
    }
}

fn checks(passed: u32, failed: u32, pending: u32) -> CheckSummary {
    CheckSummary {
        passed,
        failed,
        pending,
        required_ok: failed == 0 && pending == 0,
    }
}

fn pull_request() -> PullRequestRef {
    PullRequestRef {
        repo: slug(),
        number: 7,
        node_id: "PR_test".into(),
        title: gantry_core::text::UntrustedText::new("title".into(), "test".into(), false),
        state: PullRequestState::Open,
        head_sha: oid("head"),
        base_ref: "main".into(),
        base_sha: oid("base"),
        draft: false,
        mergeable_state: "clean".into(),
        in_merge_queue: false,
        checks: checks(1, 0, 0),
        review_state: ReviewState::None,
        synced_at: ts(),
    }
}

fn empty_paths() -> PathConstraints {
    PathConstraints {
        allow: Vec::new(),
        deny: Vec::new(),
        approval_required: Vec::new(),
    }
}

fn add_labels_request(epoch: u32) -> EnqueueRequest {
    EnqueueRequest {
        project_id: id::<Prj>(1),
        delivery_id: id::<Dlv>(2),
        payload: gantry_core::forge::ActionPayload::AddLabels(AddLabelsPayload {
            repo: slug(),
            number: 7,
            labels: vec!["bug".into()],
        }),
        requires_approval: None,
        depends_on: vec![],
        priority: Priority::Normal,
        not_before: None,
        epoch,
        enqueued_by: EnqueuedBy {
            module: EnqueuedByModule::Engine,
            causation_id: None,
        },
    }
}

fn decision_payload() -> InboxPayload {
    InboxPayload::DecisionNeeded {
        kind: DecisionKind::ManualAction,
        reason_code: ReasonCode::parse("manual_action").expect("reason code"),
        options: vec![InboxOption::new("approve", "Approve", true)],
        node_id: None,
        patch: None,
    }
}

fn raise_request(dedup: &str, project_id: Option<gantry_core::ids::ProjectId>) -> RaiseRequest {
    RaiseRequest {
        project_id,
        delivery_id: Some(id::<Dlv>(2)),
        kind: InboxKind::DecisionNeeded,
        severity: Severity::Warn,
        summary: SafeSummary::new("needs a decision"),
        payload: decision_payload(),
        dedup_key: DedupKey::new(dedup),
        supersedes: vec![],
        actor: actor(),
        causation_id: None,
    }
}

#[tokio::test]
async fn forge_outbox_is_idempotent_and_tx_is_atomic() {
    let forge = FakeForge::new(ts());
    let project = project();
    let project_id = project.id;
    forge.insert_project(project);

    let request = add_labels_request(0);
    let ctx = ctx();
    outbox_conformance(&forge, request.clone(), &ctx)
        .await
        .expect("outbox conformance");
    let before = forge.outbox_len();

    let mut tx = forge.begin_write();
    let mut retry = request.clone();
    retry.epoch = 10;
    let outcome = forge
        .enqueue_in(&mut tx, retry, &ctx)
        .await
        .expect("enqueue in tx");
    assert!(outcome.created);
    tx.rollback().await.expect("rollback");
    assert_eq!(forge.outbox_len(), before);

    let mut tx = forge.begin_write();
    let mut retry = request;
    retry.epoch = 11;
    let outcome = forge
        .enqueue_in(&mut tx, retry, &ctx)
        .await
        .expect("enqueue in tx");
    assert!(outcome.created);
    tx.commit().await.expect("commit");
    assert_eq!(forge.outbox_len(), before + 1);

    let actions = forge.outbox_list();
    assert!(actions.iter().all(|action| action.project_id == project_id));
}

#[tokio::test]
async fn add_labels_idempotency_uses_canonical_target_and_label_set() {
    let forge = FakeForge::new(ts());
    forge.insert_project(project());
    let ctx = ctx();

    let mut request = add_labels_request(0);
    if let ActionPayload::AddLabels(payload) = &mut request.payload {
        payload.labels = vec!["ui".into(), "bug".into(), "bug".into()];
    }
    let expected_key = IdempotencyKey::derive(
        &request.project_id,
        &request.delivery_id,
        ForgeActionKind::AddLabels,
        "labels:7",
        &serde_json::json!({"labels": ["bug", "ui"]}),
        0,
    )
    .unwrap();
    let first = forge.enqueue(request.clone(), &ctx).await.unwrap();
    assert!(first.created);
    assert_eq!(first.action.idempotency_key, expected_key);
    assert_eq!(first.action.payload, request.payload);

    let mut reordered = add_labels_request(0);
    if let ActionPayload::AddLabels(payload) = &mut reordered.payload {
        payload.labels = vec!["bug".into(), "ui".into()];
    }
    let second = forge.enqueue(reordered, &ctx).await.unwrap();
    assert!(!second.created);
    assert_eq!(second.action.id, first.action.id);
    assert_eq!(forge.outbox_len(), 1);

    let mut changed_set = add_labels_request(0);
    if let ActionPayload::AddLabels(payload) = &mut changed_set.payload {
        payload.labels = vec!["bug".into()];
    }
    let third = forge.enqueue(changed_set, &ctx).await.unwrap();
    assert!(third.created);
    assert_ne!(third.action.id, first.action.id);

    let mut changed_epoch = add_labels_request(1);
    if let ActionPayload::AddLabels(payload) = &mut changed_epoch.payload {
        payload.labels = vec!["ui".into(), "bug".into()];
    }
    let fourth = forge.enqueue(changed_epoch, &ctx).await.unwrap();
    assert!(fourth.created);
    assert_ne!(fourth.action.id, first.action.id);
    assert_eq!(forge.outbox_len(), 3);
}

#[tokio::test]
async fn forge_reader_reflects_webhooks_and_refreshes_pr() {
    let forge = FakeForge::new(ts());
    let project = project();
    let project_id = project.id;
    forge.insert_project(project);
    forge.upsert_pr(project_id, "feature/test", pull_request());

    let ctx = ctx();
    let before = forge
        .pull_request(&project_id, 7)
        .await
        .expect("pull_request")
        .expect("seeded pr");
    assert_eq!(before.head_sha, oid("head"));

    let new_checks = checks(2, 1, 0);
    assert!(forge.inject_webhook(
        "delivery-1",
        FakeWebhook::PrChecksUpdated {
            project: project_id,
            number: 7,
            checks: new_checks.clone(),
        }
    ));
    assert!(!forge.inject_webhook(
        "delivery-1",
        FakeWebhook::PrChecksUpdated {
            project: project_id,
            number: 7,
            checks: checks(0, 1, 0),
        }
    ));
    let after = forge
        .pull_request(&project_id, 7)
        .await
        .expect("pull_request")
        .expect("updated pr");
    assert_eq!(after.checks, new_checks);
    assert_eq!(forge.webhook_count(), 1);

    let head = forge
        .refresh_pr(&slug(), 7, &ctx)
        .await
        .expect("refresh_pr");
    assert_eq!(head.ref_name, "feature/test");
    assert_eq!(head.sha, oid("head"));

    forge
        .reconcile_now(
            ReconcileTarget::Pr {
                project: project_id,
                number: 7,
            },
            &ctx,
        )
        .await
        .expect("reconcile");
    forge
        .sync_mirror(&project_id, MirrorWant::All, &ctx)
        .await
        .expect("sync mirror");
    assert_eq!(forge.reconcile_count(), 1);
    assert_eq!(forge.mirror_sync_count(), 1);
}

#[tokio::test]
async fn repo_locks_expire_and_patch_commit_is_idempotent() {
    let repo = FakeRepo::new(ts());
    let project = project();
    let project_id = project.id;
    let delivery = id::<Dlv>(2);
    let base = oid("base");
    repo.insert_commit(
        project_id,
        base,
        CommitInfo {
            parents: vec![],
            tree: oid("tree"),
            trailers: vec![],
        },
    );
    let ctx = ctx();
    let worktree = repo
        .create_worktree(
            &ctx,
            CreateWorktree {
                project_id,
                delivery_id: delivery,
                branch: BranchName::new("gantry/test"),
                base_ref: "main".into(),
                base_sha: base,
            },
        )
        .await
        .expect("create_worktree");

    lock_conformance(
        &repo,
        &ctx,
        &worktree.id,
        &delivery,
        Duration::from_secs(60),
    )
    .await
    .expect("lock conformance");

    let engine = repo
        .acquire_lock(
            &ctx,
            &worktree.id,
            LockHolder::Engine,
            &delivery,
            Duration::from_secs(60),
        )
        .await
        .expect("engine lock");
    assert!(matches!(
        repo.acquire_lock(
            &ctx,
            &worktree.id,
            LockHolder::Human,
            &delivery,
            Duration::from_secs(60)
        )
        .await,
        Err(RepoError::WorktreeLocked { .. })
    ));
    assert!(matches!(
        repo.acquire_human(&ctx, &worktree.id, Duration::from_secs(14_401))
            .await,
        Err(LockError::LeaseTooLong { .. })
    ));

    repo.advance_ms(120_000);
    let human_worktree = repo
        .acquire_human(&ctx, &worktree.id, Duration::from_secs(60))
        .await
        .expect("human lock after expiry");
    assert!(human_worktree.lock.is_some());
    let released = repo
        .release_human(&ctx, &worktree.id)
        .await
        .expect("release human");
    assert!(released.lock.is_none());
    assert!(repo
        .renew_lock(&ctx, &engine, Duration::from_secs(60))
        .await
        .is_err());

    let job = id::<Att>(3);
    let patch = sha("patch");
    let validate = ValidatePatch {
        project_id,
        delivery_id: delivery,
        job_id: job,
        worktree_id: Some(worktree.id),
        base_sha: base,
        patch,
        declared: DeclaredPatch {
            sha256: patch,
            bytes: 10,
            base_sha: base,
            files_changed: 1,
        },
        job_paths: empty_paths(),
    };
    patch_idempotency_conformance(&repo, &ctx, validate)
        .await
        .expect("patch idempotency");

    let lock = repo
        .acquire_lock(
            &ctx,
            &worktree.id,
            LockHolder::Engine,
            &delivery,
            Duration::from_secs(60),
        )
        .await
        .expect("commit lock");
    let commit = CommitPatch {
        project_id,
        job_id: job,
        worktree_id: worktree.id,
        lock,
        approval: None,
        message: CommitMessage {
            subject: "Fix test".into(),
            body: String::new(),
        },
        timestamp: ts(),
    };
    let record = repo
        .commit_patch(&ctx, commit.clone())
        .await
        .expect("commit patch");
    assert_eq!(record.parent, base);
    assert_eq!(record.job_id, job);
    let again = repo
        .commit_patch(&ctx, commit)
        .await
        .expect("idempotent commit");
    assert_eq!(again.commit_oid, record.commit_oid);
    assert_eq!(repo.commit_record(&job), Some(record.clone()));
    assert!(repo
        .provenance(&project_id, &record.commit_oid)
        .await
        .expect("provenance")
        .is_some());
}

#[tokio::test]
async fn policy_decisions_and_approvals_are_single_use() {
    let policy = FakePolicy::new(ts());
    let project = project();
    let project_id = project.id;
    let head = oid("head");
    let ctx = ctx();
    let action = ActionRequest {
        project_id,
        delivery_id: None,
        kind: ForgeActionKind::MergePr,
        facts: ActionFacts::Other { repo: slug() },
        requested_by: actor(),
    };
    action_conformance(&policy, action.clone(), &ctx)
        .await
        .expect("action conformance");

    policy.script_decision(
        ForgeActionKind::MergePr,
        ActionDecision::Deny {
            action_id: PolicyActionId::MergePr,
            reason: DenyReason::ActionDenied,
            report: None,
            config_rev: Seq::ZERO,
        },
    );
    assert!(matches!(
        policy.decide_action(action, &ctx).await.expect("decision"),
        ActionDecision::Deny { .. }
    ));

    let spec = ApprovalSpec {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        target: ApprovalTarget::Pr {
            repo: slug(),
            number: 7,
        },
        subject_digest: SubjectDigest::git(head),
        delivery_id: None,
        ttl: Duration::from_secs(60),
    };
    let approval = policy
        .request_approval(spec.clone(), ctx.clone())
        .await
        .expect("request approval");
    let duplicate = policy
        .request_approval(spec, ctx.clone())
        .await
        .expect("duplicate approval");
    assert_eq!(duplicate.id, approval.id);
    policy.grant(&approval.id).expect("grant");

    let check = ExecutionCheck::MergePr {
        repo: slug(),
        number: 7,
        expected_head_sha: head,
        live_head_sha: head,
    };
    let act: gantry_core::ids::ActionId = id::<Act>(40);
    execution_conformance(&policy, &approval.id, &check, &act, &ctx)
        .await
        .expect("execution conformance");
    assert_eq!(
        policy.approval(&approval.id).expect("approval").status,
        ApprovalStatus::Consumed
    );

    let spec2 = ApprovalSpec {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        target: ApprovalTarget::Pr {
            repo: slug(),
            number: 7,
        },
        subject_digest: SubjectDigest::git(head),
        delivery_id: None,
        ttl: Duration::from_secs(60),
    };
    let approval2 = policy
        .request_approval(spec2, ctx.clone())
        .await
        .expect("second approval");
    policy.grant(&approval2.id).expect("grant second");
    let changed = ExecutionCheck::MergePr {
        repo: slug(),
        number: 7,
        expected_head_sha: head,
        live_head_sha: oid("moved"),
    };
    assert!(matches!(
        policy
            .consume_for_execution(&approval2.id, &changed, &id::<Act>(41), &ctx)
            .await,
        Err(GateError::HeadChanged { .. })
    ));
    assert_eq!(
        policy.approval(&approval2.id).expect("approval").status,
        ApprovalStatus::Granted
    );

    let attempt = id::<Att>(50);
    let patch = sha("patch");
    let patch_spec = ApprovalSpec {
        action: ApprovalAction::PatchApplyProtected,
        target: ApprovalTarget::Patch {
            repo: slug(),
            attempt_id: attempt,
            rules: vec!["protected".into()],
            paths: vec!["src/lib.rs".into()],
        },
        subject_digest: SubjectDigest::sha256(patch),
        delivery_id: Some(id::<Dlv>(2)),
        ttl: Duration::from_secs(60),
    };
    let patch_approval = policy
        .request_approval(patch_spec, ctx.clone())
        .await
        .expect("patch approval");
    policy.grant(&patch_approval.id).expect("grant patch");
    let patch_check = PatchApprovalCheck {
        repo: slug(),
        attempt_id: attempt,
        patch_sha256: patch,
        rules: vec![RuleId::new("protected")],
        paths: vec![RepoPath::parse("src/lib.rs").expect("path")],
    };
    let mut tx = policy.begin_write();
    let permit = policy
        .consume_patch_approval(&mut tx, &patch_approval.id, &patch_check, &ctx)
        .await
        .expect("consume patch");
    assert_eq!(permit, PatchPermit::Consumed);
    tx.rollback().await.expect("rollback patch consume");
    assert_eq!(
        policy
            .approval(&patch_approval.id)
            .expect("approval")
            .status,
        ApprovalStatus::Granted
    );
    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(&mut tx, &patch_approval.id, &patch_check, &ctx)
            .await
            .expect("consume patch"),
        PatchPermit::Consumed
    );
    tx.commit().await.expect("commit patch consume");
    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(&mut tx, &patch_approval.id, &patch_check, &ctx)
            .await
            .expect("replay patch consume"),
        PatchPermit::AlreadyConsumedBySameAttempt
    );
    tx.rollback().await.expect("rollback replay");

    policy.set_path_rules(
        project_id,
        PathConstraints {
            allow: vec!["src/*".into()],
            deny: vec!["src/secret.rs".into()],
            approval_required: vec!["src/protected.rs".into()],
        },
    );
    let rules = policy.path_rules(&project_id);
    let hits = rules.evaluate(&[
        RepoPath::parse("src/secret.rs").expect("path"),
        RepoPath::parse("docs/readme.md").expect("path"),
    ]);
    assert_eq!(hits[0].disposition, Disposition::Deny);
    assert!(hits[1].outside_allow);

    policy.script_secret_token("ghp_test", "github_token");
    let findings = policy.scan_text("token ghp_test here");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].pattern, "github_token");
}

#[tokio::test]
async fn inbox_dedups_only_open_items_and_keeps_push_tokens_private() {
    let inbox = FakeInbox::new(ts());
    let project = project();
    let project_id = project.id;
    let ctx = ctx();
    let request = raise_request("decision:test", Some(project_id));
    dedup_conformance(&inbox, request, &ctx)
        .await
        .expect("inbox conformance");

    let summary = inbox.summary().await.expect("summary");
    assert_eq!(summary.total_open, 1);
    assert_eq!(summary.by_project.len(), 1);
    assert_eq!(summary.by_project[0].project_id, Some(project_id));
    assert_eq!(summary.by_project[0].open, 1);
    assert_eq!(summary.by_project[0].max_severity, Severity::Warn);

    let escalation = RaiseRequest {
        project_id: Some(project_id),
        delivery_id: Some(id::<Dlv>(2)),
        kind: InboxKind::Escalation,
        severity: Severity::Blocker,
        summary: SafeSummary::new("escalate"),
        payload: InboxPayload::Escalation {
            reason_code: ReasonCode::parse("manual_action").expect("reason code"),
            node_id: None,
            attempt_id: None,
            target_level: None,
            evidence: vec![],
            options: Some(vec![InboxOption::new("accept", "Accept", true)]),
        },
        dedup_key: DedupKey::new("esc:test"),
        supersedes: vec![],
        actor: actor(),
        causation_id: None,
    };
    let RaiseOutcome::Created(escalation) = inbox
        .raise(None, escalation, &ctx)
        .await
        .expect("raise escalation")
    else {
        panic!("escalation should be created");
    };
    let mobile = ActorCtx {
        client_profile: Some(ClientProfile::Mobile),
        ..crate::ctx()
    };
    assert!(matches!(
        inbox
            .choose(&escalation.item.id, &OptionId::new("accept"), None, &mobile)
            .await,
        Err(gantry_core::ports::ChooseError::ProfileForbidden)
    ));
    let chosen = inbox
        .choose(&escalation.item.id, &OptionId::new("accept"), None, &ctx)
        .await
        .expect("desktop choose");
    assert_eq!(
        chosen.item.status,
        gantry_core::entities::InboxStatus::Resolved
    );
    assert_eq!(
        inbox.chosen_option(&escalation.item.id),
        Some(("accept".into(), None))
    );

    let unknown = RaiseRequest {
        dedup_key: DedupKey::new("decision:unknown"),
        ..raise_request("decision:unknown", Some(project_id))
    };
    let RaiseOutcome::Created(unknown) = inbox
        .raise(None, unknown, &ctx)
        .await
        .expect("raise unknown")
    else {
        panic!("unknown should be created");
    };
    assert!(matches!(
        inbox
            .choose(&unknown.item.id, &OptionId::new("nope"), None, &ctx)
            .await,
        Err(gantry_core::ports::ChooseError::OptionUnknown)
    ));

    let approval_request = RaiseRequest {
        kind: InboxKind::ApprovalNeeded,
        payload: InboxPayload::ApprovalNeeded {
            approval_id: id::<Apr>(60),
        },
        dedup_key: DedupKey::new("approval:test"),
        ..raise_request("approval:test", Some(project_id))
    };
    let RaiseOutcome::Created(approval) = inbox
        .raise(None, approval_request, &ctx)
        .await
        .expect("raise approval")
    else {
        panic!("approval should be created");
    };
    assert!(matches!(
        inbox
            .choose(&approval.item.id, &OptionId::new("approve"), None, &ctx)
            .await,
        Err(gantry_core::ports::ChooseError::UseApprovalRoute)
    ));

    let tx_request = raise_request("decision:tx", Some(project_id));
    let mut tx = inbox.begin_write();
    let RaiseOutcome::Created(tx_item) = inbox
        .raise(Some(&mut tx as &mut dyn TxHandle), tx_request.clone(), &ctx)
        .await
        .expect("raise in tx")
    else {
        panic!("tx item should be created");
    };
    tx.rollback().await.expect("rollback inbox tx");
    assert!(inbox.get(&tx_item.item.id).await.is_err());

    let mut tx = inbox.begin_write();
    let RaiseOutcome::Created(tx_item) = inbox
        .raise(Some(&mut tx as &mut dyn TxHandle), tx_request, &ctx)
        .await
        .expect("raise in tx")
    else {
        panic!("tx item should be created");
    };
    tx.commit().await.expect("commit inbox tx");
    assert_eq!(
        inbox
            .get(&tx_item.item.id)
            .await
            .expect("committed item")
            .item
            .id,
        tx_item.item.id
    );

    let device = id::<Dev>(70);
    let mobile_device = ActorCtx {
        device: Some(device),
        ..crate::ctx()
    };
    let token = PushToken::new("private-token");
    inbox
        .register_push_token(&mobile_device, token, ApnsEnvironment::Sandbox)
        .await
        .expect("register token");
    let record = inbox.push_token_record(&device).expect("token record");
    assert_eq!(record.0.expose_secret(), "private-token");
    assert_eq!(record.1, ApnsEnvironment::Sandbox);
    assert!(!format!("{inbox:?}").contains("private-token"));
    let item: InboxItem = inbox.get(&tx_item.item.id).await.expect("item").item;
    assert!(!format!("{item:?}").contains("private-token"));
}

#[test]
fn secrets_versions_isolation_and_debug() {
    let store = MemorySecretStore::new(ts());
    store.allow(Consumer::Forge);
    store.allow(Consumer::Pki);
    let sink = store.scoped(Consumer::Forge).expect("forge scoped");
    let name = SecretName::parse("forge.key").expect("secret name");
    let ctx = ctx();
    secrets_conformance(&sink, &name, &ctx).expect("secrets conformance");

    let secret = sink.get(&name).expect("get").expect("secret");
    assert_eq!(secret.version.get(), 3);
    assert!(!format!("{secret:?}").contains("three"));
    assert!(!format!("{sink:?}").contains("three"));

    let pki = store.reader(Consumer::Pki).expect("pki reader");
    assert!(pki.get(&name).expect("pki get").is_none());
    assert!(matches!(
        store.reader(Consumer::Egress),
        Err(SecretsError::Denied { .. })
    ));
    assert!(matches!(
        sink.put(
            &name,
            SecretValue::from_bytes(b"four".to_vec()),
            Expect::Version(SecretVersion::new(1).expect("version")),
            &ctx,
        ),
        Err(SecretsError::Conflict { .. })
    ));
}

#[tokio::test]
async fn same_fake_tx_is_idempotent_and_rolls_back() {
    let ctx = ctx();

    let forge = FakeForge::new(ts());
    forge.insert_project(project());
    let request = add_labels_request(0);
    let mut tx = forge.begin_write();
    let first = forge
        .enqueue_in(&mut tx, request.clone(), &ctx)
        .await
        .expect("first enqueue");
    let second = forge
        .enqueue_in(&mut tx, request, &ctx)
        .await
        .expect("second enqueue");
    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.action.id, second.action.id);
    tx.rollback().await.expect("rollback forge tx");
    assert_eq!(forge.outbox_len(), 0);

    let inbox = FakeInbox::new(ts());
    let request = raise_request("same-tx", Some(id::<Prj>(1)));
    let mut tx = inbox.begin_write();
    let RaiseOutcome::Created(first) = inbox
        .raise(Some(&mut tx), request.clone(), &ctx)
        .await
        .expect("first raise")
    else {
        panic!("first raise must create");
    };
    let RaiseOutcome::Deduped(second) = inbox
        .raise(Some(&mut tx), request, &ctx)
        .await
        .expect("second raise")
    else {
        panic!("second raise must deduplicate");
    };
    assert_eq!(first.item.id, second.item.id);
    tx.rollback().await.expect("rollback inbox tx");
    assert!(inbox.get(&first.item.id).await.is_err());

    let policy = FakePolicy::new(ts());
    let attempt = id::<Att>(51);
    let patch = sha("patch-same-tx");
    let approval = policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::PatchApplyProtected,
                target: ApprovalTarget::Patch {
                    repo: slug(),
                    attempt_id: attempt,
                    rules: vec!["protected".into()],
                    paths: vec!["src/lib.rs".into()],
                },
                subject_digest: SubjectDigest::sha256(patch),
                delivery_id: Some(id::<Dlv>(2)),
                ttl: Duration::from_secs(60),
            },
            ctx.clone(),
        )
        .await
        .expect("approval request");
    policy.grant(&approval.id).expect("grant");
    let check = PatchApprovalCheck {
        repo: slug(),
        attempt_id: attempt,
        patch_sha256: patch,
        rules: vec![RuleId::new("protected")],
        paths: vec![RepoPath::parse("src/lib.rs").expect("path")],
    };
    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(&mut tx, &approval.id, &check, &ctx)
            .await
            .expect("first consume"),
        PatchPermit::Consumed
    );
    assert_eq!(
        policy
            .consume_patch_approval(&mut tx, &approval.id, &check, &ctx)
            .await
            .expect("second consume"),
        PatchPermit::AlreadyConsumedBySameAttempt
    );
    tx.rollback().await.expect("rollback policy tx");
    assert_eq!(
        policy.approval(&approval.id).expect("approval").status,
        ApprovalStatus::Granted
    );

    let mut tx = policy.begin_write();
    assert_eq!(
        policy
            .consume_patch_approval(&mut tx, &approval.id, &check, &ctx)
            .await
            .expect("commit consume"),
        PatchPermit::Consumed
    );
    tx.commit().await.expect("commit policy tx");
    assert_eq!(
        policy.approval(&approval.id).expect("approval").status,
        ApprovalStatus::Consumed
    );
}

#[test]
fn fakes_compile_against_their_ports() {
    fn assert_outbox<T: OutboxPort>() {}
    fn assert_reader<T: ForgeReader>() {}
    fn assert_refresher<T: PrRefresher>() {}
    fn assert_repo<T: RepoService>() {}
    fn assert_locks<T: WorktreeLocks>() {}
    fn assert_policy<T: PolicyPort>() {}
    fn assert_path_rules<T: PathRuleSet>() {}
    fn assert_inbox<T: InboxPort>() {}
    fn assert_secret_source<T: SecretSource>() {}
    fn assert_secret_sink<T: SecretSink>() {}
    fn assert_egress<T: EgressControl>() {}
    fn assert_delivery<T: DeliveryControl>() {}
    fn assert_heal<T: HealPort>() {}
    fn assert_classify<T: ClassifyPort>() {}
    fn assert_conversation<T: ConversationService>() {}
    fn assert_models<T: ModelProvider>() {}
    fn assert_sandbox<T: SandboxService>() {}
    fn assert_worker<T: WorkerAdapter>() {}

    assert_outbox::<FakeForge>();
    assert_reader::<FakeForge>();
    assert_refresher::<FakeForge>();
    assert_repo::<FakeRepo>();
    assert_locks::<FakeRepo>();
    assert_policy::<FakePolicy>();
    assert_path_rules::<FakePathRules>();
    assert_inbox::<FakeInbox>();
    assert_secret_source::<MemorySecrets>();
    assert_secret_sink::<MemorySecrets>();
    assert_egress::<EgressStub>();
    assert_delivery::<FakeEngine>();
    assert_heal::<FakeEngine>();
    assert_classify::<FakeEngine>();
    assert_conversation::<FakeConversation>();
    assert_models::<FakeModelProvider>();
    assert_sandbox::<FakeSandboxService>();
    assert_worker::<StubWorkerAdapter>();

    // AddedLineSource 由调用方（repo）实现，没有独立替身；core/tests/ports.rs 断言其 object-safe。
}

struct ForeignTx;

impl TxHandle for ForeignTx {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[tokio::test]
async fn unknown_tx_is_rejected_by_write_ports() {
    let ctx = ctx();
    let mut foreign = ForeignTx;

    let forge = FakeForge::new(ts());
    forge.insert_project(project());
    let error = forge
        .enqueue_in(&mut foreign, add_labels_request(0), &ctx)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EnqueueError::Store(StoreError::Rejected { .. })
    ));

    let inbox = FakeInbox::new(ts());
    let error = inbox
        .raise(
            Some(&mut foreign),
            raise_request("foreign", Some(id::<Prj>(1))),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RaiseError::Store(StoreError::Rejected { .. })
    ));

    let policy = FakePolicy::new(ts());
    let attempt = id::<Att>(50);
    let patch = sha("patch");
    let approval = policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::PatchApplyProtected,
                target: ApprovalTarget::Patch {
                    repo: slug(),
                    attempt_id: attempt,
                    rules: vec!["protected".into()],
                    paths: vec!["src/lib.rs".into()],
                },
                subject_digest: SubjectDigest::sha256(patch),
                delivery_id: Some(id::<Dlv>(2)),
                ttl: Duration::from_secs(60),
            },
            ctx.clone(),
        )
        .await
        .expect("approval request");
    policy.grant(&approval.id).expect("grant");
    let check = PatchApprovalCheck {
        repo: slug(),
        attempt_id: attempt,
        patch_sha256: patch,
        rules: vec![RuleId::new("protected")],
        paths: vec![RepoPath::parse("src/lib.rs").expect("path")],
    };
    let error = policy
        .consume_patch_approval(&mut foreign, &approval.id, &check, &ctx)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        GateError::Store(StoreError::Rejected { .. })
    ));
}
