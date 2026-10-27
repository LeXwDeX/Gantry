#![cfg(feature = "testing")]

//! 内置 `inbox_items` 投影与收件箱读取（store.md §6.2、§7.2、§12）。

use std::sync::Arc;

use gantry_core::entities::{
    Actor, ActorKind, InboxItem, InboxKind, InboxPayload, InboxStatus, Severity,
};
use gantry_core::events::{
    EventType, InboxDismissedData, InboxRaisedData, InboxResolution, InboxResolvedData, NewEvent,
    Subject,
};
use gantry_core::ids::{ApprovalId, Id, InboxId, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_store::{
    InboxQuery, Order, Page, PageCursor, Store, StoreDeps, StoreError, UnprojectedEvent,
};

const NOW_MS: i64 = 1_700_000_000_000;

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW_MS));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(7)),
    ));
    Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

fn inbox_id(n: u128) -> InboxId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

fn approval_id(n: u128) -> ApprovalId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

fn project_id(n: u128) -> ProjectId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

fn item(n: u128, project: Option<ProjectId>, key: &str, severity: Severity) -> InboxItem {
    InboxItem {
        id: inbox_id(n),
        project_id: project,
        delivery_id: None,
        kind: InboxKind::ApprovalNeeded,
        severity,
        summary: "需要审批".into(),
        payload: InboxPayload::ApprovalNeeded {
            approval_id: approval_id(n),
        },
        status: InboxStatus::Open,
        dedup_key: key.into(),
        created_at: Timestamp::from_millis(NOW_MS),
        updated_at: Timestamp::from_millis(NOW_MS),
    }
}

fn inbox_event(
    ty: EventType,
    id: InboxId,
    project: Option<ProjectId>,
    data: impl serde::Serialize,
) -> NewEvent {
    NewEvent {
        ty,
        subject: Subject::parse(&id.to_string()).unwrap(),
        project_id: project,
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data: serde_json::to_value(data).unwrap(),
        dedup_key: None,
    }
}

fn raised(item: &InboxItem, supersedes: Vec<InboxId>) -> NewEvent {
    inbox_event(
        EventType::InboxRaised,
        item.id,
        item.project_id,
        InboxRaisedData {
            item: item.clone(),
            supersedes,
        },
    )
}

fn resolved(item: &InboxItem) -> NewEvent {
    inbox_event(
        EventType::InboxResolved,
        item.id,
        item.project_id,
        InboxResolvedData {
            inbox_id: item.id,
            resolution: InboxResolution::System,
            option_id: None,
            note: None,
            message_id: None,
            approval_id: None,
            reason: Some("test".into()),
        },
    )
}

fn dismissed(item: &InboxItem) -> NewEvent {
    inbox_event(
        EventType::InboxDismissed,
        item.id,
        item.project_id,
        InboxDismissedData {
            inbox_id: item.id,
            note: None,
        },
    )
}

async fn commit(store: &Store, events: Vec<NewEvent>) -> Result<(), StoreError> {
    let mut tx = store.begin_write().await?;
    for event in events {
        tx.append(event).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn status_of(store: &Store, id: &InboxId) -> Option<InboxStatus> {
    let mut snapshot = store.snapshot().await.unwrap();
    snapshot
        .inbox_item(id)
        .await
        .unwrap()
        .map(|item| item.status)
}

#[tokio::test]
async fn raised_events_are_projected_not_left_unprojected() {
    let store = store().await;
    let a = item(1, Some(project_id(100)), "approval:1", Severity::Warn);
    commit(&store, vec![raised(&a, vec![])]).await.unwrap();

    let mut snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.inbox_item(&a.id).await.unwrap(), Some(a.clone()));
    assert_eq!(
        snapshot
            .inbox_open_by_dedup_key("approval:1")
            .await
            .unwrap(),
        Some(a)
    );
    let unprojected: &[UnprojectedEvent] = snapshot.unprojected();
    assert!(unprojected.is_empty());
}

#[tokio::test]
async fn second_open_item_with_same_dedup_key_is_rejected() {
    let store = store().await;
    let a = item(1, None, "k", Severity::Info);
    let b = item(2, None, "k", Severity::Info);
    commit(&store, vec![raised(&a, vec![])]).await.unwrap();

    let error = commit(&store, vec![raised(&b, vec![])]).await.unwrap_err();
    assert!(
        matches!(error, StoreError::PreconditionFailed(_)),
        "{error}"
    );
    assert_eq!(status_of(&store, &b.id).await, None);

    // 终态之后同一 key 可再次 open。
    commit(&store, vec![resolved(&a), raised(&b, vec![])])
        .await
        .unwrap();
    let mut snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .inbox_open_by_dedup_key("k")
            .await
            .unwrap()
            .map(|item| item.id),
        Some(b.id)
    );
}

