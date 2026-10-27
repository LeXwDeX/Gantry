//! Outbox 执行器的验收：forge.md §12 A 组第 3、4、12、17、20 条与"响应丢失"、令牌不外泄。
//!
//! 全部对 `FakeGitHub`（回环 HTTP）与内存 store（A 级）运行。"崩溃"用故障注入点模拟：
//! 执行器在注入点立刻返回，不再写 store；随后拨快时钟越过租约，用新的执行器实例重放。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, RepoSlug, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind, MergeMethod,
    OutboxAction, OutboxStatus, Priority, Project,
};
use gantry_core::events::{EventType, Subject};
use gantry_core::forge::{
    ActionPayload, CloseIssuePayload, CreatePrPayload, IssueStateReason, MergeMode, MergePrPayload,
    UpdatePrPayload,
};
use gantry_core::ids::{ActionId, ApprovalId, Id, IdGen, IdKind, Prj, Ulid};
use gantry_core::ports::{
    ActorCtx, ApprovalSpec, EnqueueRequest, EnqueuedBy, EnqueuedByModule, OutboxPort, PolicyPort,
};
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::github::{FakeGitHub, ScriptedFailure};
use gantry_fakes::policy::FakePolicy;
use gantry_forge::{
    CrashPoint, ExecError, Executor, ExecutorDeps, ExecutorOptions, FaultInjector, Forge,
    ForgeConfig, Permission, ProjectBinding, StaticTokenProvider, StepOutcome,
};
use gantry_store::{Store, StoreDeps};

const MS: i64 = 1_700_000_000_000;
const REPO: &str = "acme/shop-api";
const HEAD: &str = "gantry/42-fix-pagination";
const TOKEN: &str = "ghs_TEST_TOKEN_must_not_leak_0123456789";
const LEASE_MS: i64 = 120_000;

fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS as u64, n).expect("test id parts")
}

fn oid(seed: &str) -> GitOid {
    GitOid::parse(&sha256(seed.as_bytes()).to_hex()[..40]).expect("oid")
}

