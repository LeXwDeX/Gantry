//! `Engine::raise_inbox` 的存储集成测试。

use std::sync::Arc;

use gantry_core::entities::{
    Actor, ActorKind, DeliveryOrigin, InboxKind, InboxPayload, InboxStatus, MissingAspect, Seq,
    Severity,
};
use gantry_core::ids::{DeliveryId, IdGen, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::inbox::{DedupKey, RaiseRequest, SafeSummary};
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
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

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "engine"),
        device: None,
        role: None,
        client_profile: None,
        request_id: "01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn spec_needed(delivery: DeliveryId) -> RaiseRequest {
    RaiseRequest {
        project_id: Some("prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap()),
        delivery_id: Some(delivery),
        kind: InboxKind::SpecNeeded,
        severity: Severity::Blocker,
        summary: SafeSummary::new("需要验收标准"),
        payload: InboxPayload::SpecNeeded {
            missing: vec![MissingAspect {
                aspect: "acceptance".to_owned(),
                question: "什么算完成？".to_owned(),
            }],
        },
        dedup_key: DedupKey::new("spec_needed:test"),
        supersedes: vec![],
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
    }
}

async fn engine() -> (Engine, DeliveryId) {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let inbox = Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store.clone()));
    let engine = Engine::new(store, inbox);
    let id = engine.create_delivery(new_delivery()).await.unwrap();
    (engine, id)
}

async fn events(engine: &Engine) -> Vec<gantry_core::events::Event<serde_json::Value>> {
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
}

#[tokio::test]
async fn raising_writes_inbox_event() {
    let (engine, id) = engine().await;
    let item = engine.raise_inbox(spec_needed(id), &ctx()).await.unwrap();
    assert_eq!(item.status, InboxStatus::Open);
    assert_eq!(item.kind, InboxKind::SpecNeeded);

    let all = events(&engine).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].ty, gantry_core::events::EventType::InboxRaised);
    assert_eq!(all[1].data["item"]["kind"], "spec_needed");
}

#[tokio::test]
async fn same_dedup_key_returns_the_same_item() {
    let (engine, id) = engine().await;
    let first = engine.raise_inbox(spec_needed(id), &ctx()).await.unwrap();
    let second = engine.raise_inbox(spec_needed(id), &ctx()).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(events(&engine).await.len(), 2);
}
