//! 事件投影：Low 主路径与未知事件忽略。

use gantry_core::entities::{Actor, ActorKind, Seq, Stage, StageStatus};
use gantry_core::events::{Event, Subject};
use gantry_core::ids::{EventId, ProjectId};
use gantry_core::time::Timestamp;
use gantry_engine::projection::Projection;
use serde_json::{json, Value};

const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const WORKFLOW: &str = "wfl_01JB7M8T2X6V9P3Q5R1K4W7YZC";
const NODE: &str = "nod_01JB7M8V5X1Z3N6R9Q2K4W7YZC";
const ATTEMPT: &str = "att_01JB7X4Q9ZK3M8T5V2N7RHC6WY";

fn event(seq: u64, ty: &str, subject: &str, data: Value) -> Event<Value> {
    Event {
        id: format!("evt_{:0>26}", seq).parse::<EventId>().unwrap(),
        seq: Seq::new(seq).unwrap(),
        ts: Timestamp::from_millis(1_700_000_000_000),
        ty: ty.parse().unwrap(),
        subject: Subject::parse(subject).unwrap(),
        project_id: Some(PROJECT.parse::<ProjectId>().unwrap()),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data,
    }
}

fn low_events() -> Vec<Event<Value>> {
    vec![
        event(
            1,
            "delivery.created",
            DELIVERY,
            json!({
                "title": "fix login timeout",
                "level_cap": "Low",
                "level_effective": "Low",
                "origin": "conversation",
                "source_message_id": null,
                "issues": [],
                "budget": {
                    "template": "low.single",
                    "limits": {
                        "tokens_by_tier": {"utility": 0, "planner": 0, "worker": 0},
                        "nodes": 8,
                        "rounds": 0,
                        "wall_time": 3600,
                        "forge_writes": 20,
                        "concurrency": 1
                    }
                },
                "config_rev": 1
            }),
        ),
        event(
            2,
            "delivery.stage_changed",
            DELIVERY,
            json!({
                "from": "spec",
                "to": "implement",
                "reason": "admission_skipped",
                "stage_status": "running"
            }),
        ),
        event(
            3,
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
                "delivery_id": DELIVERY,
                "conversation_id": null
            }),
        ),
        event(
            4,
            "node.started",
            NODE,
            json!({"node_id": NODE, "status": "running", "verdict": null, "superseded_by": null}),
        ),
        event(
            5,
            "attempt.started",
            ATTEMPT,
            json!({
                "attempt_id": ATTEMPT,
                "node_id": NODE,
                "n": 1,
                "provider": "anthropic",
                "model": "claude",
                "reasoning_effort": "medium",
                "config_rev": 2,
                "base_sha": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
                "tokens_total": 1000,
                "wall_time_s": 600
            }),
        ),
        event(
            6,
            "attempt.finished",
            ATTEMPT,
            json!({
                "status": "succeeded",
                "result_ref": "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9",
                "failure_class": null,
                "failure_detail": null,
                "usage": null,
                "result_tree": null,
                "served_by": null
            }),
        ),
        event(
            7,
            "node.finished",
            NODE,
            json!({"node_id": NODE, "status": "succeeded", "verdict": null, "superseded_by": null}),
        ),
        event(
            8,
            "delivery.stage_changed",
            DELIVERY,
            json!({"from": "implement", "to": "propose", "reason": null, "stage_status": "running"}),
        ),
        event(
            9,
            "delivery.stage_changed",
            DELIVERY,
            json!({"from": "propose", "to": "review", "reason": null, "stage_status": "running"}),
        ),
        event(
            10,
            "delivery.stage_changed",
            DELIVERY,
            json!({"from": "review", "to": "merge", "reason": null, "stage_status": "running"}),
        ),
        event(
            11,
            "delivery.stage_changed",
            DELIVERY,
            json!({"from": "merge", "to": "closed", "reason": null, "stage_status": "done"}),
        ),
    ]
}

#[test]
fn low_delivery_projects_stage_nodes_and_attempts() {
    let mut projection = Projection::default();
    for ev in low_events() {
        projection.apply(&ev).expect("event must project");
    }

    let delivery = projection.delivery.as_ref().expect("delivery");
    assert_eq!(delivery.id.to_string(), DELIVERY);
    assert_eq!(delivery.stage, Stage::Closed);
    assert_eq!(delivery.stage_status, StageStatus::Done);
    assert_eq!(delivery.config_rev, 1);

    let workflow = &projection.workflows[&WORKFLOW.parse().unwrap()];
    assert_eq!(workflow.template, "low.implement");
    assert_eq!(workflow.graph_rev, 1);
    assert_eq!(workflow.nodes.len(), 1);
    assert_eq!(workflow.nodes[&NODE.parse().unwrap()], "single");

    let node = &projection.nodes[&NODE.parse().unwrap()];
    assert_eq!(node.key, "single");
    assert_eq!(node.status, gantry_core::entities::NodeStatus::Succeeded);
    assert_eq!(node.workflow_id.to_string(), WORKFLOW);

    let attempt = &projection.attempts[&ATTEMPT.parse().unwrap()];
    assert_eq!(attempt.node_id.to_string(), NODE);
    assert_eq!(
        attempt.status,
        Some(gantry_core::entities::AttemptStatus::Succeeded)
    );

    assert_eq!(projection.last_seq.unwrap().as_u64(), 11);
}

#[test]
fn unrelated_events_are_ignored_without_state_change() {
    let mut projection = Projection::default();
    let ev = event(1, "settings.updated", "settings", json!({}));
    projection.apply(&ev).expect("unknown shape ignored");
    assert!(projection.delivery.is_none());
    assert_eq!(projection.last_seq.unwrap().as_u64(), 1);
}

#[test]
fn delivery_id_is_required_from_subject() {
    let mut projection = Projection::default();
    let mut ev = low_events().remove(0);
    ev.subject = Subject::parse(WORKFLOW).unwrap();
    assert!(projection.apply(&ev).is_err());
}

#[test]
fn delivery_created_must_precede_stage_change() {
    let mut projection = Projection::default();
    let ev = event(
        1,
        "delivery.stage_changed",
        DELIVERY,
        json!({"from": "spec", "to": "implement", "reason": null, "stage_status": "running"}),
    );
    assert!(projection.apply(&ev).is_err());
}

#[test]
fn attempt_must_follow_started_event() {
    let mut projection = Projection::default();
    for ev in low_events().into_iter().take(5) {
        projection.apply(&ev).unwrap();
    }
    let mut finished = low_events().remove(5);
    finished.subject = Subject::parse("att_01JB7X4Q9ZK3M8T5V2N7RHC6WX").unwrap();
    assert!(projection.apply(&finished).is_err());
}
