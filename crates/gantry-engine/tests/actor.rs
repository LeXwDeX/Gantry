//! 执行体、收敛循环与事件路由（engine.md §7.14.1、§12.2，#71）。
//!
//! 真实 store + `FakeInbox` / `FakeForge`（事务内入队）；repo 与 policy 的副作用由脚本化的
//! `EffectRunner` 模拟：它像真实模块一样把事件写进 store，再返回回报。事件经 `EventRouter`
//! 按路由表投给执行体，测试里没有任何订阅者或客户端。

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::{GitOid, RepoSlug, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, AttemptStatus, DeliveryOrigin, ForgeActionKind, GithubPrRef, GithubRef,
    InboxKind, MergeMethod, Project, PullRequestRef, Seq, Stage, Tier, TokenUsage,
};
use gantry_core::events::{NewEvent, Subject};
use gantry_core::ids::{DeliveryId, IdGen, MessageId, ProjectId, WorktreeId};
use gantry_core::level::{Effort, Level};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::policy::{ActionDecision, ApprovalSpec};
use gantry_core::ports::repo::LockToken;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_core::time::Timestamp;
use gantry_engine::actor::{ActorConfig, ActorDeps, DeliveryActor, EffectRunner, PrSnapshots};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::decide::{
    AttemptPlan, Effect, EffectKey, EffectReport, EngineConsts, EngineSettings, Finalized, Input,
    InputIdentity, ModelKey, ModelTable, OutputSignals, PatchResult, ProjectFacts,
    ReservationPolicy, ResolvedModel, SandboxLimits, TemplateRegistry, TickReason,
};
use gantry_engine::engine::Engine;
use gantry_engine::event_router::{EventRouter, CURSOR};
use gantry_engine::state::DeliveryState;
use gantry_fakes::forge::FakeForge;
use gantry_fakes::inbox::FakeInbox;
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{ClaimOpts, Store, StoreDeps};
use serde_json::json;

const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const WORKTREE: &str = "wtr_01JBA1D9Q4MT7X2C6E8H0K3PRV";
const APPROVAL: &str = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
const BASE_SHA: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
const COMMIT: &str = "d7d018f867d6c5b78e82906386420d45a4d84486";
const TREE: &str = "1405df66cbe219b0bf6355bc3d60361a8376b6b4";
const PATCH_SHA: &str = "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9";
const T0: i64 = 1_700_000_000_000;

fn oid(text: &str) -> GitOid {
    GitOid::parse(text).unwrap()
}

fn project_id() -> ProjectId {
    PROJECT.parse().unwrap()
}

// ---------------------------------------------------------------------------
// 脚本化副作用：像 repo / policy 一样写事件并回报
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum JudgeMode {
    /// 判定请求发出后"崩溃"：不回报。
    Drop,
    /// 正常判定：merge_pr 未授予审批时 NeedsApproval，其余 Allow。
    Normal,
}

struct ScriptRunner {
    store: Store,
    judge: Mutex<JudgeMode>,
    effects: Mutex<Vec<Effect>>,
    granted: Mutex<bool>,
}

impl ScriptRunner {
    fn new(store: Store) -> Self {
        Self {
            store,
            judge: Mutex::new(JudgeMode::Normal),
            effects: Mutex::new(Vec::new()),
            granted: Mutex::new(false),
        }
    }

    fn plans(&self) -> Vec<AttemptPlan> {
        self.effects
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Effect::RunAttempt(plan) => Some(plan.as_ref().clone()),
                _ => None,
            })
            .collect()
    }

    fn count(&self, pred: impl Fn(&Effect) -> bool) -> usize {
        self.effects
            .lock()
            .unwrap()
            .iter()
            .filter(|e| pred(e))
            .count()
    }

    async fn write(
        &self,
        ty: &str,
        subject: &str,
        actor: ActorKind,
        delivery: DeliveryId,
        data: serde_json::Value,
    ) -> Seq {
        let event = NewEvent {
            ty: ty.parse().unwrap(),
            subject: Subject::parse(subject).unwrap(),
            project_id: Some(project_id()),
            actor: Actor::new(actor, "test"),
            causation_id: None,
            correlation_id: Some(delivery.to_string()),
            data,
            dedup_key: None,
        };
        event.validate().unwrap();
        let mut tx = self.store.begin_write().await.unwrap();
        let appended = tx.append(event).await.unwrap();
        tx.commit().await.unwrap();
        match appended {
            gantry_store::Appended::New(event) => event.seq,
            gantry_store::Appended::Duplicate { existing_seq } => existing_seq,
        }
    }
}

