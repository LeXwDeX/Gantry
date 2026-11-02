//! 归一化与自身事件归因的验收：forge.md §12 A 组"重复投递只产生一组 ForgeEvent"、
//! "同一事实不同 GUID 只有一个 `pr_synchronized`"、"乱序：`closed` 先于 `opened`；旧 head 的
//! `check_suite` 晚到 → `stale=true`"、"人推了一个提交 → `pr_synchronized(origin=external)`"、
//! "默认分支 push → `base_advanced`（含 `affected_prs`）"、"webhook 先于 `github_ref` 到达 →
//! 停放后归因为 `self`；`sender == bot` 仍无记录 → `self_unattributed` + `orphan_effect`"。
//!
//! 收件箱行直接写进 `forge_webhook_inbox`（接收端的校验与去重见 `tests/webhook.rs`）；
//! 回读、执行器都对 `FakeGitHub`（A 级）。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, ForgeEventKind, MergeMethod, Priority, Project};
use gantry_core::events::{Event, EventType, NewEvent, Subject};
use gantry_core::forge::DriftDetectedBy;
use gantry_core::forge::{ActionPayload, CreatePrPayload};
use gantry_core::ids::{ActionId, Id, IdGen, IdKind, Prj, ProjectId, Ulid};
use gantry_core::ports::{
    ActorCtx, DriftKind, EnqueueRequest, EnqueuedBy, EnqueuedByModule, OutboxPort, ReconcileTarget,
};
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::github::FakeGitHub;
use gantry_fakes::policy::FakePolicy;
use gantry_fakes::repo::FakeRepo;
use gantry_forge::webhook::{INBOX_TABLE, MODULE};
use gantry_forge::{
    module_spec, AuthError, CrashPoint, DispatchOutcome, Dispatcher, DispatcherDeps,
    DispatcherOptions, ExecutorDeps, ExecutorOptions, FaultInjector, Forge, ForgeConfig, InboxRow,
    InboxStatus, Permission, ProjectBinding, StaticTokenProvider, StepOutcome, TokenProvider,
    TokenPurpose,
};
use gantry_store::{Store, StoreDeps};
use serde_json::{json, Value};

const MS: i64 = 1_700_000_000_000;
const REPO: &str = "acme/shop-api";
const HEAD: &str = "gantry/42-fix-pagination";
const HUMAN: i64 = 1;

fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS as u64, n).expect("test id parts")
}

fn oid(seed: &str) -> GitOid {
    GitOid::parse(&sha256(seed.as_bytes()).to_hex()[..40]).expect("oid")
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(MS as u64, 1).expect("request id"),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

struct CrashAt(CrashPoint, AtomicBool);

impl FaultInjector for CrashAt {
    fn crash(&self, _action: &ActionId, point: CrashPoint) -> bool {
        point == self.0 && !self.1.swap(true, Ordering::SeqCst)
    }
}

struct Harness {
    github: FakeGitHub,
    store: Store,
    clock: Arc<FakeClock>,
    ids: Arc<IdGen>,
    forge: Forge,
    bot: i64,
    next_delivery: std::cell::Cell<u32>,
    tokens: Arc<dyn TokenProvider>,
}

impl Harness {
    fn new() -> Self {
        Self::new_with_tokens(Arc::new(StaticTokenProvider::new(SecretString::new(
            "ghs_read".into(),
        ))))
    }

    fn new_with_tokens(tokens: Arc<dyn TokenProvider>) -> Self {
        let github = FakeGitHub::start();
        let (_, bot) = github.bot();
        let clock = Arc::new(FakeClock::new(MS));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(11))));
        let store = Store::open_in_memory_sync_with(
            StoreDeps {
                clock: clock.clone(),
                ids: ids.clone(),
            },
            &[module_spec()],
        )
        .unwrap();
        let project = Project {
            id: id::<Prj>(1),
            repo: RepoSlug::parse(REPO).unwrap(),
            installation_id: 42,
            bot_user_id: bot,
            default_branch: "main".into(),
            allowed_merge_methods: vec![MergeMethod::Squash],
            merge_queue_enabled: false,
            created_at: Timestamp::from_millis(MS),
            updated_at: Timestamp::from_millis(MS),
        };
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
        Self {
            github,
            store,
            clock,
            ids,
            forge,
            bot,
            next_delivery: std::cell::Cell::new(1),
            tokens,
        }
    }

    fn dispatcher(&self) -> Dispatcher {
        self.forge.dispatcher(DispatcherDeps {
            tokens: self.tokens.clone(),
            clock: self.clock.clone(),
            api_base: self.github.base_url(),
            options: DispatcherOptions {
                http_timeout: Duration::from_secs(5),
                ..DispatcherOptions::default()
            },
        })
    }

    /// 经执行器创建一个 PR（Gantry 自己的 PR，写入归因索引与绑定行）。
    async fn create_own_pr(&self) -> (ActionId, u64) {
        self.create_own_pr_with(None).await
    }

    /// `crash`：执行器在该点崩溃（PR 可能已建好，但 `github_ref` 未落库）。
    async fn create_own_pr_with(&self, crash: Option<CrashPoint>) -> (ActionId, u64) {
        self.github
            .set_branch(REPO, HEAD, &oid("head-1").to_string());
        let req = EnqueueRequest {
            project_id: id::<Prj>(1),
            delivery_id: id(2),
            payload: ActionPayload::CreatePr(CreatePrPayload {
                repo: RepoSlug::parse(REPO).unwrap(),
                head: HEAD.into(),
                base: "main".into(),
                title: "Fix pagination".into(),
                body: "Fixes the off-by-one.".into(),
                draft: false,
                closes: vec![],
                expected_head_oid: oid("head-1"),
                maintainer_can_modify: false,
            }),
            requires_approval: None,
            depends_on: vec![],
            priority: Priority::High,
            not_before: None,
            epoch: 0,
            enqueued_by: EnqueuedBy {
                module: EnqueuedByModule::Engine,
                causation_id: None,
            },
        };
        let action = self.forge.enqueue(req, &ctx()).await.unwrap().action.id;
        let root = std::env::temp_dir().join("gantry-forge-normalize-unused");
        let executor = self
            .forge
            .executor(ExecutorDeps {
                policy: Arc::new(FakePolicy::new_in(self.store.clone())),
                tokens: Arc::new(StaticTokenProvider::new(SecretString::new("ghs_w".into()))),
                api_base: self.github.base_url(),
                ids: self.ids.clone(),
                options: ExecutorOptions {
                    http_timeout: Duration::from_secs(5),
                    ..ExecutorOptions::default()
                },
                faults: crash.map(|point| {
                    Arc::new(CrashAt(point, AtomicBool::new(false))) as Arc<dyn FaultInjector>
                }),
                repo: Arc::new(FakeRepo::new(Timestamp::from_millis(MS))),
                git_base: format!("file://{}", root.display()),
            })
            .unwrap();
        match crash {
            None => {
                let steps = executor.drain().await.unwrap();
                assert_eq!(steps[0].outcome, StepOutcome::Succeeded);
            }
            Some(_) => assert!(executor.drain().await.is_err(), "injected crash"),
        }
        let number = self.github.pulls(REPO)[0]["number"].as_u64().unwrap();
        (action, number)
    }

    /// 模拟接收端落盘一行投递。返回 delivery id。
    async fn deliver(&self, event: &str, payload: Value) -> String {
        let n = self.next_delivery.get();
        self.next_delivery.set(n + 1);
        let delivery = format!("d-{n:04}");
        let row = InboxRow {
            delivery_id: delivery.clone(),
            event: event.into(),
            action: payload
                .get("action")
                .and_then(Value::as_str)
                .map(str::to_owned),
            installation_id: Some(42),
            repo: Some(REPO.into()),
            received_at: self.clock.now_timestamp(),
            body: Some(payload.to_string()),
            status: InboxStatus::Received,
            attempts: 0,
            retry_at: None,
            last_error: None,
        };
        let mut tx = self.store.begin_write().await.unwrap();
        tx.module_conn(MODULE)
            .insert(INBOX_TABLE, &delivery, &row)
            .unwrap();
        tx.commit().await.unwrap();
        delivery
    }

    fn pr_payload(&self, action: &str, number: u64, sender: i64) -> Value {
        json!({
            "action": action,
            "number": number,
            "pull_request": {"number": number},
            "installation": {"id": 42},
            "repository": {"full_name": REPO},
            "sender": {"login": if sender == self.bot { "gantry-test-bot[bot]" } else { "octocat" },
                       "id": sender, "type": if sender == self.bot { "Bot" } else { "User" }}
        })
    }

    async fn forge_events(&self) -> Vec<Event<Value>> {
        let subject = Subject::parse(&id::<Prj>(1).to_string()).unwrap();
        self.store
            .events()
            .by_subject(&subject)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| {
                matches!(
                    event.ty,
                    EventType::Forge(_) | EventType::ForgeSynced | EventType::ForgeDriftDetected
                )
            })
            .collect()
    }

    async fn kinds(&self) -> Vec<String> {
        self.forge_events()
            .await
            .iter()
            .map(|event| event.ty.as_str().to_owned())
            .collect()
    }
}

