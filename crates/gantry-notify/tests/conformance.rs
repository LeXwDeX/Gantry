//! `InboxPort` 一致性用例：同一组用例分别跑 `FakeInbox` 与 `Notify`
//! （notify.md §12："`FakeInbox` 与真实 `InboxPort` 实现共用一套一致性用例"）。
//!
//! 两个实现都挂在同一种内存 Store 上，事件日志就是判定依据。

mod common;

use std::sync::Arc;

use common::*;
use gantry_core::entities::InboxStatus;
use gantry_core::events::EventType;
use gantry_core::ports::{
    ChooseError, ClientProfile, DedupKey, InboxError, InboxPort, InboxQuery, OptionId, PageQuery,
    RaiseOutcome, Resolution, ResolveTarget,
};
use gantry_fakes::inbox::{dedup_conformance, FakeInbox};
use gantry_store::Store;

type Port = Arc<dyn InboxPort>;

/// (名称, Store, 端口)。
fn ports() -> Vec<(&'static str, Store, Port)> {
    let real = harness();
    let fake_store = harness().store;
    let fake = FakeInbox::new_in(fake_store.clone());
    vec![
        ("notify", real.store, Arc::new(real.notify)),
        ("fake", fake_store, Arc::new(fake)),
    ]
}

fn created(outcome: RaiseOutcome) -> gantry_core::ports::Written<gantry_core::entities::InboxItem> {
    match outcome {
        RaiseOutcome::Created(written) => written,
        RaiseOutcome::Deduped(_) => panic!("expected Created"),
    }
}

