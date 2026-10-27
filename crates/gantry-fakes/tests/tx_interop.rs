//! 三个状态替身与 Store 共享事务的回归。
//!
//! 覆盖：同一事务追加事件 + 入队 + raise + 消费审批的提交/回滚、同事务去重、
//! Store/FakeForge Outbox 同一行、跨 Store 拒绝、直接写与旧事务冲突、执行中
//! 取消、force/前缀外 push 的入队守卫。

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, RepoSlug, Sha256Digest, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, ApprovalStatus, ApprovalTarget, DecisionKind, InboxKind,
    InboxOption, InboxPayload, InboxStatus, MergeMethod, Priority, Project, ReasonCode, Seq,
    Severity,
};
use gantry_core::events::{
    EventType, InboxDismissedData, InboxRaisedData, InboxResolution, InboxResolvedData, NewEvent,
    Subject,
};
use gantry_core::forge::{
    ActionPayload, AddLabelsPayload, MergeMode, MergePrPayload, PushBranchPayload, PushTransport,
};
use gantry_core::ids::{Apr, Att, Dlv, Evt, Id, IdGen, IdKind, Msg, Prj, Ulid, Wtr};
use gantry_core::ports::{
    ActorCtx, ApprovalSpec, CancelOutcome, CancelReason, ChooseError, DedupKey, EnqueueError,
    EnqueueRequest, EnqueuedBy, EnqueuedByModule, ExecutionCheck, ExecutionPermit, ForgeReader,
    GateError, InboxError, InboxPort, Note, OptionId, OutboxPort, PatchApprovalCheck, PatchPermit,
    PolicyPort, RaiseError, RaiseOutcome, RaiseRequest, Resolution, ResolveTarget, RuleId,
    SafeSummary, StoreError as PortStoreError, TxHandle,
};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_core::worker::RepoPath;
use gantry_fakes::forge::FakeForge;
use gantry_fakes::inbox::FakeInbox;
use gantry_fakes::policy::FakePolicy;
use gantry_store::{AfterFailure, ClaimOpts, EventFilter, OutboxInsert, Store, StoreDeps};
use serde_json::json;

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

fn empty_store() -> Store {
    let clock = Arc::new(FakeClock::new(MS as i64));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(31))));
    Store::open_in_memory_sync(StoreDeps { clock, ids }).expect("in-memory store")
}

fn shared() -> (Store, FakeForge, FakePolicy, FakeInbox) {
    let store = empty_store();
    let forge = FakeForge::new_in(store.clone());
    let policy = FakePolicy::new_in(store.clone());
    let inbox = FakeInbox::new_in(store.clone());
    forge.insert_project(project());
    (store, forge, policy, inbox)
}

fn budget_event(dedup: &str) -> NewEvent {
    NewEvent {
        ty: EventType::BudgetExhausted,
        subject: Subject::parse("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB").unwrap(),
        project_id: Some(id::<Prj>(1)),
        actor: actor(),
        causation_id: None,
        correlation_id: None,
        data: json!({"dimension": "nodes", "limit": 10, "used": 10}),
        dedup_key: Some(dedup.to_owned()),
    }
}

fn add_labels_request(epoch: u32) -> EnqueueRequest {
    EnqueueRequest {
        project_id: id::<Prj>(1),
        delivery_id: id::<Dlv>(2),
        payload: ActionPayload::AddLabels(AddLabelsPayload {
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

fn merge_request() -> EnqueueRequest {
    EnqueueRequest {
        project_id: id::<Prj>(1),
        delivery_id: id::<Dlv>(2),
        payload: ActionPayload::MergePr(MergePrPayload {
            repo: slug(),
            pull_number: 7,
            expected_head_sha: oid("head"),
            merge_method: MergeMethod::Merge,
            mode: MergeMode::Auto,
            commit_title: None,
            commit_message: None,
        }),
        requires_approval: None,
        depends_on: vec![],
        priority: Priority::Normal,
        not_before: None,
        epoch: 0,
        enqueued_by: EnqueuedBy {
            module: EnqueuedByModule::Engine,
            causation_id: None,
        },
    }
}

fn push_request(
    force: bool,
    branch: &str,
    approval: Option<gantry_core::ids::ApprovalId>,
) -> EnqueueRequest {
    EnqueueRequest {
        project_id: id::<Prj>(1),
        delivery_id: id::<Dlv>(2),
        payload: ActionPayload::PushBranch(PushBranchPayload {
            repo: slug(),
            branch: branch.into(),
            base_ref: "main".into(),
            worktree_id: id::<Wtr>(3),
            local_head_oid: oid("local"),
            expected_tree_oid: oid("tree"),
            expected_remote_oid: None,
            validation_ref: sha("patch"),
            transport: PushTransport::Git,
            force,
        }),
        requires_approval: approval,
        depends_on: vec![],
        priority: Priority::Normal,
        not_before: None,
        epoch: 0,
        enqueued_by: EnqueuedBy {
            module: EnqueuedByModule::Engine,
            causation_id: None,
        },
    }
}

fn raise_request(dedup: &str) -> RaiseRequest {
    RaiseRequest {
        project_id: Some(id::<Prj>(1)),
        delivery_id: Some(id::<Dlv>(2)),
        kind: InboxKind::DecisionNeeded,
        severity: Severity::Warn,
        summary: SafeSummary::new("needs a decision"),
        payload: InboxPayload::DecisionNeeded {
            kind: DecisionKind::ManualAction,
            reason_code: ReasonCode::parse("manual_action").expect("reason code"),
            options: vec![InboxOption::new("approve", "Approve", true)],
            node_id: None,
            patch: None,
        },
        dedup_key: DedupKey::new(dedup),
        supersedes: vec![],
        actor: actor(),
        causation_id: None,
    }
}

fn operator_ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Device, "operator"),
        causation_id: Some(id::<Evt>(88)),
        ..ctx()
    }
}

fn approval_raise_request(dedup: &str, approval: Id<Apr>) -> RaiseRequest {
    RaiseRequest {
        kind: InboxKind::ApprovalNeeded,
        payload: InboxPayload::ApprovalNeeded {
            approval_id: approval,
        },
        ..raise_request(dedup)
    }
}

async fn all_events(store: &Store) -> Vec<gantry_core::events::Event> {
    store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 200)
        .await
        .unwrap()
        .events
}