fn of_kind(events: &[Event<Value>], kind: ForgeEventKind) -> Vec<&Event<Value>> {
    events
        .iter()
        .filter(|event| event.ty == EventType::Forge(kind))
        .collect()
}

#[tokio::test]
async fn own_pr_opened_is_self_bound_and_seeds_the_initial_snapshot() {
    let h = Harness::new();
    let (action, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    let out = h.dispatcher().drain().await.unwrap();
    assert!(matches!(
        out.as_slice(),
        [DispatchOutcome::Done { events: 2, .. }]
    ));

    let events = h.forge_events().await;
    assert_eq!(h.kinds().await, ["forge.pr_opened", "forge.synced"]);
    let opened = &events[0];
    assert_eq!(opened.data["origin"], "self");
    assert_eq!(opened.data["caused_by_action"], json!(action.to_string()));
    assert_eq!(
        opened.data["delivery_id"],
        json!(id::<gantry_core::ids::Dlv>(2).to_string())
    );
    assert_eq!(
        opened.correlation_id.as_deref(),
        Some(id::<gantry_core::ids::Dlv>(2).to_string().as_str())
    );
    assert_eq!(opened.data["data"]["created_by_gantry"], json!(true));
    assert_eq!(
        opened.data["dedup_key"].as_str().unwrap().split(':').next(),
        Some("pr_opened")
    );
    let synced = &events[1];
    assert_eq!(synced.data["changes"], json!([]));
    assert_eq!(synced.data["initial"]["type"], "pr");
    assert_eq!(synced.data["initial"]["state"], "open");
    assert_eq!(
        synced.data["initial"]["head_sha"],
        json!(oid("head-1").to_string())
    );
}

#[tokio::test]
async fn duplicate_and_same_fact_deliveries_produce_one_event_each() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    // 不同 GUID 的同一 opened：快照无差异，不再发。
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    assert!(matches!(
        h.dispatcher().drain().await.unwrap().as_slice(),
        [DispatchOutcome::Done { events: 0, .. }]
    ));

    // 人推了一个提交：两条不同 GUID 的 synchronize → 只有一个 pr_synchronized。
    h.github.set_pull_head(REPO, n, &oid("head-2").to_string());
    h.deliver("pull_request", h.pr_payload("synchronize", n, HUMAN))
        .await;
    h.deliver("pull_request", h.pr_payload("synchronize", n, HUMAN))
        .await;
    h.dispatcher().drain().await.unwrap();

    let events = h.forge_events().await;
    let synced = of_kind(&events, ForgeEventKind::PrSynchronized);
    assert_eq!(synced.len(), 1);
    assert_eq!(synced[0].data["origin"], "external");
    assert_eq!(synced[0].data["data"]["head_epoch"], json!(1));
    assert_eq!(
        synced[0].data["data"]["previous_head_sha"],
        json!(oid("head-1").to_string())
    );
    assert_eq!(
        synced[0].data["dedup_key"],
        json!(format!("pr_sync:{REPO}:{n}:1"))
    );
    assert_eq!(of_kind(&events, ForgeEventKind::PrOpened).len(), 1);
    // forge.synced：首次 initial + head 变化一次。
    let synced_events: Vec<_> = events
        .iter()
        .filter(|event| event.ty == EventType::ForgeSynced)
        .collect();
    assert_eq!(synced_events.len(), 2);
    assert_eq!(synced_events[1].data["changes"][0]["field"], "head_sha");
}

#[tokio::test]
async fn closed_before_opened_still_yields_opened_then_closed() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.github.set_pull_state(REPO, n, "closed");
    h.deliver("pull_request", h.pr_payload("closed", n, HUMAN))
        .await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    assert_eq!(
        h.kinds().await,
        ["forge.pr_opened", "forge.pr_closed", "forge.synced"]
    );
    let events = h.forge_events().await;
    assert_eq!(events[2].data["initial"]["state"], "closed");
    assert_eq!(events[1].data["origin"], "external");
}

#[tokio::test]
async fn late_check_suite_for_an_old_head_is_stale() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.github.set_pull_head(REPO, n, &oid("head-2").to_string());
    h.deliver("pull_request", h.pr_payload("synchronize", n, HUMAN))
        .await;
    h.dispatcher().drain().await.unwrap();

    let suite = |id: u64, head: &str| {
        json!({
            "action": "completed",
            "check_suite": {
                "id": id,
                "head_sha": oid(head).to_string(),
                "conclusion": "failure",
                "updated_at": "2023-11-14T23:00:00Z",
                "app": {"id": 15368, "slug": "github-actions", "owner": {"login": "github", "id": 9919, "type": "Organization"}},
                "pull_requests": [{"number": n}]
            },
            "installation": {"id": 42},
            "repository": {"full_name": REPO},
            "sender": {"login": "github-actions[bot]", "id": 41898282, "type": "Bot"}
        })
    };
    h.deliver("check_suite", suite(7, "head-1")).await;
    h.deliver("check_suite", suite(8, "head-2")).await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    let suites = of_kind(&events, ForgeEventKind::CheckSuiteCompleted);
    assert_eq!(suites.len(), 2);
    assert_eq!(suites[0].data["stale"], json!(true));
    assert_eq!(suites[0].data["data"]["is_current_head"], json!(false));
    assert_eq!(suites[1].data["stale"], json!(false));
    // CI 结果不是自身事件（FA §4.6）。
    assert!(suites
        .iter()
        .all(|event| event.data["origin"] == "external"));
}

