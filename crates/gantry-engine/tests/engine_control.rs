//! `Engine` 交付控制（暂停 / 继续 / 放弃）的存储集成测试。

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, Seq, Stage, StageStatus};
use gantry_core::ids::{DeliveryId, IdGen, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::{Engine, EngineError};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Store, StoreDeps};

fn level_entry() -> LevelEntry {
    LevelEntry {
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
    }
}

fn new_delivery() -> NewDelivery {
    NewDelivery {
        id: "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
            .parse::<DeliveryId>()
            .unwrap(),
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA"
            .parse::<ProjectId>()
            .unwrap(),
        title: "fix login timeout".to_owned(),
        level_cap: Level::Low,
        level_effective: Level::Low,
        origin: DeliveryOrigin::Conversation,
        source_message_id: "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD"
            .parse::<MessageId>()
            .unwrap(),
        issues: vec![],
        budget: budget_from_level(&level_entry()),
        config_rev: Seq::new(1).unwrap(),
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
    }
}

fn operator() -> Actor {
    Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM")
}

async fn engine() -> (Engine, DeliveryId) {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let engine = Engine::new(store);
    let id = engine.create_delivery(new_delivery()).await.unwrap();
    (engine, id)
}

#[tokio::test]
async fn pause_is_idempotent_and_resume_restores() {
    let (engine, id) = engine().await;
    let paused = engine.pause(id.clone(), operator()).await.unwrap();
    assert_eq!(paused.stage_status, StageStatus::Paused);
    assert_eq!(paused.status_before_pause, Some(StageStatus::Running));

    let again = engine.pause(id.clone(), operator()).await.unwrap();
    assert_eq!(again, paused);
    assert_eq!(
        engine
            .store()
            .events()
            .scan(Seq::ZERO, &filter(), 50)
            .await
            .unwrap()
            .events
            .len(),
        2
    );

    let resumed = engine.resume(id.clone(), operator()).await.unwrap();
    assert_eq!(resumed.stage_status, StageStatus::Running);
    assert_eq!(resumed.status_before_pause, None);

    assert!(matches!(
        engine.resume(id, operator()).await,
        Err(EngineError::InvalidState(_))
    ));
}

#[tokio::test]
async fn abandon_is_terminal() {
    let (engine, id) = engine().await;
    let abandoned = engine
        .abandon(id.clone(), Some("not needed".to_owned()), operator())
        .await
        .unwrap();
    assert_eq!(abandoned.stage, Stage::Abandoned);
    assert_eq!(abandoned.stage_status, StageStatus::Done);

    assert!(matches!(
        engine.abandon(id.clone(), None, operator()).await,
        Err(EngineError::InvalidState(_))
    ));
    assert!(matches!(
        engine.pause(id, operator()).await,
        Err(EngineError::InvalidState(_))
    ));
}

#[tokio::test]
async fn unknown_delivery_is_not_found() {
    let (engine, _) = engine().await;
    let missing = "dlv_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap();
    assert!(matches!(
        engine.pause(missing, operator()).await,
        Err(EngineError::NotFound)
    ));
}

fn filter() -> gantry_store::EventFilter {
    gantry_store::EventFilter {
        project_ids: None,
        include: Vec::new(),
        exclude: Vec::new(),
    }
}
