//! `ForgeReader::discussion` 的验收：forge.md §12 A 组"正文与评论均为 `UntrustedText`、受
//! `max_bytes` 截断，且不产生任何 `forge.*` 事件、不改快照"，以及 §12 D 组与 `FakeForge`
//! 共用的一致性用例。全部对 `FakeGitHub`（A 级）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Actor, ActorKind, MergeMethod, Project};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{Clock, Id, IdGen, IdKind, Prj, ProjectId};
use gantry_core::ports::{Discussion, DiscussionQuery, DiscussionTarget, ForgeError, ForgeReader};
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::forge::{
    discussion_conformance, FakeForge, DISCUSSION_BODY, DISCUSSION_COMMENTS, DISCUSSION_TITLE,
};
use gantry_fakes::github::{FakeGitHub, ScriptedFailure};
use gantry_forge::{
    AuthError, Forge, ForgeConfig, Permission, ProjectBinding, Reader, ReaderDeps,
    StaticTokenProvider, TokenProvider, TokenPurpose,
};
use gantry_store::{Store, StoreDeps};
use serde_json::{json, Value};

const MS: i64 = 1_700_000_000_000;
const REPO: &str = "acme/shop-api";

fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS as u64, n).expect("test id parts")
}

fn project_id() -> ProjectId {
    id::<Prj>(1)
}

fn project() -> Project {
    Project {
        id: project_id(),
        repo: RepoSlug::parse(REPO).expect("slug"),
        installation_id: 42,
        bot_user_id: 9,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: Timestamp::from_millis(MS),
        updated_at: Timestamp::from_millis(MS),
    }
}

fn open_store(clock: Arc<FakeClock>) -> Store {
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    Store::open_in_memory_sync(StoreDeps { clock, ids }).expect("store")
}

const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_BASE: &str = "3333333333333333333333333333333333333333";

fn untrusted_json(text: &str) -> Value {
    json!({"text": text, "source": "github:test", "truncated": false})
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
            "installation_id": 42,
            "bot_user_id": 9,
            "default_branch": "main",
            "allowed_merge_methods": ["squash"],
            "merge_queue_enabled": false,
            "created_at": "2023-11-14T22:13:20.000Z",
            "updated_at": "2023-11-14T22:13:20.000Z",
        }),
        dedup_key: None,
    }
}

fn synced(project: ProjectId, target: Value, changes: Value, at: &str) -> NewEvent {
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
            "synced_at": at,
        }),
        dedup_key: None,
    }
}

fn pr_synced(project: ProjectId, number: u64, at: &str) -> NewEvent {
    synced(
        project,
        json!({"type": "pr", "number": number, "node_id": format!("PR_{number}")}),
        json!([
            {"field": "state", "from": "closed", "to": "open"},
            {"field": "title", "from": untrusted_json("-"), "to": untrusted_json("修复登录")},
            {"field": "head_sha", "from": SHA_BASE, "to": SHA_A},
            {"field": "base_ref", "from": "-", "to": "main"},
            {"field": "base_sha", "from": SHA_A, "to": SHA_BASE},
            {"field": "draft", "from": true, "to": false},
            {"field": "mergeable_state", "from": "unknown", "to": "clean"},
            {"field": "in_merge_queue", "from": true, "to": false},
            {"field": "checks", "from": {"passed": 0, "failed": 0, "pending": 0, "required_ok": false},
             "to": {"passed": 3, "failed": 0, "pending": 1, "required_ok": false}},
            {"field": "review_state", "from": "approved", "to": "none"},
        ]),
        at,
    )
}

fn issue_synced(project: ProjectId, number: u64, at: &str) -> NewEvent {
    synced(
        project,
        json!({"type": "issue", "number": number, "node_id": format!("I_{number}")}),
        json!([
            {"field": "state", "from": "closed", "to": "open"},
            {"field": "title", "from": untrusted_json("-"), "to": untrusted_json("登录失败")},
        ]),
        at,
    )
}

async fn append_events(store: &Store, events: Vec<NewEvent>) {
    let mut tx = store.begin_write().await.unwrap();
    for event in events {
        tx.append(event).await.unwrap();
    }
    tx.commit().await.unwrap();
}

struct Harness {
    github: FakeGitHub,
    store: Store,
    clock: Arc<FakeClock>,
    reader: Reader,
}

