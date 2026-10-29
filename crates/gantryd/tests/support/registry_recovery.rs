//! 跨 crate 的离线验收：真实登记表、真实执行体与沙盒管理器，容器驱动和上游仅用替身。
//! 从仓库根目录运行：`cargo test --locked --offline --manifest-path
//! crates/gantryd/tests/support/registry-recovery/Cargo.toml --lib
//! real_registry_retains_usage_until_real_actor_commits_and_releases -- --exact`。
//! 独立清单和锁文件不改变根工作区的依赖。
#![cfg(all(unix, test))]

#[path = "../real_repo.rs"]
mod fixtures;
use fixtures::engine_cfg;

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid};
use gantry_core::egress::{IssuedTo, SandboxTier};
use gantry_core::entities::{
    Actor, ActorKind, DeliveryOrigin, InboxKind, NodeStatus, Role, Seq, Tier, TokenUsage,
};
use gantry_core::ids::{Clock, IdGen};
use gantry_core::level::Level;
use gantry_core::ports::egress::{EgressControl, EgressError, IssueJobToken, ProviderId};
use gantry_core::ports::sandbox::{RecoveredJob, SandboxService, UnfinishedAttempt};
use gantry_core::ports::secrets::SecretValue;
use gantry_core::ports::ActorCtx;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_egress::audit::MemoryAudit;
use gantry_egress::client::EgressClient;
use gantry_egress::control::ControlServer;
use gantry_egress::db::Db;
use gantry_egress::registry::{
    ControlError, Endpoints, Listener, Metered, Registry, RegistryOptions, UpstreamClass,
};
use gantry_engine::actor::{ActorDeps, DeliveryActor, EffectRunner};
use gantry_engine::attempt::{sandbox_id_for, AttemptContext, AttemptPorts, AttemptRunner};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::decide::{
    AttemptPlan, Effect, EffectKey, EffectReport, Input, InputIdentity, RetryBlocked, TickReason,
};
use gantry_engine::engine::{load_delivery_state, Engine};
use gantry_engine::event_router::EventRouter;
use gantry_engine::PortEffects;
use gantry_fakes::evented_repo::EventedRepo;
use gantry_fakes::{
    forge::FakeForge, inbox::FakeInbox, policy::FakePolicy, repo::FakeRepo,
    secrets::MemorySecretStore, worker::StubWorkerAdapter,
};
use gantry_sandbox::fake::{DriverCall, FakeBehavior, FakeDriver, FakeScript};
use gantry_sandbox::service::seams::StaticSchemas;
use gantry_sandbox::service::{SandboxConfig, SandboxDeps, SandboxManager};
use gantry_sandbox::spec::{NetworkAttach, Runtime};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Store, StoreConfig, StoreDeps};
use serde_json::json;

struct Effects {
    inner: PortEffects,
    store: Store,
    registry: Arc<Mutex<Option<Registry>>>,
    plans: Mutex<Vec<AttemptPlan>>,
    released: Mutex<Vec<gantry_core::ids::AttemptId>>,
    crash_before_release: AtomicBool,
}

struct NoSnapshots;

#[async_trait::async_trait]
impl gantry_engine::actor::PrSnapshots for NoSnapshots {
    async fn pull_request(
        &self,
        _: &gantry_core::ids::ProjectId,
        _: u64,
    ) -> Option<gantry_core::entities::PullRequestRef> {
        None
    }
}

#[async_trait::async_trait]
impl EffectRunner for Effects {
    async fn run(
        &self,
        state: &gantry_engine::state::DeliveryState,
        effect: Effect,
        ctx: &ActorCtx,
    ) -> Option<EffectReport> {
        if let Effect::RunAttempt(plan) = &effect {
            self.plans.lock().unwrap().push((**plan).clone());
            return None;
        }
        if let Effect::ReleaseSandbox { attempt_id } = &effect {
            let current = load_delivery_state(&self.store, &state.delivery.id)
                .await
                .unwrap()
                .unwrap();
            let attempt = current
                .workflows
                .values()
                .flat_map(|w| w.nodes.values())
                .flat_map(|n| n.attempts.iter())
                .find(|a| a.id == *attempt_id)
                .unwrap();
            assert!(attempt.finished_at.is_some());
            assert!(current
                .open_inbox
                .values()
                .any(|item| item.kind == InboxKind::Escalation));
            assert!(current.ledger.used.tokens_by_tier[&Tier::Worker] > 0);
            assert!(self
                .registry
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .job_usage(attempt_id)
                .is_ok());
            if self.crash_before_release.swap(false, Ordering::SeqCst) {
                return None;
            }
            self.released.lock().unwrap().push(*attempt_id);
        }
        self.inner.run(state, effect, ctx).await
    }
}

