//! 崩溃点矩阵（engine.md §12.2「重启一致」「不重复创建 PR」「release」，#74）。
//!
//! 每个崩溃点：让引擎在该点"死掉"（端口调用永不返回，随即中止运行时任务；或在引擎停机期间由
//! 外部模块写入事件），再用同一个 store 与同一组模块起新的 `EngineRuntime`，跑到 `closed`，
//! 与不崩溃的基线比较。断言：
//! - 阶段序列与基线相同，最终台账用量与基线相同；
//! - 没有重复的 `attempt.started`，同一 `job_id` 只有一个本地提交；
//! - 每个 Outbox 幂等键只有一行，`create_pr` 只执行一次（不重复创建 PR）；
//! - `release`：不崩溃时恰一次，跨重启至少一次（幂等）。

mod world;

use gantry_fakes::sandbox::SandboxCall;
use world::*;

/// 崩溃之后的观测结果。
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    stages: Vec<String>,
    attempts_started: usize,
    jobs: usize,
    commits: usize,
    /// 每个幂等键的 `outbox.enqueued` 行数。
    enqueued: BTreeMap<String, usize>,
    /// 按动作种类计的 `outbox.succeeded`。
    succeeded: BTreeMap<String, usize>,
    used: gantry_core::entities::BudgetUsed,
}

async fn all_events(world: &World) -> Vec<gantry_core::events::Event<serde_json::Value>> {
    let mut out = Vec::new();
    let mut after = Seq::ZERO;
    loop {
        let page = world
            .store
            .events()
            .scan(after, &EventFilter::default(), 200)
            .await
            .unwrap();
        if page.events.is_empty() {
            return out;
        }
        after = page.events.last().unwrap().seq;
        out.extend(page.events);
    }
}

async fn outcome(world: &World, delivery: DeliveryId) -> Outcome {
    let events = all_events(world).await;
    let count = |ty: EventType| events.iter().filter(|e| e.ty == ty).count();
    let mut enqueued = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    for event in events.iter().filter(|e| e.ty == EventType::OutboxEnqueued) {
        let key = event.data["action"]["idempotency_key"]
            .as_str()
            .unwrap()
            .to_owned();
        *enqueued.entry(key).or_insert(0) += 1;
        kinds.insert(
            event.subject.as_str().to_owned(),
            event.data["action"]["kind"].as_str().unwrap().to_owned(),
        );
    }
    let mut succeeded = BTreeMap::new();
    for event in events.iter().filter(|e| e.ty == EventType::OutboxSucceeded) {
        *succeeded
            .entry(kinds[event.subject.as_str()].clone())
            .or_insert(0) += 1;
    }
    let jobs = world.sandbox.jobs.lock().unwrap().clone();
    let state = load_delivery_state(&world.store, &delivery)
        .await
        .unwrap()
        .unwrap();
    Outcome {
        stages: world
            .stages(delivery)
            .await
            .into_iter()
            .map(|(to, _)| to)
            .collect(),
        attempts_started: count(EventType::AttemptStarted),
        jobs: jobs.len(),
        commits: jobs
            .iter()
            .filter(|job| world.repo.commit_record(job).is_some())
            .count(),
        enqueued,
        succeeded,
        used: state.ledger.used,
    }
}

/// 每个 job 的 `release` 次数。
fn releases(world: &World) -> Vec<usize> {
    let calls = world.sandbox.inner.calls();
    let jobs = world.sandbox.jobs.lock().unwrap().clone();
    jobs.iter()
        .map(|job| {
            calls
                .iter()
                .filter(|c| matches!(c, SandboxCall::Release { job_id, .. } if job_id == job))
                .count()
        })
        .collect()
}

struct Running {
    runtime: Arc<EngineRuntime>,
    main: tokio::task::JoinHandle<Result<(), gantry_engine::runtime::RuntimeError>>,
    stop: tokio::sync::oneshot::Sender<()>,
}

async fn start(world: &World) -> Running {
    let runtime = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));
    Running {
        runtime,
        main,
        stop,
    }
}

impl Running {
    /// 进程被杀：主循环任务立即中止，执行体内存随之丢失。
    async fn kill(self) {
        self.main.abort();
        let _ = self.main.await;
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        self.main.await.unwrap().unwrap();
    }
}

async fn wait_until<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn stage(world: &World, delivery: DeliveryId) -> Stage {
    load_delivery_state(&world.store, &delivery)
        .await
        .unwrap()
        .unwrap()
        .delivery
        .stage
}

