//! 归一化与自身事件归因的验收：forge.md §12 A 组"重复投递只产生一组 ForgeEvent"、
//! "同一事实不同 GUID 只有一个 `pr_synchronized`"、"乱序：`closed` 先于 `opened`；旧 head 的
//! `check_suite` 晚到 → `stale=true`"、"人推了一个提交 → `pr_synchronized(origin=external)`"、
//! "默认分支 push → `base_advanced`（含 `affected_prs`）"、"webhook 先于 `github_ref` 到达 →
//! 停放后归因为 `self`；`sender == bot` 仍无记录 → `self_unattributed` + `orphan_effect`"。
//!
//! 收件箱行直接写进 `forge_webhook_inbox`（接收端的校验与去重见 `tests/webhook.rs`）；
//! 回读、执行器都对 `FakeGitHub`（A 级）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, ForgeEventKind, MergeMethod, Priority, Project};
use gantry_core::events::{Event, EventType, Subject};
use gantry_core::forge::DriftDetectedBy;
use gantry_core::forge::{ActionPayload, CreatePrPayload};
use gantry_core::ids::{ActionId, Id, IdGen, IdKind, Prj, Ulid};
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
    module_spec, CrashPoint, DispatchOutcome, Dispatcher, DispatcherDeps, DispatcherOptions,
    ExecutorDeps, ExecutorOptions, FaultInjector, Forge, ForgeConfig, InboxRow, InboxStatus,
    Permission, ProjectBinding, StaticTokenProvider, StepOutcome,
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
}

impl Harness {
    fn new() -> Self {
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
        }
    }

    fn dispatcher(&self) -> Dispatcher {
        self.forge.dispatcher(DispatcherDeps {
            tokens: Arc::new(StaticTokenProvider::new(SecretString::new(
                "ghs_read".into(),
            ))),
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