fn patch_check() -> PatchApprovalCheck {
    PatchApprovalCheck {
        repo: slug(),
        attempt_id: id::<Att>(50),
        patch_sha256: sha("patch"),
        rules: vec![RuleId::new("protected")],
        paths: vec![RepoPath::parse("src/lib.rs").expect("path")],
    }
}

async fn patch_approval(policy: &FakePolicy) -> gantry_core::entities::Approval {
    let approval = policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::PatchApplyProtected,
                target: ApprovalTarget::Patch {
                    repo: slug(),
                    attempt_id: id::<Att>(50),
                    rules: vec!["protected".into()],
                    paths: vec!["src/lib.rs".into()],
                },
                subject_digest: SubjectDigest::sha256(sha("patch")),
                delivery_id: Some(id::<Dlv>(2)),
                ttl: Duration::from_secs(60),
            },
            ctx(),
        )
        .await
        .expect("approval request");
    policy.grant(&approval.id).expect("grant");
    approval
}

#[tokio::test]
async fn shared_tx_commit_is_visible_across_store_forge_policy_and_inbox() {
    let (store, forge, policy, inbox) = shared();
    let approval = patch_approval(&policy).await;
    let check = patch_check();

    let mut tx = store.begin_write().await.unwrap();
    tx.append(budget_event("evt-1")).await.unwrap();
    let enqueued = forge
        .enqueue_in(&mut tx, add_labels_request(0), &ctx())
        .await
        .expect("enqueue in shared tx");
    let RaiseOutcome::Created(item) = inbox
        .raise(
            Some(&mut tx as &mut dyn TxHandle),
            raise_request("tx:commit"),
            &ctx(),
        )
        .await
        .expect("raise in shared tx")
    else {
        panic!("inbox item must be created");
    };
    assert_eq!(
        policy
            .consume_patch_approval(&mut tx, &approval.id, &check, &ctx())
            .await
            .expect("consume patch approval"),
        PatchPermit::Consumed
    );
    tx.commit().await.expect("commit shared tx");

    // 入队同事务追加 `outbox.enqueued`（store.md §6.3），因此比其余三条多一条。
    assert_eq!(store.events().head().await.unwrap().as_u64(), 5);
    assert_eq!(forge.outbox_len(), 1);
    assert_eq!(
        forge.outbox_action(&enqueued.action.id).unwrap().id,
        enqueued.action.id
    );
    assert_eq!(
        inbox.get(&item.item.id).await.unwrap().item.id,
        item.item.id
    );
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Consumed
    );
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert!(page
        .events
        .iter()
        .any(|event| event.ty == EventType::BudgetExhausted));
    assert!(page
        .events
        .iter()
        .any(|event| event.ty == EventType::ApprovalRequested));
    assert!(page
        .events
        .iter()
        .any(|event| event.ty == EventType::InboxRaised));
    assert!(page
        .events
        .iter()
        .any(|event| event.ty == EventType::ApprovalConsumed));
}

#[tokio::test]
async fn shared_tx_rollback_leaves_no_trace() {
    let (store, forge, policy, inbox) = shared();
    let approval = patch_approval(&policy).await;
    let check = patch_check();
    let baseline_head = store.events().head().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    tx.append(budget_event("evt-1")).await.unwrap();
    let enqueued = forge
        .enqueue_in(&mut tx, add_labels_request(0), &ctx())
        .await
        .expect("enqueue in shared tx");
    let RaiseOutcome::Created(item) = inbox
        .raise(
            Some(&mut tx as &mut dyn TxHandle),
            raise_request("tx:rollback"),
            &ctx(),
        )
        .await
        .expect("raise in shared tx")
    else {
        panic!("inbox item must be created");
    };
    assert_eq!(
        policy
            .consume_patch_approval(&mut tx, &approval.id, &check, &ctx())
            .await
            .expect("consume patch approval"),
        PatchPermit::Consumed
    );
    tx.rollback().await.expect("rollback shared tx");

    assert_eq!(store.events().head().await.unwrap(), baseline_head);
    assert_eq!(forge.outbox_len(), 0);
    assert!(forge.outbox_action(&enqueued.action.id).is_none());
    assert!(matches!(
        inbox.get(&item.item.id).await,
        Err(InboxError::NotFound)
    ));
    assert_eq!(
        policy.approval(&approval.id).unwrap().status,
        ApprovalStatus::Granted
    );
}

