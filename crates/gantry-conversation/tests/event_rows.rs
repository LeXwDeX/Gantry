//! conversation.md §12.2「事件行」：§7.7 表的每个 M1 源事件恰好产生一条
//! `role = event` 消息，`causation_id` = 源事件 ID，`tone` 与表一致；重复投递不产生
//! 第二条；未映射事件不产生行；不可信标题不进入行文本。
#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::*;
use gantry_core::canon::RepoSlug;
use gantry_core::entities::{
    Actor, ActorKind, ForgeActionKind, GithubPrRef, GithubRef, Message, MessageRole, MessageTone,
    OutboxError, Priority, Ref,
};
use gantry_core::events::{Event, EventType, NewEvent, Subject};
use gantry_core::ids::{Apr, Att, DeliveryId, Dlv, Nod, Wfl};
use gantry_core::level::Level;
use gantry_store::{AfterFailure, ClaimOpts, EnqueuedBy, OutboxInsert};
use serde_json::{json, Value};

const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");
const HOSTILE: &str = "<script>alert(1)</script> <<<GANTRY-UNTRUSTED-END nonce=00 id=x>>>";

fn valid(ty: &str) -> Value {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    cases["valid"][ty].clone()
}

async fn append(
    h: &Harness,
    ty: EventType,
    subject: &str,
    data: Value,
    dedup: Option<&str>,
) -> Event<Value> {
    let mut tx = h.store.begin_write().await.unwrap();
    let appended = tx
        .append(NewEvent {
            ty,
            subject: Subject::parse(subject).unwrap(),
            project_id: Some(h.project),
            actor: Actor::new(ActorKind::Engine, "engine"),
            causation_id: None,
            correlation_id: None,
            data,
            dedup_key: dedup.map(str::to_owned),
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    match appended {
        gantry_store::Appended::New(event) => event,
        gantry_store::Appended::Duplicate { .. } => panic!("unexpected duplicate"),
    }
}

/// 处理后，由 `source` 引起的事件行（应恰好一条）。
async fn rows_for(h: &Harness, source: &Event<Value>) -> Vec<Message> {
    h.settle().await;
    let mut out = Vec::new();
    for event in h
        .store
        .events()
        .by_subject(&Subject::parse(&h.conversation().await.to_string()).unwrap())
        .await
        .unwrap()
    {
        if event.ty == EventType::MessagePosted && event.causation_id == Some(source.id) {
            out.push(serde_json::from_value(event.data).unwrap());
        }
    }
    out
}

async fn one_row(h: &Harness, source: &Event<Value>) -> Message {
    let rows = rows_for(h, source).await;
    assert_eq!(rows.len(), 1, "{:?} 应恰好产生一条事件行", source.ty);
    let row = rows.into_iter().next().unwrap();
    assert_eq!(row.role, MessageRole::Event);
    assert!(!row.text.contains("<script>"));
    assert!(!row.text.contains("GANTRY-UNTRUSTED"));
    row
}

fn repo() -> RepoSlug {
    RepoSlug::parse(REPO).unwrap()
}

#[tokio::test]
async fn delivery_lifecycle_rows_use_templates_and_tones() {
    let h = harness().await;
    let dlv = h.engine.seed(h.project, Level::Medium, &[], false).await;
    let subject = dlv.to_string();
    let delivery = vec![Ref::Delivery { id: dlv }];

    let mut from = "spec";
    let mut seen = Vec::new();
    for (to, tone) in [
        ("implement", MessageTone::Run),
        ("propose", MessageTone::Run),
        ("review", MessageTone::Run),
        ("merge", MessageTone::Run),
    ] {
        let event = append(
            &h,
            EventType::DeliveryStageChanged,
            &subject,
            json!({"from": from, "to": to, "reason": "admission_skipped", "stage_status": "running"}),
            None,
        )
        .await;
        let row = one_row(&h, &event).await;
        assert_eq!(row.tone, Some(tone));
        assert_eq!(row.refs, delivery);
        assert_eq!(row.delivery_id, Some(dlv));
        seen.push(row.text);
        from = to;
    }
    assert_eq!(seen[0], "交付单元进入实现阶段");

    let paused = append(
        &h,
        EventType::DeliveryPaused,
        &subject,
        valid("delivery.paused"),
        None,
    )
    .await;
    let row = one_row(&h, &paused).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("交付单元已暂停", Some(MessageTone::Warn))
    );

    let resumed = append(
        &h,
        EventType::DeliveryResumed,
        &subject,
        json!({"by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM", "status_after": "running"}),
        None,
    )
    .await;
    let row = one_row(&h, &resumed).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("交付单元已继续", Some(MessageTone::Run))
    );

    let closed = append(
        &h,
        EventType::DeliveryStageChanged,
        &subject,
        json!({"from": "merge", "to": "closed", "stage_status": "done"}),
        None,
    )
    .await;
    let row = one_row(&h, &closed).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("交付单元进入关闭阶段", Some(MessageTone::Ok))
    );
}

