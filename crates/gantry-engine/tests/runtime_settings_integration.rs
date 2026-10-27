//! 已合并设置通过运行时影响真实决策；外部端口仍是测试替身。
mod world;

use gantry_core::events::AttemptStartedData;
use gantry_core::settings::{
    builtin_defaults, effective, parse_resolved, ResolvedSettings, SettingsResource,
};
use gantry_engine::runtime_settings::from_settings;
use world::*;

const BUILD_SCHEMA: &[u8] = include_bytes!("../../../schemas/worker/output.build.schema.json");

fn settings(rev: Seq, worker_budget: u64) -> (RuntimeSettings, LevelEntry) {
    let resolve = |resource, global: Value, project: Value| {
        let merged = effective(
            resource,
            &builtin_defaults(resource),
            &global,
            Some(&project),
        );
        parse_resolved(resource, &merged.value).unwrap()
    };
    let ResolvedSettings::Models(models) = resolve(
        SettingsResource::Models,
        json!({
            "providers": {"configured": {
                "kind": "anthropic", "base_url": "https://api.anthropic.com",
                "credential": {"type": "api_key", "secret_ref": "model-key"}
            }},
            "worker": {"provider": "configured", "model": "claude-test"}
        }),
        json!({}),
    ) else {
        unreachable!()
    };
    let ResolvedSettings::Levels(levels) = resolve(
        SettingsResource::Levels,
        json!({}),
        json!({"low": {"budget": {"tokens_by_tier": {"worker": worker_budget}}}}),
    ) else {
        unreachable!()
    };
    let ResolvedSettings::Policies(policies) =
        resolve(SettingsResource::Policies, json!({}), json!({}))
    else {
        unreachable!()
    };
    let ResolvedSettings::Sandbox(sandbox) =
        resolve(SettingsResource::Sandbox, json!({}), json!({}))
    else {
        unreachable!()
    };
    let ResolvedSettings::Egress(egress) = resolve(SettingsResource::Egress, json!({}), json!({}))
    else {
        unreachable!()
    };
    let table = ModelTable::from([(
        ModelKey {
            sandbox: true,
            level: Level::Low,
            tier: Tier::Worker,
            critical: false,
        },
        ResolvedModel {
            provider_id: models.worker.provider.clone().unwrap(),
            model: models.worker.model.clone().unwrap(),
            reasoning_effort: Effort::Low,
            attempt_wall_s: models.worker.attempt_wall_s,
        },
    )]);
    let runtime = from_settings(
        rev,
        ts(T0),
        ProjectFacts {
            repo: repo_slug(),
            default_branch: "main".into(),
            branch_prefix: "gantry/".into(),
            allowed_merge_methods: vec![MergeMethod::Squash],
        },
        &models,
        &levels,
        &policies,
        &sandbox,
        &egress,
        rev,
        table,
        BUILD_SCHEMA,
    )
    .unwrap();
    (runtime, levels.low)
}

#[tokio::test]
async fn merged_settings_drive_runtime_and_keep_existing_delivery_budget() {
    let world = World::new().await;
    let (initial, low) = settings(Seq::new(7).unwrap(), 240_000);
    let forge = Arc::new(FakeForge::new_in(world.store.clone()));
    forge.insert_project(Project {
        id: project_id(),
        repo: repo_slug(),
        installation_id: 1,
        bot_user_id: 2,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: ts(T0),
        updated_at: ts(T0),
    });
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
        snapshots: Some(world.snapshots.clone()),
    };
    let runtime = EngineRuntime::new(world.store.clone(), ports, initial)
        .await
        .unwrap();
    assert_eq!(
        runtime.status().await.unwrap().applied_rev,
        Seq::new(7).unwrap()
    );
    let mut request = new_delivery();
    request.budget = budget_from_level(&low);
    request.config_rev = Seq::new(7).unwrap();
    let delivery = runtime.engine().create_delivery(request).await.unwrap();
    let before = load_delivery_state(&world.store, &delivery)
        .await
        .unwrap()
        .unwrap()
        .delivery
        .budget;
    assert_eq!(before.limits.tokens_by_tier[&Tier::Worker], 240_000);

    let (updated, updated_low) = settings(Seq::new(8).unwrap(), 600_000);
    assert_eq!(updated_low.budget.tokens_by_tier.worker, 600_000);
    runtime.reconfigure(updated).await;
    assert_eq!(
        runtime.status().await.unwrap().applied_rev,
        Seq::new(8).unwrap()
    );
    assert_eq!(
        load_delivery_state(&world.store, &delivery)
            .await
            .unwrap()
            .unwrap()
            .delivery
            .budget,
        before
    );

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));
    tokio::time::timeout(
        Duration::from_secs(10),
        drive_to_closed(&world, &runtime, delivery),
    )
    .await
    .expect("delivery reaches closed");
    let events = world
        .store
        .events()
        .scan(
            Seq::ZERO,
            &EventFilter {
                project_ids: None,
                include: vec![TypePattern::Exact(EventType::AttemptStarted)],
                exclude: vec![],
            },
            200,
        )
        .await
        .unwrap()
        .events;
    assert_eq!(events.len(), 1);
    let started: AttemptStartedData = serde_json::from_value(events[0].data.clone()).unwrap();
    assert_eq!(started.config_rev, Seq::new(8).unwrap());
    assert_eq!(started.provider, "configured");
    assert_eq!(started.model, "claude-test");
    assert_eq!(started.tokens_total, 240_000);
    assert_ne!(started.tokens_total, 1000);
    assert_eq!(
        load_delivery_state(&world.store, &delivery)
            .await
            .unwrap()
            .unwrap()
            .delivery
            .budget,
        before
    );
    assert!(runtime.status().await.unwrap().degraded.is_empty());
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), main)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
