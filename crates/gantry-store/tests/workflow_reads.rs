#![cfg(feature = "testing")]

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, NodeStatus, Stage, WorkflowStatus};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{Order, Page, Store, StoreDeps};
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const WORKFLOW: &str = "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF";
const BUILD: &str = "nod_01JB7M7V4X2N9R5T8Q3K6W1YZG";
const VERIFY: &str = "nod_01JB7M7W5X3N9R6T8Q2K4V1YZH";
const ATTEMPT: &str = "att_01JB7M9X5N3R8T2Q6K1V4W7YZH";

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

fn event(ty: EventType, subject: &str, data: serde_json::Value) -> NewEvent {
    NewEvent {
        ty,
        subject: Subject::parse(subject).unwrap(),
        project_id: Some(PROJECT.parse::<ProjectId>().unwrap()),
        actor: Actor::new(ActorKind::System, "test"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    }
}

fn workflow_created() -> NewEvent {
    event(
        EventType::WorkflowCreated,
        WORKFLOW,
        json!({
            "stage": "implement",
            "template": "medium.implement",
            "graph_rev": 1,
            "delivery_id": DELIVERY,
            "nodes": [
                {"node_id": BUILD, "key": "build", "role": "build", "depends_on": [], "required": true, "critical": false},
                {"node_id": VERIFY, "key": "verify", "role": "verify", "depends_on": ["build"], "required": true, "critical": false}
            ]
        }),
    )
}

#[tokio::test]
async fn workflow_projection_tracks_nodes_and_attempts() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(workflow_created()).await.unwrap();
    tx.append(event(
        EventType::NodeQueued,
        BUILD,
        json!({"node_id": BUILD, "status": "queued"}),
    ))
    .await
    .unwrap();
    tx.append(event(
        EventType::NodeStarted,
        BUILD,
        json!({"node_id": BUILD, "status": "running"}),
    ))
    .await
    .unwrap();
    tx.append(event(
        EventType::AttemptStarted,
        ATTEMPT,
        json!({
            "attempt_id": ATTEMPT, "node_id": BUILD, "n": 1,
            "provider": "anthropic", "model": "claude", "reasoning_effort": "medium",
            "config_rev": 1,
            "base_sha": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
            "tokens_total": 1000, "wall_time_s": 600
        }),
    ))
    .await
    .unwrap();
    tx.append(event(
        EventType::AttemptFinished,
        ATTEMPT,
        json!({
            "status": "succeeded",
            "result_ref": "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9",
            "result_tree": "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
            "result_commit": "d7d018f867d6c5b78e82906386420d45a4d84486",
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_tokens": 0, "cache_write_tokens": 0, "requests": 1}
        }),
    ))
    .await
    .unwrap();
    tx.append(event(
        EventType::NodeFinished,
        BUILD,
        json!({"node_id": BUILD, "status": "succeeded"}),
    ))
    .await
    .unwrap();
    tx.append(event(
        EventType::WorkflowFinished,
        WORKFLOW,
        json!({"status": "succeeded"}),
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut snap = store.snapshot().await.unwrap();
    let workflow = snap.workflow(&WORKFLOW.parse().unwrap()).unwrap();
    assert_eq!(workflow.status, WorkflowStatus::Succeeded);
    assert_eq!(workflow.stage, Some(Stage::Implement));
    assert_eq!(workflow.nodes.len(), 2);
    let build = workflow
        .nodes
        .iter()
        .find(|node| node.id.to_string() == BUILD)
        .unwrap();
    assert_eq!(build.status, NodeStatus::Succeeded);
    assert!(build.depends_on.is_empty());
    assert_eq!(build.attempts.len(), 1);
    assert_eq!(build.attempts[0].n, 1);
    assert!(build.attempts[0].ended_at.is_some());
    let verify = workflow
        .nodes
        .iter()
        .find(|node| node.id.to_string() == VERIFY)
        .unwrap();
    assert_eq!(verify.depends_on.len(), 1);
    assert_eq!(verify.depends_on[0].to_string(), BUILD);

    // 按交付单元过滤；不匹配的返回空页。
    let page = Page {
        limit: 10,
        cursor: None,
        order: Order::Asc,
    };
    let paged = snap
        .workflows(&DELIVERY.parse().unwrap(), Some(Stage::Implement), &page)
        .await
        .unwrap();
    assert_eq!(paged.items.len(), 1);
    assert_eq!(paged.items[0].id.to_string(), WORKFLOW);

    // attempts 默认按 n 升序，键集分页。
    let one = snap
        .attempts(
            &BUILD.parse().unwrap(),
            &Page {
                limit: 1,
                cursor: None,
                order: Order::Asc,
            },
        )
        .await
        .unwrap();
    assert_eq!(one.items.len(), 1);
    assert!(snap.attempt(&ATTEMPT.parse().unwrap()).is_some());
}

#[tokio::test]
async fn workflow_revision_adds_nodes_and_marks_superseded() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(workflow_created()).await.unwrap();
    tx.append(event(
        EventType::WorkflowRevised,
        WORKFLOW,
        json!({
            "graph_rev": 2, "reason": "review", "round": 1,
            "added": [{"node_id": "nod_01JB7M7X6Y4P9S7T8R3K5V2YZJ", "key": "fix", "role": "build", "depends_on": ["verify"], "required": true, "critical": false}],
            "superseded": [VERIFY]
        }),
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let snap = store.snapshot().await.unwrap();
    let workflow = snap.workflow(&WORKFLOW.parse().unwrap()).unwrap();
    assert_eq!(workflow.graph_rev, 2);
    assert_eq!(workflow.nodes.len(), 3);
    assert!(workflow
        .nodes
        .iter()
        .any(|node| node.id.to_string() == VERIFY && node.status == NodeStatus::Superseded));
}