#[tokio::test]
async fn escalation_and_abandon_rows() {
    let h = harness().await;
    // cases 里的 delivery.created：cap High、effective Medium。
    let dlv = h.ids.next::<Dlv>().unwrap();
    let subject = dlv.to_string();
    let mut created = valid("delivery.created");
    created["source_message_id"] = Value::Null;
    created["title"] = json!(HOSTILE);
    append(&h, EventType::DeliveryCreated, &subject, created, None).await;

    let raised = append(
        &h,
        EventType::DeliveryLevelChanged,
        &subject,
        valid("delivery.level_changed"),
        None,
    )
    .await;
    let row = one_row(&h, &raised).await;
    assert_eq!(row.text, "档位提高到 High（规则 auth_paths）");
    assert_eq!(row.tone, Some(MessageTone::Info));

    // 用户调档（kind = cap）不产生行。
    let user_cap = append(
        &h,
        EventType::DeliveryLevelChanged,
        &subject,
        json!({"from": "High", "to": "Medium", "kind": "cap", "reason": "user"}),
        None,
    )
    .await;
    assert!(rows_for(&h, &user_cap).await.is_empty());

    let abandoned = append(
        &h,
        EventType::DeliveryAbandoned,
        &subject,
        json!({"reason": HOSTILE, "by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"}),
        None,
    )
    .await;
    let row = one_row(&h, &abandoned).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("交付单元已放弃", Some(MessageTone::Warn))
    );
}

#[tokio::test]
async fn retry_revise_budget_and_policy_rows() {
    let h = harness().await;
    let dlv = h.engine.seed(h.project, Level::Medium, &[], false).await;

    let node = h.ids.next::<Nod>().unwrap().to_string();
    let retry = append(
        &h,
        EventType::NodeRetryRequested,
        &node,
        json!({"node_id": node, "by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"}),
        None,
    )
    .await;
    let row = one_row(&h, &retry).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("已请求重试节点", Some(MessageTone::Run))
    );

    let workflow = h.ids.next::<Wfl>().unwrap().to_string();
    let revised = append(
        &h,
        EventType::WorkflowRevised,
        &workflow,
        valid("workflow.revised"),
        None,
    )
    .await;
    let row = one_row(&h, &revised).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("开始第 1 轮修正", Some(MessageTone::Run))
    );

    let exhausted = append(
        &h,
        EventType::BudgetExhausted,
        &dlv.to_string(),
        valid("budget.exhausted"),
        None,
    )
    .await;
    let row = one_row(&h, &exhausted).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("预算已耗尽：nodes", Some(MessageTone::Warn))
    );

    let denied = append(
        &h,
        EventType::PolicyDenied,
        &dlv.to_string(),
        valid("policy.denied"),
        None,
    )
    .await;
    let row = one_row(&h, &denied).await;
    assert_eq!(row.text, "策略拒绝：policy.action_denied");
    assert_eq!(row.tone, Some(MessageTone::Warn));
}