#[async_trait]
impl EffectRunner for ScriptRunner {
    async fn run(
        &self,
        state: &DeliveryState,
        effect: Effect,
        _ctx: &ActorCtx,
    ) -> Option<EffectReport> {
        self.effects.lock().unwrap().push(effect.clone());
        let delivery = state.delivery.id;
        match effect {
            Effect::EnsureWorktree { branch, base_ref } => {
                let worktree_id: WorktreeId = WORKTREE.parse().unwrap();
                self.write(
                    "worktree.created",
                    WORKTREE,
                    ActorKind::Engine,
                    delivery,
                    json!({"worktree_id": WORKTREE, "project_id": PROJECT, "delivery_id": delivery,
                           "branch": branch.as_str(), "base_ref": base_ref, "base_sha": BASE_SHA}),
                )
                .await;
                Some(EffectReport::WorktreeReady { worktree_id })
            }
            Effect::AcquireLock {
                worktree_id,
                node_id,
                holder,
                lease_s,
            } => {
                let lease_until = Timestamp::from_millis(T0 + 10_000_000 + lease_s as i64 * 1000);
                let fence = self
                    .write(
                        "worktree.locked",
                        &worktree_id.to_string(),
                        ActorKind::Engine,
                        delivery,
                        json!({"worktree_id": worktree_id, "holder": holder, "delivery_id": delivery,
                               "lease_until": lease_until}),
                    )
                    .await;
                Some(EffectReport::LockAcquired {
                    node_id,
                    token: LockToken {
                        worktree_id,
                        holder,
                        delivery_id: delivery,
                        fence,
                        lease_until,
                    },
                })
            }
            // 长时副作用：结果由测试以 AttemptFinalized 回流。
            Effect::RunAttempt(_) => None,
            Effect::JudgeAction { key, request } => {
                if *self.judge.lock().unwrap() == JudgeMode::Drop {
                    return None;
                }
                let decision = if request.kind == ForgeActionKind::MergePr
                    && !*self.granted.lock().unwrap()
                {
                    let gantry_core::ports::policy::ActionFacts::MergePr {
                        expected_head_sha, ..
                    } = request.facts
                    else {
                        unreachable!()
                    };
                    ActionDecision::NeedsApproval {
                        spec: ApprovalSpec {
                            action: gantry_core::entities::ApprovalAction::Forge(
                                ForgeActionKind::MergePr,
                            ),
                            target: serde_json::from_value(
                                json!({"kind": "pr", "repo": "acme/web", "number": 12}),
                            )
                            .unwrap(),
                            subject_digest: SubjectDigest::git(expected_head_sha),
                            delivery_id: Some(delivery),
                            ttl: Duration::from_secs(3600),
                        },
                        pending: None,
                        config_rev: Seq::new(1).unwrap(),
                    }
                } else {
                    let approval = (request.kind == ForgeActionKind::MergePr)
                        .then(|| APPROVAL.parse().unwrap());
                    ActionDecision::Allow {
                        action_id: gantry_core::entities::PolicyActionId::PushBranch,
                        approval,
                        config_rev: Seq::new(1).unwrap(),
                    }
                };
                Some(EffectReport::ActionJudged { key, decision })
            }
            Effect::RequestApproval { key, spec } => {
                self.write(
                    "approval.requested",
                    APPROVAL,
                    ActorKind::Engine,
                    delivery,
                    json!({"action": "merge_pr", "target": {"kind": "pr", "repo": "acme/web", "number": 12},
                           "subject_digest": spec.subject_digest, "expires_at": "2027-10-24T12:45:00.000Z",
                           "config_rev": 1, "requested_by": {"kind": "engine", "id": "engine"}}),
                )
                .await;
                Some(EffectReport::ApprovalRequested {
                    key,
                    approval: APPROVAL.parse().unwrap(),
                })
            }
            // repo 删工作树时写 worktree.removed（与真实 repo 一致），状态里的工作树随之清空。
            Effect::RemoveWorktree { worktree_id, .. } => {
                let branch = state
                    .worktree
                    .as_ref()
                    .map(|w| w.branch.clone())
                    .unwrap_or_default();
                self.write(
                    "worktree.removed",
                    &worktree_id.to_string(),
                    ActorKind::Engine,
                    delivery,
                    json!({"worktree_id": worktree_id, "branch": branch}),
                )
                .await;
                Some(EffectReport::Done {
                    key: EffectKey::RemoveWorktree(worktree_id),
                })
            }
            other => Some(EffectReport::Done {
                key: other.key(delivery),
            }),
        }
    }
}