fn harness() -> Harness {
    harness_with(Arc::new(StaticTokenProvider::new(SecretString::new(
        "ghs_read".into(),
    ))))
}

fn harness_with(tokens: Arc<dyn TokenProvider>) -> Harness {
    let github = FakeGitHub::start();
    let clock = Arc::new(FakeClock::new(MS));
    let store = open_store(clock.clone());
    let forge = Forge::new(
        store.clone(),
        ForgeConfig::default().with_project(ProjectBinding {
            project: project(),
            granted: [Permission::IssuesWrite].into_iter().collect(),
        }),
    );
    let reader = forge.reader(ReaderDeps {
        tokens,
        api_base: github.base_url(),
        clock: clock.clone(),
        http_timeout: Duration::from_secs(5),
        reconciler: None,
        mirror: None,
    });
    Harness {
        github,
        store,
        clock,
        reader,
    }
}

#[derive(Default)]
struct RecordingTokens {
    calls: Mutex<Vec<TokenPurpose>>,
    invalidations: AtomicUsize,
}

#[async_trait::async_trait]
impl TokenProvider for RecordingTokens {
    async fn token(
        &self,
        _project: &ProjectId,
        purpose: TokenPurpose,
    ) -> Result<Arc<SecretString>, AuthError> {
        self.calls.lock().unwrap().push(purpose);
        Ok(Arc::new(SecretString::new("ghs_read".into())))
    }

    fn invalidate(&self, _project: &ProjectId) {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
    }
}

fn seed(github: &FakeGitHub, number: u64) {
    github.set_title_body(REPO, number, DISCUSSION_TITLE, DISCUSSION_BODY);
    for (login, association, created_at, body) in DISCUSSION_COMMENTS {
        github.add_comment(REPO, number, (login, association), created_at, body);
    }
}

fn query(max_bytes: u32) -> DiscussionQuery {
    DiscussionQuery {
        include_body: true,
        comments_since: None,
        max_comments: 50,
        max_bytes,
    }
}

#[tokio::test]
async fn discussion_conformance_on_fake_github() {
    let h = harness();
    let issue = h.github.insert_issue(REPO, "placeholder");
    seed(&h.github, issue);
    let pull = h
        .github
        .insert_pull(REPO, "gantry/1-fix", "main", ("octocat", 1), "");
    seed(&h.github, pull);
    for target in [
        DiscussionTarget::Issue(issue),
        DiscussionTarget::PullRequest(pull),
    ] {
        discussion_conformance(
            &h.reader,
            &project_id(),
            target,
            DiscussionTarget::Issue(999),
        )
        .await
        .expect("real forge conformance");
    }
}

#[tokio::test]
async fn discussion_conformance_on_fake_forge() {
    let forge = FakeForge::new(Timestamp::from_millis(MS));
    let comments = DISCUSSION_COMMENTS
        .iter()
        .enumerate()
        .map(
            |(n, (login, association, created_at, body))| gantry_core::ports::DiscussionComment {
                id: n as u64 + 1,
                author_login: (*login).into(),
                author_association: Some((*association).into()),
                created_at: Timestamp::parse_rfc3339(created_at).unwrap(),
                body: untrusted(body),
            },
        )
        .collect();
    forge.set_discussion(
        project_id(),
        DiscussionTarget::Issue(7),
        Discussion {
            title: untrusted(DISCUSSION_TITLE),
            body: Some(untrusted(DISCUSSION_BODY)),
            comments,
            truncated: false,
            fetched_at: Timestamp::from_millis(MS),
        },
    );
    discussion_conformance(
        &forge,
        &project_id(),
        DiscussionTarget::Issue(7),
        DiscussionTarget::Issue(999),
    )
    .await
    .expect("fake forge conformance");
}

fn untrusted(text: &str) -> gantry_core::text::UntrustedText {
    gantry_core::text::UntrustedText::new(text.into(), "fake".into(), false)
}