struct SocketDir(std::path::PathBuf);
impl SocketDir {
    fn new() -> Self {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::path::PathBuf::from(format!("/tmp/g362-{name}"));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
}
impl Drop for SocketDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn serve(registry: Registry, dir: &SocketDir) -> tokio::task::JoinHandle<std::io::Result<()>> {
    let socket = dir.0.join("control.sock");
    if socket.exists() {
        std::fs::remove_file(&socket).unwrap();
    }
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let uid = std::fs::metadata(&dir.0).unwrap().uid();
    tokio::spawn(ControlServer::new(registry, Arc::new(MemoryAudit::new()), uid).serve(listener))
}

#[tokio::test]
async fn real_registry_retains_usage_until_real_actor_commits_and_releases() {
    registry_release_recovery(false).await;
}

#[tokio::test]
async fn real_registry_legacy_outcome_survives_sweep_and_failed_release() {
    registry_release_recovery(true).await;
}

async fn registry_release_recovery(legacy_outcome: bool) {
    let tmp = fixtures::TempDir::new();
    let socket_dir = SocketDir::new();
    let clock = Arc::new(FakeClock::new(1_800_000_000_000));
    let egress_clock = Arc::new(FakeClock::new(1_800_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let ctx = fixtures::context(&ids);
    let secrets = MemorySecretStore::new(egress_clock.now());
    secrets.allow(Consumer::Egress);
    let secrets = Arc::new(secrets.scoped(Consumer::Egress).unwrap());
    let db_path = tmp.path.join("egress.db");
    let options = RegistryOptions::new(Endpoints {
        sandbox_model_base: "http://127.0.0.1:8081".into(),
        connect_proxy_authority: "127.0.0.1:3128".into(),
        loopback_model_base: "http://127.0.0.1:8082".into(),
    });
    let open = || {
        Registry::open(
            Db::open(&db_path).unwrap(),
            egress_clock.clone(),
            Arc::new(SeqEntropy::new(9)),
            secrets.clone(),
            options.clone(),
        )
        .unwrap()
    };
    let registry = open();
    registry
        .apply_config(gantry_fakes::egress::default_config(1))
        .unwrap();
    let staged = registry
        .stage_secret(
            ctx.request_id,
            SecretName::parse("model.anthropic.main").unwrap(),
            SecretValue::from_bytes(b"test-only".to_vec()),
            &ctx,
        )
        .unwrap();
    registry.activate_secret(&staged.staged_id, &ctx).unwrap();
    let mut server = serve(registry.clone(), &socket_dir);
    let client = Arc::new(EgressClient::new(
        socket_dir.0.join("control.sock"),
        egress_clock.clone(),
        IssuedTo {
            process: "gantryd".into(),
            pid: std::process::id(),
            started_at: clock.now(),
        },
    ));
    let shared_registry = Arc::new(Mutex::new(Some(registry.clone())));
    let store = Store::open(
        StoreConfig::new(tmp.path.join("sqlite")),
        StoreDeps {
            clock: clock.clone(),
            ids: ids.clone(),
        },
        &[],
    )
    .await
    .unwrap();
    let project = ids.next().unwrap();
    let delivery = ids.next().unwrap();
    let base = GitOid::parse(engine_cfg::BASE).unwrap();
    let raw_repo = Arc::new(FakeRepo::new(clock.now()));
    raw_repo.set_ref(project, "refs/mirror/heads/main", base);
    raw_repo.insert_commit(
        project,
        base,
        gantry_core::ports::repo::CommitInfo {
            parents: vec![],
            tree: GitOid::parse("1111111111111111111111111111111111111111").unwrap(),
            trailers: vec![],
        },
    );
    let repo = Arc::new(EventedRepo::new(raw_repo, store.clone()));
    let forge = Arc::new(FakeForge::new_in(store.clone()));
    let policy = Arc::new(FakePolicy::new_in(store.clone()));
    let inbox = Arc::new(FakeInbox::new_in(store.clone()));
    let workspaces = tmp.path.join("sandboxes");
    std::fs::create_dir(&workspaces).unwrap();
    let driver = Arc::new(FakeDriver::new(&workspaces));
    const SCHEMA: &[u8] = br#"{"type":"object"}"#;
    let mut runner_config = engine_cfg::runner_config();
    runner_config
        .roles
        .get_mut(&Role::Single)
        .unwrap()
        .structured_schema
        .sha256 = sha256(SCHEMA);
    let config = SandboxConfig {
        workspaces_dir: workspaces.clone(),
        runtime: Runtime::Runc,
        max_concurrent_sandboxes: 1,
        network: NetworkAttach {
            network: "gantry-sbx".into(),
            add_hosts: vec![],
        },
        log_max_bytes: 16 << 20,
        out_quota_bytes: 12 << 20,
        tmpfs_home_mb: 256,
        tmpfs_tmp_mb: 512,
        progress_sample: Duration::from_secs(1),
        log_drain_timeout: Duration::from_secs(5),
        orphan_grace: Duration::ZERO,
    };
    let manager = || {
        Arc::new(SandboxManager::new(
            SandboxDeps {
                driver: driver.clone(),
                repo: repo.clone(),
                egress: client.clone(),
                store: store.clone(),
                clock: clock.clone(),
                artifacts: Arc::new(store.cas()),
                schemas: Arc::new(StaticSchemas(BTreeMap::from([(
                    "gantry.worker.output.build/1".into(),
                    SCHEMA.to_vec(),
                )]))),
            },
            config.clone(),
        ))
    };
    let first = manager();
    let replay = manager();
    let final_manager = manager();
    let ports = |sandbox: Arc<SandboxManager<FakeDriver>>| AttemptPorts {
        sandbox,
        repo: repo.clone(),
        policy: policy.clone(),
        forge: forge.clone(),
        cas: Arc::new(store.cas()),
        adapter: Arc::new(StubWorkerAdapter::new()),
        clock: clock.clone(),
        entropy: Arc::new(SeqEntropy::new(99)),
    };
    let effects = Arc::new(Effects {
        inner: PortEffects::new(
            ports(final_manager.clone()),
            forge.clone(),
            runner_config.clone(),
            Duration::from_secs(60),
            Arc::new(|_, _, _| {}),
        ),
        store: store.clone(),
        registry: shared_registry.clone(),
        plans: Mutex::new(vec![]),
        released: Mutex::new(vec![]),
        crash_before_release: AtomicBool::new(true),
    });
    Engine::new(store.clone(), inbox.clone())
        .create_delivery(NewDelivery {
            id: delivery,
            project_id: project,
            title: "test".into(),
            user_text: "test".into(),
            level_cap: Level::Low,
            level_effective: Level::Low,
            origin: DeliveryOrigin::Conversation,
            source_message_id: ids.next().unwrap(),
            issues: vec![],
            budget: budget_from_level(&LevelEntry {
                graph_template: "low.single".into(),
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
            actor: Actor::new(ActorKind::Engine, "test"),
        })
        .await
        .unwrap();
    let mut actor = DeliveryActor::new(
        delivery,
        ActorDeps {
            store: store.clone(),
            inbox: inbox.clone(),
            outbox: forge.clone(),
            effects: effects.clone(),
            snapshots: Arc::new(NoSnapshots),
            cas: Arc::new(store.cas()),
        },
        engine_cfg::actor_config(),
    );
    actor
        .handle(
            Input::Tick(TickReason::Settle),
            InputIdentity::Tick(TickReason::Settle),
        )
        .await
        .unwrap();
    let mut router = EventRouter::open(store.clone()).await.unwrap();
    for _ in 0..20 {
        for dispatch in router.poll(&BTreeSet::from([delivery])).await.unwrap() {
            actor
                .handle(dispatch.input, dispatch.identity)
                .await
                .unwrap();
            router.processed(dispatch.delivery, dispatch.seq);
        }
        if !effects.plans.lock().unwrap().is_empty() {
            break;
        }
    }
    let mut plan = effects.plans.lock().unwrap().first().unwrap().clone();
    let sandbox_id = sandbox_id_for(&plan.attempt_id);
    driver.script(
        sandbox_id,
        FakeScript {
            behavior: FakeBehavior::Forever { honor_term: true },
            ..FakeScript::default()
        },
    );
    let state = load_delivery_state(&store, &delivery)
        .await
        .unwrap()
        .unwrap();
    let cx = AttemptContext::from_state(&state, &plan).unwrap();
    let runner = AttemptRunner::new(ports(first.clone()), Arc::new(runner_config.clone()));
    let spec = runner.assemble(&plan, &cx, &ctx).await.unwrap();
    first.submit(&ctx, spec.clone()).await.unwrap();
    let container = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(id) = driver.calls().into_iter().find_map(|call| match call {
                DriverCall::Start(id) => Some(id),
                _ => None,
            }) {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let grant = client
        .issue_job_token(
            IssueJobToken {
                job_id: spec.job_id,
                sandbox_id: spec.sandbox_id,
                project_id: spec.project_id,
                delivery_id: spec.delivery_id,
                conversation_id: spec.conversation_id,
                tier: SandboxTier::Worker,
                provider: ProviderId::new("anthropic"),
                profile: spec.egress.profile.clone(),
                groups: spec.egress.groups.clone(),
                tokens_total: spec.egress.tokens_total,
                weights: spec.egress.weights,
                wall_time_s: spec.limits.wall_time_s,
                config_rev: spec.config_rev,
            },
            &ctx,
        )
        .await
        .unwrap();
    let caller = registry
        .authenticate(Listener::Sandbox, grant.job_token.expose_secret())
        .unwrap();
    let admitted = registry
        .admit(&caller, "anthropic", "claude", None)
        .unwrap();
    registry
        .finish(
            admitted,
            Metered {
                usage: TokenUsage::new(41, 13, 7, 3, 1),
                incomplete: false,
                upstream: UpstreamClass::Ok,
            },
        )
        .unwrap();

    first.crash();
    let meta_path = workspaces.join(sandbox_id.to_string()).join("meta.json");
    if legacy_outcome {
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta.as_object_mut().unwrap().remove("job_sha256");
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    } else {
        std::fs::write(&meta_path, b"invalid metadata").unwrap();
    }
    driver.complete(&container, vec![]).unwrap();
    plan.resume = true;
    let run = |sandbox| AttemptRunner::new(ports(sandbox), Arc::new(runner_config.clone()));
    let initial = tokio::time::timeout(
        Duration::from_secs(10),
        run(replay.clone()).run(&plan, &cx, &ctx),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        initial.retry_blocked,
        Some(RetryBlocked::JobIdentityUnavailable)
    );
    assert!(initial.weighted_tokens > 0);
    let final_usage = registry.job_usage(&plan.attempt_id).unwrap();
    assert_eq!(initial.weighted_tokens, final_usage.weighted_tokens);
    replay.crash();

    let guard = workspaces.join(format!("{sandbox_id}.recovery.json"));
    if !legacy_outcome {
        // 另一任务请求同一沙盒时，真实登记表中的原任务计量和拒绝依据都必须保留。
        let collision = manager();
        let foreign_id = ids.next().unwrap();
        let foreign = UnfinishedAttempt {
            job_id: foreign_id,
            sandbox_id,
            project_id: project,
        };
        let before_guard = std::fs::read(&guard).unwrap();
        let before_events = store.events().head().await.unwrap();
        assert!(matches!(
            collision.recover(&ctx, &[foreign]).await.unwrap().as_slice(),
            [RecoveredJob::Running(handle)] if handle.job_id == foreign_id
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), collision.wait(&foreign_id))
                .await
                .is_err()
        );
        assert!(collision.release(&ctx, &foreign_id).await.is_err());
        assert_eq!(std::fs::read(&guard).unwrap(), before_guard);
        assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);
        assert!(matches!(
            registry.job_usage(&foreign_id),
            Err(ControlError::NotFound)
        ));
        assert_eq!(store.events().head().await.unwrap(), before_events);
        let recovered_owner = tokio::time::timeout(
            Duration::from_secs(10),
            run(collision.clone()).run(&plan, &cx, &ctx),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(recovered_owner.weighted_tokens, final_usage.weighted_tokens);
        assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);
        assert!(matches!(
            registry.job_usage(&foreign_id),
            Err(ControlError::NotFound)
        ));
        collision.crash();
    }

    // 跨旧 24 小时清理边界，停止控制面，重新打开同一数据库。
    egress_clock.advance_secs(86_401);
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);
    server.abort();
    let _ = server.await;
    *shared_registry.lock().unwrap() = None;
    drop(registry);
    let registry = open();
    *shared_registry.lock().unwrap() = Some(registry.clone());
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);
    server = serve(registry.clone(), &socket_dir);
    let finalized = tokio::time::timeout(
        Duration::from_secs(10),
        run(final_manager.clone()).run(&plan, &cx, &ctx),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(finalized.weighted_tokens, final_usage.weighted_tokens);
    assert_eq!(
        finalized.failure_detail.as_ref().unwrap().as_str(),
        if legacy_outcome {
            "exec.protocol"
        } else {
            "infra.job_identity_unavailable"
        }
    );
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);

    let before = store.events().head().await.unwrap();
    let mut invalid = finalized.clone();
    invalid.weight_audit = serde_json::from_value(json!({"basis":"job_token", "config_rev":1,
        "weights":{"input":0.0,"output":5.0,"cache_read":0.1,"cache_write":1.25}}))
    .unwrap();
    assert!(actor
        .handle(
            Input::Report(EffectReport::AttemptFinalized(invalid)),
            InputIdentity::Report(EffectKey::Run(plan.attempt_id))
        )
        .await
        .is_err());
    assert_eq!(store.events().head().await.unwrap(), before);
    assert!(effects.released.lock().unwrap().is_empty());
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);

    actor
        .handle(
            Input::Report(EffectReport::AttemptFinalized(finalized.clone())),
            InputIdentity::Report(EffectKey::Run(plan.attempt_id)),
        )
        .await
        .unwrap();
    let committed = load_delivery_state(&store, &delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        committed.ledger.used.tokens_by_tier[&Tier::Worker],
        final_usage.weighted_tokens
    );
    assert_eq!(
        committed.node(&plan.node_id).unwrap().status,
        NodeStatus::Failed
    );
    assert!(effects.released.lock().unwrap().is_empty());
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);