#[derive(Default)]
struct Snapshots(Mutex<Option<PullRequestRef>>);

#[async_trait]
impl PrSnapshots for Snapshots {
    async fn pull_request(&self, _: &ProjectId, number: u64) -> Option<PullRequestRef> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .filter(|pr| pr.number == number)
    }
}

fn snapshot(state: &str, required_ok: bool) -> PullRequestRef {
    serde_json::from_value(json!({
        "repo": "acme/web", "number": 12, "node_id": "PR_x",
        "title": {"text": "t", "source": "github", "truncated": false},
        "state": state, "head_sha": COMMIT, "base_ref": "main", "base_sha": BASE_SHA,
        "draft": false, "mergeable_state": "clean", "in_merge_queue": false,
        "checks": {"passed": 1, "failed": 0, "pending": 0, "required_ok": required_ok},
        "review_state": "none", "synced_at": "2026-10-24T12:41:03.120Z"
    }))
    .unwrap()
}

// ---------------------------------------------------------------------------
// 装配
// ---------------------------------------------------------------------------

struct FixedReserve;
impl ReservationPolicy for FixedReserve {
    fn reserve(&self, _: &gantry_core::entities::BudgetLedger, _: Tier, _: usize) -> u64 {
        1000
    }
}

fn config() -> ActorConfig {
    ActorConfig {
        settings: EngineSettings {
            config_rev: Seq::new(1).unwrap(),
            project: ProjectFacts {
                repo: RepoSlug::parse("acme/web").unwrap(),
                default_branch: "main".to_owned(),
                branch_prefix: "gantry/".to_owned(),
                allowed_merge_methods: vec![MergeMethod::Squash],
            },
            sandbox: SandboxLimits {
                wall_time_s: 1800,
                kill_grace_s: 10,
            },
        },
        models: ModelTable::from([(
            ModelKey {
                sandbox: true,
                level: Level::Low,
                tier: Tier::Worker,
                critical: false,
            },
            ResolvedModel {
                provider_id: "anthropic".to_owned(),
                model: "claude".to_owned(),
                reasoning_effort: Effort::Medium,
                attempt_wall_s: 900,
            },
        )]),
        templates: TemplateRegistry,
        reservation: Arc::new(FixedReserve),
        sandbox_capacity: 4,
        consts: EngineConsts::default(),
    }
}

struct Harness {
    store: Store,
    forge: Arc<FakeForge>,
    runner: Arc<ScriptRunner>,
    snapshots: Arc<Snapshots>,
    delivery: DeliveryId,
}

