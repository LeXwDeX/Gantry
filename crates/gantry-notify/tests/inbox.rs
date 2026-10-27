//! `Notify` 专属用例：notify.md §12 的 M1 条目中一致性用例覆盖不到的部分。

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use gantry_core::entities::{
    Actor, ActorKind, DecisionKind, InboxKind, InboxOption, InboxPayload, InboxStatus,
};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{Id, MessageId, ProjectId};
use gantry_core::ports::{
    ApnsEnvironment, ChooseError, ClientProfile, InboxPort, InboxQuery, OptionId, PageQuery,
    PushToken, RaiseError, RaiseOutcome, RaiseRequest, Resolution, ResolveTarget,
};
use tokio::task::JoinSet;

fn created(outcome: RaiseOutcome) -> gantry_core::ports::Written<gantry_core::entities::InboxItem> {
    match outcome {
        RaiseOutcome::Created(written) => written,
        RaiseOutcome::Deduped(_) => panic!("expected Created"),
    }
}

#[tokio::test]
async fn fifty_concurrent_raises_create_exactly_one_item() {
    let h = harness();
    let notify = Arc::new(h.notify);
    let mut tasks = JoinSet::new();
    for _ in 0..50 {
        let notify = notify.clone();
        tasks.spawn(async move { notify.raise(None, gate_reject("same"), &ctx()).await });
    }
    let mut created_ids = Vec::new();
    let mut deduped_ids = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result.unwrap().unwrap() {
            RaiseOutcome::Created(w) => created_ids.push(w.item.id),
            RaiseOutcome::Deduped(w) => deduped_ids.push(w.item.id),
        }
    }
    assert_eq!(created_ids.len(), 1);
    assert_eq!(deduped_ids.len(), 49);
    assert!(deduped_ids.iter().all(|id| *id == created_ids[0]));
    assert_eq!(events_of(&h.store, EventType::InboxRaised).await.len(), 1);
}

#[tokio::test]
async fn all_six_kinds_accept_valid_payloads() {
    let h = harness();
    for req in one_of_each("ok") {
        let kind = req.kind.clone();
        let written = created(h.notify.raise(None, req, &ctx()).await.unwrap());
        assert_eq!(written.item.kind, kind);
    }
    assert_eq!(h.notify.summary().await.unwrap().total_open, 6);
}

#[tokio::test]
async fn invalid_payloads_are_rejected_and_not_stored() {
    let h = harness();
    let mut cases: Vec<(&str, RaiseRequest)> = Vec::new();

    // kind 与 payload 不一致：六个 kind 各一例。
    let kinds = [
        InboxKind::ApprovalNeeded,
        InboxKind::DecisionNeeded,
        InboxKind::SpecNeeded,
        InboxKind::BudgetExhausted,
        InboxKind::Escalation,
        InboxKind::SecurityFinding,
    ];
    let payloads: Vec<InboxPayload> = one_of_each("x").into_iter().map(|r| r.payload).collect();
    for (i, kind) in kinds.iter().enumerate() {
        let mut req = one_of_each("bad").remove(i);
        req.kind = kind.clone();
        req.payload = payloads[(i + 1) % payloads.len()].clone();
        cases.push(("kind mismatch", req));
    }

    let mut empty_key = gate_reject("");
    empty_key.dedup_key = gantry_core::ports::DedupKey::new("");
    cases.push(("empty dedup_key", empty_key));
    cases.push((
        "decision option outside vocabulary",
        decision(DecisionKind::GateReject, &["replan", "upgrade"], "d1"),
    ));
    cases.push((
        "decision without options",
        decision(DecisionKind::ManualAction, &[], "d2"),
    ));
    cases.push((
        "duplicate option id",
        decision(DecisionKind::ReviewDispute, &["fix", "fix"], "d3"),
    ));
    let mut blank_label = decision(DecisionKind::Other, &["continue"], "d4");
    if let InboxPayload::DecisionNeeded { options, .. } = &mut blank_label.payload {
        options[0] = InboxOption::new("continue", "  ", true);
    }
    cases.push(("blank label", blank_label));
    cases.push((
        "escalation options on a reason_code without options",
        escalation("attempt_failed", Some(&["abandon"]), "e1"),
    ));
    cases.push((
        "escalation option outside vocabulary",
        escalation("level_cap_exceeded", Some(&["upgrade", "replan"]), "e2"),
    ));
    cases.push(("budget without options", budget(&[], "b1")));
    cases.push((
        "budget option outside vocabulary",
        budget(&["continue"], "b2"),
    ));

    for (what, req) in cases {
        let result = h.notify.raise(None, req, &ctx()).await;
        assert!(
            matches!(result, Err(RaiseError::InvalidPayload { .. })),
            "{what}: {result:?}"
        );
    }
    assert!(inbox_events(&h.store).await.is_empty());
    assert_eq!(h.notify.summary().await.unwrap().total_open, 0);

    // escalation 的其他 reason_code 不带选项是合法的。
    h.notify
        .raise(None, escalation("attempt_failed", None, "e3"), &ctx())
        .await
        .unwrap();
}

