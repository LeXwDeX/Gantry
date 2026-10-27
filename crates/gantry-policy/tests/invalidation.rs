//! policy.md §12.2「状态机与失效」的 M1 条目：事件驱动的失效、游标持久化、
//! 过期清扫，以及转移表的合法 / 非法转移。

mod support;

use std::time::Duration;

use gantry_core::canon::SubjectDigest;
use gantry_core::entities::{
    Actor, ActorKind, Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind,
};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::forge::ForgeEventKind;
use gantry_core::ids::{Dlv, ProjectId};
use gantry_core::machines::approval as machine;
use gantry_core::ports::{ApprovalSpec, PolicyPort};
use gantry_policy::invalidation::CURSOR_CONSUMER;
use serde_json::{json, Value};
use support::*;

const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");

/// core 夹具里的合法 forge 信封，改写成本测试的仓库、PR 与 head。
fn forge_envelope(kind: &str, number: u64, head: Option<&str>, origin: &str) -> Value {
    let cases: Value = serde_json::from_str(CASES).expect("cases.json");
    let mut envelope = cases["valid"][format!("forge.{kind}")].clone();
    envelope["repo"] = json!("acme/widgets");
    envelope["project_id"] = json!(project_id().to_string());
    envelope["target"]["number"] = json!(number);
    envelope["origin"] = json!(origin);
    if origin == "self" {
        envelope["caused_by_action"] = json!(id::<gantry_core::ids::Act>(1).to_string());
    }
    if let Some(head) = head {
        envelope["data"]["head_sha"] = json!(oid(head).to_string());
    }
    envelope
}

async fn append(
    h: &Harness,
    ty: EventType,
    subject: Subject,
    project: Option<ProjectId>,
    data: Value,
) {
    let mut tx = h.store.begin_write().await.expect("begin");
    tx.append(NewEvent {
        ty,
        subject,
        project_id: project,
        actor: Actor::new(ActorKind::Forge, "forge"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    })
    .await
    .expect("append");
    tx.commit().await.expect("commit");
}

async fn pr_event(
    h: &Harness,
    kind: ForgeEventKind,
    name: &str,
    number: u64,
    head: Option<&str>,
    origin: &str,
) {
    let subject = Subject::parse(&project_id().to_string()).expect("subject");
    append(
        h,
        EventType::Forge(kind),
        subject,
        Some(project_id()),
        forge_envelope(name, number, head, origin),
    )
    .await;
}

fn merge_spec(number: u64, head: &str, delivery: Option<u128>) -> ApprovalSpec {
    ApprovalSpec {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        target: ApprovalTarget::Pr {
            repo: slug(),
            number,
        },
        subject_digest: SubjectDigest::git(oid(head)),
        delivery_id: delivery.map(id::<Dlv>),
        ttl: Duration::from_secs(3_600),
    }
}

async fn approval(h: &Harness, spec: ApprovalSpec) -> Approval {
    h.engine
        .request_approval(spec, ctx())
        .await
        .expect("request")
}

async fn status(h: &Harness, approval: &Approval) -> ApprovalStatus {
    h.engine
        .approval(&approval.id)
        .await
        .expect("read")
        .expect("exists")
        .status
}

#[tokio::test]
async fn pr_synchronized_invalidates_old_head_merge_approvals_only() {
    let h = Harness::new();
    let pending = approval(&h, merge_spec(7, "head", None)).await;
    let granted = approval(&h, merge_spec(7, "head", Some(3))).await;
    h.grant(&granted).await;
    let other_pr = approval(&h, merge_spec(8, "head", None)).await;

    // origin = self 的推送同样使审批失效（forge-actions §4.5）。
    pr_event(
        &h,
        ForgeEventKind::PrSynchronized,
        "pr_synchronized",
        7,
        Some("new"),
        "self",
    )
    .await;
    let report = h.engine.process_events(200).await.expect("process");
    assert_eq!(report.invalidated, 2);
    assert_eq!(status(&h, &pending).await, ApprovalStatus::Invalidated);
    assert_eq!(status(&h, &granted).await, ApprovalStatus::Invalidated);
    assert_eq!(status(&h, &other_pr).await, ApprovalStatus::Pending);

    let events = h.events(EventType::ApprovalInvalidated).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data["reason"], "head_changed");
    assert_eq!(events[0].data["previous_head_sha"], oid("head").to_string());
    assert_eq!(events[0].data["current_head_sha"], oid("new").to_string());
}

#[tokio::test]
async fn replay_and_restart_have_no_side_effects() {
    let h = Harness::new();
    let pending = approval(&h, merge_spec(7, "head", None)).await;
    pr_event(
        &h,
        ForgeEventKind::PrSynchronized,
        "pr_synchronized",
        7,
        Some("new"),
        "external",
    )
    .await;
    let first = h.engine.process_events(200).await.expect("process");
    assert_eq!(first.invalidated, 1);
    let cursor = h
        .store
        .cursors()
        .load(CURSOR_CONSUMER)
        .await
        .expect("cursor");
    assert_eq!(cursor, first.cursor);

    // "重启"：同一 Store 上的新处理不重复处理，也不产生新事件。
    let again = h.engine.process_events(200).await.expect("process again");
    assert_eq!(again.events, 0);
    assert_eq!(again.invalidated, 0);

    assert_eq!(status(&h, &pending).await, ApprovalStatus::Invalidated);
    assert_eq!(h.events(EventType::ApprovalInvalidated).await.len(), 1);
}