fn slug() -> RepoSlug {
    RepoSlug::parse(REPO).unwrap()
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(MS as u64, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

struct CrashOnce {
    point: CrashPoint,
    fired: AtomicBool,
}

impl FaultInjector for CrashOnce {
    fn crash(&self, _action: &ActionId, point: CrashPoint) -> bool {
        point == self.point && !self.fired.swap(true, Ordering::SeqCst)
    }
}

struct Harness {
    github: FakeGitHub,
    store: Store,
    clock: Arc<FakeClock>,
    ids: Arc<IdGen>,
    forge: Forge,
    policy: Arc<FakePolicy>,
}

impl Harness {
    fn new() -> Self {
        Self::with_project(|_| {})
    }

    fn with_project(edit: impl FnOnce(&mut Project)) -> Self {
        let github = FakeGitHub::start();
        let (_, bot_id) = github.bot();
        let clock = Arc::new(FakeClock::new(MS));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(11))));
        let store = Store::open_in_memory_sync(StoreDeps {
            clock: clock.clone(),
            ids: ids.clone(),
        })
        .unwrap();
        let mut project = Project {
            id: id::<Prj>(1),
            repo: slug(),
            installation_id: 42,
            bot_user_id: bot_id,
            default_branch: "main".into(),
            allowed_merge_methods: vec![MergeMethod::Squash],
            merge_queue_enabled: false,
            created_at: Timestamp::from_millis(MS),
            updated_at: Timestamp::from_millis(MS),
        };
        edit(&mut project);
        let forge = Forge::new(
            store.clone(),
            ForgeConfig::default().with_project(ProjectBinding {
                project,
                granted: [
                    Permission::ContentsWrite,
                    Permission::PullRequestsWrite,
                    Permission::IssuesWrite,
                ]
                .into_iter()
                .collect(),
            }),
        );
        let policy = Arc::new(FakePolicy::new_in(store.clone()));
        Self {
            github,
            store,
            clock,
            ids,
            forge,
            policy,
        }
    }

    fn executor(&self, faults: Option<Arc<dyn FaultInjector>>) -> Executor {
        self.forge.executor(ExecutorDeps {
            policy: self.policy.clone(),
            tokens: Arc::new(StaticTokenProvider::new(SecretString::new(TOKEN.into()))),
            api_base: self.github.base_url(),
            ids: self.ids.clone(),
            options: ExecutorOptions {
                http_timeout: Duration::from_secs(5),
                ..ExecutorOptions::default()
            },
            faults,
        })
    }

    fn crashing(&self, point: CrashPoint) -> Executor {
        self.executor(Some(Arc::new(CrashOnce {
            point,
            fired: AtomicBool::new(false),
        })))
    }

    async fn enqueue(&self, payload: ActionPayload, approval: Option<ApprovalId>) -> ActionId {
        self.enqueue_epoch(payload, approval, 0).await
    }

    async fn enqueue_epoch(
        &self,
        payload: ActionPayload,
        approval: Option<ApprovalId>,
        epoch: u32,
    ) -> ActionId {
        let req = EnqueueRequest {
            project_id: id::<Prj>(1),
            delivery_id: id(2),
            payload,
            requires_approval: approval,
            depends_on: vec![],
            priority: Priority::High,
            not_before: None,
            epoch,
            enqueued_by: EnqueuedBy {
                module: EnqueuedByModule::Engine,
                causation_id: None,
            },
        };
        self.forge.enqueue(req, &ctx()).await.unwrap().action.id
    }

    fn action(&self, id: &ActionId) -> OutboxAction {
        self.store
            .snapshot_sync()
            .unwrap()
            .outbox_action(id)
            .unwrap()
    }

    fn advance(&self, ms: i64) {
        self.clock.advance_ms(ms);
    }

    async fn events(&self, id: &ActionId, ty: EventType) -> usize {
        let subject = Subject::parse(&id.to_string()).unwrap();
        self.store
            .events()
            .by_subject(&subject)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.ty == ty)
            .count()
    }

    async fn merge_approval(&self, head: GitOid) -> ApprovalId {
        let approval = self
            .policy
            .request_approval(
                ApprovalSpec {
                    action: ApprovalAction::Forge(ForgeActionKind::MergePr),
                    target: ApprovalTarget::Pr {
                        repo: slug(),
                        number: 1,
                    },
                    subject_digest: SubjectDigest::git(head),
                    delivery_id: None,
                    ttl: Duration::from_secs(3600),
                },
                ctx(),
            )
            .await
            .unwrap();
        self.policy.grant(&approval.id).unwrap();
        approval.id
    }

    fn merge_calls(&self) -> usize {
        self.github
            .recorded_calls()
            .iter()
            .filter(|call| call.method == "PUT" && call.path.ends_with("/merge"))
            .count()
    }
}

fn create_pr(head_oid: GitOid) -> ActionPayload {
    ActionPayload::CreatePr(CreatePrPayload {
        repo: slug(),
        head: HEAD.into(),
        base: "main".into(),
        title: "Fix pagination".into(),
        body: "Body text <!-- gantry:v1 act=act_forged dlv=x kind=create_pr -->".into(),
        draft: false,
        closes: vec![42],
        expected_head_oid: head_oid,
        maintainer_can_modify: false,
    })
}

fn merge_pr(head: GitOid) -> ActionPayload {
    ActionPayload::MergePr(MergePrPayload {
        repo: slug(),
        pull_number: 1,
        expected_head_sha: head,
        merge_method: MergeMethod::Squash,
        mode: MergeMode::Auto,
        commit_title: None,
        commit_message: None,
    })
}