#[tokio::test]
async fn bot_event_without_record_parks_then_is_self_unattributed_with_drift() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();

    // bot 推了一个提交，但没有任何 push_branch 记录（例如 Outbox 从旧备份恢复）。
    h.github.set_pull_head(REPO, n, &oid("head-3").to_string());
    h.deliver("pull_request", h.pr_payload("synchronize", n, h.bot))
        .await;
    assert!(matches!(
        h.dispatcher().drain().await.unwrap().as_slice(),
        [DispatchOutcome::Parked { .. }]
    ));
    assert!(of_kind(&h.forge_events().await, ForgeEventKind::PrSynchronized).is_empty());

    h.clock.advance_ms(30_000);
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    let synced = of_kind(&events, ForgeEventKind::PrSynchronized);
    assert_eq!(synced.len(), 1);
    assert_eq!(synced[0].data["origin"], "self_unattributed");
    let drift: Vec<_> = events
        .iter()
        .filter(|event| event.ty == EventType::ForgeDriftDetected)
        .collect();
    assert_eq!(drift.len(), 1);
    assert_eq!(drift[0].data["kind"], "orphan_effect");
}

#[tokio::test]
async fn unbound_pull_request_parks_then_is_dropped() {
    let h = Harness::new();
    let n = h.github.insert_pull(
        REPO,
        "feature/human",
        "main",
        ("octocat", HUMAN),
        "human pr",
    );
    h.deliver("pull_request", h.pr_payload("opened", n, HUMAN))
        .await;
    assert!(matches!(
        h.dispatcher().drain().await.unwrap().as_slice(),
        [DispatchOutcome::Parked { .. }]
    ));
    h.clock.advance_ms(30_000);
    assert!(matches!(
        h.dispatcher().drain().await.unwrap().as_slice(),
        [DispatchOutcome::Dropped { .. }]
    ));
    assert!(h.forge_events().await.is_empty());
}

#[tokio::test]
async fn default_branch_push_is_base_advanced_with_affected_prs() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.github
        .set_branch(REPO, "main", &oid("main-2").to_string());
    h.deliver(
        "push",
        json!({
            "ref": "refs/heads/main",
            "before": oid("main-1").to_string(),
            "after": oid("main-2").to_string(),
            "installation": {"id": 42},
            "repository": {"full_name": REPO},
            "sender": {"login": "octocat", "id": HUMAN, "type": "User"}
        }),
    )
    .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    let base = of_kind(&events, ForgeEventKind::BaseAdvanced);
    assert_eq!(base.len(), 1);
    assert_eq!(base[0].data["data"]["affected_prs"], json!([n]));
    assert_eq!(
        base[0].data["data"]["before"],
        json!(oid("main-1").to_string())
    );
    assert_eq!(
        base[0].data["dedup_key"],
        json!(format!("base:{REPO}:main:{}", oid("main-2")))
    );
}

/// webhook 先于 `github_ref` 落库到达：执行器在变更成功、记录之前崩溃，归因仍是 `self`
/// （预期效果是 write-ahead 写的，FA §4.2），不误判 `external`。
#[tokio::test]
async fn webhook_before_github_ref_is_still_self() {
    let h = Harness::new();
    let (action, n) = h.create_own_pr_with(Some(CrashPoint::AfterApply)).await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    let opened = of_kind(&events, ForgeEventKind::PrOpened);
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].data["origin"], "self");
    assert_eq!(
        opened[0].data["caused_by_action"],
        json!(action.to_string())
    );
}

/// §12 A：丢弃一个 `pull_request.closed` 投递 → 巡检补发 `pr_merged(source=reconcile)` 并追加
/// `forge.synced`；`dedup_key` 与 webhook 路径一致，再投递原 webhook 不产生第二个事件。
#[tokio::test]
async fn reconcile_recovers_a_dropped_close_and_dedups_with_the_late_webhook() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();

    // 有人在 GitHub 上合并了 PR，但 closed 投递丢了。
    h.github.set_pull_merged(REPO, n);
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Project(id::<Prj>(1)),
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert_eq!(
        report.emitted.len(),
        3,
        "pr_merged + drift(pr_state_diverged) + forge.synced"
    );
    assert_eq!(report.drifts, vec![DriftKind::PrStateDiverged]);
    let events = h.forge_events().await;
    let merged = of_kind(&events, ForgeEventKind::PrMerged);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].data["source"], "reconcile");
    assert_eq!(merged[0].data["delivery_ids"], json!([]));
    let merge_sha = merged[0].data["data"]["merge_commit_sha"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        merged[0].data["dedup_key"],
        json!(format!("pr_merged:{REPO}:{n}:{merge_sha}"))
    );
    let drift: Vec<_> = events
        .iter()
        .filter(|event| event.ty == EventType::ForgeDriftDetected)
        .collect();
    assert_eq!(drift[0].data["kind"], "pr_state_diverged");
    assert_eq!(drift[0].data["synthesized_events"], json!(["pr_merged"]));
    assert_eq!(drift[0].data["actual"]["state"], "merged");
    let synced = events.last().unwrap();
    assert_eq!(synced.ty, EventType::ForgeSynced);
    assert_eq!(synced.data["source"], "reconcile");
    assert_eq!(synced.data["changes"][0]["field"], "state");
    assert_eq!(synced.data["changes"][0]["to"], "merged");

    // 原 webhook 晚到：快照已更新，不产生第二个事件。
    let before = h.forge_events().await.len();
    h.deliver("pull_request", h.pr_payload("closed", n, HUMAN))
        .await;
    assert!(matches!(
        h.dispatcher().drain().await.unwrap().as_slice(),
        [DispatchOutcome::Done { events: 0, .. }]
    ));
    assert_eq!(h.forge_events().await.len(), before);

    // 已合并的 PR 不再在巡检范围内。
    let again = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Project(id::<Prj>(1)),
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(again.emitted.is_empty());
}