async fn has(world: &World, ty: EventType) -> bool {
    all_events(world).await.iter().any(|e| e.ty == ty)
}

/// 重启后跑完，返回观测结果。
async fn restart_and_finish(world: &World, delivery: DeliveryId) -> Outcome {
    let running = start(world).await;
    drive_to_closed(world, &running.runtime, delivery).await;
    running.stop().await;
    outcome(world, delivery).await
}

async fn baseline() -> Outcome {
    let world = World::new().await;
    let running = start(&world).await;
    let delivery = running
        .runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    drive_to_closed(&world, &running.runtime, delivery).await;
    running.stop().await;
    // 不崩溃：每个 job 恰好 release 一次。
    assert_eq!(releases(&world), vec![1]);
    outcome(&world, delivery).await
}

fn assert_consistent(crashed: &Outcome, baseline: &Outcome, world: &World, point: &str) {
    assert_eq!(crashed, baseline, "crash point {point}");
    assert_eq!(baseline.attempts_started, 1);
    assert_eq!(baseline.commits, 1);
    assert!(baseline.enqueued.values().all(|n| *n == 1));
    assert_eq!(baseline.succeeded.get("create_pr"), Some(&1));
    // 跨重启：每个 job 至少 release 一次。
    assert!(
        releases(world).iter().all(|n| *n >= 1),
        "crash point {point}: {:?}",
        releases(world)
    );
}

/// 布防一个端口闸门，跑到该点后杀掉引擎，再重启跑完。
async fn crash_at_gate(point: Point) -> (World, Outcome) {
    let world = World::new().await;
    world.gate.arm(point);
    let running = start(&world).await;
    let delivery = running
        .runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    // Judged 点在 propose 阶段才触发；运行时自己推进到那里（forge 尚未执行，PR 动作判定先于入队）。
    let gate = world.gate.clone();
    tokio::time::timeout(Duration::from_secs(20), gate.hit())
        .await
        .unwrap_or_else(|_| panic!("gate {point:?} never hit"));
    running.kill().await;
    let outcome = restart_and_finish(&world, delivery).await;
    (world, outcome)
}

#[tokio::test]
async fn crash_points_converge_to_the_baseline() {
    let baseline = baseline().await;

    for point in [
        Point::AttemptStarted,
        Point::BeforeCommit,
        Point::AfterCommit,
        Point::Judged,
    ] {
        let (world, crashed) = crash_at_gate(point).await;
        assert_consistent(&crashed, &baseline, &world, &format!("{point:?}"));
    }
}

/// `delivery.created` 之后、引擎处理之前崩溃：重启后由 `Tick(Recover)` 接手。
#[tokio::test]
async fn crash_after_delivery_created() {
    let baseline = baseline().await;
    let world = World::new().await;
    let first = start(&world).await;
    first.kill().await;
    // 引擎不在时写入（create_delivery 是同步写日志的入口）。
    let delivery = world
        .runtime()
        .await
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    let crashed = restart_and_finish(&world, delivery).await;
    assert_consistent(&crashed, &baseline, &world, "delivery.created");
}

/// 入队之后（forge 尚未执行）崩溃；以及 `outbox.succeeded` 之后（引擎尚未处理）崩溃。
#[tokio::test]
async fn crash_after_enqueue_and_after_outbox_succeeded() {
    let baseline = baseline().await;

    // 入队之后：push_branch / create_pr 已入队，引擎被杀；forge 在引擎停机期间不执行。
    let world = World::new().await;
    let running = start(&world).await;
    let delivery = running
        .runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    wait_until("outbox.enqueued", || has(&world, EventType::OutboxEnqueued)).await;
    running.kill().await;
    let crashed = restart_and_finish(&world, delivery).await;
    assert_consistent(&crashed, &baseline, &world, "outbox.enqueued");

    // outbox.succeeded 之后：引擎停机期间 forge 执行完（含 create_pr），重启后才看到成功事件。
    let world = World::new().await;
    let running = start(&world).await;
    let delivery = running
        .runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    wait_until("propose", || async {
        stage(&world, delivery).await == Stage::Propose
            && has(&world, EventType::OutboxEnqueued).await
    })
    .await;
    running.kill().await;
    let done = world.execute_outbox(world.committed()).await;
    assert!(done.contains(&ForgeActionKind::CreatePr), "{done:?}");
    let crashed = restart_and_finish(&world, delivery).await;
    assert_consistent(&crashed, &baseline, &world, "outbox.succeeded");
}