#[tokio::test]
async fn discussion_is_untrusted_bounded_and_writes_nothing() {
    let h = harness();
    let issue = h.github.insert_issue(REPO, "placeholder");
    seed(&h.github, issue);
    let before = h.store.snapshot_sync().unwrap().head_seq();

    let discussion = h
        .reader
        .discussion(&project_id(), DiscussionTarget::Issue(issue), query(10))
        .await
        .expect("discussion");
    let body = discussion.body.expect("body");
    assert_eq!(body.as_untrusted_str(), &DISCUSSION_BODY[..10]);
    assert!(body.truncated());
    assert!(body.source().starts_with("github:"));
    assert!(discussion.comments.is_empty(), "第一条评论就超出 10 字节");
    assert!(discussion.truncated);
    assert_eq!(discussion.fetched_at, Timestamp::from_millis(MS));

    // 不写 GitHub、不追加事件。
    assert_eq!(h.github.write_count(), 0);
    assert_eq!(h.store.snapshot_sync().unwrap().head_seq(), before);
}

#[tokio::test]
async fn issue_target_that_is_a_pull_request_is_not_found() {
    let h = harness();
    let pull = h
        .github
        .insert_pull(REPO, "gantry/1-fix", "main", ("octocat", 1), "");
    let error = h
        .reader
        .discussion(&project_id(), DiscussionTarget::Issue(pull), query(1024))
        .await
        .expect_err("pull is not an issue");
    assert_eq!(error, gantry_core::ports::ForgeError::NotFound);
}

/// forge.md §7.12：读路径 401 → invalidate 后换只读令牌重试一次。
#[tokio::test]
async fn discussion_retries_once_after_401_with_a_read_token() {
    let tokens = Arc::new(RecordingTokens::default());
    let h = harness_with(tokens.clone());
    let issue = h.github.insert_issue(REPO, "placeholder");
    seed(&h.github, issue);
    h.github.queue_failure(401);

    let reader: &dyn ForgeReader = &h.reader;
    let discussion = reader
        .discussion(&project_id(), DiscussionTarget::Issue(issue), query(1024))
        .await
        .expect("retry after 401");
    assert_eq!(discussion.title.as_untrusted_str(), DISCUSSION_TITLE);
    // 第一次取正文：401 后换令牌重试；随后取评论再取一次。全部是只读令牌。
    assert_eq!(
        *tokens.calls.lock().unwrap(),
        vec![TokenPurpose::Read, TokenPurpose::Read, TokenPurpose::Read]
    );
    assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1);
}

/// 连续两次 401：只重试一次，第二次直接把错误交给调用方。
#[tokio::test]
async fn discussion_gives_up_after_two_401s() {
    let tokens = Arc::new(RecordingTokens::default());
    let h = harness_with(tokens.clone());
    let issue = h.github.insert_issue(REPO, "placeholder");
    for _ in 0..2 {
        h.github.push_failure(ScriptedFailure {
            status: 401,
            body: r#"{"message":"Bad credentials"}"#.into(),
            retry_after_s: None,
        });
    }
    let reader: &dyn ForgeReader = &h.reader;
    reader
        .discussion(&project_id(), DiscussionTarget::Issue(issue), query(1024))
        .await
        .unwrap_err();
    assert_eq!(tokens.calls.lock().unwrap().len(), 2);
    assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1);
}

/// forge.md §6 本地快照：按项目绑定的 RepoSlug 读 store，不访问 GitHub、不写事件。
#[tokio::test]
async fn snapshots_come_from_the_bound_project_store() {
    let h = harness();
    let p = project_id();
    append_events(
        &h.store,
        vec![
            project_added(p, REPO),
            pr_synced(p, 7, "2023-11-14T22:15:00.000Z"),
            issue_synced(p, 7, "2023-11-14T22:16:00.000Z"),
        ],
    )
    .await;
    let before = h.store.snapshot_sync().unwrap().head_seq();

    let reader: &dyn ForgeReader = &h.reader;
    let pr = reader.pull_request(&p, 7).await.unwrap().unwrap();
    assert_eq!(pr.node_id, "PR_7");
    assert_eq!(pr.head_sha.to_string(), SHA_A);
    assert_eq!(pr.synced_at.to_string(), "2023-11-14T22:15:00.000Z");
    let issue = reader.issue(&p, 7).await.unwrap().unwrap();
    assert_eq!(issue.node_id, "I_7");
    assert_eq!(issue.synced_at.to_string(), "2023-11-14T22:16:00.000Z");
    assert!(reader.pull_request(&p, 8).await.unwrap().is_none());
    assert!(reader.issue(&p, 8).await.unwrap().is_none());

    assert_eq!(h.github.recorded_calls().len(), 0, "no GitHub access");
    assert_eq!(
        h.store.snapshot_sync().unwrap().head_seq(),
        before,
        "no events"
    );
}