impl Harness {
    async fn new() -> Self {
        let clock = Arc::new(FakeClock::new(T0));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(11))));
        let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
            .await
            .unwrap();
        let forge = Arc::new(FakeForge::new_in(store.clone()));
        forge.insert_project(Project {
            id: project_id(),
            repo: RepoSlug::parse("acme/web").unwrap(),
            installation_id: 1,
            bot_user_id: 2,
            default_branch: "main".into(),
            allowed_merge_methods: vec![MergeMethod::Squash],
            merge_queue_enabled: false,
            created_at: Timestamp::from_millis(T0),
            updated_at: Timestamp::from_millis(T0),
        });
        let inbox = Arc::new(FakeInbox::new_in(store.clone()));
        let engine = Engine::new(store.clone(), inbox);
        let delivery = engine
            .create_delivery(NewDelivery {
                id: "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap(),
                project_id: project_id(),
                title: "fix login timeout".to_owned(),
                user_text: "修复登录超时".to_owned(),
                level_cap: Level::Low,
                level_effective: Level::Low,
                origin: DeliveryOrigin::Conversation,
                source_message_id: "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD"
                    .parse::<MessageId>()
                    .unwrap(),
                issues: vec![],
                budget: budget_from_level(&LevelEntry {
                    graph_template: "low.single".to_owned(),
                    budget: LevelBudget {
                        tokens_by_tier: TierBudget {
                            utility: 50_000,
                            planner: 100_000,
                            worker: 1_000_000,
                        },
                        nodes: 8,
                        rounds: 0,
                        wall_time_s: 3600,
                        concurrency: 1,
                        forge_writes: 20,
                        forge_writes_per_hour: 30,
                    },
                }),
                config_rev: Seq::new(1).unwrap(),
                actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
            })
            .await
            .unwrap();
        Self {
            runner: Arc::new(ScriptRunner::new(store.clone())),
            snapshots: Arc::new(Snapshots::default()),
            store,
            forge,
            delivery,
        }
    }

    fn actor(&self) -> DeliveryActor {
        DeliveryActor::new(
            self.delivery,
            ActorDeps {
                store: self.store.clone(),
                inbox: Arc::new(FakeInbox::new_in(self.store.clone())),
                outbox: self.forge.clone(),
                effects: self.runner.clone(),
                snapshots: self.snapshots.clone(),
                cas: Arc::new(self.store.cas()),
            },
            config(),
        )
    }

    /// 把路由器读到的事件全部投给执行体，直到没有新事件；之后推进游标。
    async fn pump(&self, router: &mut EventRouter, actor: &mut DeliveryActor) {
        let active = BTreeSet::from([self.delivery]);
        loop {
            let dispatches = router.poll(&active).await.unwrap();
            if dispatches.is_empty() {
                break;
            }
            for d in dispatches {
                actor.handle(d.input, d.identity).await.unwrap();
                router.processed(d.delivery, d.seq);
            }
        }
        router.advance_cursor().await.unwrap();
    }

    async fn settle(&self, actor: &mut DeliveryActor) {
        actor
            .handle(
                Input::Tick(TickReason::Settle),
                InputIdentity::Tick(TickReason::Settle),
            )
            .await
            .unwrap();
    }

    async fn state(&self) -> DeliveryState {
        gantry_engine::engine::load_delivery_state(&self.store, &self.delivery)
            .await
            .unwrap()
            .unwrap()
    }

    /// 模拟 forge 执行器：认领并完成全部排队动作；create_pr 绑定 PR 12。
    async fn execute_outbox(&self) {
        loop {
            let mut tx = self.store.begin_write().await.unwrap();
            let claimed = tx
                .outbox()
                .claim(ClaimOpts {
                    lease: Duration::from_secs(60),
                    exclude_deliveries: BTreeSet::new(),
                })
                .await
                .unwrap();
            let Some(claimed) = claimed else {
                tx.rollback().await.unwrap();
                return;
            };
            let action = claimed.action;
            let github_ref = GithubRef {
                bot: None,
                pr: (action.kind == ForgeActionKind::CreatePr).then(|| GithubPrRef {
                    number: 12,
                    node_id: "PR_x".into(),
                    head_sha: oid(COMMIT),
                }),
                marker: None,
                recorded_at: None,
            };
            tx.outbox().complete(&action.id, github_ref).await.unwrap();
            tx.commit().await.unwrap();
        }
    }

    /// repo 写 patch.validated，AttemptRunner 回报终结。
    async fn finalize(&self, actor: &mut DeliveryActor, plan: &AttemptPlan) {
        self.runner
            .write(
                "patch.validated",
                &plan.attempt_id.to_string(),
                ActorKind::Engine,
                self.delivery,
                json!({"job_id": plan.attempt_id, "sha256": PATCH_SHA, "base_sha": BASE_SHA, "result_tree": TREE,
                       "verdict": "allow", "files": [], "reasons": [], "config_rev": 1}),
            )
            .await;
        let finalized = Finalized {
            attempt_id: plan.attempt_id,
            node_id: plan.node_id,
            status: AttemptStatus::Succeeded,
            failure_class: None,
            failure_detail: None,
            result_ref: Some(serde_json::from_value(json!(PATCH_SHA)).unwrap()),
            usage_metered: TokenUsage::default(),
            weighted_tokens: 0,
            weight_audit: serde_json::from_value(json!({"basis": "job_token", "config_rev": 1, "weights": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25}})).unwrap(),
            wall_time_s: 60,
            verdict: None,
            patch: PatchResult::Committed {
                commit: oid(COMMIT),
                tree: oid(TREE),
                sha256: serde_json::from_value(json!(PATCH_SHA)).unwrap(),
            },
            proposals: vec![],
            signals: OutputSignals::default(),
        };
        actor
            .handle(
                Input::Report(EffectReport::AttemptFinalized(finalized)),
                InputIdentity::Report(EffectKey::Run(plan.attempt_id)),
            )
            .await
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// §12.2"关闭客户端任务继续"：没有任何订阅者，交付单元照常推进到 closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delivery_reaches_closed_without_any_client() {
    let h = Harness::new().await;
    let mut router = EventRouter::open(h.store.clone()).await.unwrap();
    let mut actor = h.actor();

    // 建单后的第一次求值：T2、建图、入队、建工作树、取锁、派发。
    h.settle(&mut actor).await;
    h.pump(&mut router, &mut actor).await;
    let plans = h.runner.plans();
    assert_eq!(plans.len(), 1, "exactly one attempt dispatched");
    assert!(!actor.is_degraded());

    // 终结 → T6 → 判定并入队 push_branch / create_pr。
    h.finalize(&mut actor, &plans[0]).await;
    h.pump(&mut router, &mut actor).await;
    assert_eq!(h.state().await.delivery.stage, Stage::Propose);
    let kinds: Vec<_> = h.forge.outbox_list().iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![ForgeActionKind::PushBranch, ForgeActionKind::CreatePr]
    );

    // forge 执行成功；快照绑定且 head = 已推送提交 → T8。
    h.execute_outbox().await;
    *h.snapshots.0.lock().unwrap() = Some(snapshot("open", false));
    h.pump(&mut router, &mut actor).await;
    assert_eq!(h.state().await.delivery.stage, Stage::Review);

    // CI 通过（快照刷新）→ T10 → 判定 merge_pr → 请求审批 → 上报 approval_needed。
    *h.snapshots.0.lock().unwrap() = Some(snapshot("open", true));
    h.settle(&mut actor).await;
    h.pump(&mut router, &mut actor).await;
    let state = h.state().await;
    assert_eq!(state.delivery.stage, Stage::Merge);
    assert!(state
        .open_inbox
        .values()
        .any(|i| i.kind == InboxKind::ApprovalNeeded));
    assert!(!h
        .forge
        .outbox_list()
        .iter()
        .any(|a| a.kind == ForgeActionKind::MergePr));

    // 授予 → 再次判定 → 入队 merge_pr（带审批 ID）。
    *h.runner.granted.lock().unwrap() = true;
    h.runner
        .write(
            "approval.granted",
            APPROVAL,
            ActorKind::Device,
            h.delivery,
            json!({"approval_id": APPROVAL, "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
                   "approval_pubkey_fpr": PATCH_SHA, "payload_sha256": PATCH_SHA}),
        )
        .await;
    h.pump(&mut router, &mut actor).await;
    let merge = h
        .forge
        .outbox_list()
        .into_iter()
        .find(|a| a.kind == ForgeActionKind::MergePr)
        .expect("merge_pr enqueued after approval");
    assert_eq!(merge.requires_approval, Some(APPROVAL.parse().unwrap()));

    // 合并 → T12 → closed，释放工作树。
    h.execute_outbox().await;
    *h.snapshots.0.lock().unwrap() = Some(snapshot("merged", true));
    h.settle(&mut actor).await;
    h.pump(&mut router, &mut actor).await;
    assert_eq!(h.state().await.delivery.stage, Stage::Closed);
    assert_eq!(
        h.runner
            .count(|e| matches!(e, Effect::RemoveWorktree { .. })),
        1
    );

    // 游标追上日志头。
    assert_eq!(
        h.store.cursors().load(CURSOR).await.unwrap(),
        h.store.events().head().await.unwrap()
    );
}