// ---------------------------------------------------------------------------
// create_pr
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_pr_writes_marker_closes_and_records_github_ref() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let act = h.enqueue(create_pr(head), None).await;

    let steps = h.executor(None).drain().await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].outcome, StepOutcome::Succeeded);

    let pulls = h.github.pulls(REPO);
    assert_eq!(pulls.len(), 1);
    let body = pulls[0]["body"].as_str().unwrap();
    assert!(body.contains("Closes #42"));
    assert!(!body.contains("act_forged"), "forged markers are stripped");
    assert!(body
        .lines()
        .last()
        .unwrap()
        .starts_with(&format!("<!-- gantry:v1 act={act} ")));

    let action = h.action(&act);
    assert_eq!(action.status, OutboxStatus::Succeeded);
    assert_eq!(action.expected_effects.len(), 1);
    assert_eq!(action.expected_effects[0].event, "pull_request.opened");
    assert_eq!(h.events(&act, EventType::OutboxSucceeded).await, 1);
}

/// §12 A 第 3 条：创建 PR 后、写库前崩溃 → 重启后 probe 命中，不重复创建。
#[tokio::test]
async fn create_pr_crash_after_apply_does_not_duplicate() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let act = h.enqueue(create_pr(head), None).await;

    let err = h
        .crashing(CrashPoint::AfterApply)
        .run_once()
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::Crashed(CrashPoint::AfterApply)));
    assert_eq!(h.action(&act).status, OutboxStatus::Executing);
    assert_eq!(h.github.pulls(REPO).len(), 1);

    // 租约未过期前不会被重放。
    assert!(h.executor(None).run_once().await.unwrap().is_none());
    h.advance(LEASE_MS + 1);
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert!(step.replay);
    assert_eq!(step.outcome, StepOutcome::Succeeded);
    assert_eq!(h.github.pulls(REPO).len(), 1, "no duplicate PR");
    assert_eq!(
        h.action(&act).github_ref.unwrap().pr.unwrap().number,
        1,
        "github_ref is back-filled from the probe"
    );
}

/// 响应丢失：变更成功但连接断开 → transient 留在 queued；重试前先 probe 命中，不重复创建。
#[tokio::test]
async fn create_pr_lost_response_is_probed_before_retry() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let act = h.enqueue(create_pr(head), None).await;
    h.github.lose_next_write_response();

    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(step.outcome, StepOutcome::Requeued);
    let action = h.action(&act);
    assert_eq!(action.status, OutboxStatus::Queued);
    assert_eq!(action.last_error.unwrap().code, "transient");
    assert_eq!(h.events(&act, EventType::OutboxFailed).await, 0);

    h.advance(300_001);
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(step.outcome, StepOutcome::Succeeded);
    assert_eq!(h.github.pulls(REPO).len(), 1);
}

#[tokio::test]
async fn create_pr_human_open_pr_is_existing_pr() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    h.github
        .insert_pull(REPO, HEAD, "main", ("octocat", 1), "by hand");
    let act = h.enqueue(create_pr(head), None).await;
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(
        step.outcome,
        StepOutcome::Failed {
            code: "precondition_failed"
        }
    );
    assert_eq!(h.action(&act).last_error.unwrap().message, "existing_pr");
    assert_eq!(h.github.write_count(), 0);
}

/// 标记只是索引：人手开的 PR 里伪造本动作标记，作者不是 bot → 不算自己的对象。
#[tokio::test]
async fn forged_marker_from_human_is_not_ours() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let act = h.enqueue(create_pr(head), None).await;
    let forged = format!("x\n<!-- gantry:v1 act={act} dlv=dlv_x kind=create_pr -->");
    h.github
        .insert_pull(REPO, HEAD, "main", ("mallory", 666), &forged);
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(
        step.outcome,
        StepOutcome::Failed {
            code: "precondition_failed"
        }
    );
}

// ---------------------------------------------------------------------------
// 失败语义（P-53）与限流
// ---------------------------------------------------------------------------