#[tokio::test]
async fn head_unchanged_sync_keeps_the_approval() {
    let h = Harness::new();
    let pending = approval(&h, merge_spec(7, "head", None)).await;
    pr_event(
        &h,
        ForgeEventKind::PrSynchronized,
        "pr_synchronized",
        7,
        Some("head"),
        "external",
    )
    .await;
    let report = h.engine.process_events(200).await.expect("process");
    assert_eq!(report.invalidated, 0);
    assert_eq!(status(&h, &pending).await, ApprovalStatus::Pending);
}

#[tokio::test]
async fn pr_closed_merged_and_delivery_abandoned() {
    let h = Harness::new();
    let closed = approval(&h, merge_spec(7, "head", None)).await;
    let merged_pending = approval(&h, merge_spec(8, "head", None)).await;
    let merged_granted = approval(&h, merge_spec(8, "head2", None)).await;
    h.grant(&merged_granted).await;
    let abandoned = approval(&h, merge_spec(9, "head", Some(5))).await;

    pr_event(
        &h,
        ForgeEventKind::PrClosed,
        "pr_closed",
        7,
        None,
        "external",
    )
    .await;
    pr_event(
        &h,
        ForgeEventKind::PrMerged,
        "pr_merged",
        8,
        None,
        "external",
    )
    .await;
    let delivery = id::<Dlv>(5);
    let cases: Value = serde_json::from_str(CASES).expect("cases.json");
    append(
        &h,
        EventType::DeliveryCreated,
        Subject::parse(&delivery.to_string()).expect("subject"),
        Some(project_id()),
        cases["valid"]["delivery.created"].clone(),
    )
    .await;
    append(
        &h,
        EventType::DeliveryAbandoned,
        Subject::parse(&delivery.to_string()).expect("subject"),
        Some(project_id()),
        json!({ "reason": "不做了", "by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM" }),
    )
    .await;
    h.engine.process_events(200).await.expect("process");

    assert_eq!(status(&h, &closed).await, ApprovalStatus::Invalidated);
    assert_eq!(
        status(&h, &merged_pending).await,
        ApprovalStatus::Invalidated
    );
    // granted 的由消费路径处理（forge 执行点的 head 复核）。
    assert_eq!(status(&h, &merged_granted).await, ApprovalStatus::Granted);
    assert_eq!(status(&h, &abandoned).await, ApprovalStatus::Invalidated);
    let reasons: Vec<String> = h
        .events(EventType::ApprovalInvalidated)
        .await
        .iter()
        .map(|event| event.data["reason"].as_str().expect("reason").to_owned())
        .collect();
    assert_eq!(
        reasons,
        vec!["pr_closed", "pr_merged", "delivery_abandoned"]
    );
}

#[tokio::test]
async fn sweeper_expires_each_due_approval_once() {
    let h = Harness::new();
    let a = approval(&h, merge_spec(7, "a", None)).await;
    let b = approval(&h, merge_spec(7, "b", None)).await;
    h.grant(&b).await;
    let fresh_spec = ApprovalSpec {
        ttl: Duration::from_secs(86_400),
        ..merge_spec(7, "c", None)
    };
    let fresh = approval(&h, fresh_spec).await;

    h.advance(Duration::from_secs(3_600));
    assert_eq!(h.engine.sweep_expired().await.expect("sweep"), 2);
    assert_eq!(h.engine.sweep_expired().await.expect("sweep"), 0);
    assert_eq!(h.events(EventType::ApprovalExpired).await.len(), 2);
    assert_eq!(status(&h, &a).await, ApprovalStatus::Expired);
    assert_eq!(status(&h, &b).await, ApprovalStatus::Expired);
    assert_eq!(status(&h, &fresh).await, ApprovalStatus::Pending);
}

/// 七条合法转移各一，其余组合全部被转移表拒绝（data-model §4.4）。
#[test]
fn transition_table_is_exactly_the_seven_legal_edges() {
    use ApprovalStatus::*;
    let all = [Pending, Granted, Consumed, Invalidated, Expired, Denied];
    let legal = [
        (Pending, Granted),
        (Pending, Denied),
        (Pending, Invalidated),
        (Pending, Expired),
        (Granted, Consumed),
        (Granted, Invalidated),
        (Granted, Expired),
    ];
    for from in all {
        for to in all {
            assert_eq!(
                machine::check(from, to).is_ok(),
                legal.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
}

/// 终态审批不能再被转移：对 consumed / denied 的审批再作废会失败，事件不增加。
#[tokio::test]
async fn terminal_approvals_reject_further_transitions() {
    let h = Harness::new();
    let denied = approval(&h, merge_spec(7, "head", None)).await;
    h.deny(&denied).await;
    pr_event(
        &h,
        ForgeEventKind::PrClosed,
        "pr_closed",
        7,
        None,
        "external",
    )
    .await;
    let report = h.engine.process_events(200).await.expect("process");
    assert_eq!(report.invalidated, 0);
    assert_eq!(status(&h, &denied).await, ApprovalStatus::Denied);
    assert!(h.events(EventType::ApprovalInvalidated).await.is_empty());
}