/// §12 A：人在 GitHub 上推了一个提交 → `forge.drift_detected(head_diverged)` +
/// `pr_synchronized(origin=external)`。
#[tokio::test]
async fn human_push_after_gantry_push_is_head_diverged() {
    let h = Harness::new();
    let (action, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    // 白盒：写入"该分支最近一次成功的 push_branch"（执行器成功推送时写的行，见 effects.rs）。
    let mut tx = h.store.begin_write().await.unwrap();
    tx.module_conn(MODULE)
        .put(
            "forge_effect_index",
            &format!("push|{REPO}|{HEAD}"),
            &json!({
                "action_id": action.to_string(),
                "delivery_id": id::<gantry_core::ids::Dlv>(2).to_string(),
                "head_sha": oid("head-1").to_string(),
            }),
        )
        .unwrap();
    tx.commit().await.unwrap();

    h.github
        .set_pull_head(REPO, n, &oid("human-commit").to_string());
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert_eq!(report.drifts, vec![DriftKind::HeadDiverged]);
    let events = h.forge_events().await;
    let synced = of_kind(&events, ForgeEventKind::PrSynchronized);
    assert_eq!(synced.len(), 1);
    assert_eq!(synced[0].data["origin"], "external");
    assert_eq!(synced[0].data["source"], "reconcile");
    let drift: Vec<_> = events
        .iter()
        .filter(|event| event.ty == EventType::ForgeDriftDetected)
        .collect();
    assert_eq!(drift.len(), 1);
    assert_eq!(drift[0].data["kind"], "head_diverged");
    assert_eq!(
        drift[0].data["expected"]["head_sha"],
        json!(oid("head-1").to_string())
    );
    assert_eq!(
        drift[0].data["expected"]["basis_action"],
        json!(action.to_string())
    );
    assert_eq!(
        drift[0].data["actual"]["head_sha"],
        json!(oid("human-commit").to_string())
    );
    assert_eq!(
        drift[0].data["synthesized_events"],
        json!(["pr_synchronized"])
    );
    assert_eq!(drift[0].data["detected_by"], "patrol");
}

/// 评审：`review_submitted`（针对旧 head 的标 `stale`）、`forge.synced(review_state)`、
/// `review_dismissed`。
#[tokio::test]
async fn reviews_are_normalized_and_drive_review_state() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();

    let review_id = h.github.add_review(
        REPO,
        n,
        ("alice", 7),
        "CHANGES_REQUESTED",
        &oid("head-1").to_string(),
    );
    let mut payload = h.pr_payload("submitted", n, 7);
    payload["review"] = json!({
        "id": review_id,
        "state": "changes_requested",
        "commit_id": oid("head-1").to_string(),
        "submitted_at": "2023-11-14T23:00:00Z",
        "user": {"login": "alice", "id": 7, "type": "User"},
        "body": "please add a test"
    });
    h.deliver("pull_request_review", payload).await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    let reviews = of_kind(&events, ForgeEventKind::ReviewSubmitted);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].data["data"]["state"], "changes_requested");
    assert_eq!(reviews[0].data["stale"], json!(false));
    assert_eq!(reviews[0].data["origin"], "external");
    assert_eq!(
        reviews[0].data["dedup_key"],
        json!(format!("review:{REPO}:{review_id}"))
    );
    let synced = events.last().unwrap();
    assert_eq!(synced.ty, EventType::ForgeSynced);
    assert_eq!(synced.data["changes"][0]["field"], "review_state");
    assert_eq!(synced.data["changes"][0]["to"], "changes_requested");

    let mut dismissed = h.pr_payload("dismissed", n, HUMAN);
    dismissed["review"] = json!({"id": review_id, "state": "dismissed"});
    h.deliver("pull_request_review", dismissed).await;
    h.dispatcher().drain().await.unwrap();
    assert_eq!(
        of_kind(&h.forge_events().await, ForgeEventKind::ReviewDismissed).len(),
        1
    );
}

#[tokio::test]
async fn check_run_for_an_old_head_is_stale() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.github.set_pull_head(REPO, n, &oid("head-2").to_string());
    h.deliver("pull_request", h.pr_payload("synchronize", n, HUMAN))
        .await;
    h.dispatcher().drain().await.unwrap();
    h.deliver(
        "check_run",
        json!({
            "action": "completed",
            "check_run": {
                "id": 31, "name": "test", "head_sha": oid("head-1").to_string(),
                "conclusion": "failure", "completed_at": "2023-11-14T23:10:00Z",
                "check_suite": {"id": 7},
                "app": {"id": 15368, "slug": "github-actions"},
                "output": {"title": "1 failed", "summary": "x".repeat(10), "annotations_count": 2},
                "pull_requests": [{"number": n}]
            },
            "installation": {"id": 42},
            "repository": {"full_name": REPO},
            "sender": {"login": "github-actions[bot]", "id": 41898282, "type": "Bot"}
        }),
    )
    .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    let runs = of_kind(&events, ForgeEventKind::CheckRunCompleted);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].data["stale"], json!(true));
    assert_eq!(runs[0].data["data"]["annotations_count"], json!(2));
    assert_eq!(runs[0].data["origin"], "external");
}

#[tokio::test]
async fn installation_and_repository_events_are_system_events() {
    let h = Harness::new();
    h.deliver(
        "installation",
        json!({
            "action": "new_permissions_accepted",
            "installation": {"id": 42},
            "sender": {"login": "octocat", "id": HUMAN, "type": "User"}
        }),
    )
    .await;
    h.deliver(
        "repository",
        json!({
            "action": "edited",
            "changes": {"default_branch": {"from": "master"}},
            "repository": {"full_name": REPO, "default_branch": "main"},
            "installation": {"id": 42},
            "sender": {"login": "octocat", "id": HUMAN, "type": "User"}
        }),
    )
    .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    let installation = of_kind(&events, ForgeEventKind::InstallationChanged);
    assert_eq!(installation.len(), 1);
    assert_eq!(
        installation[0].data["data"]["action"],
        "new_permissions_accepted"
    );
    assert!(installation[0].data["dedup_key"]
        .as_str()
        .unwrap()
        .starts_with("inst:42:new_permissions_accepted:"));
    let repo = of_kind(&events, ForgeEventKind::RepoChanged);
    assert_eq!(repo.len(), 1);
    assert_eq!(repo[0].data["data"]["default_branch"], "main");
    // ForgeEventEnvelope.occurred_at 序列化为三位毫秒 + `Z`（§12 A）。
    for event in events
        .iter()
        .filter(|event| matches!(event.ty, EventType::Forge(_)))
    {
        let occurred = event.data["occurred_at"].as_str().unwrap();
        assert_eq!(
            occurred.len(),
            "2023-11-14T22:13:20.000Z".len(),
            "{occurred}"
        );
        assert!(occurred.ends_with('Z') && occurred.as_bytes()[19] == b'.');
    }
}

