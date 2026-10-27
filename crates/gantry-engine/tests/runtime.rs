//! 引擎运行时（engine.md §7.15，#277）：`EngineRuntime::new` + `run(shutdown)` 在进程内用替身
//! 跑通 §12.2「端到端（替身）」主路径 spec → implement → propose → review → merge → closed。
//!
//! 生产装配只换端口：`FakeSandboxService`（按派发的 job 现场脚本化黄金结果）、`FakeRepo`（外包一层
//! 把 `worktree.*` / `patch.validated` 写进 store，与真实 repo 一致）、`FakePolicy`、`FakeForge`
//! （事务内入队）。测试只扮演 forge 执行器与 GitHub 快照来源，不直接调用执行体。

mod world;

use world::*;

#[tokio::test]
async fn low_delivery_reaches_closed_with_new_and_run_only() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));

    // 建单走现有写入口；之后不再直接碰执行体。
    let delivery = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    let driven = tokio::time::timeout(
        Duration::from_secs(5),
        drive_to_closed(&world, &runtime, delivery),
    )
    .await;
    if driven.is_err() && main.is_finished() {
        panic!("run exited: {:?}", main.await);
    }

    let stages = world.stages(delivery).await;
    let order: Vec<&str> = stages.iter().map(|(to, _)| to.as_str()).collect();
    assert_eq!(order, ["implement", "propose", "review", "merge", "closed"]);
    assert_eq!(stages[0].1, "admission_skipped");
    assert_eq!(world.sandbox.jobs.lock().unwrap().len(), 1);
    assert!(runtime.degraded().await.is_empty());

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), main)
        .await
        .expect("run returns after shutdown")
        .unwrap()
        .unwrap();
    assert_eq!(
        world.store.cursors().load(CURSOR).await.unwrap(),
        world.store.events().head().await.unwrap(),
        "cursor catches up with the log head"
    );
}

#[tokio::test]
async fn restart_resumes_from_the_cursor_without_redispatching() {
    let world = World::new().await;
    let first = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(first.clone().run(async {
        let _ = stopped.await;
    }));
    let delivery = first
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();

    // 跑到 propose（补丁已提交、PR 动作已入队）后停机。
    let deadline = Instant::now() + Duration::from_secs(20);
    while load_delivery_state(&world.store, &delivery)
        .await
        .unwrap()
        .unwrap()
        .delivery
        .stage
        != Stage::Propose
    {
        assert!(Instant::now() < deadline, "never reached propose");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();

    // 新进程：同一 store，新的运行时；Tick(Recover) 接管，不重新派发尝试。
    let second = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(second.clone().run(async {
        let _ = stopped.await;
    }));
    drive_to_closed(&world, &second, delivery).await;
    assert_eq!(world.sandbox.jobs.lock().unwrap().len(), 1);
    let order: Vec<String> = world
        .stages(delivery)
        .await
        .into_iter()
        .map(|(to, _)| to)
        .collect();
    assert_eq!(order, ["implement", "propose", "review", "merge", "closed"]);
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();
}

/// 端口拒绝事务内决策（这里：policy 放行了不带审批的 merge_pr，forge 按 I8 拒绝入队）是引擎缺陷：
/// 只让该交付单元停止决策，主循环继续运行（§7.14.1）。
#[tokio::test]
async fn a_rejected_enqueue_degrades_only_that_delivery() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));
    let delivery = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    let tick = runtime.mailbox();
    let deadline = Instant::now() + Duration::from_secs(20);
    while runtime.degraded().await.is_empty() {
        let head = world.committed();
        world.execute_outbox(head).await;
        if let Some(head) = head {
            *world.snapshots.0.lock().unwrap() = Some(snapshot("open", head));
            tick(
                delivery,
                Input::Tick(TickReason::Settle),
                InputIdentity::Tick(TickReason::Settle),
            );
        }
        assert!(Instant::now() < deadline, "never degraded");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let degraded = runtime.degraded().await;
    assert!(degraded[&delivery].contains("approval"), "{degraded:?}");
    assert!(!main.is_finished(), "run keeps going");
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// DeliveryControl（#73）：命令收 ActorCtx，经执行体由 decide 求值
// ---------------------------------------------------------------------------

fn ctx(key: &str, n: u128) -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        device: None,
        role: None,
        client_profile: None,
        request_id: gantry_core::ids::Ulid::from_parts(T0 as u64, n).unwrap(),
        idempotency_key: None,
        dedup_key: Some(key.to_owned()),
        causation_id: None,
    }
}