#[tokio::test]
async fn same_tx_deduplicates_events_enqueues_and_inbox_items() {
    let (store, forge, _policy, inbox) = shared();
    let mut tx = store.begin_write().await.unwrap();
    let first = forge
        .enqueue_in(&mut tx, add_labels_request(0), &ctx())
        .await
        .unwrap();
    let second = forge
        .enqueue_in(&mut tx, add_labels_request(0), &ctx())
        .await
        .unwrap();
    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.action.id, second.action.id);

    let RaiseOutcome::Created(raised) = inbox
        .raise(
            Some(&mut tx as &mut dyn TxHandle),
            raise_request("tx:dedup"),
            &ctx(),
        )
        .await
        .unwrap()
    else {
        panic!("first raise must create");
    };
    let RaiseOutcome::Deduped(deduped) = inbox
        .raise(
            Some(&mut tx as &mut dyn TxHandle),
            raise_request("tx:dedup"),
            &ctx(),
        )
        .await
        .unwrap()
    else {
        panic!("second raise must dedup");
    };
    assert_eq!(raised.item.id, deduped.item.id);

    let first_event = tx.append(budget_event("same-dedup")).await.unwrap();
    let second_event = tx.append(budget_event("same-dedup")).await.unwrap();
    assert!(matches!(first_event, gantry_store::Appended::New(_)));
    assert!(matches!(
        second_event,
        gantry_store::Appended::Duplicate { .. }
    ));
    tx.commit().await.unwrap();
    assert_eq!(forge.outbox_len(), 1);
    // 一条 `outbox.enqueued`（第二次入队去重，不追加）+ inbox.raised + budget 事件。
    assert_eq!(store.events().head().await.unwrap().as_u64(), 3);
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(
        page.events
            .iter()
            .filter(|event| event.ty == EventType::InboxRaised)
            .count(),
        1
    );
}

#[tokio::test]
async fn inbox_raise_appends_event_with_request_envelope_and_real_seq() {
    let (store, _forge, _policy, inbox) = shared();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(budget_event("existing-before-raise"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut request = raise_request("tx:event");
    request.causation_id = Some(id::<Evt>(77));
    let expected_project = request.project_id;
    let expected_delivery = request.delivery_id;
    let expected_actor = request.actor.clone();
    let RaiseOutcome::Created(created) = inbox.raise(None, request, &ctx()).await.unwrap() else {
        panic!("first raise must create");
    };
    assert_eq!(created.last_seq.as_u64(), 2);

    let RaiseOutcome::Deduped(deduped) = inbox
        .raise(None, raise_request("tx:event"), &ctx())
        .await
        .unwrap()
    else {
        panic!("second raise must dedup");
    };
    assert_eq!(deduped.last_seq, created.last_seq);

    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    let event = &page.events[1];
    assert_eq!(event.ty, EventType::InboxRaised);
    assert_eq!(event.seq, created.last_seq);
    assert_eq!(event.subject.as_str(), created.item.id.to_string());
    assert_eq!(event.project_id, expected_project);
    assert_eq!(event.actor, expected_actor);
    assert_eq!(event.causation_id, Some(id::<Evt>(77)));
    assert_eq!(
        event.correlation_id,
        expected_delivery.map(|id| id.to_string())
    );
    let data: InboxRaisedData = serde_json::from_value(event.data.clone()).unwrap();
    assert_eq!(data.item, created.item);
    assert!(data.supersedes.is_empty());
}

#[tokio::test]
async fn inbox_supersedes_records_resolved_ids_in_the_raise_event() {
    let (store, _forge, _policy, inbox) = shared();
    let ctx = ctx();
    let RaiseOutcome::Created(old) = inbox
        .raise(None, raise_request("supersede:old"), &ctx)
        .await
        .unwrap()
    else {
        panic!("old item must be created");
    };

    let mut request = raise_request("supersede:new");
    request.supersedes = vec![
        DedupKey::new("supersede:old"),
        DedupKey::new("supersede:old"),
    ];
    let RaiseOutcome::Created(replacement) = inbox.raise(None, request, &ctx).await.unwrap() else {
        panic!("replacement item must be created");
    };

    assert_eq!(
        inbox.get(&old.item.id).await.unwrap().item.status,
        InboxStatus::Resolved
    );
    assert_eq!(
        inbox.resolution(&old.item.id),
        Some(Resolution::Superseded {
            by: replacement.item.id
        })
    );

    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    let event = &page.events[1];
    assert_eq!(event.ty, EventType::InboxRaised);
    assert_eq!(event.seq, replacement.last_seq);
    let data: InboxRaisedData = serde_json::from_value(event.data.clone()).unwrap();
    assert_eq!(data.item, replacement.item);
    assert_eq!(data.supersedes, vec![old.item.id]);
}

#[tokio::test]
async fn cross_store_tx_is_rejected_by_all_fakes() {
    let (_store_a, forge, policy, inbox) = shared();
    let approval = patch_approval(&policy).await;
    let check = patch_check();
    let store_b = empty_store();
    let mut tx_b = store_b.begin_write().await.unwrap();

    let error = forge
        .enqueue_in(&mut tx_b, add_labels_request(0), &ctx())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EnqueueError::Store(PortStoreError::Rejected { .. })
    ));

    let error = inbox
        .raise(Some(&mut tx_b), raise_request("foreign"), &ctx())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RaiseError::Store(PortStoreError::Rejected { .. })
    ));

    let error = policy
        .consume_patch_approval(&mut tx_b, &approval.id, &check, &ctx())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        GateError::Store(PortStoreError::Rejected { .. })
    ));
}