/// 分发器启动扫描（forge.md §7.4 末段）：崩溃时停在 processing 的行重新入队并处理。
#[tokio::test]
async fn recover_requeues_rows_left_in_processing() {
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    let delivery = h
        .deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    // 模拟"标 processing 后崩溃"。
    let mut tx = h.store.begin_write().await.unwrap();
    let mut row: InboxRow = tx
        .module_conn(MODULE)
        .get(INBOX_TABLE, &delivery)
        .unwrap()
        .unwrap();
    row.status = InboxStatus::Processing;
    tx.module_conn(MODULE)
        .put(INBOX_TABLE, &delivery, &row)
        .unwrap();
    tx.commit().await.unwrap();
    assert!(h.dispatcher().drain().await.unwrap().is_empty());

    assert_eq!(h.dispatcher().recover().await.unwrap(), 1);
    h.dispatcher().drain().await.unwrap();
    assert_eq!(
        of_kind(&h.forge_events().await, ForgeEventKind::PrOpened).len(),
        1
    );
}

/// `ForgeReader::reconcile_now` 交给分发器（on_demand）。
#[tokio::test]
async fn reconcile_now_runs_through_the_reader() {
    use gantry_core::ports::ForgeReader;
    let h = Harness::new();
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    h.github.set_pull_state(REPO, n, "closed");
    let reader = h.forge.reader(gantry_forge::ReaderDeps {
        tokens: Arc::new(StaticTokenProvider::new(SecretString::new(
            "ghs_read".into(),
        ))),
        api_base: h.github.base_url(),
        clock: h.clock.clone(),
        http_timeout: Duration::from_secs(5),
        reconciler: Some(Arc::new(h.dispatcher())),
        mirror: None,
    });
    let report = reader
        .reconcile_now(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(report.drifts, vec![DriftKind::PrStateDiverged]);
    let events = h.forge_events().await;
    let closed = of_kind(&events, ForgeEventKind::PrClosed);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].data["source"], "reconcile");
    let drift: Vec<_> = events
        .iter()
        .filter(|event| event.ty == EventType::ForgeDriftDetected)
        .collect();
    assert_eq!(drift[0].data["detected_by"], "on_demand");
}

// ---------------------------------------------------------------------------
// #409：当前 PR head 的检查汇总（REST check-runs）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingTokens {
    calls: Mutex<Vec<TokenPurpose>>,
    invalidations: AtomicUsize,
}

#[async_trait::async_trait]
impl TokenProvider for RecordingTokens {
    async fn token(
        &self,
        _project: &ProjectId,
        purpose: TokenPurpose,
    ) -> Result<Arc<SecretString>, AuthError> {
        self.calls.lock().unwrap().push(purpose);
        Ok(Arc::new(SecretString::new("ghs_read".into())))
    }

    fn invalidate(&self, _project: &ProjectId) {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
    }
}

fn check_run(
    id: u64,
    app: u64,
    name: &str,
    status: &str,
    conclusion: Value,
    head_sha: &str,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "head_sha": head_sha,
        "status": status,
        "conclusion": conclusion,
        "check_suite": {"id": id},
        "app": {"id": app, "slug": format!("app-{app}")},
    })
}

fn checks(passed: u32, failed: u32, pending: u32, required_ok: bool) -> Value {
    json!({
        "passed": passed,
        "failed": failed,
        "pending": pending,
        "required_ok": required_ok,
    })
}

fn synced_events(events: &[Event<Value>]) -> Vec<&Event<Value>> {
    events
        .iter()
        .filter(|event| event.ty == EventType::ForgeSynced)
        .collect()
}

