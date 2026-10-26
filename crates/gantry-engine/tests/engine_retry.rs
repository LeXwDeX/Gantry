//! `Engine::retry_node` 的存储集成测试。

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, NodeStatus, Seq};
use gantry_core::events::{NewEvent, Subject};
use gantry_core::ids::{DeliveryId, IdGen, MessageId, NodeId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::{Engine, EngineError};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Store, StoreDeps};
use serde_json::json;

const NODE: &str = "nod_01JB7M8V5X1Z3N6R9Q2K4W7YZC";
const WORKFLOW: &str = "wfl_01JB7M8T2X6V9P3Q5R1K4W7YZC";

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

async fn engine_with_failed_node() -> (Engine, DeliveryId) {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let inbox = Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store.clone()));
    let engine = Engine::new(store, inbox);
    let id = engine.create_delivery(new_delivery()).await.unwrap();
    let project = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap();

    for (ty, subject, data, dedup) in [
        (
            "workflow.created",
            WORKFLOW,
            json!({
                "stage": "implement",
                "template": "low.implement",
                "graph_rev": 1,
                "nodes": [{
                    "node_id": NODE,
                    "key": "single",
                    "role": "single",
                    "depends_on": [],
                    "required": true,
                    "critical": false
                }],
                "delivery_id": id.to_string(),
                "conversation_id": null
            }),
            Some("workflow.created:test".to_owned()),
        ),
        (
            "node.finished",
            NODE,
            json!({
                "node_id": NODE,
                "status": "failed",
                "verdict": null,
                "superseded_by": null
            }),
            Some("node.finished:test".to_owned()),
        ),
    ] {
        let event = NewEvent {
            ty: ty.parse().unwrap(),
            subject: Subject::parse(subject).unwrap(),
            project_id: Some(project),
            actor: Actor::new(ActorKind::Engine, "engine"),
            causation_id: None,
            correlation_id: Some(id.to_string()),
            data,
            dedup_key: dedup,
        };
        event.validate().unwrap();
        let mut tx = engine.store().begin_write().await.unwrap();
        tx.append(event).await.unwrap();
        tx.commit().await.unwrap();
    }
    (engine, id)
}

#[tokio::test]
async fn failed_node_is_requeued() {
    let (engine, _) = engine_with_failed_node().await;
    let node: NodeId = NODE.parse().unwrap();
    let retried = engine.retry_node(node, operator()).await.unwrap();
    assert_eq!(retried.status, NodeStatus::Queued);
    assert!(matches!(
        engine.retry_node(node, operator()).await,
        Err(EngineError::InvalidState(_))
    ));
}

#[tokio::test]
async fn unknown_node_is_not_found() {
    let (engine, _) = engine_with_failed_node().await;
    let missing = "nod_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap();
    assert!(matches!(
        engine.retry_node(missing, operator()).await,
        Err(EngineError::NotFound)
    ));
}

#[tokio::test]
async fn terminal_delivery_rejects_retry() {
    let (engine, id) = engine_with_failed_node().await;
    engine.abandon(id, None, operator()).await.unwrap();
    assert!(matches!(
        engine.retry_node(NODE.parse().unwrap(), operator()).await,
        Err(EngineError::InvalidState(_))
    ));
}
