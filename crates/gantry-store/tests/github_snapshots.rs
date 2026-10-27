#![cfg(feature = "testing")]

//! GitHub 快照投影（#150；store.md §6.2 `pr_snapshot` / `issue_snapshot`、§7.2、§12.3）。
//!
//! 一致性用例只经 `Store` 的公开接口，SQLite 版落地后对同一组用例再跑一遍。

use std::sync::Arc;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{
    Actor, ActorKind, CheckSummary, IssueState, PullRequestState, ReviewState,
};
use gantry_core::events::{Event, EventType, NewEvent, Subject};
use gantry_core::ids::{Id, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{GithubSnapshots, Store, StoreDeps, StoreError};
use serde_json::{json, Value};

const NOW_MS: i64 = 1_700_000_000_000;
const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_B: &str = "2222222222222222222222222222222222222222";
const SHA_BASE: &str = "3333333333333333333333333333333333333333";
const PR_NODE: &str = "PR_kwDOabc123_57";
const ISSUE_NODE: &str = "I_kwDOabc123_12";

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW_MS));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(11)),
    ));
    Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

fn project(n: u128) -> ProjectId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

fn repo(name: &str) -> RepoSlug {
    RepoSlug::parse(name).unwrap()
}

fn project_added(project: ProjectId, repo: &str) -> NewEvent {
    NewEvent {
        ty: EventType::ProjectAdded,
        subject: Subject::parse(&project.to_string()).unwrap(),
        project_id: Some(project),
        actor: Actor::new(ActorKind::System, "api"),
        causation_id: None,
        correlation_id: None,
        data: json!({
            "id": project,
            "repo": repo,
            "installation_id": 1,
            "bot_user_id": 2,
            "default_branch": "main",
            "allowed_merge_methods": ["squash"],
            "merge_queue_enabled": false,
            "created_at": "2026-10-24T12:00:00.000Z",
            "updated_at": "2026-10-24T12:00:00.000Z",
        }),
        dedup_key: None,
    }
}

fn synced(project: ProjectId, target: Value, changes: Value, synced_at: &str) -> NewEvent {
    NewEvent {
        ty: EventType::ForgeSynced,
        subject: Subject::parse(&project.to_string()).unwrap(),
        project_id: Some(project),
        actor: Actor::new(ActorKind::Forge, "forge"),
        causation_id: None,
        correlation_id: None,
        data: json!({
            "target": target,
            "source": "reconcile",
            "changes": changes,
            "synced_at": synced_at,
        }),
        dedup_key: None,
    }
}

fn pr(number: u64) -> Value {
    json!({"type": "pr", "number": number, "node_id": PR_NODE})
}

fn issue(number: u64) -> Value {
    json!({"type": "issue", "number": number, "node_id": ISSUE_NODE})
}

fn title(text: &str) -> Value {
    json!({"text": text, "source": "github:pr.title", "truncated": false})
}

fn checks(passed: u32, failed: u32, pending: u32, required_ok: bool) -> Value {
    json!({"passed": passed, "failed": failed, "pending": pending, "required_ok": required_ok})
}

/// 用 `changes` 一次写满（投影在本地无值时不比较 `from`）。forge 的首次快照改用
/// `initial`（#150 拍板），见 `initial_snapshot_fills_the_row_then_changes_apply`。
fn first_pr_snapshot() -> Value {
    json!([
        {"field": "state", "from": "closed", "to": "open"},
        {"field": "title", "from": title("-"), "to": title("修复登录")},
        {"field": "head_sha", "from": SHA_B, "to": SHA_A},
        {"field": "base_ref", "from": "-", "to": "main"},
        {"field": "base_sha", "from": SHA_A, "to": SHA_BASE},
        {"field": "draft", "from": true, "to": false},
        {"field": "mergeable_state", "from": "unknown", "to": "clean"},
        {"field": "in_merge_queue", "from": true, "to": false},
        {"field": "checks", "from": checks(0, 0, 0, false), "to": checks(3, 0, 1, false)},
        {"field": "review_state", "from": "approved", "to": "none"},
    ])
}