#[tokio::test]
async fn supersedes_resolves_listed_items_in_the_same_event() {
    let store = store().await;
    let old = item(1, None, "spec:1", Severity::Info);
    let new = item(2, None, "spec:2", Severity::Info);
    commit(&store, vec![raised(&old, vec![])]).await.unwrap();
    commit(&store, vec![raised(&new, vec![old.id])])
        .await
        .unwrap();
    assert_eq!(
        status_of(&store, &old.id).await,
        Some(InboxStatus::Resolved)
    );
    assert_eq!(status_of(&store, &new.id).await, Some(InboxStatus::Open));

    // 已终态的项不能再被 supersede；整个事务回滚。
    let newer = item(3, None, "spec:3", Severity::Info);
    let error = commit(&store, vec![raised(&newer, vec![old.id])])
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::PreconditionFailed(_)),
        "{error}"
    );
    assert_eq!(status_of(&store, &newer.id).await, None);
}

#[tokio::test]
async fn transitions_follow_the_inbox_state_machine() {
    let store = store().await;
    let a = item(1, None, "a", Severity::Info);
    let b = item(2, None, "b", Severity::Info);
    commit(&store, vec![raised(&a, vec![]), raised(&b, vec![])])
        .await
        .unwrap();
    commit(&store, vec![resolved(&a), dismissed(&b)])
        .await
        .unwrap();
    assert_eq!(status_of(&store, &a.id).await, Some(InboxStatus::Resolved));
    assert_eq!(status_of(&store, &b.id).await, Some(InboxStatus::Dismissed));

    for event in [resolved(&a), dismissed(&a), resolved(&b), dismissed(&b)] {
        let error = commit(&store, vec![event]).await.unwrap_err();
        assert!(
            matches!(error, StoreError::PreconditionFailed(_)),
            "{error}"
        );
    }

    // 不存在的项。
    let ghost = item(9, None, "ghost", Severity::Info);
    assert!(commit(&store, vec![resolved(&ghost)]).await.is_err());
}

#[tokio::test]
async fn envelope_project_id_must_match_item() {
    let store = store().await;
    let a = item(1, Some(project_id(100)), "a", Severity::Info);
    let mut event = raised(&a, vec![]);
    event.project_id = Some(project_id(101));
    assert!(matches!(
        commit(&store, vec![event]).await.unwrap_err(),
        StoreError::InvalidEvent(_)
    ));

    commit(&store, vec![raised(&a, vec![])]).await.unwrap();
    let mut event = resolved(&a);
    event.project_id = None;
    assert!(matches!(
        commit(&store, vec![event]).await.unwrap_err(),
        StoreError::InvalidEvent(_)
    ));
}

#[tokio::test]
async fn tx_reader_sees_uncommitted_writes_and_rollback_discards_them() {
    let store = store().await;
    let a = item(1, Some(project_id(100)), "a", Severity::Blocker);
    let mut tx = store.begin_write().await.unwrap();
    tx.append(raised(&a, vec![])).await.unwrap();
    {
        let mut reader = tx.read();
        assert_eq!(reader.inbox_item(&a.id).await.unwrap(), Some(a.clone()));
        assert!(reader.inbox_open_by_dedup_key("a").await.unwrap().is_some());
        assert_eq!(reader.inbox_summary().await.unwrap().len(), 1);
        let page = reader
            .inbox(&InboxQuery::default(), &Page::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
    }
    tx.rollback().await.unwrap();

    let mut snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.inbox_item(&a.id).await.unwrap(), None);
    assert!(snapshot.inbox_summary().await.unwrap().is_empty());
}