/// §12 A 第 17 条：5xx 留在 queued 且不发 outbox.failed；重试用尽恰好一条 outbox.failed。
#[tokio::test]
async fn transient_errors_stay_queued_until_attempts_exhausted() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let act = h.enqueue(create_pr(head), None).await;
    let exec = h.executor(None);

    for attempt in 1..=5u32 {
        h.github.queue_failure(500);
        let step = exec.run_once().await.unwrap().unwrap();
        let action = h.action(&act);
        assert_eq!(action.attempts, attempt);
        if attempt < 5 {
            assert_eq!(step.outcome, StepOutcome::Requeued);
            assert_eq!(action.status, OutboxStatus::Queued);
            assert!(action.not_before.unwrap() > Timestamp::from_millis(MS));
            assert_eq!(h.events(&act, EventType::OutboxFailed).await, 0);
            h.advance(300_001);
        } else {
            assert_eq!(step.outcome, StepOutcome::Failed { code: "transient" });
            assert_eq!(action.status, OutboxStatus::Failed);
        }
    }
    assert_eq!(h.events(&act, EventType::OutboxFailed).await, 1);
    assert_eq!(h.github.pulls(REPO).len(), 0);
}

/// §12 A 第 15 条（动作侧）：403 + retry-after → 回 queued，尝试次数不增，到期前零请求。
#[tokio::test]
async fn secondary_rate_limit_requeues_without_consuming_attempts() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let act = h.enqueue(create_pr(head), None).await;
    h.github.push_failure(ScriptedFailure {
        status: 403,
        body: r#"{"message":"You have exceeded a secondary rate limit"}"#.into(),
        retry_after_s: Some(30),
    });
    let exec = h.executor(None);
    assert_eq!(
        exec.run_once().await.unwrap().unwrap().outcome,
        StepOutcome::Requeued
    );
    let action = h.action(&act);
    assert_eq!(action.status, OutboxStatus::Queued);
    assert_eq!(action.attempts, 0);
    assert_eq!(action.not_before, Some(Timestamp::from_millis(MS + 30_000)));

    let calls = h.github.recorded_calls().len();
    h.advance(29_000);
    assert!(exec.run_once().await.unwrap().is_none());
    assert_eq!(
        h.github.recorded_calls().len(),
        calls,
        "zero requests while waiting"
    );
    h.advance(1_000);
    assert_eq!(
        exec.run_once().await.unwrap().unwrap().outcome,
        StepOutcome::Succeeded
    );
}

/// 403 无限流特征 → permission_denied 终态，不重试。
#[tokio::test]
async fn forbidden_without_rate_limit_is_permission_denied() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    h.enqueue(create_pr(head), None).await;
    h.github.push_failure(ScriptedFailure {
        status: 403,
        body: r#"{"message":"Resource not accessible by integration"}"#.into(),
        retry_after_s: None,
    });
    assert_eq!(
        h.executor(None).run_once().await.unwrap().unwrap().outcome,
        StepOutcome::Failed {
            code: "permission_denied"
        }
    );
}

// ---------------------------------------------------------------------------
// merge_pr（§12 A 第 12 条）
// ---------------------------------------------------------------------------

async fn open_pr(h: &Harness) -> GitOid {
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let (login, bot) = h.github.bot();
    h.github
        .insert_pull(REPO, HEAD, "main", (&login, bot), "pr");
    head
}

#[tokio::test]
async fn merge_pr_consumes_approval_before_merging() {
    let h = Harness::new();
    let head = open_pr(&h).await;
    let approval = h.merge_approval(head).await;
    let act = h.enqueue(merge_pr(head), Some(approval)).await;

    // 崩溃在"消费之后、apply 之前"：审批已是 consumed，GitHub 尚无合并请求。
    let err = h
        .crashing(CrashPoint::AfterConsume)
        .run_once()
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::Crashed(CrashPoint::AfterConsume)));
    let consumed = h.policy.approval(&approval).unwrap();
    assert_eq!(consumed.status, ApprovalStatus::Consumed);
    assert_eq!(
        consumed.consumed_by.as_deref(),
        Some(act.to_string().as_str())
    );
    assert_eq!(h.merge_calls(), 0);

    h.advance(LEASE_MS + 1);
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(step.outcome, StepOutcome::Succeeded);
    assert_eq!(h.merge_calls(), 1);
    assert_eq!(h.github.pull(REPO, 1).unwrap()["merged"], true);
}