async fn append_all(store: &Store, events: Vec<NewEvent>) -> Result<(), StoreError> {
    let mut tx = store.begin_write().await?;
    for event in events {
        tx.append(event).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn all_events(store: &Store) -> Vec<Event<Value>> {
    let log = store.events();
    let head = log.head().await.unwrap();
    let mut out = Vec::new();
    for seq in 1..=head.as_u64() {
        out.push(
            log.get_by_seq(gantry_core::entities::Seq::new(seq).unwrap())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    out
}

#[tokio::test]
async fn first_full_sync_makes_pr_readable() {
    let store = store().await;
    let p = project(1);
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            synced(p, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z"),
        ],
    )
    .await
    .unwrap();

    let mut snap = store.snapshot().await.unwrap();
    let got = snap
        .pr_snapshot(&repo("acme/web"), 57)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.node_id, PR_NODE);
    assert_eq!(got.state, PullRequestState::Open);
    assert_eq!(got.head_sha.to_string(), SHA_A);
    assert_eq!(got.base_ref, "main");
    assert_eq!(got.mergeable_state, "clean");
    assert_eq!(
        got.checks,
        CheckSummary {
            passed: 3,
            failed: 0,
            pending: 1,
            required_ok: false
        }
    );
    assert_eq!(got.review_state, ReviewState::None);
    assert_eq!(got.synced_at.to_string(), "2026-10-24T12:41:03.120Z");
    assert!(snap
        .pr_snapshot(&repo("acme/web"), 58)
        .await
        .unwrap()
        .is_none());
    assert!(snap
        .pr_snapshot(&repo("acme/other"), 57)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn partial_pr_snapshot_is_not_readable() {
    let store = store().await;
    let p = project(1);
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            synced(
                p,
                pr(57),
                json!([{"field": "mergeable_state", "from": "clean", "to": "dirty"}]),
                "2026-10-24T12:41:03.120Z",
            ),
        ],
    )
    .await
    .unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert!(snap
        .pr_snapshot(&repo("acme/web"), 57)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn incremental_sync_updates_only_changed_fields() {
    let store = store().await;
    let p = project(1);
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            synced(p, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z"),
            synced(
                p,
                pr(57),
                json!([
                    {"field": "head_sha", "from": SHA_A, "to": SHA_B},
                    {"field": "checks", "from": checks(3, 0, 1, false), "to": checks(4, 0, 0, true)},
                ]),
                "2026-10-24T12:45:00.000Z",
            ),
        ],
    )
    .await
    .unwrap();
    let mut snap = store.snapshot().await.unwrap();
    let got = snap
        .pr_snapshot(&repo("acme/web"), 57)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.head_sha.to_string(), SHA_B);
    assert!(got.checks.required_ok);
    assert_eq!(got.mergeable_state, "clean");
    assert_eq!(got.synced_at.to_string(), "2026-10-24T12:45:00.000Z");
}

#[tokio::test]
async fn tx_reader_sees_uncommitted_snapshot() {
    let store = store().await;
    let p = project(1);
    let mut tx = store.begin_write().await.unwrap();
    tx.append(project_added(p, "acme/web")).await.unwrap();
    tx.append(synced(
        p,
        pr(57),
        first_pr_snapshot(),
        "2026-10-24T12:41:03.120Z",
    ))
    .await
    .unwrap();
    let got = tx.read().pr_snapshot(&repo("acme/web"), 57).await.unwrap();
    assert!(got.is_some());
    tx.rollback().await.unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert!(snap
        .pr_snapshot(&repo("acme/web"), 57)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn stale_from_is_rejected_and_rolls_back_only_that_append() {
    let store = store().await;
    let p = project(1);
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            synced(p, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z"),
        ],
    )
    .await
    .unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let error = tx
        .append(synced(
            p,
            pr(57),
            json!([
                {"field": "head_sha", "from": SHA_A, "to": SHA_B},
                // 快照里是 clean：from 不符，整条事件拒绝，head_sha 也不变。
                {"field": "mergeable_state", "from": "dirty", "to": "blocked"},
            ]),
            "2026-10-24T12:45:00.000Z",
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::PreconditionFailed(_)),
        "{error:?}"
    );
    let got = tx
        .read()
        .pr_snapshot(&repo("acme/web"), 57)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.head_sha.to_string(), SHA_A);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn envelope_must_come_from_forge_with_project_subject() {
    let store = store().await;
    let p = project(1);
    append_all(&store, vec![project_added(p, "acme/web")])
        .await
        .unwrap();

    let mut not_forge = synced(p, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z");
    not_forge.actor = Actor::new(ActorKind::Engine, "engine");
    let mut wrong_subject = synced(p, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z");
    wrong_subject.subject = Subject::parse(&project(2).to_string()).unwrap();

    for event in [not_forge, wrong_subject] {
        let mut tx = store.begin_write().await.unwrap();
        assert!(tx.append(event).await.is_err());
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn node_id_mismatch_is_rejected() {
    let store = store().await;
    let p = project(1);
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            synced(p, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z"),
        ],
    )
    .await
    .unwrap();
    let mut tx = store.begin_write().await.unwrap();
    let error = tx
        .append(synced(
            p,
            json!({"type": "pr", "number": 57, "node_id": "PR_other"}),
            json!([{"field": "draft", "from": false, "to": true}]),
            "2026-10-24T12:45:00.000Z",
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn issue_snapshot_tracks_state_and_reason() {
    let store = store().await;
    let p = project(1);
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            synced(
                p,
                issue(12),
                json!([
                    {"field": "state", "from": "closed", "to": "open"},
                    {"field": "title", "from": title("-"), "to": title("登录失败")},
                ]),
                "2026-10-24T12:41:03.120Z",
            ),
        ],
    )
    .await
    .unwrap();
    let mut snap = store.snapshot().await.unwrap();
    let open = snap
        .issue_snapshot(&repo("acme/web"), 12)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(open.state, IssueState::Open);
    assert_eq!(open.state_reason, None);

    append_all(
        &store,
        vec![synced(
            p,
            issue(12),
            json!([
                {"field": "state", "from": "open", "to": "closed"},
                {"field": "state_reason", "from": null, "to": "not_planned"},
            ]),
            "2026-10-24T13:00:00.000Z",
        )],
    )
    .await
    .unwrap();
    let mut snap = store.snapshot().await.unwrap();
    let closed = snap
        .issue_snapshot(&repo("acme/web"), 12)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closed.state, IssueState::Closed);
    assert_eq!(closed.state_reason.as_deref(), Some("not_planned"));
    // PR 与 Issue 分开存放：同号的 PR 不存在。
    assert!(snap
        .pr_snapshot(&repo("acme/web"), 12)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn ambiguous_repo_binding_reads_none() {
    let store = store().await;
    let (a, b) = (project(1), project(2));
    append_all(
        &store,
        vec![
            project_added(a, "acme/web"),
            project_added(b, "acme/web"),
            synced(a, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z"),
        ],
    )
    .await
    .unwrap();
    let mut snap = store.snapshot().await.unwrap();
    assert!(snap
        .pr_snapshot(&repo("acme/web"), 57)
        .await
        .unwrap()
        .is_none());
}

/// §12.3：从日志重建的投影与增量折叠一致。
#[tokio::test]
async fn rebuild_from_log_matches_incremental_fold() {
    let store = store().await;
    let p = project(1);
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            synced(p, pr(57), first_pr_snapshot(), "2026-10-24T12:41:03.120Z"),
            synced(
                p,
                issue(12),
                json!([
                    {"field": "state", "from": "closed", "to": "open"},
                    {"field": "title", "from": title("-"), "to": title("登录失败")},
                ]),
                "2026-10-24T12:41:04.000Z",
            ),
        ],
    )
    .await
    .unwrap();
    append_all(
        &store,
        vec![synced(
            p,
            pr(57),
            json!([
                {"field": "state", "from": "open", "to": "merged"},
                {"field": "in_merge_queue", "from": false, "to": true},
            ]),
            "2026-10-24T13:00:00.000Z",
        )],
    )
    .await
    .unwrap();

    let mut rebuilt = GithubSnapshots::default();
    for event in all_events(&store).await {
        if GithubSnapshots::folds(event.ty) {
            rebuilt.apply(&event).unwrap();
        }
    }
    let mut snap = store.snapshot().await.unwrap();
    assert_eq!(
        rebuilt.pr_snapshot(&repo("acme/web"), 57),
        snap.pr_snapshot(&repo("acme/web"), 57).await.unwrap()
    );
    assert_eq!(
        rebuilt.issue_snapshot(&repo("acme/web"), 12),
        snap.issue_snapshot(&repo("acme/web"), 12).await.unwrap()
    );
    assert_eq!(
        rebuilt.pr_snapshot(&repo("acme/web"), 57).unwrap().state,
        PullRequestState::Merged
    );
}

/// #150 拍板的首次快照：`initial` 一次写满整行，之后的 `changes` 按 `from` 校验。
#[tokio::test]
async fn initial_snapshot_fills_the_row_then_changes_apply() {
    let store = store().await;
    let p = project(1);
    let mut first = synced(p, pr(57), json!([]), "2026-10-24T12:41:03.120Z");
    first.data["initial"] = json!({
        "type": "pr",
        "state": "open",
        "title": title("修复登录"),
        "head_sha": SHA_A,
        "base_ref": "main",
        "base_sha": SHA_BASE,
        "draft": false,
        "mergeable_state": "unknown",
        "in_merge_queue": false,
        "checks": checks(0, 0, 0, false),
        "review_state": "none"
    });
    let mut issue_first = synced(p, issue(42), json!([]), "2026-10-24T12:41:03.120Z");
    issue_first.data["initial"] = json!({
        "type": "issue",
        "state": "open",
        "state_reason": null,
        "title": title("登录失败")
    });
    append_all(
        &store,
        vec![
            project_added(p, "acme/web"),
            first,
            issue_first,
            synced(
                p,
                pr(57),
                json!([{"field": "mergeable_state", "from": "unknown", "to": "clean"}]),
                "2026-10-24T12:42:00.000Z",
            ),
        ],
    )
    .await
    .unwrap();

    let mut snap = store.snapshot().await.unwrap();
    let got = snap
        .pr_snapshot(&repo("acme/web"), 57)
        .await
        .unwrap()
        .expect("initial makes the pr readable");
    assert_eq!(got.head_sha.to_string(), SHA_A);
    assert_eq!(got.mergeable_state, "clean");
    assert_eq!(got.synced_at.to_string(), "2026-10-24T12:42:00.000Z");
    let issue = snap
        .issue_snapshot(&repo("acme/web"), 42)
        .await
        .unwrap()
        .expect("initial makes the issue readable");
    assert_eq!(issue.state_reason, None);

    // initial 之后 from 照常校验。
    let stale = synced(
        p,
        pr(57),
        json!([{"field": "mergeable_state", "from": "unknown", "to": "dirty"}]),
        "2026-10-24T12:43:00.000Z",
    );
    assert!(append_all(&store, vec![stale]).await.is_err());
}