async fn seed_project(h: &Harness) {
    let mut tx = h.store.begin_write().await.unwrap();
    tx.append(NewEvent {
        ty: EventType::ProjectAdded,
        subject: Subject::parse(&id::<Prj>(1).to_string()).unwrap(),
        project_id: Some(id::<Prj>(1)),
        actor: Actor::new(ActorKind::System, "api"),
        causation_id: None,
        correlation_id: None,
        data: json!({
            "id": id::<Prj>(1),
            "repo": REPO,
            "installation_id": 42,
            "bot_user_id": 9,
            "default_branch": "main",
            "allowed_merge_methods": ["squash"],
            "merge_queue_enabled": false,
            "created_at": "2023-11-14T22:13:20.000Z",
            "updated_at": "2023-11-14T22:13:20.000Z",
        }),
        dedup_key: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn reader_snapshot(h: &Harness, number: u64) -> gantry_core::entities::PullRequestRef {
    use gantry_core::ports::ForgeReader;
    let reader = h.forge.reader(gantry_forge::ReaderDeps {
        tokens: h.tokens.clone(),
        api_base: h.github.base_url(),
        clock: h.clock.clone(),
        http_timeout: Duration::from_secs(5),
        reconciler: None,
        mirror: None,
    });
    reader
        .pull_request(&id::<Prj>(1), number)
        .await
        .unwrap()
        .expect("snapshot")
}

/// 初次回读写 `initial.checks`；后续 reconcile 只产生 `checks` 变化；真实 Reader 读 store 投影。
#[tokio::test]
async fn checks_are_read_from_github_and_flow_through_reader() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    h.github.add_check_run(
        REPO,
        &head,
        check_run(1, 15368, "test", "completed", json!("success"), &head),
    );
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();

    let events = h.forge_events().await;
    let synced = synced_events(&events);
    assert_eq!(synced[0].data["initial"]["checks"], checks(1, 0, 0, true));
    assert_eq!(
        reader_snapshot(&h, n).await.checks,
        gantry_core::entities::CheckSummary {
            passed: 1,
            failed: 0,
            pending: 0,
            required_ok: true,
        }
    );

    // 新增一个失败检查；真实 Dispatcher.reconcile 回读 FakeGitHub 的 checks 响应。
    h.github.add_check_run(
        REPO,
        &head,
        check_run(2, 15368, "lint", "completed", json!("failure"), &head),
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert_eq!(report.emitted.len(), 1, "only forge.synced changes");
    let events = h.forge_events().await;
    let synced = synced_events(&events);
    let latest = synced.last().unwrap();
    assert_eq!(
        latest.data["changes"],
        json!([
            {"field": "checks", "from": checks(1, 0, 0, true), "to": checks(1, 1, 0, false)}
        ])
    );
    let snapshot = reader_snapshot(&h, n).await;
    assert_eq!(
        snapshot.checks,
        gantry_core::entities::CheckSummary {
            passed: 1,
            failed: 1,
            pending: 0,
            required_ok: false,
        }
    );

    let calls = h.github.recorded_calls();
    assert!(calls
        .iter()
        .any(|call| call.path.contains("/commits/") && call.path.contains("check-runs")));
    assert!(calls
        .iter()
        .any(|call| call.path.contains("/commits/") && call.path.contains("/statuses")));
}

/// 传统 commit statuses 与 check runs 一起汇总；同 context 只取最新一条。
#[tokio::test]
async fn commit_statuses_merge_with_check_runs_and_keep_latest() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    h.github.add_check_run(
        REPO,
        &head,
        check_run(1, 15368, "test", "completed", json!("success"), &head),
    );
    // 官方按创建时间倒序返回：同 context 的第一条最新。
    for (id, context, state) in [
        (3, "ci/legacy", "pending"),
        (2, "ci/legacy", "failure"),
        (1, "ci/legacy", "success"),
    ] {
        h.github.add_commit_status(
            REPO,
            &head,
            json!({"id": id, "context": context, "state": state, "sha": head}),
        );
    }
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    assert_eq!(
        synced_events(&events)[0].data["initial"]["checks"],
        checks(1, 0, 1, false)
    );
    assert_eq!(
        reader_snapshot(&h, n).await.checks,
        gantry_core::entities::CheckSummary {
            passed: 1,
            failed: 0,
            pending: 1,
            required_ok: false,
        }
    );
}

/// statuses 跨页读取完整。
#[tokio::test]
async fn commit_status_pagination_is_complete() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    for i in 1..=101_u64 {
        h.github.add_commit_status(
            REPO,
            &head,
            json!({"id": i, "context": format!("ci/{i}"), "state": "success", "sha": head}),
        );
    }
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    assert_eq!(
        synced_events(&events)[0].data["initial"]["checks"],
        checks(101, 0, 0, true)
    );
    let pages = h
        .github
        .recorded_calls()
        .into_iter()
        .filter(|call| call.path.contains("/commits/") && call.path.contains("/statuses"))
        .count();
    assert_eq!(pages, 2, "one page per 100 statuses");
}

/// statuses 晚页失败不能把前面页的成功误当绿。
#[tokio::test]
async fn commit_status_late_page_failure_is_not_green() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let before = reader_snapshot(&h, n).await;

    let head = oid("head-1").to_string();
    for i in 1..=150_u64 {
        h.github.add_commit_status(
            REPO,
            &head,
            json!({"id": i, "context": format!("ci/{i}"), "state": "success", "sha": head}),
        );
    }
    h.github
        .fail_next_request_matching("/statuses?per_page=100&page=2", 500);
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// statuses 的 ref 与当前 head 不一致时整体失败，不把旧 head 的结论算进当前快照。
#[tokio::test]
async fn commit_status_head_mismatch_fails_closed() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let before = reader_snapshot(&h, n).await;

    let head = oid("head-1").to_string();
    h.github.add_check_run(
        REPO,
        &head,
        check_run(1, 15368, "test", "completed", json!("success"), &head),
    );
    h.github.add_commit_status(
        REPO,
        &head,
        json!({"id": 1, "context": "ci/legacy", "state": "success", "sha": oid("head-2")}),
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 没有观察到任何检查时不能当作通过；unknown 结论/状态保守算失败。
#[tokio::test]
async fn empty_and_unknown_checks_are_conservative() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    assert_eq!(
        synced_events(&events)[0].data["initial"]["checks"],
        checks(0, 0, 0, false)
    );

    let head = oid("head-1").to_string();
    h.github.add_check_run(
        REPO,
        &head,
        check_run(1, 1, "mystery", "mystery", json!("mystery"), &head),
    );
    h.github.add_check_run(
        REPO,
        &head,
        check_run(2, 1, "pending", "in_progress", Value::Null, &head),
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert_eq!(report.emitted.len(), 1);
    let events = h.forge_events().await;
    let synced = synced_events(&events);
    let latest = synced.last().unwrap();
    assert_eq!(latest.data["changes"][0]["to"], checks(0, 1, 1, false));
}

/// success/neutral/skipped 通过；failure/cancelled/timed_out/action_required/stale 失败。
#[tokio::test]
async fn check_conclusions_are_classified_conservatively() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    for (id, name, conclusion) in [
        (1, "success", "success"),
        (2, "neutral", "neutral"),
        (3, "skipped", "skipped"),
        (4, "failure", "failure"),
        (5, "cancelled", "cancelled"),
        (6, "timed-out", "timed_out"),
        (7, "action-required", "action_required"),
        (8, "stale", "stale"),
    ] {
        h.github.add_check_run(
            REPO,
            &head,
            check_run(id, 1, name, "completed", json!(conclusion), &head),
        );
    }
    h.github.add_check_run(
        REPO,
        &head,
        check_run(9, 1, "queued", "queued", Value::Null, &head),
    );
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let events = h.forge_events().await;
    assert_eq!(
        synced_events(&events)[0].data["initial"]["checks"],
        checks(3, 5, 1, false)
    );
}

/// 新 head 不继承旧 head 的检查；只汇总当前 head 的 check runs。
#[tokio::test]
async fn new_head_does_not_inherit_old_head_checks() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();

    let old = oid("head-1").to_string();
    let new = oid("head-2").to_string();
    h.github.add_check_run(
        REPO,
        &old,
        check_run(1, 1, "old-pass", "completed", json!("success"), &old),
    );
    h.github.add_check_run(
        REPO,
        &old,
        check_run(2, 1, "old-fail", "completed", json!("failure"), &old),
    );
    h.github.add_check_run(
        REPO,
        &new,
        check_run(3, 1, "new-pass", "completed", json!("success"), &new),
    );
    h.github.set_pull_head(REPO, n, &new);

    h.dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    let events = h.forge_events().await;
    assert_eq!(of_kind(&events, ForgeEventKind::PrSynchronized).len(), 1);
    let synced = synced_events(&events);
    let latest = synced.last().unwrap();
    assert_eq!(
        latest.data["changes"],
        json!([
            {"field": "head_sha", "from": old, "to": new},
            {"field": "checks", "from": checks(0, 0, 0, false), "to": checks(1, 0, 0, true)},
        ])
    );
    let snapshot = reader_snapshot(&h, n).await;
    assert_eq!(snapshot.head_sha.to_string(), new);
    assert!(snapshot.checks.required_ok);
}

/// 读 PR 与读检查之间 head 前进 → 安全失败，不把两个 head 的事实混在一起。
#[tokio::test]
async fn head_change_during_read_is_rejected_without_snapshot_update() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let before = reader_snapshot(&h, n).await;

    let next = oid("head-3").to_string();
    h.github.add_check_run(
        REPO,
        &oid("head-1").to_string(),
        check_run(1, 1, "pass", "completed", json!("success"), &head),
    );
    h.github.set_pull_head_after_next_check_runs(REPO, n, &next);
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty(), "no mixed-head snapshot");
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.head_sha, before.head_sha);
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 相同结果重复对账不产生重复 `forge.synced`。
#[tokio::test]
async fn repeated_reconcile_with_same_checks_emits_nothing() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    h.github.add_check_run(
        REPO,
        &oid("head-1").to_string(),
        check_run(1, 1, "pass", "completed", json!("success"), &head),
    );
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let before = h.forge_events().await.len();
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    assert_eq!(h.forge_events().await.len(), before);
}