#[tokio::test]
async fn merge_pr_crash_after_apply_does_not_merge_twice() {
    let h = Harness::new();
    let head = open_pr(&h).await;
    let approval = h.merge_approval(head).await;
    let act = h.enqueue(merge_pr(head), Some(approval)).await;
    assert!(h.crashing(CrashPoint::AfterApply).run_once().await.is_err());
    h.advance(LEASE_MS + 1);
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert!(step.replay);
    assert_eq!(step.outcome, StepOutcome::Succeeded);
    assert_eq!(h.merge_calls(), 1, "replay probes instead of merging again");
    assert_eq!(h.action(&act).status, OutboxStatus::Succeeded);
    assert_eq!(
        h.policy.approval(&approval).unwrap().status,
        ApprovalStatus::Consumed
    );
}

#[tokio::test]
async fn merge_pr_live_head_mismatch_sends_no_merge_and_keeps_approval() {
    let h = Harness::new();
    let head = open_pr(&h).await;
    let approval = h.merge_approval(head).await;
    h.enqueue(merge_pr(head), Some(approval)).await;
    h.github.set_pull_head(REPO, 1, &oid("moved").to_string());
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(
        step.outcome,
        StepOutcome::Failed {
            code: "precondition_failed"
        }
    );
    assert_eq!(h.merge_calls(), 0);
    assert_eq!(
        h.policy.approval(&approval).unwrap().status,
        ApprovalStatus::Granted
    );
}

#[tokio::test]
async fn merge_pr_invalid_approval_cancels_without_merge() {
    let h = Harness::new();
    let head = open_pr(&h).await;
    // 审批主体是另一个 head：policy 判 HeadChanged 之外的不一致由 policy 决定；
    // 这里用未授予（pending）的审批，期望 approval_invalid → cancelled。
    let pending = h
        .policy
        .request_approval(
            ApprovalSpec {
                action: ApprovalAction::Forge(ForgeActionKind::MergePr),
                target: ApprovalTarget::Pr {
                    repo: slug(),
                    number: 1,
                },
                subject_digest: SubjectDigest::git(head),
                delivery_id: None,
                ttl: Duration::from_secs(3600),
            },
            ctx(),
        )
        .await
        .unwrap();
    let act = h.enqueue(merge_pr(head), Some(pending.id)).await;
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(step.outcome, StepOutcome::Cancelled);
    assert_eq!(h.action(&act).status, OutboxStatus::Cancelled);
    assert_eq!(h.merge_calls(), 0);
}

/// 405 终态失败 → 动作 failed、审批仍 consumed；同键入队不复活；epoch+1 用旧审批被取消。
#[tokio::test]
async fn merge_pr_not_mergeable_is_terminal_and_approval_stays_consumed() {
    let h = Harness::new();
    let head = open_pr(&h).await;
    h.github.set_pull_state(REPO, 1, "closed");
    let approval = h.merge_approval(head).await;
    let act = h.enqueue(merge_pr(head), Some(approval)).await;
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(
        step.outcome,
        StepOutcome::Failed {
            code: "not_mergeable"
        }
    );
    assert_eq!(h.events(&act, EventType::OutboxFailed).await, 1);
    assert_eq!(
        h.policy.approval(&approval).unwrap().status,
        ApprovalStatus::Consumed
    );

    // 同键不复活。
    assert_eq!(h.enqueue(merge_pr(head), Some(approval)).await, act);
    assert_eq!(h.action(&act).status, OutboxStatus::Failed);

    // epoch + 1 但沿用已消费的审批 → 执行前被拒（ConsumedByOther）→ cancelled，不发合并。
    let retry = h.enqueue_epoch(merge_pr(head), Some(approval), 1).await;
    let calls = h.merge_calls();
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(step.action, retry);
    assert_eq!(step.outcome, StepOutcome::Cancelled);
    assert_eq!(h.merge_calls(), calls);
}

