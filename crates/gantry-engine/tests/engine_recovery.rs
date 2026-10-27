//! 崩溃恢复：同一事件日志重建引擎后继续推进。

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, Seq, Stage, StageStatus};
use gantry_core::ids::{DeliveryId, IdGen, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::StageTrigger;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::{Engine, StageRequest};
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

fn delivery(id: &str, message: &str) -> NewDelivery {
    NewDelivery {
        id: id.parse::<DeliveryId>().unwrap(),
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA"
            .parse::<ProjectId>()
            .unwrap(),
        title: "fix login timeout".to_owned(),
        user_text: "修复登录超时".to_owned(),
        level_cap: Level::Low,
        level_effective: Level::Low,
        origin: DeliveryOrigin::Conversation,
        source_message_id: message.parse::<MessageId>().unwrap(),
        issues: vec![],
        budget: budget_from_level(&level_entry()),
        config_rev: Seq::new(1).unwrap(),
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
    }
}

fn stage_request(id: DeliveryId) -> StageRequest {
    StageRequest {
        delivery_id: id,
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap(),
        from: Stage::Spec,
        level_effective: Level::Low,
        trigger: StageTrigger::AdmissionSkipped,
        reason: None,
        actor: Actor::new(ActorKind::Engine, "engine"),
    }
}

#[tokio::test]
async fn replayed_events_keep_deliveries_isolated() {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let engine = Engine::new(
        store.clone(),
        Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store.clone())),
    );

    let first: DeliveryId = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap();
    let second: DeliveryId = "dlv_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap();
    engine
        .create_delivery(delivery(
            &first.to_string(),
            "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD",
        ))
        .await
        .unwrap();
    engine
        .create_delivery(delivery(
            &second.to_string(),
            "msg_01JB7N3R5T8X2V6M9Q4K1W7YZE",
        ))
        .await
        .unwrap();
    engine.advance_stage(stage_request(first)).await.unwrap();

    let recovered = Engine::new(
        store.clone(),
        Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store)),
    );
    assert_eq!(recovered.template_catalog().templates, ["low.single"]);
    assert_eq!(
        recovered
            .load_delivery(&first)
            .await
            .unwrap()
            .unwrap()
            .stage,
        Stage::Implement
    );
    assert_eq!(
        recovered
            .load_delivery(&second)
            .await
            .unwrap()
            .unwrap()
            .stage,
        Stage::Spec
    );

    let paused = recovered
        .pause(first, Actor::new(ActorKind::Device, "dev"))
        .await
        .unwrap();
    assert_eq!(paused.stage_status, StageStatus::Paused);
    let resumed = recovered
        .resume(first, Actor::new(ActorKind::Device, "dev"))
        .await
        .unwrap();
    assert_eq!(resumed.stage_status, StageStatus::Running);
}