    // 提交后、释放前崩溃：新管理器依据持久记录执行真实 release。
    final_manager.crash();
    let restarted_manager = manager();
    let evidence = if legacy_outcome { &meta_path } else { &guard };
    let before_evidence = std::fs::read(evidence).unwrap();
    if legacy_outcome {
        // 模拟旧版本已丢失本地依据；登记表仍有用量时不得把 release 当作已完成。
        let saved = workspaces.join(format!("{sandbox_id}.saved-meta"));
        std::fs::rename(evidence, &saved).unwrap();
        assert!(matches!(
            restarted_manager.release(&ctx, &plan.attempt_id).await,
            Err(gantry_core::ports::sandbox::SandboxError::Infra { .. })
        ));
        assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);
        std::fs::rename(saved, evidence).unwrap();
    }
    restarted_manager
        .sweep_orphans(&ctx, &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(evidence).unwrap(), before_evidence);
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);

    // 真实控制面不可达时 release 失败；再重启并巡检后仍可重放。
    server.abort();
    let _ = server.await;
    assert!(restarted_manager
        .release(&ctx, &plan.attempt_id)
        .await
        .is_err());
    assert_eq!(std::fs::read(evidence).unwrap(), before_evidence);
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);
    restarted_manager.crash();
    let restarted_manager = manager();
    server = serve(registry.clone(), &socket_dir);
    restarted_manager
        .sweep_orphans(&ctx, &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(evidence).unwrap(), before_evidence);
    assert_eq!(registry.job_usage(&plan.attempt_id).unwrap(), final_usage);
    let restarted_effects = Arc::new(Effects {
        inner: PortEffects::new(
            ports(restarted_manager.clone()),
            forge.clone(),
            runner_config.clone(),
            Duration::from_secs(60),
            Arc::new(|_, _, _| {}),
        ),
        store: store.clone(),
        registry: shared_registry.clone(),
        plans: Mutex::new(vec![]),
        released: Mutex::new(vec![]),
        crash_before_release: AtomicBool::new(false),
    });
    let mut restarted_actor = DeliveryActor::new(
        delivery,
        ActorDeps {
            store: store.clone(),
            inbox: inbox.clone(),
            outbox: forge.clone(),
            effects: restarted_effects.clone(),
            snapshots: Arc::new(NoSnapshots),
            cas: Arc::new(store.cas()),
        },
        engine_cfg::actor_config(),
    );
    restarted_actor
        .handle(
            Input::Tick(TickReason::Recover),
            InputIdentity::Tick(TickReason::Recover),
        )
        .await
        .unwrap();
    assert_eq!(
        restarted_effects.released.lock().unwrap().as_slice(),
        &[plan.attempt_id]
    );
    assert!(matches!(
        client.job_usage(&plan.attempt_id).await,
        Err(EgressError::NotFound)
    ));
    assert!(matches!(
        registry.job_usage(&plan.attempt_id),
        Err(ControlError::NotFound)
    ));
    client.forget_job(&plan.attempt_id, &ctx).await.unwrap();
    assert!(matches!(
        restarted_manager.release(&ctx, &plan.attempt_id).await,
        Err(gantry_core::ports::sandbox::SandboxError::NotFound)
    ));
    let state = load_delivery_state(&store, &delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state.ledger.used.tokens_by_tier[&Tier::Worker],
        final_usage.weighted_tokens
    );
    assert_eq!(
        state.node(&plan.node_id).unwrap().status,
        NodeStatus::Failed
    );
    restarted_manager.crash();
    let head = store.events().head().await.unwrap();
    restarted_actor
        .handle(
            Input::Report(EffectReport::AttemptFinalized(finalized)),
            InputIdentity::Report(EffectKey::Run(plan.attempt_id)),
        )
        .await
        .unwrap();
    restarted_actor
        .handle(
            Input::Tick(TickReason::Settle),
            InputIdentity::Tick(TickReason::Settle),
        )
        .await
        .unwrap();
    assert_eq!(store.events().head().await.unwrap(), head);
    assert!(restarted_effects.plans.lock().unwrap().is_empty());
    let mut after_cleanup = DeliveryActor::new(
        delivery,
        ActorDeps {
            store: store.clone(),
            inbox,
            outbox: forge.clone(),
            effects: Arc::new(PortEffects::new(
                ports(manager()),
                forge.clone(),
                runner_config.clone(),
                Duration::from_secs(60),
                Arc::new(|_, _, _| {}),
            )),
            snapshots: Arc::new(NoSnapshots),
            cas: Arc::new(store.cas()),
        },
        engine_cfg::actor_config(),
    );
    for tick in [
        TickReason::Recover,
        TickReason::Timer {
            at: gantry_core::time::Timestamp::from_millis(clock.now().as_millis() + 9_000_000),
            kind: gantry_engine::decide::TimerKind::RetryBackoff {
                node_id: plan.node_id,
            },
        },
    ] {
        after_cleanup
            .handle(Input::Tick(tick.clone()), InputIdentity::Tick(tick))
            .await
            .unwrap();
    }
    assert_eq!(store.events().head().await.unwrap(), head);
    assert_eq!(
        driver
            .calls()
            .iter()
            .filter(|call| matches!(call, DriverCall::Start(_)))
            .count(),
        1
    );
    server.abort();
    let _ = server.await;
    *shared_registry.lock().unwrap() = None;
    drop(registry);
    let reopened = open();
    assert!(matches!(
        reopened.job_usage(&plan.attempt_id),
        Err(ControlError::NotFound)
    ));
}