/// checks 读取 401：换令牌整体重读一次；第二次仍 401 → 失败闭合，不更新快照。
#[tokio::test]
async fn checks_401_retries_once_then_fails_closed() {
    let tokens = Arc::new(RecordingTokens::default());
    let h = Harness::new_with_tokens(tokens.clone());
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let before = reader_snapshot(&h, n).await;

    let head = oid("head-1").to_string();
    h.github.add_check_run(
        REPO,
        &head,
        check_run(1, 1, "pass", "completed", json!("success"), &head),
    );
    let baseline = tokens.calls.lock().unwrap().len();
    h.github.fail_next_request_matching("/check-runs", 401);
    h.github.fail_next_request_matching("/check-runs", 401);

    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
    // reconcile 的两次尝试各取一次只读令牌；Reader 读快照不取令牌。
    assert_eq!(tokens.calls.lock().unwrap().len(), baseline + 2);
    assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1);
}

/// 全部页成功才汇总；晚页失败不能把前面页的成功误当绿。
#[tokio::test]
async fn check_run_pagination_is_complete_and_late_failure_is_not_green() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let before = reader_snapshot(&h, n).await;

    let head = oid("head-1").to_string();
    for id in 1..=150 {
        h.github.add_check_run(
            REPO,
            &head,
            check_run(
                id,
                1,
                &format!("ok-{id}"),
                "completed",
                json!("success"),
                &head,
            ),
        );
    }
    h.github
        .fail_next_request_matching("/check-runs?filter=latest&per_page=100&page=2", 500);
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);

    // 去掉故障后，两页都读到才计数；第二页的失败不能被第一页的成功掩盖。
    h.github.add_check_run(
        REPO,
        &head,
        check_run(
            151,
            1,
            "fail-on-page-2",
            "completed",
            json!("failure"),
            &head,
        ),
    );
    h.dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    let snapshot = reader_snapshot(&h, n).await;
    assert_eq!(
        snapshot.checks,
        gantry_core::entities::CheckSummary {
            passed: 150,
            failed: 1,
            pending: 0,
            required_ok: false,
        }
    );
}

/// 服务端 `filter=latest` 返回的集合按 id 独立保留；同 app 同名两条不能合并成一条绿色。
#[tokio::test]
async fn latest_response_keeps_same_name_runs_independent() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();

    // 服务端 latest 集合同 app、同名但 id 不同：一条失败、一条成功，必须都计数。
    h.github.set_check_run_page(
        REPO,
        &head,
        "latest",
        1,
        2,
        vec![
            check_run(1, 10, "test", "completed", json!("failure"), &head),
            check_run(2, 10, "test", "completed", json!("success"), &head),
        ],
    );
    h.dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert_eq!(
        reader_snapshot(&h, n).await.checks,
        gantry_core::entities::CheckSummary {
            passed: 1,
            failed: 1,
            pending: 0,
            required_ok: false,
        }
    );

    // latest 集合给出两条同名成功时，两条都保留，不能只算一条。
    h.github.set_check_run_page(
        REPO,
        &head,
        "latest",
        1,
        2,
        vec![
            check_run(3, 20, "test", "completed", json!("success"), &head),
            check_run(4, 20, "test", "completed", json!("success"), &head),
        ],
    );
    h.dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert_eq!(
        reader_snapshot(&h, n).await.checks,
        gantry_core::entities::CheckSummary {
            passed: 2,
            failed: 0,
            pending: 0,
            required_ok: true,
        }
    );
}

/// 检查端点 404（权限不足 / ref 不存在）不能当空结果成功。
#[tokio::test]
async fn checks_404_is_not_empty_success() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let before = reader_snapshot(&h, n).await;
    h.github.add_check_run(
        REPO,
        &oid("head-1").to_string(),
        check_run(1, 1, "pass", "completed", json!("success"), &head),
    );

    h.github.fail_next_request_matching("/check-runs", 404);
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty(), "check-runs 404 must not update");
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 旧持久 `PrSnap` 没有 `checks` 字段：保守默认可读，并与新值正确做差。
#[tokio::test]
async fn legacy_pr_snapshot_without_checks_reads_and_diffs() {
    use gantry_forge::normalize::snapshot::SNAPSHOT_TABLE;
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();

    let key = format!("pr|{}|{:020}", id::<Prj>(1), n);
    let mut tx = h.store.begin_write().await.unwrap();
    let mut old: Value = tx
        .module_conn(MODULE)
        .get(SNAPSHOT_TABLE, &key)
        .unwrap()
        .expect("seeded snapshot");
    old.as_object_mut().unwrap().remove("checks");
    tx.module_conn(MODULE)
        .put(SNAPSHOT_TABLE, &key, &old)
        .unwrap();
    tx.commit().await.unwrap();

    h.github.add_check_run(
        REPO,
        &oid("head-1").to_string(),
        check_run(1, 1, "pass", "completed", json!("success"), &head),
    );
    h.dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    let events = h.forge_events().await;
    let synced = synced_events(&events);
    let latest = synced.last().unwrap();
    assert_eq!(
        latest.data["changes"],
        json!([
            {"field": "checks", "from": checks(0, 0, 0, false), "to": checks(1, 0, 0, true)}
        ])
    );
}

fn next_link_header(url: &str) -> String {
    format!("<{url}>; rel=\"next\"")
}

fn head_check_url(h: &Harness, endpoint: &str, page: usize) -> String {
    format!(
        "{}/repos/{REPO}/commits/{}/{}?{}&page={page}",
        h.github.base_url(),
        oid("head-1"),
        endpoint,
        if endpoint == "check-runs" {
            "filter=latest&per_page=100"
        } else {
            "per_page=100"
        }
    )
}

