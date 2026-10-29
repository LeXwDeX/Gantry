#![cfg(feature = "testing")]

use std::path::PathBuf;
use std::sync::Arc;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Actor, ActorKind, Project};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{Id, IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{
    Order, Page, PageCursor, Store, StoreConfig, StoreDeps, StoreError, VerifyLevel,
};
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;

fn deps() -> StoreDeps {
    let clock = Arc::new(FakeClock::new(NOW));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    StoreDeps { clock, ids }
}

fn id(n: u128) -> ProjectId {
    Id::from_parts(NOW as u64, n).unwrap()
}

fn project(n: u128, repo: &str, updated: bool) -> Project {
    serde_json::from_value(json!({
        "id": id(n), "repo": repo,
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

fn issue_sync(project: ProjectId) -> NewEvent {
    NewEvent {
        ty: EventType::ForgeSynced,
        subject: Subject::parse(&project.to_string()).unwrap(),
        project_id: Some(project),
        actor: Actor::new(ActorKind::Forge, "forge"),
        causation_id: None,
        correlation_id: None,
        data: json!({
            "target": {"type": "issue", "number": 12, "node_id": "I_12"},
            "source": "reconcile", "changes": [],
            "initial": {"type": "issue", "state": "open", "state_reason": null,
                "title": {"text": "test", "source": "github:issue.title", "truncated": false}},
            "synced_at": "2026-10-24T12:41:03.120Z"
        }),
        dedup_key: None,
    }
}

fn repo(s: &str) -> RepoSlug {
    RepoSlug::parse(s).unwrap()
}

async fn exercise(store: &Store) {
    let a = project(1, "acme/one", false);
    let changed = project(1, "acme/new", true);
    let b = project(3, "acme/three", false);
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectAdded, &a)).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        store
            .snapshot()
            .await
            .unwrap()
            .project(&a.id)
            .await
            .unwrap(),
        None
    );
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectAdded, &a)).await.unwrap();
    tx.append(issue_sync(a.id)).await.unwrap();
    assert_eq!(tx.read().head_seq().as_u64(), 2);
    assert_eq!(tx.read().project(&a.id).await.unwrap(), Some(a.clone()));
    assert_eq!(
        tx.read().projects(&Page::default()).await.unwrap().items,
        vec![a.clone()]
    );
    assert!(tx
        .read()
        .issue_snapshot(&a.repo, 12)
        .await
        .unwrap()
        .is_some());
    tx.commit().await.unwrap();

    let mut old = store.snapshot().await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectUpdated, &changed))
        .await
        .unwrap();
    tx.append(event(EventType::ProjectAdded, &b)).await.unwrap();
    let mut invalid = event(EventType::ProjectUpdated, &a);
    invalid.data["allowed_merge_methods"] = json!([]);
    assert!(matches!(
        tx.append(invalid).await,
        Err(StoreError::InvalidEvent(_))
    ));
    assert_eq!(
        tx.read().project(&a.id).await.unwrap(),
        Some(changed.clone())
    );
    assert!(tx
        .read()
        .issue_snapshot(&changed.repo, 12)
        .await
        .unwrap()
        .is_some());
    assert!(tx
        .read()
        .issue_snapshot(&a.repo, 12)
        .await
        .unwrap()
        .is_none());
    tx.commit().await.unwrap();
    assert_eq!(old.head_seq().as_u64(), 2);
    assert_eq!(old.project(&a.id).await.unwrap(), Some(a.clone()));
    assert!(old.issue_snapshot(&a.repo, 12).await.unwrap().is_some());

    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(snap.head_seq().as_u64(), 4);
    assert!(snap.unprojected().is_empty());
    assert_eq!(snap.project(&a.id).await.unwrap(), Some(changed.clone()));
    assert_eq!(
        snap.project_by_repo(&changed.repo).await.unwrap(),
        Some(changed.clone())
    );
    assert_eq!(snap.project_by_repo(&a.repo).await.unwrap(), None);
    let first = snap
        .projects(&Page {
            limit: 1,
            ..Page::default()
        })
        .await
        .unwrap();
    assert_eq!(first.items, vec![changed.clone()]);
    let cursor = first.next_cursor.unwrap();
    let desc = snap
        .projects(&Page {
            limit: 1,
            order: Order::Desc,
            cursor: None,
        })
        .await
        .unwrap();
    assert_eq!(desc.items, vec![b.clone()]);
    let desc_next = snap
        .projects(&Page {
            limit: 1,
            order: Order::Desc,
            cursor: desc.next_cursor,
        })
        .await
        .unwrap();
    assert_eq!(desc_next.items, vec![changed.clone()]);
    for cursor in [
        PageCursor::new("devices:asc:invalid"),
        PageCursor::new("projects:asc:invalid"),
        cursor.clone(),
    ] {
        let order = if cursor.as_str().starts_with("projects:asc:prj_") {
            Order::Desc
        } else {
            Order::Asc
        };
        assert!(matches!(
            snap.projects(&Page {
                limit: 1,
                cursor: Some(cursor),
                order
            })
            .await,
            Err(StoreError::InvalidCursor)
        ));
    }
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
    let middle = project(2, "acme/middle", false);
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectAdded, &middle))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut next = store.snapshot().await.unwrap();
    let page = next
        .projects(&Page {
            limit: 2,
            cursor: Some(cursor),
            order: Order::Asc,
        })
        .await
        .unwrap();
    assert_eq!(page.items, vec![middle, b.clone()]);
    assert!(page.next_cursor.is_none());

    let duplicate_repo = project(4, "acme/new", false);
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectAdded, &duplicate_repo))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(snap.project_by_repo(&changed.repo).await.unwrap(), None);
    assert!(snap
        .issue_snapshot(&changed.repo, 12)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        snap.project(&duplicate_repo.id).await.unwrap(),
        Some(duplicate_repo)
    );
    let moved = project(4, "acme/four", true);
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(EventType::ProjectUpdated, &moved))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(
        snap.project_by_repo(&changed.repo).await.unwrap(),
        Some(changed.clone())
    );
    assert!(snap
        .issue_snapshot(&changed.repo, 12)
        .await
        .unwrap()
        .is_some());
    assert!(store.verify(VerifyLevel::Full).await.unwrap().is_ok());
}

#[tokio::test]
async fn memory_projection_and_queries() {
    let store = Store::open_in_memory(deps(), &[]).await.unwrap();
    exercise(&store).await;
}

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn sqlite_projection_replays_after_reopen() {
    let dir = Dir(std::env::temp_dir().join(format!(
        "gantry-projects-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    {
        let store = Store::open(StoreConfig::new(&dir.0), deps(), &[])
            .await
            .unwrap();
        exercise(&store).await;
    }
    let store = Store::open(StoreConfig::new(&dir.0), deps(), &[])
        .await
        .unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(snap.head_seq().as_u64(), 7);
    assert_eq!(
        snap.project(&id(1)).await.unwrap(),
        Some(project(1, "acme/new", true))
    );
    assert_eq!(
        snap.projects(&Page::default()).await.unwrap().items.len(),
        4
    );
    assert_eq!(
        snap.project_by_repo(&repo("acme/new")).await.unwrap(),
        Some(project(1, "acme/new", true))
    );
    assert!(snap
        .issue_snapshot(&repo("acme/new"), 12)
        .await
        .unwrap()
        .is_some());
    assert!(store.verify(VerifyLevel::Full).await.unwrap().is_ok());
}
