mod world;

use gantry_engine::runtime::RuntimeError;
use world::*;

fn settings(rev: u64) -> RuntimeSettings {
    let mut actor = actor_config();
    let mut runner = runner_config();
    actor.settings.config_rev = Seq::new(rev).unwrap();
    runner.config_rev = Seq::new(rev).unwrap();
    RuntimeSettings {
        actor,
        runner,
        poll_interval: Duration::from_millis(5),
        cleanup_lease: Duration::from_secs(60),
    }
}

async fn tick(runtime: &EngineRuntime, delivery: DeliveryId) {
    runtime
        .deliver(
            delivery,
            Input::Tick(TickReason::Settle),
            InputIdentity::Tick(TickReason::Settle),
        )
        .await
        .unwrap();
}

async fn runtime_with_settings(world: &World, settings: RuntimeSettings) -> Arc<EngineRuntime> {
    let forge = Arc::new(FakeForge::new_in(world.store.clone()));
    let ports = EnginePorts {
        inbox: Arc::new(FakeInbox::new_in(world.store.clone())),
        outbox: forge.clone(),
        policy: world.policy_port.clone(),
        forge,
        repo: world.repo_port.clone(),
        sandbox: world.sandbox.clone(),
        worker: Arc::new(StubWorkerAdapter::new()),
        cas: Arc::new(world.store.cas()),
        clock: Arc::new(FakeClock::new(T0)),
        entropy: Arc::new(SeqEntropy::new(99)),
        snapshots: None,
    };
    EngineRuntime::new(world.store.clone(), ports, settings)
        .await
        .unwrap()
}

async fn assert_inconsistent(runtime: &EngineRuntime, expected: &str) {
    match runtime.status().await {
        Err(RuntimeError::ConfigurationInconsistent { reason }) => assert_eq!(reason, expected),
        other => panic!("expected configuration error, got {other:?}"),
    }
}

#[tokio::test]
async fn status_rejects_inconsistent_initial_settings() {
    let world = World::new().await;
    let mut config = settings(1);
    config.runner.config_rev = Seq::new(2).unwrap();
    let runtime = runtime_with_settings(&world, config).await;
    assert_inconsistent(&runtime, "actor and runner revisions differ").await;

    let mut config = settings(1);
    config.runner.repo = RepoSlug::parse("other/repo").unwrap();
    let runtime = runtime_with_settings(&world, config).await;
    assert_inconsistent(&runtime, "actor and runner repositories differ").await;
}

#[tokio::test]
async fn status_rejects_unproven_reconfiguration_and_recovers_for_bound_settings() {
    let world = World::new().await;
    let runtime = world.runtime().await;

    let mut config = settings(2);
    config.runner.config_rev = Seq::new(3).unwrap();
    runtime.reconfigure(config).await;
    assert_inconsistent(&runtime, "actor and runner revisions differ").await;

    let mut config = settings(2);
    config.runner.repo = RepoSlug::parse("other/repo").unwrap();
    runtime.reconfigure(config).await;
    assert_inconsistent(&runtime, "actor and runner repositories differ").await;

    let mut config = settings(2);
    config.actor.settings.project.repo = RepoSlug::parse("other/repo").unwrap();
    config.runner.repo = config.actor.settings.project.repo.clone();
    runtime.reconfigure(config).await;
    assert_inconsistent(&runtime, "repository differs from runtime binding").await;

    let mut config = settings(2);
    config.cleanup_lease = Duration::from_secs(61);
    runtime.reconfigure(config).await;
    assert_inconsistent(&runtime, "cleanup lease differs from runtime binding").await;

    runtime.reconfigure(settings(3)).await;
    assert_eq!(
        runtime.status().await.unwrap().applied_rev,
        Seq::new(3).unwrap()
    );
}

#[tokio::test]
async fn status_tracks_actors_configuration_and_persistent_cursor() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let initial = runtime.status().await.unwrap();
    assert_eq!(initial.applied_rev, Seq::new(1).unwrap());
    assert_eq!(initial.cursor, Seq::ZERO);
    assert_eq!(initial.actor_count, 0);
    assert!(initial.degraded.is_empty());
    let json = serde_json::to_value(&initial).unwrap();
    assert_eq!(json["applied_rev"], 1);
    assert_eq!(json["cursor"], 0);

    let delivery = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    tick(&runtime, delivery).await;
    assert_eq!(runtime.status().await.unwrap().actor_count, 1);

    runtime.reconfigure(settings(2)).await;
    assert_eq!(
        runtime.status().await.unwrap().applied_rev,
        Seq::new(2).unwrap()
    );

    let cursor = world.store.events().head().await.unwrap();
    world.store.cursors().advance(CURSOR, cursor).await.unwrap();
    assert_eq!(runtime.status().await.unwrap().cursor, cursor);
    let restarted = world.runtime().await;
    assert_eq!(restarted.status().await.unwrap().cursor, cursor);
}

#[tokio::test]
async fn status_reports_the_same_degraded_reasons_as_degraded() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let mut config = settings(2);
    config.actor.consts.max_settle = 1;
    runtime.reconfigure(config).await;
    let delivery = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    tick(&runtime, delivery).await;

    let status = runtime.status().await.unwrap();
    assert_eq!(status.actor_count, 1);
    assert_eq!(status.degraded, runtime.degraded().await);
    assert!(status.degraded[&delivery].contains("MAX_SETTLE"));
    let json = serde_json::to_value(status).unwrap();
    assert!(json["degraded"][delivery.to_string()].is_string());
}

#[tokio::test]
async fn status_reconfigure_and_submit_do_not_deadlock() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let delivery = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        let updates = async {
            for rev in 2..22 {
                runtime.reconfigure(settings(rev)).await;
                tokio::task::yield_now().await;
            }
        };
        let inputs = async {
            for _ in 0..20 {
                tick(&runtime, delivery).await;
                tokio::task::yield_now().await;
            }
        };
        let reads = async {
            for _ in 0..40 {
                let status = runtime.status().await.unwrap();
                assert!(status.applied_rev.as_u64() >= 1);
                assert!(status.actor_count <= 1);
                tokio::task::yield_now().await;
            }
        };
        tokio::join!(updates, inputs, reads);
    })
    .await
    .expect("concurrent calls must complete");
    let status = runtime.status().await.unwrap();
    assert_eq!(status.applied_rev, Seq::new(21).unwrap());
    assert_eq!(status.actor_count, 1);
}
