#![cfg(feature = "testing")]

use std::sync::Arc;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Actor, ActorKind, Project};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{Id, IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{Order, Page, PageCursor, Store, StoreDeps, StoreError};
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

fn project(n: u128, repo: &str, updated: bool) -> Project {
    let id: ProjectId = Id::from_parts(NOW as u64, n).unwrap();
    serde_json::from_value(json!({
        "id": id, "repo": repo,
        "installation_id": if updated { 11 } else { 1 },
        "bot_user_id": if updated { 22 } else { 2 },
        "default_branch": if updated { "develop" } else { "main" },
        "allowed_merge_methods": if updated { vec!["merge", "rebase"] } else { vec!["squash"] },
        "merge_queue_enabled": updated,
        "created_at": "2026-10-24T12:00:00.000Z",
        "updated_at": if updated { "2026-10-25T12:00:00.000Z" } else { "2026-10-24T12:00:00.000Z" }
    }))
    .unwrap()
}

fn event(ty: EventType, row: &Project) -> NewEvent {
    NewEvent {
        ty,
        subject: Subject::parse(&row.id.to_string()).unwrap(),
        project_id: Some(row.id),
        actor: Actor::new(ActorKind::System, "api"),
        causation_id: None,
        correlation_id: None,
        data: serde_json::to_value(row).unwrap(),
        dedup_key: None,
    }
}

#[tokio::test]
async fn project_fold_snapshot_and_repo_ambiguity() {
    let store = store().await;
    let first = project(1, "acme/one", false);
    let updated = project(1, "acme/new", true);
    let other = project(2, "acme/new", false);
    let moved = project(2, "acme/two", true);

    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectAdded, &first))
        .await
        .unwrap();
    assert_eq!(
        tx.read().project(&first.id).await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        tx.read().project_by_repo(&first.repo).await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        tx.read().projects(&Page::default()).await.unwrap().items,
        vec![first.clone()]
    );
    tx.commit().await.unwrap();
    let mut old = store.snapshot().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectUpdated, &updated))
        .await
        .unwrap();
    assert_eq!(
        tx.read().project(&first.id).await.unwrap(),
        Some(updated.clone())
    );
    assert_eq!(tx.read().project_by_repo(&first.repo).await.unwrap(), None);
    assert_eq!(
        tx.read().project_by_repo(&updated.repo).await.unwrap(),
        Some(updated.clone())
    );
    tx.append(event(EventType::ProjectAdded, &other))
        .await
        .unwrap();
    assert_eq!(
        tx.read().project_by_repo(&updated.repo).await.unwrap(),
        None
    );
    tx.commit().await.unwrap();
    assert_eq!(old.project(&first.id).await.unwrap(), Some(first));
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(
        snap.project(&updated.id).await.unwrap(),
        Some(updated.clone())
    );
    assert_eq!(snap.project_by_repo(&updated.repo).await.unwrap(), None);

    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectUpdated, &moved))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(
        snap.project_by_repo(&updated.repo).await.unwrap(),
        Some(updated)
    );
    assert_eq!(
        snap.project_by_repo(&moved.repo).await.unwrap(),
        Some(moved)
    );
    assert_eq!(
        snap.project_by_repo(&RepoSlug::parse("acme/missing").unwrap())
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn project_keyset_pagination_and_cursor_validation() {
    let store = store().await;
    let rows = [
        project(1, "acme/one", false),
        project(2, "acme/two", false),
        project(3, "acme/three", false),
    ];
    let mut tx = store.begin_write().await.unwrap();
    for row in &rows {
        tx.append(event(EventType::ProjectAdded, row))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let mut snap = store.snapshot().await.unwrap();
    for order in [Order::Asc, Order::Desc] {
        let first = snap
            .projects(&Page {
                limit: 2,
                order,
                cursor: None,
            })
            .await
            .unwrap();
        let expected: Vec<Project> = match order {
            Order::Asc => rows.to_vec(),
            Order::Desc => rows.iter().rev().cloned().collect(),
        };
        assert_eq!(first.items, expected[..2]);
        assert!(matches!(
            snap.projects(&Page {
                limit: 2,
                order: match order {
                    Order::Asc => Order::Desc,
                    Order::Desc => Order::Asc,
                },
                cursor: first.next_cursor.clone(),
            })
            .await,
            Err(StoreError::InvalidCursor)
        ));
        let second = snap
            .projects(&Page {
                limit: 2,
                order,
                cursor: first.next_cursor,
            })
            .await
            .unwrap();
        assert_eq!(second.items, expected[2..]);
        assert!(second.next_cursor.is_none());
    }
    for cursor in [
        "devices:asc:invalid",
        "projects:asc:invalid",
        "projects:desc:invalid",
    ] {
        assert!(matches!(
            snap.projects(&Page {
                limit: 1,
                order: Order::Asc,
                cursor: Some(PageCursor::new(cursor))
            })
            .await,
            Err(StoreError::InvalidCursor)
        ));
    }
    // 资源标签不符但 ID 本身合法：必须由资源标签检查拒绝，而不是 ID 解析失败。
    let valid_project: ProjectId = Id::from_parts(NOW as u64, 1).unwrap();
    assert!(matches!(
        snap.projects(&Page {
            limit: 1,
            order: Order::Asc,
            cursor: Some(PageCursor::new(format!("devices:asc:{valid_project}"))),
        })
        .await,
        Err(StoreError::InvalidCursor)
    ));
    for limit in [0, 201] {
        assert!(matches!(
            snap.projects(&Page {
                limit,
                ..Page::default()
            })
            .await,
            Err(StoreError::PreconditionFailed(_))
        ));
    }
}

async fn open_sqlite() -> Store {
    let clock = Arc::new(FakeClock::new(NOW));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    Store::open_temp(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

/// #280 K2：`subject`、`project_id`、`data.id` 任一不一致都必须拒绝追加，且不改投影。
async fn assert_envelope_mismatch_rejected(store: &Store) {
    let row = project(1, "acme/one", false);
    let other = project(2, "acme/two", false);

    let mut cases = Vec::new();
    let mut bad = event(EventType::ProjectAdded, &row);
    bad.project_id = Some(other.id);
    cases.push(("project_id", bad));

    let mut bad = event(EventType::ProjectAdded, &row);
    bad.subject = Subject::parse(&other.id.to_string()).unwrap();
    cases.push(("subject", bad));

    let mut bad = event(EventType::ProjectAdded, &row);
    bad.data = serde_json::to_value(&other).unwrap();
    cases.push(("data.id", bad));

    for (field, bad) in cases {
        let mut tx = store.begin_write().await.unwrap();
        let error = tx.append(bad).await.unwrap_err();
        match error {
            StoreError::Corrupt(message) => {
                assert!(message.contains("seq 1"), "{field}: {message}");
                assert!(message.contains("project.added"), "{field}: {message}");
            }
            other => panic!("{field}: unexpected error {other:?}"),
        }
        assert!(tx.read().project(&row.id).await.unwrap().is_none());
        assert!(tx.read().project(&other.id).await.unwrap().is_none());
        tx.rollback().await.unwrap();
    }

    let mut snap = store.snapshot().await.unwrap();
    assert!(snap.project(&row.id).await.unwrap().is_none());
    drop(snap);

    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectAdded, &row))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(snap.project(&row.id).await.unwrap(), Some(row));
}

#[tokio::test]
async fn mismatched_project_envelope_is_rejected_and_rolled_back_in_memory() {
    let store = store().await;
    assert_envelope_mismatch_rejected(&store).await;
}

#[tokio::test]
async fn mismatched_project_envelope_is_rejected_and_rolled_back_in_sqlite() {
    let store = open_sqlite().await;
    assert_envelope_mismatch_rejected(&store).await;
}