// ---------------------------------------------------------------------------
// §12.2"两段式判定"：JudgeAction 发出后、回报前崩溃，重启后同一幂等键只有一行
// ---------------------------------------------------------------------------

#[tokio::test]
async fn judge_crash_then_recover_enqueues_once() {
    let h = Harness::new().await;
    let mut router = EventRouter::open(h.store.clone()).await.unwrap();
    let mut actor = h.actor();
    h.settle(&mut actor).await;
    h.pump(&mut router, &mut actor).await;
    let plan = h.runner.plans().remove(0);

    // 判定请求发出后"崩溃"：没有回报，Outbox 为空。
    *h.runner.judge.lock().unwrap() = JudgeMode::Drop;
    h.finalize(&mut actor, &plan).await;
    h.pump(&mut router, &mut actor).await;
    assert_eq!(h.state().await.delivery.stage, Stage::Propose);
    assert!(h.forge.outbox_list().is_empty());
    let judged_before = h.runner.count(|e| matches!(e, Effect::JudgeAction { .. }));
    assert!(judged_before >= 1);
    drop(actor);

    // 重启：在途集合为空，Tick(Recover) 重新判定并入队。
    *h.runner.judge.lock().unwrap() = JudgeMode::Normal;
    let mut actor = h.actor();
    actor
        .handle(
            Input::Tick(TickReason::Recover),
            InputIdentity::Tick(TickReason::Recover),
        )
        .await
        .unwrap();
    let rows = h.forge.outbox_list();
    assert_eq!(rows.len(), 2);
    let keys: BTreeSet<_> = rows.iter().map(|a| a.idempotency_key.clone()).collect();
    assert_eq!(keys.len(), 2);

    // 再次恢复：同一幂等键不会多出一行，也不再判定。
    let judged = h.runner.count(|e| matches!(e, Effect::JudgeAction { .. }));
    actor
        .handle(
            Input::Tick(TickReason::Recover),
            InputIdentity::Tick(TickReason::Recover),
        )
        .await
        .unwrap();
    assert_eq!(h.forge.outbox_list().len(), 2);
    assert_eq!(
        h.runner.count(|e| matches!(e, Effect::JudgeAction { .. })),
        judged
    );
}

