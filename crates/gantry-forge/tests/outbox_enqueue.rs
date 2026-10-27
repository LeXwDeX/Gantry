//! Outbox 入队面的验收：forge.md §12 A 组第 5、6 条，§12 D 组的入队部分。
//!
//! 全部跑在内存 store 上（A 级，本机替身）；不访问 GitHub。

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, IdempotencyKey, RepoSlug};
use gantry_core::entities::{
    Actor, ActorKind, MergeMethod, OutboxError, OutboxStatus, Priority, Project,
};
use gantry_core::forge::{
    ActionPayload, CloseIssuePayload, CommentTarget, CommentTargetType, CreateCommentPayload,
    CreatePrPayload, IssueStateReason, MergeMode, MergePrPayload, PushBranchPayload, PushTransport,
};
use gantry_core::ids::{Apr, DeliveryId, Id, IdGen, IdKind, Prj, ProjectId, Ulid, Wtr};
use gantry_core::ports::{
    ActorCtx, CancelOutcome, CancelReason, EnqueueError, EnqueueRequest, EnqueuedBy,
    EnqueuedByModule, OutboxPort,
};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::forge::{outbox_conformance, FakeForge};
use gantry_forge::{derive_target_intent, Forge, ForgeConfig, Permission, ProjectBinding};
use gantry_store::{AfterFailure, ClaimOpts, Store, StoreDeps};

const MS: u64 = 1_700_000_000_000;

fn ts() -> Timestamp {
    Timestamp::from_millis(MS as i64)
}

fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS, n).expect("test id parts")
}

fn oid(seed: &str) -> GitOid {
    GitOid::parse(&sha256(seed.as_bytes()).to_hex()[..40]).expect("test git oid")
}

fn slug() -> RepoSlug {
    RepoSlug::parse("acme/shop-api").expect("slug")
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(MS, 1).expect("request id"),
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
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: ts(),
        updated_at: ts(),
    }
}

fn all_permissions() -> BTreeSet<Permission> {
    [
        Permission::ContentsWrite,
        Permission::PullRequestsWrite,
        Permission::IssuesWrite,
    ]
    .into_iter()
    .collect()
}

fn open_store() -> Store {
    let clock = Arc::new(FakeClock::new(MS as i64));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    Store::open_in_memory_sync(StoreDeps { clock, ids }).expect("store")
}

fn forge_with(store: Store, granted: BTreeSet<Permission>) -> Forge {
    Forge::new(
        store,
        ForgeConfig::default().with_project(ProjectBinding {
            project: project(),
            granted,
        }),
    )
}

fn create_pr_payload() -> CreatePrPayload {
    CreatePrPayload {
        repo: slug(),
        head: "gantry/42-fix-pagination".into(),
        base: "main".into(),
        title: "Fix pagination off-by-one in /orders".into(),
        body: "Fixes the off-by-one when page size divides total count.".into(),
        draft: false,
        closes: vec![42],
        expected_head_oid: GitOid::parse("d7d018f867d6c5b78e82906386420d45a4d84486").unwrap(),
        maintainer_can_modify: false,
    }
}

fn request(payload: ActionPayload, epoch: u32) -> EnqueueRequest {
    EnqueueRequest {
        project_id: id::<Prj>(1),
        delivery_id: id(2),
        payload,
        requires_approval: None,
        depends_on: vec![],
        priority: Priority::High,
        not_before: None,
        epoch,
        enqueued_by: EnqueuedBy {
            module: EnqueuedByModule::Engine,
            causation_id: None,
        },
    }
}

fn push_payload(branch: &str, force: bool) -> PushBranchPayload {
    PushBranchPayload {
        repo: slug(),
        branch: branch.into(),
        base_ref: "main".into(),
        worktree_id: id::<Wtr>(3),
        local_head_oid: oid("local"),
        expected_tree_oid: oid("tree"),
        expected_remote_oid: None,
        validation_ref: sha256(b"patch"),
        transport: PushTransport::Git,
        force,
    }
}

fn merge_payload() -> MergePrPayload {
    MergePrPayload {
        repo: slug(),
        pull_number: 57,
        expected_head_sha: oid("head"),
        merge_method: MergeMethod::Squash,
        mode: MergeMode::Auto,
        commit_title: None,
        commit_message: None,
    }
}

// ---------------------------------------------------------------------------
// 黄金向量（forge.md §12 A 第 6 条；forge-actions §2.1 / §2.2）
// ---------------------------------------------------------------------------

