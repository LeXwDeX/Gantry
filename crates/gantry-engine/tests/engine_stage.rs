//! `Engine::advance_stage` 的存储集成测试。

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, Seq, Stage};
use gantry_core::events::EventType;
use gantry_core::ids::{DeliveryId, IdGen, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::StageTrigger;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::{Engine, StageRequest};
use gantry_engine::Projection;
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

async fn engine_and_delivery() -> (Engine, DeliveryId) {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let engine = Engine::new(store);
    let id = engine.create_delivery(delivery()).await.unwrap();
    (engine, id)
}

fn request(id: &DeliveryId, trigger: StageTrigger, reason: Option<&str>) -> StageRequest {
    StageRequest {
        delivery_id: id.to_string().parse().unwrap(),
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap(),
        from: Stage::Spec,
        level_effective: Level::Low,
        trigger,
        reason: reason.map(str::to_owned),
        actor: Actor::new(ActorKind::Engine, "engine"),
    }
}

fn empty_filter() -> EventFilter {
    EventFilter {
        project_ids: None,
        include: Vec::new(),
        exclude: Vec::new(),
    }
}

async fn events(engine: &Engine) -> Vec<gantry_core::events::Event<serde_json::Value>> {
    engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 50)
        .await
        .unwrap()
        .events
}

#[tokio::test]
async fn advance_appends_stage_changed_and_projects() {
    let (engine, id) = engine_and_delivery().await;
    let transition = engine
        .advance_stage(request(
            &id,
            StageTrigger::AdmissionSkipped,
            Some("admission_skipped"),
        ))
        .await
        .expect("advance");
    assert_eq!(transition.to, Stage::Implement);

    let all = events(&engine).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].ty, EventType::DeliveryStageChanged);
    assert_eq!(all[1].data["to"], "implement");
    assert_eq!(all[1].data["reason"], "admission_skipped");

    let mut projection = Projection::default();
    for event in &all {
        projection.apply(event).unwrap();
    }
    let delivery = projection.delivery.as_ref().unwrap();
    assert_eq!(delivery.stage, Stage::Implement);
    assert_eq!(
        delivery.stage_status,
        gantry_core::entities::StageStatus::Running
    );
}

#[tokio::test]
async fn illegal_transition_is_not_appended() {
    let (engine, id) = engine_and_delivery().await;
    let mut request = request(&id, StageTrigger::MergedAndIssuesClosed, None);
    request.from = Stage::Spec;
    assert!(engine.advance_stage(request).await.is_err());
    assert_eq!(events(&engine).await.len(), 1);
}

#[tokio::test]
async fn terminal_transition_sets_done() {
    let (engine, id) = engine_and_delivery().await;
    engine
        .advance_stage(request(&id, StageTrigger::AdmissionSkipped, None))
        .await
        .unwrap();
    for (from, trigger) in [
        (Stage::Implement, StageTrigger::PatchValidated),
        (Stage::Propose, StageTrigger::PrCreated),
        (Stage::Review, StageTrigger::ReviewAcceptCiGreen),
        (Stage::Merge, StageTrigger::MergedAndIssuesClosed),
    ] {
        let mut request = request(&id, trigger, None);
        request.from = from;
        engine.advance_stage(request).await.unwrap();
    }
    let mut projection = Projection::default();
    for event in events(&engine).await {
        projection.apply(&event).unwrap();
    }
    assert_eq!(projection.delivery.unwrap().stage, Stage::Closed);
}
