//! 本地 GitRepo + SQLite 联测；没有生产装配、网络或沙盒。
#![cfg(unix)]

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

#[path = "support/engine_cfg.rs"]
#[allow(dead_code)]
mod engine_cfg;

use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, LockHolder, Seq};
use gantry_core::events::{EventType, NewEvent, PatchRejectedData, PatchValidatedData, Subject};
use gantry_core::ids::IdGen;
use gantry_core::ports::{
    ActorCtx, BranchName, CommitMessage, CommitPatch, CreateWorktree, DeclaredPatch, PatchOutcome,
    RepoError, RepoService, RevSpec, ValidatePatch,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::{PatchRejectReason, PatchVerdict, PathConstraints};
use gantry_core::{
    entities::DeliveryOrigin,
    level::Level,
    settings::resource::{LevelBudget, LevelEntry, TierBudget},
};
use gantry_engine::{
    actor::EffectRunner,
    attempt::AttemptPorts,
    create::{budget_from_level, NewDelivery},
    decide::{Effect, EffectErrorClass, EffectReport},
    engine::{load_delivery_state, Engine},
    PortEffects,
};
use gantry_fakes::policy::FakePolicy;
use gantry_fakes::{
    forge::FakeForge, inbox::FakeInbox, sandbox::FakeSandboxService, worker::StubWorkerAdapter,
};
use gantry_repo::{GitRepo, RepoConfig, RepoDeps};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{EventFilter, Store, StoreConfig, StoreDeps, TypePattern};
use serde_json::json;

struct TempDir {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl TempDir {
    fn new() -> Self {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        for _ in 0..16 {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).unwrap();
            let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let path = parent.join(format!("gantry-real-repo-{name}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let metadata = std::fs::symlink_metadata(&path).unwrap();
                    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
                    assert_eq!(path.canonicalize().unwrap(), path);
                    return Self {
                        path,
                        dev: metadata.dev(),
                        ino: metadata.ino(),
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create test directory: {e}"),
            }
        }
        panic!("could not create an exclusive test directory");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && (metadata.dev(), metadata.ino()) == (self.dev, self.ino)
            {
                std::fs::remove_dir_all(&self.path).expect("remove owned test directory");
            }
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2027-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2027-01-01T00:00:00Z")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn context(ids: &IdGen) -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "real-repo-test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: ids.next_ulid().unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

async fn events(
    store: &Store,
    ty: EventType,
) -> Vec<gantry_core::events::Event<serde_json::Value>> {
    store
        .events()
        .scan(
            Seq::ZERO,
            &EventFilter {
                project_ids: None,
                include: vec![TypePattern::Exact(ty)],
                exclude: vec![],
            },
            200,
        )
        .await
        .unwrap()
        .events
}

async fn open_repo(store: &Store, dir: &Path, clock: Arc<FakeClock>, ids: Arc<IdGen>) -> GitRepo {
    let mut cfg = RepoConfig::new(dir.join("repo"));
    cfg.git_bin = Some(PathBuf::from("/usr/bin/git"));
    GitRepo::open(
        cfg,
        RepoDeps {
            store: store.clone(),
            clock,
            ids,
            policy: Arc::new(FakePolicy::new_in(store.clone())),
            cas: Arc::new(store.cas()),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn engine_resolves_real_mirror_branch_without_local_fallback() {
    let tmp = TempDir::new();
    let clock = Arc::new(FakeClock::new(1_800_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open(
        StoreConfig::new(tmp.path.join("sqlite")),
        StoreDeps {
            clock: clock.clone(),
            ids: ids.clone(),
        },
        &[],
    )
    .await
    .unwrap();
    let repo = Arc::new(open_repo(&store, &tmp.path, clock.clone(), ids.clone()).await);
    let ctx = context(&ids);
    let project = ids.next().unwrap();
    repo.ensure_mirror(&ctx, &project, &RepoSlug::parse("fixture/local").unwrap())
        .await
        .unwrap();
    let mirror = tmp.path.join("repo/mirrors/fixture/local.git");
    let upstream = tmp.path.join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    git(
        &upstream,
        &["init", "-q", "-b", "main", "--template=/dev/null"],
    );
    std::fs::write(upstream.join("README"), "first\n").unwrap();
    git(&upstream, &["add", "README"]);
    git(&upstream, &["commit", "-q", "-m", "first"]);
    let first = GitOid::parse(git(&upstream, &["rev-parse", "HEAD"]).trim()).unwrap();
    std::fs::write(upstream.join("README"), "second\n").unwrap();
    git(&upstream, &["add", "README"]);
    git(&upstream, &["commit", "-q", "-m", "second"]);
    let second = GitOid::parse(git(&upstream, &["rev-parse", "HEAD"]).trim()).unwrap();
    git(&upstream, &["tag", "-a", "annotated", "-m", "fixture"]);
    git(
        &upstream,
        &[
            "push",
            "-q",
            mirror.to_str().unwrap(),
            "HEAD:refs/mirror/heads/main",
            "HEAD:refs/mirror/heads/feature/x",
            "HEAD:refs/mirror/heads/topic/a\u{a0}b",
            "refs/tags/annotated:refs/tags/annotated",
            &format!("{first}:refs/heads/local-only"),
        ],
    );
    let forge = Arc::new(FakeForge::new(Timestamp::from_millis(1_800_000_000_000)));
    let fx = PortEffects::new(
        AttemptPorts {
            repo: repo.clone(),
            sandbox: Arc::new(FakeSandboxService::new()),
            policy: Arc::new(FakePolicy::new_in(store.clone())),
            forge: forge.clone(),
            cas: Arc::new(store.cas()),
            adapter: Arc::new(StubWorkerAdapter::new()),
            clock: clock.clone(),
            entropy: Arc::new(SeqEntropy::new(99)),
        },
        forge,
        engine_cfg::runner_config(),
        Duration::from_secs(60),
        Arc::new(|_, _, _| {}),
    );
    let engine = Engine::new(store.clone(), Arc::new(FakeInbox::new_in(store.clone())));
    let budget = budget_from_level(&LevelEntry {
        graph_template: "low.single".into(),
        budget: LevelBudget {
            tokens_by_tier: TierBudget {
                utility: 100,
                planner: 100,
                worker: 100,
            },
            nodes: 8,
            rounds: 0,
            wall_time_s: 3600,
            concurrency: 1,
            forge_writes: 20,
            forge_writes_per_hour: 30,
        },
    });
    let delivery = ids.next().unwrap();
    engine
        .create_delivery(NewDelivery {
            id: delivery,
            project_id: project,
            title: "test".into(),
            user_text: "test".into(),
            level_cap: Level::Low,
            level_effective: Level::Low,
            origin: DeliveryOrigin::Conversation,
            source_message_id: ids.next().unwrap(),
            issues: vec![],
            budget,
            config_rev: Seq::new(1).unwrap(),
            actor: Actor::new(ActorKind::Engine, "test"),
        })
        .await
        .unwrap();
    let mut state = load_delivery_state(&store, &delivery)
        .await
        .unwrap()
        .unwrap();
    let describe_name = format!("missing-0-g{second}");
    assert_eq!(
        git(
            &mirror,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/mirror/heads/{describe_name}^{{commit}}")
            ]
        )
        .trim(),
        second.to_string()
    );
    assert_eq!(
        repo.resolve(&project, &RevSpec::new(second.to_string()))
            .await
            .unwrap(),
        second
    );
    assert_eq!(
        repo.resolve(&project, &RevSpec::new(format!("{second}^")))
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        repo.resolve(&project, &RevSpec::new("refs/tags/annotated"))
            .await
            .unwrap(),
        second
    );
    assert!(matches!(
        repo.resolve(&project, &RevSpec::new("refs/mirror/heads/main^"))
            .await,
        Err(RepoError::BaseNotInMirror { .. })
    ));
    let blob = git(
        &mirror,
        &[
            "hash-object",
            "-w",
            upstream.join("README").to_str().unwrap(),
        ],
    );
    git(
        &mirror,
        &["update-ref", "refs/mirror/heads/blob", blob.trim()],
    );
    assert!(matches!(
        repo.resolve(&project, &RevSpec::new("refs/mirror/heads/blob"))
            .await,
        Err(RepoError::BaseNotInMirror { .. })
    ));
    let refs_before = git(
        &mirror,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    for base in [
        "local-only",
        describe_name.as_str(),
        "main~1",
        "main^",
        "refs/heads/main",
        "..",
        "",
        "main\0x",
    ] {
        let result = fx
            .run(
                &state,
                Effect::EnsureWorktree {
                    branch: BranchName::new("gantry/test"),
                    base_ref: base.into(),
                },
                &ctx,
            )
            .await;
        assert!(
            matches!(result, Some(EffectReport::EffectFailed { .. })),
            "{base:?}"
        );
        if base != "local-only" && base != describe_name {
            assert!(matches!(
                result,
                Some(EffectReport::EffectFailed {
                    error: EffectErrorClass::Defect {
                        what: "default_branch_invalid"
                    },
                    ..
                })
            ));
        }
        assert!(events(&store, EventType::WorktreeCreated).await.is_empty());
        assert_eq!(
            git(
                &mirror,
                &["for-each-ref", "--format=%(refname) %(objectname)"]
            ),
            refs_before
        );
    }
    for (base, expected) in [
        ("main", second),
        ("feature/x", second),
        ("topic/a\u{a0}b", second),
    ] {
        state.delivery.id = ids.next().unwrap();
        let result = fx
            .run(
                &state,
                Effect::EnsureWorktree {
                    branch: BranchName::new("gantry/test"),
                    base_ref: base.into(),
                },
                &ctx,
            )
            .await;
        assert!(matches!(result, Some(EffectReport::WorktreeReady { .. })));
        let created = events(&store, EventType::WorktreeCreated).await;
        let data = &created.last().unwrap().data;
        assert_eq!(data["base_ref"], base);
        assert_eq!(data["base_sha"], expected.to_string());
        if base == "main" {
            git(
                &upstream,
                &[
                    "push",
                    "-q",
                    mirror.to_str().unwrap(),
                    &format!("{first}:refs/heads/main"),
                ],
            );
        }
    }
    // 本地 main 指向另一个提交时，新交付单元仍取镜像分支。
    state.delivery.id = ids.next().unwrap();
    assert!(matches!(
        fx.run(
            &state,
            Effect::EnsureWorktree {
                branch: BranchName::new("gantry/test"),
                base_ref: "main".into()
            },
            &ctx
        )
        .await,
        Some(EffectReport::WorktreeReady { .. })
    ));
    let created = events(&store, EventType::WorktreeCreated).await;
    assert_eq!(created.last().unwrap().data["base_sha"], second.to_string());
}

#[tokio::test]
async fn real_git_sqlite_patch_commit_and_reopen() {
    let tmp = TempDir::new();
    let clock = Arc::new(FakeClock::new(1_800_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store_dir = tmp.path.join("sqlite");
    let deps = || StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    };
    let store = Store::open(StoreConfig::new(&store_dir), deps(), &[])
        .await
        .unwrap();
    let repo = open_repo(&store, &tmp.path, clock.clone(), ids.clone()).await;
    let ctx = context(&ids);
    let project = ids.next().unwrap();
    let delivery = ids.next().unwrap();
    let slug = RepoSlug::parse("fixture/local").unwrap();
    repo.ensure_mirror(&ctx, &project, &slug).await.unwrap();
    let mirror = tmp.path.join("repo/mirrors/fixture/local.git");
    let upstream = tmp.path.join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.join("README.md"), "hello\n").unwrap();
    git(&upstream, &["add", "README.md"]);
    git(&upstream, &["commit", "-q", "-m", "fixture base"]);
    let base = GitOid::parse(git(&upstream, &["rev-parse", "HEAD"]).trim()).unwrap();
    git(
        &upstream,
        &[
            "push",
            "-q",
            mirror.to_str().unwrap(),
            "HEAD:refs/mirror/heads/main",
        ],
    );

    // 显式 base OID 只证明本地 Git；不声称 resolve(main) 能解析镜像 ref。
    let create = CreateWorktree {
        project_id: project,
        delivery_id: delivery,
        branch: BranchName::new("gantry/fixture"),
        base_ref: "main".into(),
        base_sha: base,
    };
    let wt = repo.create_worktree(&ctx, create.clone()).await.unwrap();
    assert_eq!(repo.create_worktree(&ctx, create).await.unwrap().id, wt.id);
    let lock = repo
        .acquire_lock(
            &ctx,
            &wt.id,
            LockHolder::Engine,
            &delivery,
            Duration::from_secs(900),
        )
        .await
        .unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let rev = tx.read().config_rev(None);
    tx.append(NewEvent {
        ty: EventType::SettingsUpdated, subject: Subject::parse("settings").unwrap(), project_id: None,
        actor: Actor::new(ActorKind::User, "test"), causation_id: None, correlation_id: None,
        data: json!({"scope":"global", "resource":"github", "schema_version":1,
            "base_rev":rev.as_u64(), "value":{"commit_identity":{"name":"Fixture Bot","email":"bot@example.invalid"}},
            "relaxes":[], "secret_changes":[]}), dedup_key: None,
    }).await.unwrap();
    tx.commit().await.unwrap();

    std::fs::write(upstream.join("README.md"), "hello\nworld\n").unwrap();
    git(&upstream, &["add", "README.md"]);
    let patch = git(
        &upstream,
        &["diff", "--cached", "--full-index", "--no-renames", "HEAD"],
    );
    let tree = GitOid::parse(git(&upstream, &["write-tree"]).trim()).unwrap();
    let digest = store
        .cas()
        .put(patch.as_bytes(), gantry_store::CasKind::Artifact)
        .await
        .unwrap();
    assert_eq!(digest, sha256(patch.as_bytes()));
    let job = ids.next().unwrap();
    let req = ValidatePatch {
        project_id: project,
        delivery_id: delivery,
        job_id: job,
        worktree_id: Some(wt.id),
        base_sha: base,
        patch: digest,
        declared: DeclaredPatch {
            sha256: digest,
            bytes: patch.len() as u64,
            base_sha: base,
            files_changed: 1,
        },
        job_paths: PathConstraints {
            allow: vec!["**".into()],
            deny: vec![],
            approval_required: vec![],
        },
    };
    let before = git(
        &mirror,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    let outcome = repo.validate_patch(&ctx, req.clone()).await.unwrap();
    assert!(
        matches!(&outcome, PatchOutcome::Validated(p) if p.verdict == PatchVerdict::Allow && p.result_tree == Some(tree) && p.sha256 == digest)
    );
    assert_eq!(
        repo.validate_patch(&ctx, req.clone()).await.unwrap(),
        outcome
    );
    assert_eq!(
        git(
            &mirror,
            &["for-each-ref", "--format=%(refname) %(objectname)"]
        ),
        before
    );

    // 有效 diff 格式但上下文不匹配，不能应用且必须隔离留存。
    let bad = patch.replace(" hello", " wrong");
    assert_ne!(bad, patch);
    let bad_digest = store
        .cas()
        .put(bad.as_bytes(), gantry_store::CasKind::Artifact)
        .await
        .unwrap();
    let bad_job = ids.next().unwrap();
    let bad_req = ValidatePatch {
        job_id: bad_job,
        patch: bad_digest,
        declared: DeclaredPatch {
            sha256: bad_digest,
            bytes: bad.len() as u64,
            base_sha: base,
            files_changed: 1,
        },
        ..req.clone()
    };
    let rejection = repo.validate_patch(&ctx, bad_req.clone()).await.unwrap();
    assert!(matches!(
        rejection,
        PatchOutcome::Rejected {
            reason: PatchRejectReason::NotApplicable,
            ..
        }
    ));
    assert_eq!(repo.validate_patch(&ctx, bad_req).await.unwrap(), rejection);
    assert!(
        store
            .cas()
            .stat(&bad_digest)
            .await
            .unwrap()
            .unwrap()
            .quarantined
    );
    assert_eq!(
        git(
            &mirror,
            &["for-each-ref", "--format=%(refname) %(objectname)"]
        ),
        before
    );

    let commit = CommitPatch {
        project_id: project,
        job_id: job,
        worktree_id: wt.id,
        lock,
        approval: None,
        message: CommitMessage {
            subject: "fixture patch".into(),
            body: String::new(),
        },
        timestamp: Timestamp::from_millis(1_800_000_000_000),
    };
    assert_eq!(
        repo.commit_patch(
            &ctx,
            CommitPatch {
                job_id: bad_job,
                ..commit.clone()
            }
        )
        .await,
        Err(RepoError::PatchRecordMissing)
    );
    assert_eq!(
        git(
            &mirror,
            &["for-each-ref", "--format=%(refname) %(objectname)"]
        ),
        before
    );
    let record = repo.commit_patch(&ctx, commit.clone()).await.unwrap();
    assert_eq!(
        (
            record.parent,
            record.tree_oid,
            record.patch_sha256,
            record.job_id,
            record.worktree_id
        ),
        (base, tree, digest, job, wt.id)
    );
    assert_eq!(
        repo.provenance(&project, &record.commit_oid).await.unwrap(),
        Some(record.clone())
    );
    assert_eq!(repo.provenance(&project, &base).await.unwrap(), None);
    let raw = git(
        &mirror,
        &["cat-file", "commit", &record.commit_oid.to_string()],
    );
    assert!(raw.contains(&format!("tree {tree}\nparent {base}\n")));
    assert!(raw.ends_with(&format!("Gantry-Attempt: {job}\n")));
    let refs = git(
        &mirror,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    assert!(refs.contains(&format!("refs/gantry/attempt/{job} {}", record.commit_oid)));
    assert!(refs.contains(&format!("refs/gantry/wt/{} {}", wt.id, record.commit_oid)));
    assert!(refs.contains(&format!("refs/mirror/heads/main {base}")));
    assert_eq!(
        git(
            &mirror,
            &["rev-list", "--count", &record.commit_oid.to_string()]
        )
        .trim(),
        "2"
    );
    let objects = git(&mirror, &["count-objects", "-v"]);
    assert_eq!(
        repo.commit_patch(&ctx, commit.clone()).await.unwrap(),
        record
    );
    assert_eq!(git(&mirror, &["count-objects", "-v"]), objects);
    assert_eq!(
        git(
            &mirror,
            &["for-each-ref", "--format=%(refname) %(objectname)"]
        ),
        refs
    );

    assert_eq!(events(&store, EventType::WorktreeCreated).await.len(), 1);
    assert_eq!(events(&store, EventType::WorktreeLocked).await.len(), 1);
    let validated = events(&store, EventType::PatchValidated).await;
    assert_eq!(validated.len(), 1);
    let data: PatchValidatedData = serde_json::from_value(validated[0].data.clone()).unwrap();
    assert_eq!(
        (data.job_id, data.base_sha, data.result_tree, data.sha256),
        (job, base, tree, digest)
    );
    let rejected = events(&store, EventType::PatchRejected).await;
    assert_eq!(rejected.len(), 1);
    let data: PatchRejectedData = serde_json::from_value(rejected[0].data.clone()).unwrap();
    assert_eq!(
        (data.job_id, data.sha256, data.reason),
        (bad_job, bad_digest, PatchRejectReason::NotApplicable)
    );

    // 释放 repo、policy 与 CAS 的全部 SQLite 持有者，再从同一磁盘目录打开。
    drop(repo);
    drop(store);
    let store = Store::open(StoreConfig::new(&store_dir), deps(), &[])
        .await
        .unwrap();
    assert_eq!(
        store
            .cas()
            .get_bytes(&digest, patch.len())
            .await
            .unwrap()
            .unwrap(),
        patch.as_bytes()
    );
    assert!(
        store
            .cas()
            .stat(&bad_digest)
            .await
            .unwrap()
            .unwrap()
            .quarantined
    );
    let repo = open_repo(&store, &tmp.path, clock.clone(), ids.clone()).await;
    assert_eq!(repo.get_worktree(&wt.id).await.unwrap().unwrap().id, wt.id);
    assert_eq!(repo.branch_head(&wt.id).await.unwrap(), record.commit_oid);
    assert_eq!(
        repo.provenance(&project, &record.commit_oid).await.unwrap(),
        Some(record.clone())
    );
    assert_eq!(repo.validate_patch(&ctx, req).await.unwrap(), outcome);
    assert_eq!(repo.commit_patch(&ctx, commit).await.unwrap(), record);
    assert_eq!(git(&mirror, &["count-objects", "-v"]), objects);
    assert_eq!(
        git(
            &mirror,
            &["for-each-ref", "--format=%(refname) %(objectname)"]
        ),
        refs
    );
    assert_eq!(events(&store, EventType::WorktreeCreated).await.len(), 1);
    assert_eq!(events(&store, EventType::PatchValidated).await.len(), 1);
    assert_eq!(events(&store, EventType::PatchRejected).await.len(), 1);
}