#[test]
fn golden_vector_create_pr_key() {
    let derived = derive_target_intent(&ActionPayload::CreatePr(create_pr_payload())).unwrap();
    assert_eq!(derived.target, "pr:gantry/42-fix-pagination->main");
    let digest = gantry_core::canon::jcs_digest(&derived.intent).unwrap();
    assert_eq!(
        digest.to_hex(),
        "d94ed68ae5111e1a996d038b3283e0b787bac2d1363dba2090df25b5754968fb"
    );
    let project: ProjectId = "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK".parse().unwrap();
    let delivery: DeliveryId = "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM".parse().unwrap();
    let key = IdempotencyKey::derive(
        &project,
        &delivery,
        gantry_core::entities::ForgeActionKind::CreatePr,
        &derived.target,
        &derived.intent,
        0,
    )
    .unwrap();
    assert_eq!(key.as_str(), "gk1_anbfwcip7r36is6wsqcuxxfd34");
}

#[test]
fn targets_follow_forge_actions_3_2() {
    let target = |payload| derive_target_intent(&payload).unwrap().target;
    assert_eq!(
        target(ActionPayload::PushBranch(push_payload("gantry/x", false))),
        "branch:gantry/x"
    );
    assert_eq!(
        target(ActionPayload::MergePr(merge_payload())),
        "pr:57:merge"
    );
    assert_eq!(
        target(ActionPayload::CloseIssue(CloseIssuePayload {
            repo: slug(),
            number: 42,
            state_reason: IssueStateReason::Completed,
            require_merged_pr: Some(57),
        })),
        "issue:42:close"
    );
}

#[test]
fn update_pr_intent_only_contains_changed_fields() {
    let payload = ActionPayload::UpdatePr(gantry_core::forge::UpdatePrPayload {
        repo: slug(),
        pull_number: 57,
        title: Some("t".into()),
        body: None,
        base: None,
        state: None,
    });
    let derived = derive_target_intent(&payload).unwrap();
    assert_eq!(derived.target, "pr:57");
    assert_eq!(derived.intent, serde_json::json!({ "title": "t" }));
}

// ---------------------------------------------------------------------------
// 共享一致性（§12 D）：FakeForge 与真实 Forge 跑同一组用例
// ---------------------------------------------------------------------------

async fn tx_conformance(port: &dyn OutboxPort, store: &Store) {
    let ctx = ctx();
    let before = store.snapshot_sync().unwrap().outbox_list().len();

    let mut tx = store.begin_write().await.unwrap();
    let outcome = port
        .enqueue_in(
            &mut tx,
            request(ActionPayload::CreatePr(create_pr_payload()), 10),
            &ctx,
        )
        .await
        .expect("enqueue in tx");
    assert!(outcome.created);
    tx.rollback().await.unwrap();
    assert_eq!(store.snapshot_sync().unwrap().outbox_list().len(), before);

    let mut tx = store.begin_write().await.unwrap();
    port.enqueue_in(
        &mut tx,
        request(ActionPayload::CreatePr(create_pr_payload()), 11),
        &ctx,
    )
    .await
    .expect("enqueue in tx");
    tx.commit().await.unwrap();
    assert_eq!(
        store.snapshot_sync().unwrap().outbox_list().len(),
        before + 1
    );
}

#[tokio::test]
async fn conformance_fake_forge() {
    let store = open_store();
    let fake = FakeForge::new_in(store.clone());
    fake.insert_project(project());
    outbox_conformance(
        &fake,
        request(ActionPayload::CreatePr(create_pr_payload()), 0),
        &ctx(),
    )
    .await
    .expect("fake conformance");
    tx_conformance(&fake, &store).await;
}

#[tokio::test]
async fn conformance_real_forge() {
    let store = open_store();
    let forge = forge_with(store.clone(), all_permissions());
    outbox_conformance(
        &forge,
        request(ActionPayload::CreatePr(create_pr_payload()), 0),
        &ctx(),
    )
    .await
    .expect("forge conformance");
    tx_conformance(&forge, &store).await;
}

// ---------------------------------------------------------------------------
// §12 A 第 5 条：同键一行；epoch+1 新行；failed / cancelled 不复活
// ---------------------------------------------------------------------------