#[tokio::test]
async fn merge_queue_repos_are_rejected_in_m1() {
    let h = Harness::with_project(|project| project.merge_queue_enabled = true);
    let head = open_pr(&h).await;
    let approval = h.merge_approval(head).await;
    let act = h.enqueue(merge_pr(head), Some(approval)).await;
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(
        step.outcome,
        StepOutcome::Failed {
            code: "validation_failed"
        }
    );
    assert_eq!(
        h.action(&act).last_error.unwrap().message,
        "merge_queue_unsupported"
    );
    assert_eq!(h.github.recorded_calls().len(), 0);
}

// ---------------------------------------------------------------------------
// 崩溃矩阵（§12 A 第 4 条；FA §2.3）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Kind {
    CreatePr,
    UpdatePr,
    CloseIssue,
    MergePr,
}

async fn setup(h: &Harness, kind: Kind) -> ActionId {
    match kind {
        Kind::CreatePr => {
            let head = oid("head");
            h.github.set_branch(REPO, HEAD, &head.to_string());
            h.enqueue(create_pr(head), None).await
        }
        Kind::UpdatePr => {
            open_pr(h).await;
            h.enqueue(
                ActionPayload::UpdatePr(UpdatePrPayload {
                    repo: slug(),
                    pull_number: 1,
                    title: Some("New title".into()),
                    body: Some("New body".into()),
                    base: None,
                    state: None,
                }),
                None,
            )
            .await
        }
        Kind::CloseIssue => {
            let number = h.github.insert_issue(REPO, "issue");
            h.enqueue(
                ActionPayload::CloseIssue(CloseIssuePayload {
                    repo: slug(),
                    number,
                    state_reason: IssueStateReason::Completed,
                    require_merged_pr: None,
                }),
                None,
            )
            .await
        }
        Kind::MergePr => {
            let head = open_pr(h).await;
            let approval = h.merge_approval(head).await;
            h.enqueue(merge_pr(head), Some(approval)).await
        }
    }
}

/// 变更调用的 (方法, 路径后缀)，用来数"GitHub 上恰好发生一次写"。
fn writes(h: &Harness, kind: Kind) -> usize {
    let (method, suffix) = match kind {
        Kind::CreatePr => ("POST", "/pulls"),
        Kind::UpdatePr => ("PATCH", "/pulls/1"),
        Kind::CloseIssue => ("PATCH", "/issues/1"),
        Kind::MergePr => ("PUT", "/pulls/1/merge"),
    };
    h.github
        .recorded_calls()
        .iter()
        .filter(|call| call.method == method && call.path.ends_with(suffix))
        .count()
}

fn assert_effect(h: &Harness, kind: Kind) {
    match kind {
        Kind::CreatePr => assert_eq!(h.github.pulls(REPO).len(), 1),
        Kind::UpdatePr => {
            let pull = h.github.pull(REPO, 1).unwrap();
            assert_eq!(pull["title"], "New title");
            assert!(pull["body"].as_str().unwrap().starts_with("New body"));
        }
        Kind::CloseIssue => assert_eq!(h.github.issue(REPO, 1).unwrap()["state"], "closed"),
        Kind::MergePr => assert_eq!(h.github.pull(REPO, 1).unwrap()["merged"], true),
    }
}