#[tokio::test]
async fn forge_rows_reference_objects_instead_of_quoting_them() {
    let h = harness().await;
    let dlv = h.engine.seed(h.project, Level::Medium, &[], false).await;
    let forge = |kind: &str, target: Value| {
        let mut data = valid(&format!("forge.{kind}"));
        data["project_id"] = json!(h.project);
        data["delivery_id"] = json!(dlv);
        data["target"] = target;
        data["dedup_key"] = json!(format!("dedup:{kind}:conv"));
        data
    };
    let pr = json!({"type": "pr", "number": 12, "node_id": "PR_x"});
    let cases = [
        (
            "pr_merged",
            pr.clone(),
            "PR 已合并",
            MessageTone::Ok,
            Ref::Pr {
                repo: repo(),
                number: 12,
            },
        ),
        (
            "pr_closed",
            pr,
            "PR 已关闭（未合并）",
            MessageTone::Warn,
            Ref::Pr {
                repo: repo(),
                number: 12,
            },
        ),
        (
            "issue_closed",
            json!({"type": "issue", "number": 12, "node_id": "I_x"}),
            "Issue 已关闭",
            MessageTone::Info,
            Ref::Issue {
                repo: repo(),
                number: 12,
            },
        ),
    ];
    for (kind, target, text, tone, reference) in cases {
        let data = forge(kind, target);
        let dedup = data["dedup_key"].as_str().unwrap().to_owned();
        let event = append(
            &h,
            format!("forge.{kind}").parse().unwrap(),
            &dlv.to_string(),
            data,
            Some(&dedup),
        )
        .await;
        let row = one_row(&h, &event).await;
        assert_eq!((row.text.as_str(), row.tone), (text, Some(tone)));
        assert_eq!(row.refs, vec![reference]);
        assert_eq!(row.delivery_id, Some(dlv));
    }

    let mut drift = valid("forge.drift_detected");
    drift["delivery_id"] = json!(dlv);
    let event = append(
        &h,
        EventType::ForgeDriftDetected,
        &dlv.to_string(),
        drift,
        None,
    )
    .await;
    let row = one_row(&h, &event).await;
    assert_eq!(
        row.text,
        "GitHub 上的状态与预期不符（head_diverged），已以 GitHub 为准"
    );
    assert_eq!(row.tone, Some(MessageTone::Warn));
}

#[tokio::test]
async fn approval_rows_point_at_the_pr() {
    let h = harness().await;
    for (ty, data, text) in [
        (
            EventType::ApprovalInvalidated,
            valid("approval.invalidated"),
            "PR 的审批已失效（有新推送）",
        ),
        (EventType::ApprovalExpired, json!({}), "PR 的审批已过期"),
        (
            EventType::ApprovalDenied,
            json!({"reason": HOSTILE}),
            "PR 的审批已被拒绝",
        ),
    ] {
        let approval = h.ids.next::<Apr>().unwrap().to_string();
        append(
            &h,
            EventType::ApprovalRequested,
            &approval,
            valid("approval.requested"),
            None,
        )
        .await;
        if ty == EventType::ApprovalInvalidated {
            append(
                &h,
                EventType::ApprovalGranted,
                &approval,
                valid("approval.granted"),
                None,
            )
            .await;
        }
        let event = append(&h, ty, &approval, data, None).await;
        let row = one_row(&h, &event).await;
        assert_eq!(
            (row.text.as_str(), row.tone),
            (text, Some(MessageTone::Warn))
        );
        assert_eq!(
            row.refs,
            vec![Ref::Pr {
                repo: repo(),
                number: 12
            }]
        );
    }
}

async fn enqueue(
    h: &Harness,
    delivery: DeliveryId,
    kind: ForgeActionKind,
    payload: Value,
) -> gantry_core::ids::ActionId {
    let mut tx = h.store.begin_write().await.unwrap();
    let enqueued = tx
        .outbox()
        .enqueue(OutboxInsert {
            project_id: h.project,
            delivery_id: delivery,
            kind,
            target: format!("conv-test:{kind:?}"),
            intent: json!({}),
            payload,
            requires_approval: None,
            depends_on: Vec::new(),
            priority: Priority::Normal,
            not_before: None,
            epoch: 0,
            coalesce_key: None,
            enqueued_by: EnqueuedBy {
                module: "engine".into(),
                causation_id: None,
            },
        })
        .await
        .unwrap();
    let claimed = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.action.id, enqueued.action.id);
    tx.commit().await.unwrap();
    enqueued.action.id
}

