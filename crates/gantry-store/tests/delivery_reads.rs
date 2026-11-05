#![cfg(feature = "testing")]

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, Stage};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{DeliveryQuery, Order, Page, Store, StoreDeps};
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const OTHER_PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZB";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const OTHER_DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC";
const WORKTREE: &str = "wtr_01JB7M8Q2X5V9N3R6T1K4W0YZE";

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

fn event(ty: EventType, subject: &str, project: &str, data: serde_json::Value) -> NewEvent {
    NewEvent {
        ty,
        subject: Subject::parse(subject).unwrap(),
        project_id: Some(project.parse::<ProjectId>().unwrap()),
        actor: Actor::new(ActorKind::System, "test"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    }
}

fn created(subject: &str, project: &str, title: &str) -> NewEvent {
    event(
        EventType::DeliveryCreated,
        subject,
        project,
        json!({
            "title": title,
            "level_cap": "Medium",
            "level_effective": "Low",
            "origin": "conversation",
            "source_message_id": null,
            "user_text": "修复登录超时",
            "issues": [],
            "budget": {
                "template": "low.single",
                "limits": {
                    "tokens_by_tier": {"utility": 50000, "planner": 100000, "worker": 1000000},
                    "nodes": 6, "rounds": 0, "wall_time": 3600, "forge_writes": 20, "concurrency": 1
                }
            },
            "config_rev": 1
        }),
    )
}

fn stage(subject: &str, project: &str, from: &str, to: &str) -> NewEvent {
    event(
        EventType::DeliveryStageChanged,
        subject,
        project,
        json!({"from": from, "to": to, "reason": null, "stage_status": "running"}),
    )
}

#[tokio::test]
async fn delivery_rows_filter_page_and_track_worktree() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(created(DELIVERY, PROJECT, "fix login"))
        .await
        .unwrap();
    tx.append(created(OTHER_DELIVERY, OTHER_PROJECT, "other"))
        .await
        .unwrap();
    tx.append(event(
        EventType::WorktreeCreated,
        WORKTREE,
        PROJECT,
        json!({"worktree_id": WORKTREE, "project_id": PROJECT, "delivery_id": DELIVERY,
               "branch": "gantry/dlv-1", "base_ref": "main",
               "base_sha": "1405df66cbe219b0bf6355bc3d60361a8376b6b4"}),
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut snap = store.snapshot().await.unwrap();
    let row = snap.delivery(&DELIVERY.parse().unwrap()).unwrap();
    assert_eq!(
        row.worktree_id.map(|id| id.to_string()),
        Some(WORKTREE.to_owned())
    );
    assert!(row.updated_at >= row.created_at);

    let page = Page {
        limit: 10,
        cursor: None,
        order: Order::Asc,
    };
    let project = DeliveryQuery {
        project_id: Some(PROJECT.parse().unwrap()),
        ..DeliveryQuery::default()
    };
    assert_eq!(
        snap.deliveries(&project, &page).await.unwrap().items.len(),
        1
    );

    let spec = DeliveryQuery {
        stages: vec![Stage::Spec],
        ..DeliveryQuery::default()
    };
    assert_eq!(snap.deliveries(&spec, &page).await.unwrap().items.len(), 2);
    let closed = DeliveryQuery {
        stages: vec![Stage::Closed],
        ..DeliveryQuery::default()
    };
    assert!(snap
        .deliveries(&closed, &page)
        .await
        .unwrap()
        .items
        .is_empty());

    // 键集分页：limit=1 的下一页从游标继续。
    let first = snap
        .deliveries(
            &DeliveryQuery::default(),
            &Page {
                limit: 1,
                cursor: None,
                order: Order::Asc,
            },
        )
        .await
        .unwrap();
    assert_eq!(first.items.len(), 1);
    let cursor = first.next_cursor.expect("second page exists");
    let second = snap
        .deliveries(
            &DeliveryQuery::default(),
            &Page {
                limit: 1,
                cursor: Some(cursor),
                order: Order::Asc,
            },
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_ne!(first.items[0].id, second.items[0].id);

    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(
        EventType::WorktreeRemoved,
        WORKTREE,
        PROJECT,
        json!({"worktree_id": WORKTREE, "branch": "gantry/dlv-1"}),
    ))
    .await
    .unwrap();
    tx.append(stage(DELIVERY, PROJECT, "spec", "closed"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(
        snap.delivery(&DELIVERY.parse().unwrap())
            .unwrap()
            .worktree_id,
        None
    );
    let active = DeliveryQuery {
        active: true,
        ..DeliveryQuery::default()
    };
    assert_eq!(
        snap.deliveries(&active, &page).await.unwrap().items.len(),
        1
    );
}
