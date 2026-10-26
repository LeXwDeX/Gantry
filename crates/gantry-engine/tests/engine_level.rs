//! `Engine::set_level_cap` 的存储集成测试。

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, Seq};
use gantry_core::ids::{DeliveryId, IdGen, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::{Engine, EngineError};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{EventFilter, Store, StoreDeps};

fn level_entry() -> LevelEntry {
    LevelEntry {
        graph_template: "high.multi_review".to_owned(),
        budget: LevelBudget {
            tokens_by_tier: TierBudget {
                utility: 50_000,
                planner: 400_000,
                worker: 2_000_000,
            },
            nodes: 20,
            rounds: 0,
            wall_time_s: 7200,
            concurrency: 2,
            forge_writes: 40,
            forge_writes_per_hour: 30,
        },
    }
}

fn new_delivery(cap: Level, effective: Level) -> NewDelivery {
    NewDelivery {
        id: "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse::<DeliveryId>().unwrap(),
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse::<ProjectId>().unwrap(),
        title: "fix login timeout".to_owned(),
        level_cap: cap,
        level_effective: effective,
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

async fn engine(cap: Level, effective: Level) -> (Engine, DeliveryId) {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[]).await.unwrap();
    let engine = Engine::new(store);
    let id = engine
        .create_delivery(new_delivery(cap, effective))
        .await
        .unwrap();
    (engine, id)
}

async fn event_count(engine: &Engine) -> usize {
    engine
        .store()
        .events()
        .scan(
            Seq::ZERO,
            &EventFilter {
                project_ids: None,
                include: Vec::new(),
                exclude: Vec::new(),
            },
            50,
        )
        .await
        .unwrap()
        .events
        .len()
}

#[tokio::test]
async fn raising_cap_keeps_effective() {
    let (engine, id) = engine(Level::Low, Level::Low).await;
    let updated = engine
        .set_level_cap(id, Level::Medium, operator())
        .await
        .unwrap();
    assert_eq!(updated.level_cap, Level::Medium);
    assert_eq!(updated.level_effective, Level::Low);
    assert_eq!(event_count(&engine).await, 2);
}

#[tokio::test]
async fn same_cap_writes_nothing() {
    let (engine, id) = engine(Level::Low, Level::Low).await;
    engine
        .set_level_cap(id, Level::Low, operator())
        .await
        .unwrap();
    assert_eq!(event_count(&engine).await, 1);
}

#[tokio::test]
async fn none_cap_is_rejected() {
    let (engine, id) = engine(Level::Low, Level::Low).await;
    assert!(matches!(
        engine.set_level_cap(id, Level::None, operator()).await,
        Err(EngineError::Validation(_))
    ));
}

#[tokio::test]
async fn lowering_below_effective_is_blocked_until_contract_allows_it() {
    let (engine, id) = engine(Level::High, Level::High).await;
    assert!(matches!(
        engine.set_level_cap(id, Level::Low, operator()).await,
        Err(EngineError::Unsupported(_))
    ));
    assert_eq!(event_count(&engine).await, 1);
}

#[tokio::test]
async fn terminal_delivery_rejects_cap_change() {
    let (engine, id) = engine(Level::Low, Level::Low).await;
    engine.abandon(id, None, operator()).await.unwrap();
    assert!(matches!(
        engine.set_level_cap(id, Level::Medium, operator()).await,
        Err(EngineError::InvalidState(_))
    ));
}