#[tokio::test]
async fn repeated_new_in_keeps_the_registered_script_state() {
    let store = empty_store();
    let forge = FakeForge::new_in(store.clone());
    forge.insert_project(project());
    let policy = FakePolicy::new_in(store.clone());
    let approval = policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::ForcePush,
                target: ApprovalTarget::Branch {
                    repo: slug(),
                    branch: "gantry/1-fix".into(),
                    expected_remote_oid: None,
                },
                subject_digest: SubjectDigest::git(oid("local")),
                delivery_id: None,
                ttl: Duration::from_secs(60),
            },
            ctx(),
        )
        .await
        .unwrap();

    // 重复 `new_in` 是幂等注册：不能把已写入的项目与审批重置为初始状态。
    let forge_again = FakeForge::new_in(store.clone());
    let policy_again = FakePolicy::new_in(store.clone());
    assert!(forge_again.project_snapshot(&id::<Prj>(1)).await.is_ok());
    assert_eq!(
        policy_again.approval(&approval.id).unwrap().status,
        ApprovalStatus::Pending
    );
}

#[tokio::test]
async fn store_and_fake_forge_share_the_same_outbox_row() {
    let (store, forge, _policy, _inbox) = shared();
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = forge
        .enqueue_in(&mut tx, add_labels_request(0), &ctx())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let snapshot = store.snapshot_sync().unwrap();
    let stored = snapshot.outbox_action(&enqueued.action.id).unwrap();
    let seen = forge.outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(stored, seen);
    assert_eq!(snapshot.outbox_list(), forge.outbox_list());
}

#[tokio::test]
async fn direct_write_after_begin_conflicts_with_the_open_tx() {
    let (store, forge, _policy, _inbox) = shared();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(budget_event("evt-1")).await.unwrap();
    // 直接写：脚本注入不经过 WriteGate。
    forge.insert_project(project());
    let error = tx.commit().await.unwrap_err();
    assert!(matches!(
        error,
        gantry_store::StoreError::PreconditionFailed(_)
    ));
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
    assert_eq!(
        forge
            .project_snapshot(&id::<Prj>(1))
            .await
            .unwrap()
            .project
            .id,
        id::<Prj>(1)
    );
}

#[tokio::test]
async fn executing_action_cannot_be_cancelled_through_the_port() {
    let (store, forge, _policy, _inbox) = shared();
    let enqueued = forge.enqueue(add_labels_request(0), &ctx()).await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();

    let outcome = forge
        .cancel(&enqueued.action.id, CancelReason::Superseded, &ctx())
        .await
        .unwrap();
    assert_eq!(outcome, CancelOutcome::NotCancellable);
    assert_eq!(
        forge.outbox_action(&enqueued.action.id).unwrap().status,
        gantry_core::entities::OutboxStatus::Executing
    );
}