#[tokio::test]
async fn shared_dedup_conformance() {
    for (name, _, port) in ports() {
        dedup_conformance(port.as_ref(), gate_reject("conf:dedup"), &ctx())
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

#[tokio::test]
async fn raise_appends_one_event_and_written_seq_matches_it() {
    for (name, store, port) in ports() {
        let written = created(port.raise(None, gate_reject("k"), &ctx()).await.unwrap());
        let events = events_of(&store, EventType::InboxRaised).await;
        assert_eq!(events.len(), 1, "{name}");
        assert_eq!(written.last_seq, events[0].seq, "{name}");
        assert_eq!(events[0].project_id, written.item.project_id, "{name}");
        assert_eq!(written.item.status, InboxStatus::Open, "{name}");

        let deduped = port.raise(None, gate_reject("k"), &ctx()).await.unwrap();
        let RaiseOutcome::Deduped(deduped) = deduped else {
            panic!("{name}: expected Deduped");
        };
        assert_eq!(deduped.item.id, written.item.id, "{name}");
        assert_eq!(deduped.last_seq, written.last_seq, "{name}");
        assert_eq!(
            inbox_events(&store).await.len(),
            1,
            "{name}: dedup 不产生事件"
        );
    }
}

#[tokio::test]
async fn dismissed_then_raised_again_is_a_new_item() {
    for (name, _, port) in ports() {
        let first = created(port.raise(None, gate_reject("k"), &ctx()).await.unwrap());
        port.dismiss(&first.item.id, &device_ctx(ClientProfile::Desktop, 9), None)
            .await
            .unwrap();
        let second = created(port.raise(None, gate_reject("k"), &ctx()).await.unwrap());
        assert_ne!(second.item.id, first.item.id, "{name}");
    }
}

#[tokio::test]
async fn supersedes_resolves_the_old_item() {
    for (name, store, port) in ports() {
        let old = created(
            port.raise(None, spec_needed("spec:1"), &ctx())
                .await
                .unwrap(),
        );
        let mut req = spec_needed("spec:2");
        req.supersedes = vec![DedupKey::new("spec:1")];
        let new = created(port.raise(None, req, &ctx()).await.unwrap());
        assert_eq!(
            port.get(&old.item.id).await.unwrap().item.status,
            InboxStatus::Resolved,
            "{name}"
        );
        assert_eq!(new.item.status, InboxStatus::Open, "{name}");
        let raised = events_of(&store, EventType::InboxRaised).await;
        let data: gantry_core::events::InboxRaisedData =
            serde_json::from_value(raised[1].data.clone()).unwrap();
        assert_eq!(data.supersedes, vec![old.item.id], "{name}");
    }
}

#[tokio::test]
async fn raise_joins_caller_tx_and_rolls_back_with_it() {
    for (name, store, port) in ports() {
        let mut tx = store.begin_write().await.unwrap();
        let written = created(
            port.raise(Some(&mut tx), gate_reject("k"), &ctx())
                .await
                .unwrap(),
        );
        // 同一事务里再 raise 同一 key：看得到未提交的项。
        let again = port
            .raise(Some(&mut tx), gate_reject("k"), &ctx())
            .await
            .unwrap();
        assert!(
            matches!(again, RaiseOutcome::Deduped(ref w) if w.item.id == written.item.id),
            "{name}"
        );
        tx.rollback().await.unwrap();
        assert!(
            matches!(port.get(&written.item.id).await, Err(InboxError::NotFound)),
            "{name}"
        );
        assert!(inbox_events(&store).await.is_empty(), "{name}");
        assert_eq!(port.summary().await.unwrap().total_open, 0, "{name}");
    }
}

#[tokio::test]
async fn choose_rules_in_spec_order() {
    for (name, store, port) in ports() {
        let desktop = device_ctx(ClientProfile::Desktop, 9);
        let mobile = device_ctx(ClientProfile::Mobile, 8);
        let approval = created(port.raise(None, approval_needed(1), &ctx()).await.unwrap());
        let esc = created(port.raise(None, level_cap("esc"), &ctx()).await.unwrap());
        let decision = created(port.raise(None, gate_reject("dec"), &ctx()).await.unwrap());
        let spec = created(port.raise(None, spec_needed("spec"), &ctx()).await.unwrap());

        assert!(
            matches!(
                port.choose(&approval.item.id, &OptionId::new("replan"), None, &desktop)
                    .await,
                Err(ChooseError::UseApprovalRoute)
            ),
            "{name}"
        );
        assert!(
            matches!(
                port.choose(&esc.item.id, &OptionId::new("upgrade"), None, &mobile)
                    .await,
                Err(ChooseError::ProfileForbidden)
            ),
            "{name}"
        );
        assert!(
            matches!(
                port.choose(&decision.item.id, &OptionId::new("nope"), None, &desktop)
                    .await,
                Err(ChooseError::OptionUnknown)
            ),
            "{name}"
        );
        assert!(
            matches!(
                port.choose(&spec.item.id, &OptionId::new("replan"), None, &desktop)
                    .await,
                Err(ChooseError::OptionUnknown)
            ),
            "{name}: spec_needed 没有 options"
        );

        let note = gantry_core::ports::Note::new("按计划重来");
        let chosen = port
            .choose(&decision.item.id, &OptionId::new("replan"), note, &mobile)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(chosen.item.status, InboxStatus::Resolved, "{name}");
        let resolved = events_of(&store, EventType::InboxResolved).await;
        assert_eq!(resolved.len(), 1, "{name}");
        assert_eq!(resolved[0].seq, chosen.last_seq, "{name}");
        assert_eq!(resolved[0].data["resolution"], "chosen", "{name}");
        assert_eq!(resolved[0].data["option_id"], "replan", "{name}");
        assert_eq!(resolved[0].data["note"], "按计划重来", "{name}");

        assert!(
            matches!(
                port.choose(&decision.item.id, &OptionId::new("abandon"), None, &desktop)
                    .await,
                Err(ChooseError::NotOpen)
            ),
            "{name}"
        );
        port.choose(&esc.item.id, &OptionId::new("upgrade"), None, &desktop)
            .await
            .unwrap_or_else(|error| panic!("{name}: desktop escalation {error}"));
    }
}

#[tokio::test]
async fn dismiss_is_idempotent_and_rejects_resolved() {
    for (name, store, port) in ports() {
        let desktop = device_ctx(ClientProfile::Desktop, 9);
        let a = created(port.raise(None, gate_reject("a"), &ctx()).await.unwrap());
        let first = port.dismiss(&a.item.id, &desktop, None).await.unwrap();
        assert_eq!(first.item.status, InboxStatus::Dismissed, "{name}");
        let again = port.dismiss(&a.item.id, &desktop, None).await.unwrap();
        assert_eq!(again.last_seq, first.last_seq, "{name}");
        assert_eq!(
            events_of(&store, EventType::InboxDismissed).await.len(),
            1,
            "{name}"
        );

        let b = created(port.raise(None, gate_reject("b"), &ctx()).await.unwrap());
        port.resolve(None, ResolveTarget::Id(b.item.id), system(), &ctx())
            .await
            .unwrap();
        assert!(
            port.dismiss(&b.item.id, &desktop, None).await.is_err(),
            "{name}"
        );
        assert!(
            matches!(
                port.choose(&a.item.id, &OptionId::new("replan"), None, &desktop)
                    .await,
                Err(ChooseError::NotOpen)
            ),
            "{name}: dismissed 之后 choose"
        );
    }
}

#[tokio::test]
async fn resolve_is_idempotent_on_terminal_items() {
    for (name, store, port) in ports() {
        let a = created(port.raise(None, gate_reject("a"), &ctx()).await.unwrap());
        let first = port
            .resolve(None, ResolveTarget::Id(a.item.id), system(), &ctx())
            .await
            .unwrap();
        assert_eq!(first.len(), 1, "{name}");
        let second = port
            .resolve(None, ResolveTarget::Id(a.item.id), system(), &ctx())
            .await
            .unwrap();
        assert!(second.is_empty(), "{name}");
        assert_eq!(
            events_of(&store, EventType::InboxResolved).await.len(),
            1,
            "{name}"
        );
    }
}

#[tokio::test]
async fn resolve_targets_match_only_their_items() {
    for (name, _, port) in ports() {
        let a1 = created(port.raise(None, approval_needed(1), &ctx()).await.unwrap());
        let a2 = created(port.raise(None, approval_needed(2), &ctx()).await.unwrap());
        let mut other = gate_reject("other");
        other.delivery_id = Some(delivery(201));
        let other = created(port.raise(None, other, &ctx()).await.unwrap());
        let mine = created(port.raise(None, gate_reject("mine"), &ctx()).await.unwrap());

        let by_approval = port
            .resolve(None, ResolveTarget::Approval(approval(1)), system(), &ctx())
            .await
            .unwrap();
        assert_eq!(
            by_approval.iter().map(|w| w.item.id).collect::<Vec<_>>(),
            vec![a1.item.id],
            "{name}"
        );

        let by_delivery = port
            .resolve(
                None,
                ResolveTarget::Delivery {
                    id: delivery(200),
                    kinds: Some(vec![gantry_core::entities::InboxKind::DecisionNeeded]),
                },
                Resolution::DeliveryClosed,
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(
            by_delivery.iter().map(|w| w.item.id).collect::<Vec<_>>(),
            vec![mine.item.id],
            "{name}"
        );
        assert_eq!(
            port.get(&a2.item.id).await.unwrap().item.status,
            InboxStatus::Open,
            "{name}"
        );
        assert_eq!(
            port.get(&other.item.id).await.unwrap().item.status,
            InboxStatus::Open,
            "{name}"
        );

        let by_key = port
            .resolve(
                None,
                ResolveTarget::DedupKey(DedupKey::new("other")),
                system(),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(by_key.len(), 1, "{name}");
    }
}

#[tokio::test]
async fn summary_matches_open_list() {
    for (name, _, port) in ports() {
        for (i, mut req) in one_of_each("s").into_iter().enumerate() {
            req.project_id = match i % 3 {
                0 => Some(project(100)),
                1 => Some(project(101)),
                _ => None,
            };
            port.raise(None, req, &ctx()).await.unwrap();
        }
        let summary = port.summary().await.unwrap();
        let open = port
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
        assert_eq!(summary.total_open as usize, open.len(), "{name}");
        assert_eq!(summary.by_project.len(), 3, "{name}");
        for row in &summary.by_project {
            let rows: Vec<_> = open
                .iter()
                .filter(|i| i.project_id == row.project_id)
                .collect();
            assert_eq!(row.open as usize, rows.len(), "{name}");
            assert_eq!(
                Some(&row.max_severity),
                rows.iter().map(|i| &i.severity).max(),
                "{name}"
            );
        }
    }
}

#[tokio::test]
async fn list_pages_without_gaps_and_filters() {
    for (name, _, port) in ports() {
        for n in 0..7 {
            let mut req = gate_reject(&format!("k{n}"));
            if n % 2 == 0 {
                req.project_id = Some(project(101));
            }
            port.raise(None, req, &ctx()).await.unwrap();
        }
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let page = port
                .list(InboxQuery {
                    page: PageQuery { cursor, limit: 3 },
                    ..InboxQuery::default()
                })
                .await
                .unwrap();
            seen.extend(page.items.iter().map(|i| i.id));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen.len(), 7, "{name}");
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{name}");

        let filtered = port
            .list(InboxQuery {
                project_id: Some(project(101)),
                ..InboxQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(filtered.items.len(), 4, "{name}");
        let none = port
            .list(InboxQuery {
                delivery_id: Some(delivery(999)),
                ..InboxQuery::default()
            })
            .await
            .unwrap();
        assert!(none.items.is_empty(), "{name}");
        assert!(port
            .list(InboxQuery {
                page: PageQuery {
                    cursor: Some("garbage".into()),
                    limit: 3,
                },
                ..InboxQuery::default()
            })
            .await
            .is_err());
    }
}

fn system() -> Resolution {
    Resolution::System {
        reason: "conformance".into(),
    }
}