#[tokio::test]
async fn terminal_rows_are_not_revived() {
    let store = open_store();
    let forge = forge_with(store.clone(), all_permissions());
    let ctx = ctx();
    let req = || request(ActionPayload::CreatePr(create_pr_payload()), 0);

    // cancelled 不复活
    let first = forge.enqueue(req(), &ctx).await.unwrap();
    assert_eq!(
        forge
            .cancel(&first.action.id, CancelReason::DeliveryAbandoned, &ctx)
            .await
            .unwrap(),
        CancelOutcome::Cancelled
    );
    let again = forge.enqueue(req(), &ctx).await.unwrap();
    assert!(!again.created);
    assert_eq!(again.action.id, first.action.id);
    assert_eq!(again.action.status, OutboxStatus::Cancelled);

    // failed 不复活：经 store 原语把 epoch=1 的行推进到终态 failed
    let mut retry = req();
    retry.epoch = 1;
    let second = forge.enqueue(retry.clone(), &ctx).await.unwrap();
    assert!(second.created);
    let mut tx = store.begin_write().await.unwrap();
    let claimed = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(120),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .expect("claim");
    assert_eq!(claimed.action.id, second.action.id);
    // 执行中不可取消
    tx.commit().await.unwrap();
    assert_eq!(
        forge
            .cancel(&second.action.id, CancelReason::Shutdown, &ctx)
            .await
            .unwrap(),
        CancelOutcome::NotCancellable
    );
    let mut tx = store.begin_write().await.unwrap();
    tx.outbox()
        .fail(
            &second.action.id,
            OutboxError {
                code: "validation_failed".into(),
                message: String::new(),
            },
            AfterFailure::Terminal,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let revived = forge.enqueue(retry.clone(), &ctx).await.unwrap();
    assert!(!revived.created);
    assert_eq!(revived.action.status, OutboxStatus::Failed);

    // 人工重试 = epoch + 1 的新动作
    retry.epoch = 2;
    assert!(forge.enqueue(retry, &ctx).await.unwrap().created);
}

// ---------------------------------------------------------------------------
// 入队时的机械校验（forge.md §7.8、§12 A 第 12、15 条的入队部分）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_pr_requires_approval() {
    let forge = forge_with(open_store(), all_permissions());
    let err = forge
        .enqueue(request(ActionPayload::MergePr(merge_payload()), 0), &ctx())
        .await
        .unwrap_err();
    assert!(matches!(err, EnqueueError::ApprovalRequired { .. }));

    let mut req = request(ActionPayload::MergePr(merge_payload()), 0);
    req.requires_approval = Some(id::<Apr>(5));
    assert!(forge.enqueue(req, &ctx()).await.unwrap().created);
}

#[tokio::test]
async fn push_branch_force_or_outside_prefix_requires_approval() {
    let forge = forge_with(open_store(), all_permissions());
    for payload in [
        push_payload("gantry/x", true),
        push_payload("feature/x", false),
    ] {
        let err = forge
            .enqueue(
                request(ActionPayload::PushBranch(payload.clone()), 0),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, EnqueueError::ApprovalRequired { .. }),
            "{payload:?}"
        );
        let mut req = request(ActionPayload::PushBranch(payload), 0);
        req.requires_approval = Some(id::<Apr>(5));
        assert!(forge.enqueue(req, &ctx()).await.unwrap().created);
    }
    let ok = forge
        .enqueue(
            request(
                ActionPayload::PushBranch(push_payload("gantry/y", false)),
                0,
            ),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(ok.created);
}

#[tokio::test]
async fn missing_permission_is_rejected_at_enqueue() {
    let granted = [Permission::PullRequestsWrite, Permission::IssuesWrite]
        .into_iter()
        .collect();
    let forge = forge_with(open_store(), granted);
    let err = forge
        .enqueue(
            request(
                ActionPayload::PushBranch(push_payload("gantry/x", false)),
                0,
            ),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        EnqueueError::PermissionMissing {
            kind: gantry_core::entities::ForgeActionKind::PushBranch,
            permission: "contents:write",
        }
    );
}

#[tokio::test]
async fn unknown_project_wrong_repo_and_m2_kinds_are_rejected() {
    let forge = forge_with(open_store(), all_permissions());

    let mut req = request(ActionPayload::CreatePr(create_pr_payload()), 0);
    req.project_id = id::<Prj>(99);
    assert_eq!(
        forge.enqueue(req, &ctx()).await.unwrap_err(),
        EnqueueError::ProjectUnknown
    );

    let mut payload = create_pr_payload();
    payload.repo = RepoSlug::parse("other/repo").unwrap();
    let err = forge
        .enqueue(request(ActionPayload::CreatePr(payload), 0), &ctx())
        .await
        .unwrap_err();
    assert!(
        matches!(err, EnqueueError::InvalidPayload { ref pointer, .. } if pointer == "/payload/repo")
    );

    let comment = ActionPayload::CreateComment(CreateCommentPayload {
        repo: slug(),
        target: CommentTarget {
            kind: CommentTargetType::Pr,
            number: 57,
        },
        client_ref: "c1".into(),
        body: "hi".into(),
    });
    let err = forge
        .enqueue(request(comment, 0), &ctx())
        .await
        .unwrap_err();
    assert!(matches!(err, EnqueueError::InvalidPayload { ref pointer, .. } if pointer == "/kind"));
}