async fn head(world: &World) -> Seq {
    world.store.events().head().await.unwrap()
}

#[tokio::test]
async fn delivery_control_goes_through_the_actor() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let id = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();

    // pause：写 delivery.paused（dedup_key = ctx.dedup_key，actor 取调用方）。
    let paused = runtime.pause(ctx("p1", 1), id).await.unwrap();
    assert_eq!(paused.stage_status, StageStatus::Paused);
    assert_eq!(paused.id, id);
    let seq = world
        .store
        .events()
        .by_dedup_key("p1")
        .await
        .unwrap()
        .unwrap();
    let event = world.store.events().get_by_seq(seq).await.unwrap().unwrap();
    assert_eq!(event.ty, EventType::DeliveryPaused);
    assert_eq!(event.actor.kind, ActorKind::Device);
    // 同一请求重放 → 首次结果，不写事件。
    let before = head(&world).await;
    assert_eq!(
        runtime.pause(ctx("p1", 2), id).await.unwrap().stage_status,
        StageStatus::Paused
    );
    assert_eq!(head(&world).await, before);
    // 暂停期间不派发。
    assert!(world.sandbox.jobs.lock().unwrap().is_empty());

    // resume；新请求在未暂停时 → InvalidState。
    let resumed = runtime.resume(ctx("r1", 3), id).await.unwrap();
    assert_eq!(resumed.stage, Stage::Implement);
    assert!(matches!(
        runtime.resume(ctx("r2", 4), id).await,
        Err(ModuleError::InvalidState { .. })
    ));

    // set_level_cap(None) → Validation，指向 /level_cap。
    assert!(matches!(
        runtime.set_level_cap(ctx("c1", 5), id, Level::None).await,
        Err(ModuleError::Validation { pointer, .. }) if pointer == "/level_cap"
    ));

    // abandon → 终态；之后的新命令 → InvalidState。
    let abandoned = runtime
        .abandon(ctx("a1", 6), id, Some("not needed".to_owned()))
        .await
        .unwrap();
    assert_eq!(abandoned.stage, Stage::Abandoned);
    assert!(matches!(
        runtime.pause(ctx("p2", 7), id).await,
        Err(ModuleError::InvalidState { .. })
    ));

    // 未知交付单元 → NotFound，且不留下执行体。
    let missing: DeliveryId = "dlv_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap();
    assert!(matches!(
        runtime.pause(ctx("p3", 8), missing).await,
        Err(ModuleError::NotFound)
    ));
    assert!(runtime.degraded().await.is_empty());
}

#[tokio::test]
async fn retry_node_through_delivery_control() {
    let world = World::new().await;
    *world.sandbox.fail.lock().unwrap() = true;
    let runtime = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));
    let id = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();

    let failed_node = || async {
        let state = load_delivery_state(&world.store, &id)
            .await
            .unwrap()
            .unwrap();
        state
            .workflows
            .values()
            .flat_map(|w| w.nodes.values())
            .find(|n| n.status == NodeStatus::Failed)
            .map(|n| n.id)
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let node = loop {
        if let Some(node) = failed_node().await {
            break node;
        }
        assert!(Instant::now() < deadline, "node never failed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    // 正例：failed → queued，随后重新派发（这次成功）。
    *world.sandbox.fail.lock().unwrap() = false;
    let retried = runtime.retry_node(ctx("retry-1", 1), node).await.unwrap();
    assert_eq!(retried.id, node);
    assert_ne!(retried.status, NodeStatus::Failed);
    // 同一请求重放 → Ok；新请求（节点已不是 failed）→ InvalidState。
    assert!(runtime.retry_node(ctx("retry-1", 2), node).await.is_ok());
    assert!(matches!(
        runtime.retry_node(ctx("retry-2", 3), node).await,
        Err(ModuleError::InvalidState { .. })
    ));
    // 未知节点 → NotFound。
    let missing: NodeId = "nod_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap();
    assert!(matches!(
        runtime.retry_node(ctx("retry-3", 4), missing).await,
        Err(ModuleError::NotFound)
    ));
    let deadline = Instant::now() + Duration::from_secs(20);
    while world.sandbox.jobs.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "retry never dispatched");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();
}
