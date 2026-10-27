//! 自动关闭 R1、R2（notify.md §7.5、§12「自动关闭」）。

mod common;

use common::*;
use gantry_core::entities::{Actor, ActorKind, InboxStatus};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{ApprovalId, DeliveryId, ProjectId};
use gantry_core::ports::{InboxPort, RaiseOutcome};
use gantry_notify::{AutoClose, Notify};
use gantry_store::Store;
use serde_json::{json, Value};

const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");

fn valid(ty: &str) -> Value {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    cases["valid"][ty].clone()
}

fn event(ty: &str, subject: &str, project: Option<ProjectId>, data: Value) -> NewEvent {
    NewEvent {
        ty: ty.parse().unwrap(),
        subject: Subject::parse(subject).unwrap(),
        project_id: project,
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    }
}

async fn append(store: &Store, events: Vec<NewEvent>) {
    let mut tx = store.begin_write().await.unwrap();
    for event in events {
        tx.append(event).await.unwrap();
    }
    tx.commit().await.unwrap();
}

fn approval_event(ty: &str, id: ApprovalId) -> NewEvent {
    let mut data = valid(ty);
    if data.get("approval_id").is_some() {
        data["approval_id"] = json!(id.to_string());
    }
    event(ty, &id.to_string(), Some(project(100)), data)
}

fn delivery_created(id: DeliveryId) -> NewEvent {
    event(
        "delivery.created",
        &id.to_string(),
        Some(project(100)),
        valid("delivery.created"),
    )
}

async fn raise_id(
    notify: &Notify,
    req: gantry_core::ports::RaiseRequest,
) -> gantry_core::ids::InboxId {
    match notify.raise(None, req, &ctx()).await.unwrap() {
        RaiseOutcome::Created(w) => w.item.id,
        RaiseOutcome::Deduped(_) => panic!("expected Created"),
    }
}

async fn status(notify: &Notify, id: &gantry_core::ids::InboxId) -> InboxStatus {
    notify.get(id).await.unwrap().item.status
}

#[tokio::test]
async fn r1_each_approval_settlement_closes_only_its_item() {
    for (n, ty) in [
        "approval.granted",
        "approval.denied",
        "approval.invalidated",
        "approval.expired",
        "approval.consumed",
    ]
    .into_iter()
    .enumerate()
    {
        let h = harness();
        let n = n as u128 + 1;
        let mine = raise_id(&h.notify, approval_needed(n)).await;
        let other = raise_id(&h.notify, approval_needed(n + 50)).await;
        append(&h.store, vec![approval_event(ty, approval(n))]).await;

        let closed = AutoClose::new(h.notify.clone()).run_once().await.unwrap();
        assert_eq!(closed, 1, "{ty}");
        assert_eq!(
            status(&h.notify, &mine).await,
            InboxStatus::Resolved,
            "{ty}"
        );
        assert_eq!(status(&h.notify, &other).await, InboxStatus::Open, "{ty}");

        let resolved = events_of(&h.store, EventType::InboxResolved).await;
        assert_eq!(resolved.len(), 1, "{ty}");
        assert_eq!(resolved[0].data["resolution"], "approval_settled", "{ty}");
        assert_eq!(
            resolved[0].data["approval_id"],
            approval(n).to_string(),
            "{ty}"
        );
        assert_eq!(
            resolved[0].actor,
            Actor::new(ActorKind::System, "notify"),
            "{ty}"
        );
        assert!(resolved[0].causation_id.is_some(), "{ty}");
    }
}