// ---------------------------------------------------------------------------
// §12.2"决策纯度"（收敛）：Tick(Settle) 在 MAX_SETTLE 内收敛，重复 Tick 不产生事件
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settle_converges_and_repeated_ticks_are_idempotent() {
    let h = Harness::new().await;
    let mut router = EventRouter::open(h.store.clone()).await.unwrap();
    let mut actor = h.actor();
    // 回报本身不触发重新求值；后续推进由路由器投来的事件驱动（worktree.created、worktree.locked）。
    h.settle(&mut actor).await;
    h.pump(&mut router, &mut actor).await;
    assert!(!actor.is_degraded());
    let head = h.store.events().head().await.unwrap();
    // 收敛后再 Tick：在途键挡住重复副作用，状态未变，不追加事件。
    h.settle(&mut actor).await;
    h.settle(&mut actor).await;
    assert_eq!(h.store.events().head().await.unwrap(), head);
    assert_eq!(h.runner.count(|e| matches!(e, Effect::RunAttempt(_))), 1);
}

// ---------------------------------------------------------------------------
// 事件路由与游标
// ---------------------------------------------------------------------------

/// 空闲执行体不拖住游标：只有"已投递未处理"的执行体决定低水位。
#[tokio::test]
async fn idle_actors_do_not_hold_the_cursor() {
    let h = Harness::new().await;
    let mut router = EventRouter::open(h.store.clone()).await.unwrap();
    let active = BTreeSet::from([h.delivery]);
    // 没有投递给任何执行体的事件（delivery.created 只折叠）：游标直接追到读到的位置。
    assert!(router.poll(&active).await.unwrap().is_empty());
    assert_eq!(
        router.advance_cursor().await.unwrap(),
        h.store.events().head().await.unwrap()
    );

    // 投递一条（worktree.created）但未处理：低水位停在它之前。
    let seq = h
        .runner
        .write(
            "worktree.created",
            WORKTREE,
            ActorKind::Engine,
            h.delivery,
            json!({"worktree_id": WORKTREE, "project_id": PROJECT, "delivery_id": h.delivery,
                   "branch": "gantry/x", "base_ref": "main", "base_sha": BASE_SHA}),
        )
        .await;
    let dispatched = router.poll(&active).await.unwrap();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(router.low_water(), Seq::new(seq.as_u64() - 1).unwrap());
    assert_eq!(
        router.advance_cursor().await.unwrap(),
        Seq::new(seq.as_u64() - 1).unwrap()
    );

    // 处理完后追上。
    router.processed(h.delivery, seq);
    assert_eq!(router.advance_cursor().await.unwrap(), seq);

    // 重启后从游标恢复：已处理的事件不再投递。
    let mut reopened = EventRouter::open(h.store.clone()).await.unwrap();
    assert!(reopened.poll(&active).await.unwrap().is_empty());
}

