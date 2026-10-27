//! `Engine::create_delivery` 的存储集成测试（内存 Store）。

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, Seq};
use gantry_core::events::EventType;
use gantry_core::ids::IdGen;
use gantry_core::ids::{DeliveryId, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_core::time::Timestamp;
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::Engine;
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{EventFilter, Store, StoreDeps};

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

fn delivery() -> NewDelivery {
    NewDelivery {
        id: "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
            .parse::<DeliveryId>()
            .unwrap(),
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA"
            .parse::<ProjectId>()
            .unwrap(),
        title: "fix login timeout".to_owned(),
        user_text: "修复登录超时".to_owned(),
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

async fn engine() -> Engine {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let inbox = Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store.clone()));
    Engine::new(store, inbox)
}

fn empty_filter() -> EventFilter {
    EventFilter {
        project_ids: None,
        include: Vec::new(),
        exclude: Vec::new(),
    }
}

#[tokio::test]
async fn create_delivery_appends_one_event() {
    let engine = engine().await;
    let id = engine.create_delivery(delivery()).await.expect("created");
    assert_eq!(id.to_string(), "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB");

    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].ty, EventType::DeliveryCreated);
    assert_eq!(page.events[0].subject.as_str(), id.to_string());
    assert_eq!(page.events[0].data["level_cap"], "Low");
}

#[tokio::test]
async fn same_source_message_is_idempotent() {
    let engine = engine().await;
    engine.create_delivery(delivery()).await.unwrap();
    engine.create_delivery(delivery()).await.unwrap();
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
}

#[tokio::test]
async fn invalid_delivery_is_not_appended() {
    let engine = engine().await;
    let mut delivery = delivery();
    delivery.level_cap = Level::None;
    delivery.level_effective = Level::None;
    assert!(engine.create_delivery(delivery).await.is_err());
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert!(page.events.is_empty());
}

#[tokio::test]
async fn event_time_comes_from_the_store_clock() {
    let engine = engine().await;
    engine.create_delivery(delivery()).await.unwrap();
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events[0].ts, Timestamp::from_millis(1_700_000_000_000));
}

/// 同一条消息重复创建：不追加第二条事件，返回第一次创建的交付单元（engine.md §6）。
#[tokio::test]
async fn duplicate_message_returns_the_existing_delivery() {
    let engine = engine().await;
    let first = engine.create_delivery(delivery()).await.expect("created");
    let mut again = delivery();
    again.id = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC"
        .parse::<DeliveryId>()
        .unwrap();
    let second = engine.create_delivery(again).await.expect("deduped");
    assert_eq!(second, first);
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
}