#[tokio::test]
async fn r2_abandoned_or_closed_delivery_closes_all_its_open_items() {
    for (ty, to) in [
        ("delivery.abandoned", None),
        ("delivery.stage_changed", Some("closed")),
    ] {
        let h = harness();
        let mine = delivery(200);
        let other = delivery(201);
        append(
            &h.store,
            vec![delivery_created(mine), delivery_created(other)],
        )
        .await;
        let a = raise_id(&h.notify, gate_reject("a")).await;
        let b = raise_id(&h.notify, level_cap("b")).await;
        let mut elsewhere = gate_reject("c");
        elsewhere.delivery_id = Some(other);
        let c = raise_id(&h.notify, elsewhere).await;

        let data = match to {
            None => valid("delivery.abandoned"),
            Some(to) => json!({ "from": "spec", "to": to, "stage_status": "done" }),
        };
        append(
            &h.store,
            vec![event(ty, &mine.to_string(), Some(project(100)), data)],
        )
        .await;
        let closed = AutoClose::new(h.notify.clone()).run_once().await.unwrap();
        assert_eq!(closed, 2, "{ty}");
        assert_eq!(status(&h.notify, &a).await, InboxStatus::Resolved, "{ty}");
        assert_eq!(status(&h.notify, &b).await, InboxStatus::Resolved, "{ty}");
        assert_eq!(status(&h.notify, &c).await, InboxStatus::Open, "{ty}");
        let expected = if to.is_some() {
            "delivery_closed"
        } else {
            "delivery_abandoned"
        };
        for event in events_of(&h.store, EventType::InboxResolved).await {
            assert_eq!(event.data["resolution"], expected, "{ty}");
        }
    }
}

#[tokio::test]
async fn non_terminal_stage_changes_and_forge_events_close_nothing() {
    let h = harness();
    let d = delivery(200);
    append(&h.store, vec![delivery_created(d)]).await;
    let a = raise_id(&h.notify, gate_reject("a")).await;
    let mut merged = valid("forge.pr_merged");
    merged["delivery_id"] = json!(d.to_string());
    merged["origin"] = json!("self");
    merged["caused_by_action"] = json!("act_01JBA1F3Q9TX2N7B5D8H0M4RVC");
    append(
        &h.store,
        vec![
            event(
                "delivery.stage_changed",
                &d.to_string(),
                Some(project(100)),
                json!({ "from": "spec", "to": "plan", "stage_status": "running" }),
            ),
            event(
                "forge.pr_merged",
                &d.to_string(),
                Some(project(100)),
                merged,
            ),
        ],
    )
    .await;
    let head = h.store.events().head().await.unwrap();
    assert_eq!(
        AutoClose::new(h.notify.clone()).run_once().await.unwrap(),
        0
    );
    assert_eq!(status(&h.notify, &a).await, InboxStatus::Open);
    // 自动关闭只会追加 inbox.resolved；这里什么都没追加。
    assert_eq!(h.store.events().head().await.unwrap(), head);
}

#[tokio::test]
async fn cursor_makes_runs_incremental_and_replay_is_idempotent() {
    let h = harness();
    let first = raise_id(&h.notify, approval_needed(1)).await;
    append(
        &h.store,
        vec![approval_event("approval.expired", approval(1))],
    )
    .await;
    let closer = AutoClose::new(h.notify.clone());
    assert_eq!(closer.run_once().await.unwrap(), 1);
    assert_eq!(
        closer.run_once().await.unwrap(),
        0,
        "游标之前的事件不再处理"
    );

    // 新事件到达后增量处理。
    let second = raise_id(&h.notify, approval_needed(2)).await;
    append(
        &h.store,
        vec![approval_event("approval.denied", approval(2))],
    )
    .await;
    assert_eq!(closer.run_once().await.unwrap(), 1);
    assert_eq!(status(&h.notify, &first).await, InboxStatus::Resolved);
    assert_eq!(status(&h.notify, &second).await, InboxStatus::Resolved);

    // 换一个游标从头重放：结果相同，不产生新事件。
    let before = inbox_events(&h.store).await;
    let replay = AutoClose::with_consumer(h.notify.clone(), "notify.autoclose.replay");
    assert_eq!(replay.run_once().await.unwrap(), 0);
    assert_eq!(replay.run_once().await.unwrap(), 0);
    assert_eq!(inbox_events(&h.store).await, before);
    assert_eq!(closer.run_once().await.unwrap(), 0);
    assert_eq!(inbox_events(&h.store).await, before);
}