/// 后续页 `total_count` 与首页不一致：不能把两页拼成完整集合。
#[tokio::test]
async fn check_run_pagination_total_change_fails_closed() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let before = reader_snapshot(&h, n).await;
    // 首页 total=5，只给 2 条并带下一页；第二页改口 total=6、给 3 条。
    // 累计 5 恰好等于首页 total；若信任变化后的 total / 只看最终长度，会误当完整集合。
    h.github.set_check_run_page(
        REPO,
        &head,
        "latest",
        1,
        5,
        vec![
            check_run(1, 1, "a", "completed", json!("success"), &head),
            check_run(2, 1, "b", "completed", json!("success"), &head),
        ],
    );
    let page2 = head_check_url(&h, "check-runs", 2);
    h.github.set_pagination_link(
        REPO,
        &head,
        "check-runs",
        1,
        Some(&next_link_header(&page2)),
    );
    h.github.set_check_run_page(
        REPO,
        &head,
        "latest",
        2,
        6,
        vec![
            check_run(3, 1, "c", "completed", json!("success"), &head),
            check_run(4, 1, "d", "completed", json!("success"), &head),
            check_run(5, 1, "e", "completed", json!("success"), &head),
        ],
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 跨页重复 run id：拒绝，不把它当成完整集合。
#[tokio::test]
async fn check_run_pagination_duplicate_id_fails_closed() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let before = reader_snapshot(&h, n).await;
    for id in 1..=101 {
        h.github.add_check_run(
            REPO,
            &head,
            check_run(
                id,
                1,
                &format!("ok-{id}"),
                "completed",
                json!("success"),
                &head,
            ),
        );
    }
    h.github.set_check_run_page(
        REPO,
        &head,
        "latest",
        2,
        101,
        vec![check_run(
            1,
            1,
            "duplicate",
            "completed",
            json!("success"),
            &head,
        )],
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 恶意 next 指向别的 origin：不跟随，也不把 Authorization 发到外域。
#[tokio::test]
async fn check_run_pagination_rejects_malicious_next() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let before = reader_snapshot(&h, n).await;
    for id in 1..=101 {
        h.github.add_check_run(
            REPO,
            &head,
            check_run(
                id,
                1,
                &format!("ok-{id}"),
                "completed",
                json!("success"),
                &head,
            ),
        );
    }
    let malicious = "http://evil.example/repos/acme/shop-api/commits/x/check-runs?page=2";
    h.github.set_pagination_link(
        REPO,
        &head,
        "check-runs",
        1,
        Some(&next_link_header(malicious)),
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    assert_eq!(reader_snapshot(&h, n).await.checks, before.checks);
    assert!(!h
        .github
        .recorded_calls()
        .iter()
        .any(|call| call.path.contains("evil.example")));
}

/// next 指回已访问的页：检测循环并失败。
#[tokio::test]
async fn check_run_pagination_rejects_cycle() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let before = reader_snapshot(&h, n).await;
    for id in 1..=101 {
        h.github.add_check_run(
            REPO,
            &head,
            check_run(
                id,
                1,
                &format!("ok-{id}"),
                "completed",
                json!("success"),
                &head,
            ),
        );
    }
    let page1 = head_check_url(&h, "check-runs", 1);
    let page2 = head_check_url(&h, "check-runs", 2);
    h.github.set_pagination_link(
        REPO,
        &head,
        "check-runs",
        1,
        Some(&next_link_header(&page2)),
    );
    h.github.set_pagination_link(
        REPO,
        &head,
        "check-runs",
        2,
        Some(&next_link_header(&page1)),
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    assert_eq!(reader_snapshot(&h, n).await.checks, before.checks);
}

/// check-runs 分页不以短页终止：短页带 next 仍要继续请求下一页。
#[tokio::test]
async fn check_run_short_page_with_next_is_not_terminal() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    let head = oid("head-1").to_string();
    h.github.set_check_run_page(
        REPO,
        &head,
        "latest",
        1,
        2,
        vec![check_run(
            1,
            1,
            "ci-a",
            "completed",
            json!("success"),
            &head,
        )],
    );
    h.github.set_check_run_page(
        REPO,
        &head,
        "latest",
        2,
        2,
        vec![check_run(
            2,
            1,
            "ci-b",
            "completed",
            json!("success"),
            &head,
        )],
    );
    let page2 = head_check_url(&h, "check-runs", 2);
    h.github.set_pagination_link(
        REPO,
        &head,
        "check-runs",
        1,
        Some(&next_link_header(&page2)),
    );
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    assert!(
        h.github
            .recorded_calls()
            .iter()
            .any(|call| call.path.contains("/check-runs?") && call.path.contains("page=2")),
        "short page with rel=next must be followed"
    );
    assert_eq!(
        reader_snapshot(&h, n).await.checks,
        gantry_core::entities::CheckSummary {
            passed: 2,
            failed: 0,
            pending: 0,
            required_ok: true,
        }
    );
}

/// next 指向别的端点（同源、同 repo/head）也不能跟随。
#[tokio::test]
async fn check_run_pagination_rejects_cross_endpoint_next() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let before = reader_snapshot(&h, n).await;
    for id in 1..=101 {
        h.github.add_check_run(
            REPO,
            &head,
            check_run(
                id,
                1,
                &format!("ok-{id}"),
                "completed",
                json!("success"),
                &head,
            ),
        );
    }
    let wrong = format!(
        "{}/repos/{REPO}/commits/{}/check-suites?page=2",
        h.github.base_url(),
        oid("head-1")
    );
    h.github.set_pagination_link(
        REPO,
        &head,
        "check-runs",
        1,
        Some(&next_link_header(&wrong)),
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
    assert!(!h
        .github
        .recorded_calls()
        .iter()
        .any(|call| call.path.contains("/check-suites")));
}

/// 官方 1000 check suite 上限：命中上限时不能把结果当完整集合。
#[tokio::test]
async fn check_run_suite_limit_fails_closed() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let before = reader_snapshot(&h, n).await;
    for id in 1..=1000 {
        h.github.add_check_run(
            REPO,
            &head,
            check_run(
                id,
                1,
                &format!("ok-{id}"),
                "completed",
                json!("success"),
                &head,
            ),
        );
    }
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 检查声明的 head_sha 与请求 head 不同：拒绝，不混入新快照。
#[tokio::test]
async fn check_run_head_mismatch_fails_closed() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let old = oid("head-2").to_string();
    let before = reader_snapshot(&h, n).await;
    h.github.add_check_run(
        REPO,
        &head,
        check_run(1, 1, "stale", "completed", json!("success"), &old),
    );
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 检查缺 head_sha：拒绝，不把它当成通过。
#[tokio::test]
async fn check_run_missing_head_fails_closed() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    let before = reader_snapshot(&h, n).await;
    let mut run = check_run(1, 1, "missing-head", "completed", json!("success"), &head);
    run.as_object_mut().unwrap().remove("head_sha");
    h.github.add_check_run(REPO, &head, run);
    let report = h
        .dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    assert!(report.emitted.is_empty());
    let after = reader_snapshot(&h, n).await;
    assert_eq!(after.checks, before.checks);
    assert_eq!(after.synced_at, before.synced_at);
}

/// 读检查期间同 head 上 PR 已合并：用第二次读取的 PR 对象构造快照，不能返回首张 open。
#[tokio::test]
async fn confirmed_pr_object_reflects_state_changed_during_read() {
    let h = Harness::new();
    seed_project(&h).await;
    let (_, n) = h.create_own_pr().await;
    h.deliver("pull_request", h.pr_payload("opened", n, h.bot))
        .await;
    h.dispatcher().drain().await.unwrap();
    let head = oid("head-1").to_string();
    h.github.add_check_run(
        REPO,
        &head,
        check_run(1, 1, "pass", "completed", json!("success"), &head),
    );
    h.github.set_pull_merged_after_next_check_runs(REPO, n);
    h.dispatcher()
        .reconcile(
            ReconcileTarget::Pr {
                project: id::<Prj>(1),
                number: n,
            },
            DriftDetectedBy::Patrol,
        )
        .await
        .unwrap();
    let events = h.forge_events().await;
    assert_eq!(of_kind(&events, ForgeEventKind::PrMerged).len(), 1);
    let snapshot = reader_snapshot(&h, n).await;
    assert_eq!(
        snapshot.state,
        gantry_core::entities::PullRequestState::Merged
    );
    assert_eq!(
        snapshot.checks,
        gantry_core::entities::CheckSummary {
            passed: 1,
            failed: 0,
            pending: 0,
            required_ok: true,
        }
    );
}