#[tokio::test]
async fn crash_matrix_replays_to_the_same_result() {
    let kinds = [
        Kind::CreatePr,
        Kind::UpdatePr,
        Kind::CloseIssue,
        Kind::MergePr,
    ];
    let points = [
        None, // claim 之前崩溃：行仍是 queued，正常执行
        Some(CrashPoint::AfterClaim),
        Some(CrashPoint::AfterConsume),
        Some(CrashPoint::AfterRecordEffects),
        Some(CrashPoint::AfterApply),
    ];
    for kind in kinds {
        for point in points {
            let h = Harness::new();
            let act = setup(&h, kind).await;
            if let Some(point) = point {
                let result = h.crashing(point).run_once().await;
                // 只有 merge_pr 有"消费审批"这一步；其余动作不会经过 AfterConsume。
                let expect_crash =
                    !(point == CrashPoint::AfterConsume && !matches!(kind, Kind::MergePr));
                assert_eq!(
                    result.is_err(),
                    expect_crash,
                    "{kind:?} at {point:?}: {result:?}"
                );
                if expect_crash {
                    assert_eq!(h.action(&act).status, OutboxStatus::Executing);
                    h.advance(LEASE_MS + 1);
                }
            }
            h.executor(None).drain().await.unwrap();
            let action = h.action(&act);
            assert_eq!(
                action.status,
                OutboxStatus::Succeeded,
                "{kind:?} at {point:?}"
            );
            assert_eq!(
                writes(&h, kind),
                1,
                "{kind:?} at {point:?}: exactly one write"
            );
            assert_effect(&h, kind);
            assert_eq!(h.events(&act, EventType::OutboxSucceeded).await, 1);

            // 记录之后崩溃：已 succeeded，再跑一次无事发生。
            assert!(h.executor(None).run_once().await.unwrap().is_none());
            assert_eq!(writes(&h, kind), 1);
        }
    }
}

// ---------------------------------------------------------------------------
// 其他动作语义
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_pr_keeps_creation_marker_and_is_idempotent() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let create = h.enqueue(create_pr(head), None).await;
    h.executor(None).drain().await.unwrap();
    let update = ActionPayload::UpdatePr(UpdatePrPayload {
        repo: slug(),
        pull_number: 1,
        title: None,
        body: Some("Rewritten".into()),
        base: None,
        state: None,
    });
    h.enqueue(update.clone(), None).await;
    h.executor(None).drain().await.unwrap();
    let body = h.github.pull(REPO, 1).unwrap()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(body.starts_with("Rewritten"));
    assert!(body.contains(&format!("act={create} ")));

    // 同样的改动换 epoch 再来一次：逐字段相等 → 已完成，不发 PATCH。
    let patches = writes(&h, Kind::UpdatePr);
    h.enqueue_epoch(update, None, 1).await;
    h.executor(None).drain().await.unwrap();
    assert_eq!(writes(&h, Kind::UpdatePr), patches);
}

#[tokio::test]
async fn close_issue_requires_merged_pr() {
    let h = Harness::new();
    open_pr(&h).await;
    let issue = h.github.insert_issue(REPO, "issue");
    h.enqueue(
        ActionPayload::CloseIssue(CloseIssuePayload {
            repo: slug(),
            number: issue,
            state_reason: IssueStateReason::Completed,
            require_merged_pr: Some(1),
        }),
        None,
    )
    .await;
    assert_eq!(
        h.executor(None).run_once().await.unwrap().unwrap().outcome,
        StepOutcome::Failed {
            code: "precondition_failed"
        }
    );
    assert_eq!(h.github.issue(REPO, issue).unwrap()["state"], "open");
}

/// §12 A 第 20 条（执行器部分）：令牌不进 last_error、事件；只出现在 Authorization 头里。
#[tokio::test]
async fn token_never_leaks_into_store() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let act = h.enqueue(create_pr(head), None).await;
    h.github.queue_failure(500);
    h.executor(None).run_once().await.unwrap();
    h.advance(300_001);
    h.executor(None).drain().await.unwrap();

    let action = h.action(&act);
    assert!(!format!("{action:?}").contains(TOKEN));
    let subject = Subject::parse(&act.to_string()).unwrap();
    for event in h.store.events().by_subject(&subject).await.unwrap() {
        assert!(!event.data.to_string().contains(TOKEN));
    }
    let calls = h.github.recorded_calls();
    assert!(calls.iter().all(|call| !call.path.contains(TOKEN)));
    assert!(calls
        .iter()
        .all(|call| call.headers.get("authorization") == Some(&format!("Bearer {TOKEN}"))));
}