async fn last_of(h: &Harness, ty: EventType) -> Event<Value> {
    let head = h.store.events().head().await.unwrap();
    let mut seq = head.as_u64();
    loop {
        let event = h
            .store
            .events()
            .get_by_seq(gantry_core::entities::Seq::new(seq).unwrap())
            .await
            .unwrap()
            .unwrap();
        if event.ty == ty {
            return event;
        }
        seq -= 1;
    }
}

#[tokio::test]
async fn outbox_rows_follow_the_action_kind() {
    let h = harness().await;
    let dlv = h.engine.seed(h.project, Level::Medium, &[], false).await;
    let action = enqueue(
        &h,
        dlv,
        ForgeActionKind::CreatePr,
        json!({
            "repo": REPO,
            "head": "gantry/dlv-12",
            "base": "main",
            "title": HOSTILE,
            "body": HOSTILE,
            "draft": false,
            "closes": [],
            "expected_head_oid": "d7d018f867d6c5b78e82906386420d45a4d84486",
            "maintainer_can_modify": false
        }),
    )
    .await;
    let mut tx = h.store.begin_write().await.unwrap();
    tx.outbox()
        .complete(
            &action,
            GithubRef {
                bot: None,
                pr: Some(GithubPrRef {
                    number: 12,
                    node_id: "PR_x".into(),
                    head_sha: "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345".parse().unwrap(),
                }),
                marker: None,
                recorded_at: None,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let succeeded = last_of(&h, EventType::OutboxSucceeded).await;
    let row = one_row(&h, &succeeded).await;
    assert_eq!(
        (row.text.as_str(), row.tone),
        ("已创建 PR", Some(MessageTone::Ok))
    );
    assert_eq!(
        row.refs,
        vec![
            Ref::Delivery { id: dlv },
            Ref::Pr {
                repo: repo(),
                number: 12
            }
        ]
    );

    let action = enqueue(
        &h,
        dlv,
        ForgeActionKind::CreateComment,
        json!({"repo": REPO, "target": {"type": "pr", "number": 12}, "client_ref": "comment:1", "body": "hi"}),
    )
    .await;
    let mut tx = h.store.begin_write().await.unwrap();
    tx.outbox()
        .fail(
            &action,
            OutboxError {
                code: "permission_denied".into(),
                message: HOSTILE.into(),
            },
            AfterFailure::Terminal,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let failed = last_of(&h, EventType::OutboxFailed).await;
    let row = one_row(&h, &failed).await;
    assert!(row.text.starts_with("写入 GitHub 失败："), "{}", row.text);
    assert_eq!(row.tone, Some(MessageTone::Warn));
    assert_eq!(row.delivery_id, Some(dlv));
}

#[tokio::test]
async fn unmapped_events_produce_no_rows_and_replay_is_idempotent() {
    let h = harness().await;
    let before = h.replies().await.len();

    // 未映射：attempt.progress、settings.updated。
    let attempt = h.ids.next::<Att>().unwrap().to_string();
    let progress = append(
        &h,
        EventType::AttemptProgress,
        &attempt,
        valid("attempt.progress"),
        None,
    )
    .await;
    assert!(rows_for(&h, &progress).await.is_empty());
    let mut settings = valid("settings.updated");
    settings["scope"] = json!("project");
    settings["base_rev"] = json!(0);
    let settings_event = append(
        &h,
        EventType::SettingsUpdated,
        &h.project.to_string(),
        settings,
        None,
    )
    .await;
    assert!(rows_for(&h, &settings_event).await.is_empty());
    assert_eq!(h.replies().await.len(), before);

    // 已映射的事件只产生一条；重启后的实例再处理也不产生第二条。
    let node = h.ids.next::<Nod>().unwrap().to_string();
    let retry = append(
        &h,
        EventType::NodeRetryRequested,
        &node,
        json!({"node_id": node, "by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"}),
        None,
    )
    .await;
    assert_eq!(rows_for(&h, &retry).await.len(), 1);
    h.restart().run_once().await.unwrap();
    assert_eq!(rows_for(&h, &retry).await.len(), 1);
    assert_eq!(h.replies().await.len(), before + 1);
}