/// 故障注入：上报并入调用方事务后，调用方的下一次追加被投影拒绝 → 调用方放弃
/// 事务，上报与事件一起消失，不出现只有一半的状态。
#[tokio::test]
async fn caller_tx_failure_rolls_back_the_raise() {
    let h = harness();
    let mut tx = h.store.begin_write().await.unwrap();
    let written = created(
        h.notify
            .raise(Some(&mut tx), gate_reject("k"), &ctx())
            .await
            .unwrap(),
    );
    // 对一个不存在的上报项追加 inbox.dismissed：投影拒绝。
    let ghost: gantry_core::ids::InboxId = Id::from_parts(NOW_MS as u64, 0xdead).unwrap();
    let bad = NewEvent {
        ty: EventType::InboxDismissed,
        subject: Subject::parse(&ghost.to_string()).unwrap(),
        project_id: None,
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data: serde_json::json!({ "inbox_id": ghost.to_string() }),
        dedup_key: None,
    };
    assert!(tx.append(bad).await.is_err());
    drop(tx);

    assert!(h.notify.get(&written.item.id).await.is_err());
    assert!(inbox_events(&h.store).await.is_empty());
    assert_eq!(
        h.store.events().head().await.unwrap(),
        gantry_core::entities::Seq::ZERO
    );

    // 并入的事务提交后才可见；dedup 命中同事务的项时 last_seq 取事务 head 上界。
    let mut tx = h.store.begin_write().await.unwrap();
    let first = created(
        h.notify
            .raise(Some(&mut tx), gate_reject("k"), &ctx())
            .await
            .unwrap(),
    );
    let RaiseOutcome::Deduped(again) = h
        .notify
        .raise(Some(&mut tx), gate_reject("k"), &ctx())
        .await
        .unwrap()
    else {
        panic!("expected Deduped");
    };
    assert_eq!(again.item.id, first.item.id);
    assert!(again.last_seq >= first.last_seq);
    tx.commit().await.unwrap();
    assert_eq!(
        h.notify.get(&first.item.id).await.unwrap().last_seq,
        first.last_seq
    );
}

#[tokio::test]
async fn two_devices_choosing_concurrently_yield_one_resolution() {
    let h = harness();
    let notify = Arc::new(h.notify);
    let item = created(notify.raise(None, gate_reject("k"), &ctx()).await.unwrap());
    let mut tasks = JoinSet::new();
    for (n, option) in [(7u128, "replan"), (8, "abandon")] {
        let notify = notify.clone();
        tasks.spawn(async move {
            notify
                .choose(
                    &item.item.id,
                    &OptionId::new(option),
                    None,
                    &device_ctx(ClientProfile::Desktop, n),
                )
                .await
        });
    }
    let mut ok = 0;
    let mut not_open = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(_) => ok += 1,
            Err(ChooseError::NotOpen) => not_open += 1,
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((ok, not_open), (1, 1));
    let resolved = events_of(&h.store, EventType::InboxResolved).await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].actor.kind, ActorKind::Device);
}

#[tokio::test]
async fn mobile_profile_rules_for_budget_and_decisions() {
    let h = harness();
    let mobile = device_ctx(ClientProfile::Mobile, 8);
    let b = created(
        h.notify
            .raise(None, budget(&["raise_cap", "abandon"], "b"), &ctx())
            .await
            .unwrap(),
    );
    assert!(matches!(
        h.notify
            .choose(&b.item.id, &OptionId::new("raise_cap"), None, &mobile)
            .await,
        Err(ChooseError::ProfileForbidden)
    ));
    h.notify
        .choose(&b.item.id, &OptionId::new("abandon"), None, &mobile)
        .await
        .unwrap();
}

#[tokio::test]
async fn dismiss_only_appends_inbox_dismissed() {
    let h = harness();
    let item = created(
        h.notify
            .raise(None, gate_reject("k"), &ctx())
            .await
            .unwrap(),
    );
    let head = h.store.events().head().await.unwrap();
    let written = h
        .notify
        .dismiss(
            &item.item.id,
            &device_ctx(ClientProfile::Desktop, 9),
            gantry_core::ports::Note::new("看到了"),
        )
        .await
        .unwrap();
    let after: Vec<_> = inbox_events(&h.store)
        .await
        .into_iter()
        .filter(|event| event.seq > head)
        .collect();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].ty, EventType::InboxDismissed);
    assert_eq!(after[0].data["note"], "看到了");
    assert_eq!(h.store.events().head().await.unwrap(), written.last_seq);
}