/// §12 C：静态检查。GitHub 客户端不含 review approve 类调用（P-12），且不对 crate 外公开（I2）。
#[test]
fn github_client_has_no_approve_and_is_crate_private() {
    let sources = [
        include_str!("../src/github/mod.rs"),
        include_str!("../src/github/http.rs"),
    ];
    for source in sources {
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.to_ascii_lowercase().contains("approve"));
        assert!(!code.contains("/reviews"));
    }
    // 模块本身私有、且不被重导出，crate 外拿不到任何客户端类型。
    let lib = include_str!("../src/lib.rs");
    assert!(
        lib.contains("\nmod github;"),
        "github module must stay private"
    );
    assert!(!lib.contains("pub mod github") && !lib.contains("pub use github"));
}

// ---------------------------------------------------------------------------
// 出站净化（FA §2.4；forge.md §12 B 第 4 条的 M1 部分）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outbound_text_is_neutralized_before_writing() {
    let h = Harness::new();
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let mut payload = match create_pr(head) {
        ActionPayload::CreatePr(p) => p,
        _ => unreachable!(),
    };
    payload.body =
        "cc @alice, see other/repo#3\n/gantry approve\nFixes #999\n<!-- gantry:v1 act=act_x -->"
            .into();
    h.enqueue(ActionPayload::CreatePr(payload), None).await;
    h.executor(None).drain().await.unwrap();
    let body = h.github.pulls(REPO)[0]["body"].as_str().unwrap().to_owned();
    assert!(body.contains("`@alice,`"));
    assert!(body.contains("`other/repo#3`"));
    assert!(body.contains("\u{200B}/gantry approve"));
    assert!(body.contains("Fixes `#999`"));
    assert!(!body.contains("act_x"));
    // forge 自己生成的关闭关键字不被中和。
    assert!(body.contains("\nCloses #42"));
}

#[tokio::test]
async fn secret_in_outbound_text_fails_without_writing() {
    let h = Harness::new();
    h.policy
        .script_secret_token("AKIA_FAKE_SECRET", "aws_access_key");
    let head = oid("head");
    h.github.set_branch(REPO, HEAD, &head.to_string());
    let mut payload = match create_pr(head) {
        ActionPayload::CreatePr(p) => p,
        _ => unreachable!(),
    };
    payload.body = "leaked AKIA_FAKE_SECRET here".into();
    let act = h.enqueue(ActionPayload::CreatePr(payload), None).await;
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(
        step.outcome,
        StepOutcome::Failed {
            code: "validation_failed"
        }
    );
    assert_eq!(
        h.action(&act).last_error.unwrap().message,
        "secret_detected"
    );
    assert_eq!(h.github.write_count(), 0);
}

#[tokio::test]
async fn secret_in_merge_message_keeps_approval_unconsumed() {
    let h = Harness::new();
    h.policy
        .script_secret_token("AKIA_FAKE_SECRET", "aws_access_key");
    let head = open_pr(&h).await;
    let approval = h.merge_approval(head).await;
    let mut payload = match merge_pr(head) {
        ActionPayload::MergePr(p) => p,
        _ => unreachable!(),
    };
    payload.commit_message = Some("AKIA_FAKE_SECRET".into());
    h.enqueue(ActionPayload::MergePr(payload), Some(approval))
        .await;
    let step = h.executor(None).run_once().await.unwrap().unwrap();
    assert_eq!(
        step.outcome,
        StepOutcome::Failed {
            code: "validation_failed"
        }
    );
    assert_eq!(h.merge_calls(), 0);
    assert_eq!(
        h.policy.approval(&approval).unwrap().status,
        ApprovalStatus::Granted
    );
}
