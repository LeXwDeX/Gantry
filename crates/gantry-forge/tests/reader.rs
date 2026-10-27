//! `ForgeReader::discussion` 的验收：forge.md §12 A 组"正文与评论均为 `UntrustedText`、受
//! `max_bytes` 截断，且不产生任何 `forge.*` 事件、不改快照"，以及 §12 D 组与 `FakeForge`
//! 共用的一致性用例。全部对 `FakeGitHub`（A 级）。

use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{MergeMethod, Project};
use gantry_core::ids::{Id, IdGen, IdKind, Prj, ProjectId};
use gantry_core::ports::{Discussion, DiscussionQuery, DiscussionTarget, ForgeReader};
use gantry_core::secret::SecretString;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::forge::{
    discussion_conformance, FakeForge, DISCUSSION_BODY, DISCUSSION_COMMENTS, DISCUSSION_TITLE,
};
use gantry_fakes::github::FakeGitHub;
use gantry_forge::{
    Forge, ForgeConfig, Permission, ProjectBinding, Reader, ReaderDeps, StaticTokenProvider,
};
use gantry_store::{Store, StoreDeps};

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

struct Harness {
    github: FakeGitHub,
    store: Store,
    reader: Reader,
}

fn harness() -> Harness {
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
        tokens: Arc::new(StaticTokenProvider::new(SecretString::new(
            "ghs_read".into(),
        ))),
        api_base: github.base_url(),
        clock,
        http_timeout: Duration::from_secs(5),
    });
    Harness {
        github,
        store,
        reader,
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