#[tokio::test]
async fn resolve_answered_records_the_message() {
    let h = harness();
    let item = created(
        h.notify
            .raise(None, spec_needed("spec"), &ctx())
            .await
            .unwrap(),
    );
    let message: MessageId = Id::from_parts(NOW_MS as u64, 77).unwrap();
    let written = h
        .notify
        .resolve(
            None,
            ResolveTarget::Id(item.item.id),
            Resolution::Answered {
                message_id: message,
            },
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(written[0].item.status, InboxStatus::Resolved);
    let resolved = events_of(&h.store, EventType::InboxResolved).await;
    assert_eq!(resolved[0].data["resolution"], "answered");
    assert_eq!(resolved[0].data["message_id"], message.to_string());
    assert_eq!(
        resolved[0].correlation_id.as_deref(),
        Some(delivery(200).to_string().as_str())
    );
}

/// 属性测试（确定性伪随机）：客户端按 `inbox.*` 事件增量维护的计数，在任意
/// 操作序列之后都与 `summary()` 一致；每个事件信封的 `project_id` 等于该项的。
#[tokio::test]
async fn incremental_client_counts_track_summary() {
    let projects: [Option<ProjectId>; 3] = [Some(project(100)), Some(project(101)), None];
    for seed in 1..=8u64 {
        let h = harness();
        let mut rng = Lcg(seed);
        let mut known: Vec<gantry_core::ids::InboxId> = Vec::new();
        for step in 0..60 {
            match rng.next() % 4 {
                0 | 1 => {
                    let mut req = gate_reject(&format!("k{}", rng.next() % 12));
                    req.project_id = projects[(rng.next() % 3) as usize];
                    if let RaiseOutcome::Created(w) =
                        h.notify.raise(None, req, &ctx()).await.unwrap()
                    {
                        known.push(w.item.id);
                    }
                }
                2 if !known.is_empty() => {
                    let id = known[(rng.next() as usize) % known.len()];
                    let _ = h
                        .notify
                        .dismiss(&id, &device_ctx(ClientProfile::Desktop, 9), None)
                        .await;
                }
                3 if !known.is_empty() => {
                    let id = known[(rng.next() as usize) % known.len()];
                    let option = if step % 2 == 0 { "replan" } else { "abandon" };
                    let _ = h
                        .notify
                        .choose(
                            &id,
                            &OptionId::new(option),
                            None,
                            &device_ctx(ClientProfile::Desktop, 9),
                        )
                        .await;
                }
                _ => {}
            }
        }

        let mut client: BTreeMap<Option<ProjectId>, i64> = BTreeMap::new();
        for event in inbox_events(&h.store).await {
            let id: gantry_core::ids::InboxId = event.subject.as_str().parse().unwrap();
            let item = h.notify.get(&id).await.unwrap().item;
            assert_eq!(event.project_id, item.project_id, "seed {seed}");
            let delta = match event.ty {
                EventType::InboxRaised => 1,
                EventType::InboxResolved | EventType::InboxDismissed => -1,
                other => panic!("unexpected {other:?}"),
            };
            *client.entry(event.project_id).or_default() += delta;
        }
        client.retain(|_, open| *open != 0);
        let summary = h.notify.summary().await.unwrap();
        let server: BTreeMap<Option<ProjectId>, i64> = summary
            .by_project
            .iter()
            .map(|row| (row.project_id, i64::from(row.open)))
            .collect();
        assert_eq!(client, server, "seed {seed}");
        assert_eq!(
            client.values().sum::<i64>(),
            i64::from(summary.total_open),
            "seed {seed}"
        );
    }
}

#[tokio::test]
async fn list_is_stable_when_items_arrive_during_paging() {
    let h = harness();
    for n in 0..5 {
        h.notify
            .raise(None, gate_reject(&format!("k{n}")), &ctx())
            .await
            .unwrap();
    }
    let mut cursor = None;
    let mut seen = Vec::new();
    let mut inserted = 0;
    loop {
        let page = h
            .notify
            .list(InboxQuery {
                page: PageQuery { cursor, limit: 2 },
                ..InboxQuery::default()
            })
            .await
            .unwrap();
        seen.extend(page.items.iter().map(|i| i.id));
        // 每翻一页插入一项；ULID 递增，新项排在已翻过的位置之后。
        h.clock.advance_ms(1);
        h.notify
            .raise(None, gate_reject(&format!("late{inserted}")), &ctx())
            .await
            .unwrap();
        inserted += 1;
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "不重复");
    assert_eq!(seen, sorted, "按 ID 递增");
    let all = h
        .notify
        .list(InboxQuery {
            page: PageQuery {
                cursor: None,
                limit: 200,
            },
            ..InboxQuery::default()
        })
        .await
        .unwrap()
        .items;
    // 分页开始前已存在的 5 项一个不漏。
    assert!(all[..5].iter().all(|item| seen.contains(&item.id)));
}

#[tokio::test]
async fn push_tokens_are_not_accepted_before_m4() {
    let h = harness();
    assert!(h
        .notify
        .register_push_token(
            &device_ctx(ClientProfile::Mobile, 8),
            PushToken::new("token"),
            ApnsEnvironment::Sandbox,
        )
        .await
        .is_err());
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}