#[tokio::test]
async fn force_and_off_prefix_pushes_require_approval() {
    let (_store, forge, _policy, _inbox) = shared();
    let error = forge
        .enqueue(push_request(true, "gantry/1-fix", None), &ctx())
        .await
        .unwrap_err();
    assert!(matches!(error, EnqueueError::ApprovalRequired { .. }));

    let error = forge
        .enqueue(push_request(false, "feature/fix", None), &ctx())
        .await
        .unwrap_err();
    assert!(matches!(error, EnqueueError::ApprovalRequired { .. }));

    let approved = forge
        .enqueue(
            push_request(true, "gantry/1-fix", Some(id::<Apr>(90))),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(approved.created);

    let error = forge.enqueue(merge_request(), &ctx()).await.unwrap_err();
    assert!(matches!(error, EnqueueError::ApprovalRequired { .. }));
}

#[tokio::test]
async fn outbox_insert_is_rejected_for_an_unknown_project() {
    let (_store, forge, _policy, _inbox) = shared();
    let mut request = add_labels_request(0);
    request.project_id = id::<Prj>(99);
    let error = forge.enqueue(request, &ctx()).await.unwrap_err();
    assert!(matches!(error, EnqueueError::ProjectUnknown));
}

#[tokio::test]
async fn terminal_fail_through_store_rejects_queued_rows() {
    let (store, _forge, _policy, _inbox) = shared();
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx
        .outbox()
        .enqueue(OutboxInsert {
            project_id: id::<Prj>(1),
            delivery_id: id::<Dlv>(2),
            kind: gantry_core::entities::ForgeActionKind::CreateComment,
            target: "comment:pr:7:comment:1".into(),
            intent: json!({"body": "hi"}),
            payload: json!({
                "repo": "acme/widgets",
                "target": {"type": "pr", "number": 7},
                "client_ref": "comment:1",
                "body": "hi"
            }),
            requires_approval: None,
            depends_on: vec![],
            priority: Priority::Normal,
            not_before: None,
            epoch: 0,
            coalesce_key: None,
            enqueued_by: gantry_store::EnqueuedBy {
                module: "guardian".into(),
                causation_id: None,
            },
        })
        .await
        .unwrap();
    let error = tx
        .outbox()
        .fail(
            &enqueued.action.id,
            gantry_core::entities::OutboxError {
                code: "permission_denied".into(),
                message: "no access".into(),
            },
            AfterFailure::Terminal,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        gantry_store::StoreError::PreconditionFailed(_)
    ));
}

// ---------------------------------------------------------------------------
// 异步端口竞争写令牌：单线程运行时里不得同步阻塞
// ---------------------------------------------------------------------------

/// 持有者先拿到写令牌，再让 `call` 的端口入口竞争；入口必须在持有者让出后完成。
///
/// 等待者先发 `started` 信号再调用端口入口。旧实现用 `begin_write_sync`
/// 阻塞 runtime 线程，单线程运行时下持有者再也无法推进；外部看门狗会杀掉子进程。
async fn contended<F, T>(store: &Store, call: impl FnOnce() -> F + Send + 'static) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let holder_store = store.clone();
    let holder = tokio::spawn(async move {
        let tx = holder_store.begin_write().await.unwrap();
        held_tx.send(()).unwrap();
        release_rx.await.unwrap();
        drop(tx);
    });
    held_rx.await.unwrap();

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        started_tx.send(()).unwrap();
        let result = call().await;
        done_tx.send(()).unwrap();
        result
    });
    started_rx.await.unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut done_rx)
            .await
            .is_err(),
        "fake async port must not block the runtime thread while the write gate is held"
    );

    release_tx.send(()).unwrap();
    holder.await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut done_rx)
        .await
        .expect("fake async port must finish after the write gate is released")
        .expect("port call must signal completion");
    waiter.await.unwrap()
}

async fn contention_rounds() {
    let store = empty_store();
    let forge = FakeForge::new_in(store.clone());
    let policy = FakePolicy::new_in(store.clone());
    let inbox = FakeInbox::new_in(store.clone());
    forge.insert_project(project());

    let for_waiter = forge.clone();
    let enqueued = contended(&store, move || async move {
        for_waiter.enqueue(add_labels_request(0), &ctx()).await
    })
    .await
    .expect("enqueue must win the write gate after release");
    assert!(enqueued.created);

    let cancelled = forge.enqueue(add_labels_request(1), &ctx()).await.unwrap();
    let for_waiter = forge.clone();
    let outcome = contended(&store, move || async move {
        for_waiter
            .cancel(&cancelled.action.id, CancelReason::Superseded, &ctx())
            .await
    })
    .await
    .expect("cancel must win the write gate after release");
    assert_eq!(outcome, CancelOutcome::Cancelled);

    let for_waiter = inbox.clone();
    let raised = contended(&store, move || async move {
        for_waiter
            .raise(None, raise_request("contention:raise"), &ctx())
            .await
    })
    .await
    .expect("raise must win the write gate after release");
    let RaiseOutcome::Created(raised) = raised else {
        panic!("contention raise must create an item");
    };

    let for_waiter = inbox.clone();
    let written = contended(&store, move || async move {
        for_waiter
            .resolve(
                None,
                ResolveTarget::Id(raised.item.id),
                Resolution::System {
                    reason: "contention".into(),
                },
                &ctx(),
            )
            .await
    })
    .await
    .expect("resolve must win the write gate after release");
    assert_eq!(written.len(), 1);

    let for_waiter = policy.clone();
    let approval = contended(&store, move || async move {
        for_waiter
            .request_approval(
                ApprovalSpec {
                    action: ApprovalAction::ForcePush,
                    target: ApprovalTarget::Branch {
                        repo: slug(),
                        branch: "gantry/contention".into(),
                        expected_remote_oid: None,
                    },
                    subject_digest: SubjectDigest::git(oid("local")),
                    delivery_id: None,
                    ttl: Duration::from_secs(60),
                },
                ctx(),
            )
            .await
    })
    .await
    .expect("request_approval must win the write gate after release");
    assert_eq!(approval.status, ApprovalStatus::Pending);
    policy.grant(&approval.id).expect("grant");

    let for_waiter = policy.clone();
    let permit = contended(&store, move || async move {
        for_waiter
            .consume_for_execution(
                &approval.id,
                &ExecutionCheck::PushBranch {
                    repo: slug(),
                    branch: "gantry/contention".into(),
                    force: true,
                    local_head_oid: oid("local"),
                    expected_remote_oid: None,
                    validation_ref: Some(sha("patch")),
                },
                &id::<gantry_core::ids::Act>(91),
                &ctx(),
            )
            .await
    })
    .await
    .expect("consume_for_execution must win the write gate after release");
    assert_eq!(permit, ExecutionPermit::Consumed);
}