/// settings.updated 广播给所有活跃执行体。
#[tokio::test]
async fn settings_updated_is_broadcast() {
    let h = Harness::new().await;
    let mut router = EventRouter::open(h.store.clone()).await.unwrap();
    let other: DeliveryId = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC".parse().unwrap();
    let active = BTreeSet::from([h.delivery, other]);
    let _ = router.poll(&active).await.unwrap();
    let event = NewEvent {
        ty: "settings.updated".parse().unwrap(),
        subject: Subject::parse("settings").unwrap(),
        project_id: None,
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        causation_id: None,
        correlation_id: None,
        data: json!({"scope": "global", "resource": "levels", "schema_version": 1, "base_rev": 0,
                     "value": {}, "relaxes": [], "secret_changes": []}),
        dedup_key: None,
    };
    let mut tx = h.store.begin_write().await.unwrap();
    tx.append(event).await.unwrap();
    tx.commit().await.unwrap();
    let dispatched = router.poll(&active).await.unwrap();
    let targets: BTreeSet<_> = dispatched.iter().map(|d| d.delivery).collect();
    assert_eq!(targets, active);
}

// ---------------------------------------------------------------------------
// 事务内读取（§7.1，#71 拍板 4）：持租约期间外部写被挡住
// ---------------------------------------------------------------------------

/// 执行体先拿写租约再从已提交日志折叠：租约期间别的写者拿不到写事务，日志不会变。
#[tokio::test]
async fn external_writes_wait_while_the_lease_is_held() {
    let h = Harness::new().await;
    let tx = h.store.begin_write().await.unwrap();
    let head = h.store.events().head().await.unwrap();
    let store = h.store.clone();
    let blocked = tokio::time::timeout(Duration::from_millis(100), store.begin_write()).await;
    assert!(blocked.is_err(), "a second writer must wait for the lease");
    assert_eq!(h.store.events().head().await.unwrap(), head);
    tx.rollback().await.unwrap();
    let second = tokio::time::timeout(Duration::from_millis(100), h.store.begin_write()).await;
    assert!(second.is_ok(), "the lease is released after rollback");
}