#[tokio::test]
async fn summary_counts_open_items_per_project_and_matches_list() {
    let store = store().await;
    let projects = [project_id(100), project_id(101), project_id(102)];
    let mut events = Vec::new();
    let mut items = Vec::new();
    let severities = [Severity::Info, Severity::Warn, Severity::Blocker];
    for n in 0..12u128 {
        let project = if n % 4 == 3 {
            None
        } else {
            Some(projects[(n % 3) as usize])
        };
        let item = item(
            n + 1,
            project,
            &format!("k{n}"),
            severities[(n % 3) as usize].clone(),
        );
        events.push(raised(&item, vec![]));
        items.push(item);
    }
    commit(&store, events).await.unwrap();
    // 关闭一部分：resolved 与 dismissed 都不计入。
    commit(&store, vec![resolved(&items[0]), dismissed(&items[5])])
        .await
        .unwrap();

    let mut snapshot = store.snapshot().await.unwrap();
    let summary = snapshot.inbox_summary().await.unwrap();
    let open: Vec<InboxItem> = snapshot
        .inbox(
            &InboxQuery::default(),
            &Page {
                limit: 200,
                ..Page::default()
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(open.len(), 10);
    assert_eq!(summary.iter().map(|row| row.open).sum::<u32>(), 10);
    assert_eq!(summary[0].project_id, None, "全局项排在最前");
    for row in &summary {
        let rows: Vec<&InboxItem> = open
            .iter()
            .filter(|item| item.project_id == row.project_id)
            .collect();
        assert!(row.open > 0);
        assert_eq!(row.open as usize, rows.len());
        let max = rows.iter().map(|item| item.severity.clone()).max().unwrap();
        assert_eq!(row.max_severity, max);
    }
    assert!(summary
        .windows(2)
        .all(|w| w[0].project_id < w[1].project_id));
}

#[tokio::test]
async fn inbox_paging_filters_and_cursor_rules() {
    let store = store().await;
    let p = project_id(100);
    let mut events = Vec::new();
    for n in 1..=7u128 {
        let project = if n % 2 == 0 { Some(p) } else { None };
        events.push(raised(
            &item(n, project, &format!("k{n}"), Severity::Info),
            vec![],
        ));
    }
    commit(&store, events).await.unwrap();
    commit(&store, vec![resolved(&item(7, None, "k7", Severity::Info))])
        .await
        .unwrap();
    let mut snapshot = store.snapshot().await.unwrap();

    // 升序分页：不重不漏，默认 status = open。
    let mut seen = Vec::new();
    let mut page = Page {
        limit: 2,
        ..Page::default()
    };
    loop {
        let result = snapshot.inbox(&InboxQuery::default(), &page).await.unwrap();
        seen.extend(result.items.iter().map(|item| item.id));
        match result.next_cursor {
            Some(cursor) => page.cursor = Some(cursor),
            None => break,
        }
    }
    assert_eq!(seen, (1..=6).map(inbox_id).collect::<Vec<_>>());

    // 降序。
    let desc = Page {
        limit: 4,
        order: Order::Desc,
        cursor: None,
    };
    let first = snapshot.inbox(&InboxQuery::default(), &desc).await.unwrap();
    assert_eq!(
        first.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [6, 5, 4, 3].map(inbox_id)
    );
    let cursor = first.next_cursor.clone().unwrap();
    let second = snapshot
        .inbox(
            &InboxQuery::default(),
            &Page {
                cursor: Some(cursor.clone()),
                ..desc.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        second.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [2, 1].map(inbox_id)
    );
    assert!(second.next_cursor.is_none());

    // 方向不符、资源不符、被篡改 → InvalidCursor。
    for bad in [
        Page {
            cursor: Some(cursor),
            ..Page::default()
        },
        Page {
            cursor: Some(PageCursor::new(format!("delivery:asc:{}", inbox_id(1)))),
            ..Page::default()
        },
        Page {
            cursor: Some(PageCursor::new("inbox:asc:garbage")),
            ..Page::default()
        },
    ] {
        assert!(matches!(
            snapshot.inbox(&InboxQuery::default(), &bad).await,
            Err(StoreError::InvalidCursor)
        ));
    }
    for limit in [0, 201] {
        assert!(snapshot
            .inbox(
                &InboxQuery::default(),
                &Page {
                    limit,
                    ..Page::default()
                }
            )
            .await
            .is_err());
    }

    // 过滤：project_id、status、kind。
    let by_project = snapshot
        .inbox(
            &InboxQuery {
                project_id: Some(p),
                ..InboxQuery::default()
            },
            &Page::default(),
        )
        .await
        .unwrap();
    assert_eq!(by_project.items.len(), 3);
    let resolved_only = snapshot
        .inbox(
            &InboxQuery {
                status: Some(InboxStatus::Resolved),
                ..InboxQuery::default()
            },
            &Page::default(),
        )
        .await
        .unwrap();
    assert_eq!(resolved_only.items.len(), 1);
    let other_kind = snapshot
        .inbox(
            &InboxQuery {
                kind: Some(InboxKind::Escalation),
                ..InboxQuery::default()
            },
            &Page::default(),
        )
        .await
        .unwrap();
    assert!(other_kind.items.is_empty());
}

#[tokio::test]
async fn failed_raise_leaves_no_partial_fold_in_the_transaction() {
    let store = store().await;
    let old = item(1, None, "spec:1", Severity::Info);
    let taken = item(2, None, "taken", Severity::Info);
    commit(&store, vec![raised(&old, vec![]), raised(&taken, vec![])])
        .await
        .unwrap();

    // supersedes 合法，但新项的 dedup_key 与另一 open 项冲突：整次折叠都不生效。
    let clash = item(3, None, "taken", Severity::Info);
    let mut tx = store.begin_write().await.unwrap();
    let head = tx.read().head_seq();
    assert!(tx.append(raised(&clash, vec![old.id])).await.is_err());
    {
        let mut reader = tx.read();
        assert_eq!(reader.head_seq(), head);
        assert_eq!(
            reader.inbox_item(&old.id).await.unwrap().map(|i| i.status),
            Some(InboxStatus::Open)
        );
        assert_eq!(reader.inbox_item(&clash.id).await.unwrap(), None);
    }
    // 同一事务里后续写入照常可用。
    tx.append(resolved(&old)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        status_of(&store, &old.id).await,
        Some(InboxStatus::Resolved)
    );
}

#[tokio::test]
async fn superseding_an_open_item_with_the_same_dedup_key_is_allowed() {
    let store = store().await;
    let old = item(1, None, "same", Severity::Info);
    let new = item(2, None, "same", Severity::Warn);
    commit(&store, vec![raised(&old, vec![])]).await.unwrap();
    commit(&store, vec![raised(&new, vec![old.id])])
        .await
        .unwrap();
    let mut snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .inbox_open_by_dedup_key("same")
            .await
            .unwrap()
            .map(|i| i.id),
        Some(new.id)
    );
}