/// 子进程入口：只在父测试的看门狗下运行，避免旧实现的同步阻塞挂死测试进程。
#[test]
#[ignore = "child process: driven by fakes_async_contention_completes watchdog"]
fn fakes_async_contention_child() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(contention_rounds());
}

/// 外部进程看门狗：单线程运行时里 fake 端口与持有者竞争写令牌必须有界完成。
#[test]
fn fakes_async_contention_completes() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fakes_async_contention_child", "--ignored"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stdout = String::new();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                pipe.read_to_string(&mut stdout).unwrap();
            }
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_string(&mut stderr).unwrap();
            }
            assert!(
                status.success(),
                "fake contention child failed: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert!(
                stdout.contains("1 passed"),
                "fake contention child did not run the regression test:\n{stdout}"
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "fake contention child did not finish within {CHILD_TIMEOUT:?}; \
                 fake async ports must not block the runtime thread"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// 收件箱写路径：每个实际状态变化追加一条事件，`last_seq` 来自事件本身
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbox_resolve_appends_one_event_per_item_with_real_seqs() {
    let (store, _forge, _policy, inbox) = shared();
    let ctx = operator_ctx();
    let approval = id::<Apr>(61);
    let RaiseOutcome::Created(first) = inbox
        .raise(
            None,
            approval_raise_request("resolve:first", approval),
            &ctx,
        )
        .await
        .unwrap()
    else {
        panic!("first item must be created");
    };
    let RaiseOutcome::Created(second) = inbox
        .raise(
            None,
            approval_raise_request("resolve:second", approval),
            &ctx,
        )
        .await
        .unwrap()
    else {
        panic!("second item must be created");
    };
    assert_eq!(first.last_seq.as_u64(), 1);
    assert_eq!(second.last_seq.as_u64(), 2);

    let written = inbox
        .resolve(
            None,
            ResolveTarget::Approval(approval),
            Resolution::ApprovalSettled {
                approval_id: approval,
                status: ApprovalStatus::Granted,
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(written.len(), 2);
    assert_eq!(written[0].item.id, first.item.id);
    assert_eq!(written[1].item.id, second.item.id);
    assert_eq!(written[0].last_seq.as_u64(), 3);
    assert_eq!(written[1].last_seq.as_u64(), 4);

    let head = store.events().head().await.unwrap();
    assert_eq!(head.as_u64(), 4);
    let events = all_events(&store).await;
    assert_eq!(events.len(), 4);
    for (written, event) in written.iter().zip(events[2..].iter()) {
        assert!(written.last_seq <= head);
        assert_eq!(written.item.status, InboxStatus::Resolved);
        assert_eq!(event.ty, EventType::InboxResolved);
        assert_eq!(event.seq, written.last_seq);
        assert_eq!(event.subject.as_str(), written.item.id.to_string());
        assert_eq!(event.project_id, Some(id::<Prj>(1)));
        assert_eq!(event.actor, ctx.actor);
        assert_eq!(event.causation_id, ctx.causation_id);
        let data: InboxResolvedData = serde_json::from_value(event.data.clone()).unwrap();
        assert_eq!(data.inbox_id, written.item.id);
        assert_eq!(data.resolution, InboxResolution::ApprovalSettled);
        assert_eq!(data.approval_id, Some(approval));
    }

    let again = inbox
        .resolve(
            None,
            ResolveTarget::Approval(approval),
            Resolution::ApprovalSettled {
                approval_id: approval,
                status: ApprovalStatus::Granted,
            },
            &ctx,
        )
        .await
        .unwrap();
    assert!(again.is_empty());
    assert_eq!(store.events().head().await.unwrap(), head);

    let RaiseOutcome::Created(next) = inbox
        .raise(None, raise_request("resolve:next"), &ctx)
        .await
        .unwrap()
    else {
        panic!("next item must be created");
    };
    assert_eq!(next.last_seq.as_u64(), 5);
    assert_eq!(store.events().head().await.unwrap(), next.last_seq);
}

#[tokio::test]
async fn inbox_resolve_event_data_maps_every_resolution_variant() {
    let (store, _forge, _policy, inbox) = shared();
    let ctx = operator_ctx();
    let dedups = [
        "variant:answered",
        "variant:approval",
        "variant:closed",
        "variant:abandoned",
        "variant:external",
        "variant:system",
        "variant:superseded",
    ];
    let mut ids = Vec::new();
    for dedup in dedups {
        let RaiseOutcome::Created(item) =
            inbox.raise(None, raise_request(dedup), &ctx).await.unwrap()
        else {
            panic!("{dedup} must be created");
        };
        ids.push(item.item.id);
    }
    let by = ids[0];
    let cases = [
        (
            ids[0],
            Resolution::Answered {
                message_id: id::<Msg>(21),
            },
            InboxResolvedData {
                inbox_id: ids[0],
                resolution: InboxResolution::Answered,
                option_id: None,
                note: None,
                message_id: Some(id::<Msg>(21)),
                approval_id: None,
                reason: None,
            },
        ),
        (
            ids[1],
            Resolution::ApprovalSettled {
                approval_id: id::<Apr>(22),
                status: ApprovalStatus::Denied,
            },
            InboxResolvedData {
                inbox_id: ids[1],
                resolution: InboxResolution::ApprovalSettled,
                option_id: None,
                note: None,
                message_id: None,
                approval_id: Some(id::<Apr>(22)),
                reason: None,
            },
        ),
        (
            ids[2],
            Resolution::DeliveryClosed,
            InboxResolvedData {
                inbox_id: ids[2],
                resolution: InboxResolution::DeliveryClosed,
                option_id: None,
                note: None,
                message_id: None,
                approval_id: None,
                reason: None,
            },
        ),
        (
            ids[3],
            Resolution::DeliveryAbandoned,
            InboxResolvedData {
                inbox_id: ids[3],
                resolution: InboxResolution::DeliveryAbandoned,
                option_id: None,
                note: None,
                message_id: None,
                approval_id: None,
                reason: None,
            },
        ),
        (
            ids[4],
            Resolution::External {
                reason: "outside".into(),
            },
            InboxResolvedData {
                inbox_id: ids[4],
                resolution: InboxResolution::External,
                option_id: None,
                note: None,
                message_id: None,
                approval_id: None,
                reason: Some("outside".into()),
            },
        ),
        (
            ids[5],
            Resolution::System {
                reason: "cleanup".into(),
            },
            InboxResolvedData {
                inbox_id: ids[5],
                resolution: InboxResolution::System,
                option_id: None,
                note: None,
                message_id: None,
                approval_id: None,
                reason: Some("cleanup".into()),
            },
        ),
        (
            ids[6],
            Resolution::Superseded { by },
            InboxResolvedData {
                inbox_id: ids[6],
                resolution: InboxResolution::Superseded,
                option_id: None,
                note: None,
                message_id: None,
                approval_id: None,
                reason: None,
            },
        ),
    ];

    let mut expected_seq = 7;
    for (id, how, expected) in cases {
        let written = inbox
            .resolve(None, ResolveTarget::Id(id), how, &ctx)
            .await
            .unwrap();
        assert_eq!(written.len(), 1);
        expected_seq += 1;
        assert_eq!(written[0].last_seq.as_u64(), expected_seq);
        let event = all_events(&store).await.pop().unwrap();
        assert_eq!(event.ty, EventType::InboxResolved);
        assert_eq!(event.seq, written[0].last_seq);
        let data: InboxResolvedData = serde_json::from_value(event.data).unwrap();
        assert_eq!(data, expected);
    }
}

#[tokio::test]
async fn inbox_dismiss_appends_event_at_the_real_seq_and_is_idempotent() {
    let (store, _forge, _policy, inbox) = shared();
    let ctx = operator_ctx();
    let RaiseOutcome::Created(raised) = inbox
        .raise(None, raise_request("dismiss:one"), &ctx)
        .await
        .unwrap()
    else {
        panic!("item must be created");
    };

    let dismissed = inbox
        .dismiss(&raised.item.id, &ctx, Note::new("seen"))
        .await
        .unwrap();
    assert_eq!(dismissed.item.status, InboxStatus::Dismissed);
    assert_eq!(dismissed.last_seq.as_u64(), 2);
    let head = store.events().head().await.unwrap();
    assert_eq!(head, dismissed.last_seq);
    let event = all_events(&store).await.pop().unwrap();
    assert_eq!(event.ty, EventType::InboxDismissed);
    assert_eq!(event.seq, dismissed.last_seq);
    assert_eq!(event.subject.as_str(), raised.item.id.to_string());
    assert_eq!(event.project_id, Some(id::<Prj>(1)));
    assert_eq!(event.actor, ctx.actor);
    assert_eq!(event.causation_id, ctx.causation_id);
    let data: InboxDismissedData = serde_json::from_value(event.data).unwrap();
    assert_eq!(data.inbox_id, raised.item.id);
    assert_eq!(data.note.as_deref(), Some("seen"));

    let again = inbox.dismiss(&raised.item.id, &ctx, None).await.unwrap();
    assert_eq!(again.last_seq, dismissed.last_seq);
    assert_eq!(store.events().head().await.unwrap(), head);

    let RaiseOutcome::Created(next) = inbox
        .raise(None, raise_request("dismiss:two"), &ctx)
        .await
        .unwrap()
    else {
        panic!("next item must be created");
    };
    assert_eq!(next.last_seq.as_u64(), 3);
    assert!(next.last_seq > dismissed.last_seq);

    let resolved = inbox
        .resolve(
            None,
            ResolveTarget::Id(next.item.id),
            Resolution::System {
                reason: "done".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(resolved[0].last_seq.as_u64(), 4);
    let before = store.events().head().await.unwrap();
    assert!(matches!(
        inbox.dismiss(&next.item.id, &ctx, None).await,
        Err(InboxError::InvalidRequest { .. })
    ));
    assert_eq!(store.events().head().await.unwrap(), before);
}

#[tokio::test]
async fn inbox_choose_appends_resolved_event_and_rejects_non_open() {
    let (store, _forge, _policy, inbox) = shared();
    let ctx = operator_ctx();
    let RaiseOutcome::Created(raised) = inbox
        .raise(None, raise_request("choose:one"), &ctx)
        .await
        .unwrap()
    else {
        panic!("item must be created");
    };

    let chosen = inbox
        .choose(
            &raised.item.id,
            &OptionId::new("approve"),
            Note::new("go"),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(chosen.item.status, InboxStatus::Resolved);
    assert_eq!(chosen.last_seq.as_u64(), 2);
    let head = store.events().head().await.unwrap();
    assert_eq!(head, chosen.last_seq);
    let event = all_events(&store).await.pop().unwrap();
    assert_eq!(event.ty, EventType::InboxResolved);
    assert_eq!(event.seq, chosen.last_seq);
    assert_eq!(event.subject.as_str(), raised.item.id.to_string());
    assert_eq!(event.project_id, Some(id::<Prj>(1)));
    assert_eq!(event.actor, ctx.actor);
    assert_eq!(event.causation_id, ctx.causation_id);
    let data: InboxResolvedData = serde_json::from_value(event.data).unwrap();
    assert_eq!(data.inbox_id, raised.item.id);
    assert_eq!(data.resolution, InboxResolution::Chosen);
    assert_eq!(data.option_id.as_deref(), Some("approve"));
    assert_eq!(data.note.as_deref(), Some("go"));

    assert!(matches!(
        inbox
            .choose(&raised.item.id, &OptionId::new("approve"), None, &ctx)
            .await,
        Err(ChooseError::NotOpen)
    ));
    assert_eq!(store.events().head().await.unwrap(), head);

    let RaiseOutcome::Created(other) = inbox
        .raise(None, raise_request("choose:unknown"), &ctx)
        .await
        .unwrap()
    else {
        panic!("item must be created");
    };
    assert_eq!(other.last_seq.as_u64(), 3);
    assert!(matches!(
        inbox
            .choose(&other.item.id, &OptionId::new("nope"), None, &ctx)
            .await,
        Err(ChooseError::OptionUnknown)
    ));
    assert_eq!(store.events().head().await.unwrap(), other.last_seq);
}

#[tokio::test]
async fn inbox_resolve_commit_and_rollback_follow_the_shared_tx() {
    let (store, _forge, _policy, inbox) = shared();
    let ctx = operator_ctx();
    let RaiseOutcome::Created(raised) = inbox
        .raise(None, raise_request("tx:resolve"), &ctx)
        .await
        .unwrap()
    else {
        panic!("item must be created");
    };
    let head = store.events().head().await.unwrap();
    let how = Resolution::System {
        reason: "tx".into(),
    };

    let mut tx = store.begin_write().await.unwrap();
    let written = inbox
        .resolve(
            Some(&mut tx as &mut dyn TxHandle),
            ResolveTarget::Id(raised.item.id),
            how.clone(),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].last_seq.as_u64(), head.as_u64() + 1);
    assert_eq!(
        inbox.get(&raised.item.id).await.unwrap().item.status,
        InboxStatus::Open
    );
    tx.rollback().await.unwrap();

    let visible = inbox.get(&raised.item.id).await.unwrap();
    assert_eq!(visible.item.status, InboxStatus::Open);
    assert_eq!(visible.last_seq, raised.last_seq);
    assert_eq!(inbox.resolution(&raised.item.id), None);
    assert_eq!(store.events().head().await.unwrap(), head);

    let mut tx = store.begin_write().await.unwrap();
    let written = inbox
        .resolve(
            Some(&mut tx as &mut dyn TxHandle),
            ResolveTarget::Id(raised.item.id),
            how.clone(),
            &ctx,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let visible = inbox.get(&raised.item.id).await.unwrap();
    assert_eq!(visible.item.status, InboxStatus::Resolved);
    assert_eq!(visible.last_seq, written[0].last_seq);
    assert_eq!(store.events().head().await.unwrap(), written[0].last_seq);
    assert_eq!(inbox.resolution(&raised.item.id), Some(how));
}

// 编译期检查：动态事务句柄确实由 Store 的 WriteTx 实现。
const _: fn() = || {
    fn assert_tx<T: TxHandle>() {}
    assert_tx::<gantry_store::WriteTx>();
};