/// 两张绑定项目各自的仓库不能互读同号快照。
#[tokio::test]
async fn snapshots_do_not_cross_projects() {
    let github = FakeGitHub::start();
    let clock = Arc::new(FakeClock::new(MS));
    let store = open_store(clock.clone());
    let a = project_id();
    let b = id::<Prj>(2);
    let mut project_b = project();
    project_b.id = b;
    project_b.repo = RepoSlug::parse("acme/other").unwrap();
    let forge = Forge::new(
        store.clone(),
        ForgeConfig::default()
            .with_project(ProjectBinding {
                project: project(),
                granted: [Permission::IssuesWrite].into_iter().collect(),
            })
            .with_project(ProjectBinding {
                project: project_b,
                granted: [Permission::IssuesWrite].into_iter().collect(),
            }),
    );
    append_events(
        &store,
        vec![
            project_added(a, REPO),
            project_added(b, "acme/other"),
            pr_synced(a, 7, "2023-11-14T22:15:00.000Z"),
            issue_synced(a, 7, "2023-11-14T22:16:00.000Z"),
            pr_synced(b, 8, "2023-11-14T22:17:00.000Z"),
        ],
    )
    .await;
    let reader = forge.reader(ReaderDeps {
        tokens: Arc::new(StaticTokenProvider::new(SecretString::new(
            "ghs_read".into(),
        ))),
        api_base: github.base_url(),
        clock,
        http_timeout: Duration::from_secs(5),
        reconciler: None,
        mirror: None,
    });
    let reader: &dyn ForgeReader = &reader;

    assert!(reader.pull_request(&a, 7).await.unwrap().is_some());
    assert!(reader.issue(&a, 7).await.unwrap().is_some());
    assert!(
        reader.pull_request(&b, 7).await.unwrap().is_none(),
        "project B must not see project A's repo"
    );
    assert!(reader.issue(&b, 7).await.unwrap().is_none());
    assert_eq!(
        reader.pull_request(&b, 8).await.unwrap().unwrap().node_id,
        "PR_8"
    );
    assert_eq!(github.recorded_calls().len(), 0);
}

/// `synced_at` 是已有绑定的更新时间，不是读取时的当前时间。
#[tokio::test]
async fn project_snapshot_uses_the_bound_updated_at() {
    let h = harness();
    h.clock.advance_ms(5_000);
    let reader: &dyn ForgeReader = &h.reader;
    let snapshot = reader.project_snapshot(&project_id()).await.unwrap();
    assert_eq!(snapshot.project.id, project_id());
    assert_eq!(snapshot.synced_at, Timestamp::from_millis(MS));
    assert_ne!(snapshot.synced_at, h.clock.now());
}

/// 未知项目不能静默返回空快照或空 PR / Issue。
#[tokio::test]
async fn unknown_project_is_an_error() {
    let h = harness();
    let unknown = id::<Prj>(99);
    let reader: &dyn ForgeReader = &h.reader;
    for error in [
        reader.pull_request(&unknown, 1).await.unwrap_err(),
        reader.issue(&unknown, 1).await.unwrap_err(),
        reader.project_snapshot(&unknown).await.unwrap_err(),
    ] {
        assert!(
            matches!(error, ForgeError::InvalidTarget { .. }),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn comments_are_paged_past_one_hundred() {
    let h = harness();
    let issue = h.github.insert_issue(REPO, "placeholder");
    for n in 0..150 {
        h.github.add_comment(
            REPO,
            issue,
            ("alice", "OWNER"),
            &format!("2023-11-14T23:{:02}:{:02}Z", n / 60, n % 60),
            "x",
        );
    }
    let discussion = h
        .reader
        .discussion(
            &project_id(),
            DiscussionTarget::Issue(issue),
            DiscussionQuery {
                include_body: false,
                comments_since: None,
                max_comments: 120,
                max_bytes: 4096,
            },
        )
        .await
        .unwrap();
    assert_eq!(discussion.comments.len(), 120);
    assert!(discussion.truncated);
}
