//! 本地 GitRepo + SQLite 联测；无生产装配或外网，Egress 替身监听本地回环。
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

#[path = "support/engine_cfg.rs"]
#[allow(dead_code)]
pub(crate) mod engine_cfg;

use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::entities::{
    Actor, ActorKind, AttemptStatus, Exec, FailureClass, LockHolder, Role, Seq,
};
use gantry_core::entities::{SandboxLimits, SandboxTrust, TokenWeights};
use gantry_core::events::{EventType, NewEvent, PatchRejectedData, PatchValidatedData, Subject};
use gantry_core::ids::{Clock, IdGen};
use gantry_core::ports::sandbox::{
    ArtifactName, EgressRequest, ExecSpec, GroupName, ProfileName, SandboxService, SandboxSpec,
    TerminalCause, Timers, TokenEnvSpec, WorkspaceMode, WorkspaceRequest,
};
use gantry_core::ports::{
    ActorCtx, BranchName, CommitMessage, CommitPatch, CreateWorktree, DeclaredPatch, PatchOutcome,
    PolicyPort, RepoError, RepoService, RevSpec, ValidatePatch,
};
use gantry_core::settings::resource::{
    ArtifactIo, MaterialIo, ProgressIo, SandboxDriver, SandboxIo,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::JobSpec;
use gantry_core::worker::{
    BundleFileName, FileArtifact, PatchRejectReason, PatchVerdict, PathConstraints, ResultBundle,
};
use gantry_core::{
    entities::DeliveryOrigin,
    level::Level,
    settings::resource::{LevelBudget, LevelEntry, TierBudget},
};
use gantry_engine::{
    actor::EffectRunner,
    attempt::{sandbox_id_for, AttemptContext, AttemptPorts, AttemptRunner},
    create::{budget_from_level, NewDelivery},
    decide::{
        AttemptPlan, Effect, EffectErrorClass, EffectReport, PatchResult, ResolvedModel,
        TrustedInstructions,
    },
    engine::{load_delivery_state, Engine},
    PortEffects,
};
use gantry_fakes::egress::{EgressCall, EgressStub, RevokeFault};
use gantry_fakes::policy::FakePolicy;
use gantry_fakes::repo::FakeRepo;
use gantry_fakes::{
    forge::FakeForge, inbox::FakeInbox, sandbox::FakeSandboxService, worker::StubWorkerAdapter,
};
use gantry_policy::{PolicyDeps, PolicyEngine, POLICY_MODULE};
use gantry_repo::{GitRepo, RepoConfig, RepoDeps};
use gantry_sandbox::fake::{DriverCall, FakeBehavior, FakeDriver, FakeOutEntry, FakeScript};
use gantry_sandbox::service::seams::StaticSchemas;
use gantry_sandbox::service::{SandboxConfig, SandboxDeps, SandboxManager};
use gantry_sandbox::spec::{NetworkAttach, Runtime};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{EventFilter, Store, StoreConfig, StoreDeps, TypePattern};
use serde_json::json;

pub(crate) struct TempDir {
    pub(crate) path: PathBuf,
    dev: u64,
    ino: u64,
}

impl TempDir {
    pub(crate) fn new() -> Self {
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
                gantry_sandbox::stage::unseal(&self.path.join("sandboxes"));
                if let Err(error) = std::fs::remove_dir_all(&self.path) {
                    if std::thread::panicking() {
                        eprintln!(
                            "failed to remove owned test directory {:?}: {error}",
                            self.path
                        );
                    } else {
                        panic!("remove owned test directory {:?}: {error}", self.path);
                    }
                }
            }
        }
    }
}

#[test]
fn temp_dir_cleans_sealed_sandbox_during_unwind() {
    let tmp = TempDir::new();
    let path = tmp.path.clone();
    let inputs = path.join("sandboxes/job/inputs");
    std::fs::create_dir_all(&inputs).unwrap();
    std::fs::write(inputs.join("job.json"), b"fixture").unwrap();
    std::fs::set_permissions(&inputs, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = std::panic::catch_unwind(move || {
        let _tmp = tmp;
        panic!("exercise cleanup during unwind");
    });
    assert!(result.is_err());
    assert!(!path.exists());
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

pub(crate) fn context(ids: &IdGen) -> ActorCtx {
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
    open_repo_with_policy(
        store,
        dir,
        clock,
        ids,
        Arc::new(FakePolicy::new_in(store.clone())),
    )
    .await
}

async fn open_repo_with_policy(
    store: &Store,
    dir: &Path,
    clock: Arc<FakeClock>,
    ids: Arc<IdGen>,
    policy: Arc<dyn PolicyPort>,
) -> GitRepo {
    let mut cfg = RepoConfig::new(dir.join("repo"));
    cfg.git_bin = Some(PathBuf::from("/usr/bin/git"));
    GitRepo::open(
        cfg,
        RepoDeps {
            store: store.clone(),
            clock,
            ids,
            policy,
            cas: Arc::new(store.cas()),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn attempt_real_repo_and_policy_commit_or_deny() {
    for path in ["README.md", ".github/workflows/ci.yml"] {
        let tmp = TempDir::new();
        let clock = Arc::new(FakeClock::new(1_800_000_000_000));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
        let store = Store::open(
            StoreConfig::new(tmp.path.join("sqlite")),
            StoreDeps {
                clock: clock.clone(),
                ids: ids.clone(),
            },
            &[POLICY_MODULE],
        )
        .await
        .unwrap();
        let project = ids.next().unwrap();
        let delivery = ids.next().unwrap();
        let ctx = context(&ids);
        let slug = RepoSlug::parse("fixture/local").unwrap();
        let forge = Arc::new(FakeForge::new_in(store.clone()));
        let now = Timestamp::from_millis(1_800_000_000_000);
        forge.insert_project(gantry_core::entities::Project {
            id: project,
            repo: slug.clone(),
            installation_id: 42,
            bot_user_id: 9,
            default_branch: "main".into(),
            allowed_merge_methods: vec![gantry_core::entities::MergeMethod::Merge],
            merge_queue_enabled: false,
            created_at: now,
            updated_at: now,
        });
        let policy = Arc::new(PolicyEngine::new(PolicyDeps {
            store: store.clone(),
            forge: forge.clone(),
            clock: clock.clone(),
            ids: ids.clone(),
        }));
        let repo = Arc::new(
            open_repo_with_policy(
                &store,
                &tmp.path,
                clock.clone(),
                ids.clone(),
                policy.clone(),
            )
            .await,
        );
        repo.ensure_mirror(&ctx, &project, &slug).await.unwrap();
        let mirror = tmp.path.join("repo/mirrors/fixture/local.git");
        let upstream = tmp.path.join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        git(
            &upstream,
            &["init", "-q", "-b", "main", "--template=/dev/null"],
        );
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
        let wt = repo
            .create_worktree(
                &ctx,
                CreateWorktree {
                    project_id: project,
                    delivery_id: delivery,
                    branch: BranchName::new("gantry/fixture"),
                    base_ref: "main".into(),
                    base_sha: base,
                },
            )
            .await
            .unwrap();
        let lock = repo
            .acquire_lock(
                &ctx,
                &wt.id,
                LockHolder::Engine,
                &delivery,
                Duration::from_secs(3600),
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

        if let Some(parent) = upstream.join(path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(upstream.join(path), "hello\nworld\n").unwrap();
        git(&upstream, &["add", path]);
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
        let attempt = ids.next().unwrap();
        let sandbox_id = sandbox_id_for(&attempt);
        let sandbox = Arc::new(FakeSandboxService::new());
        let mut cfg = engine_cfg::runtime_settings().runner;
        cfg.repo = slug;
        let ports = AttemptPorts {
            repo: repo.clone(),
            sandbox: sandbox.clone(),
            policy,
            forge,
            cas: Arc::new(store.cas()),
            adapter: Arc::new(StubWorkerAdapter::new()),
            clock: clock.clone(),
            entropy: Arc::new(SeqEntropy::new(99)),
        };
        let cfg = Arc::new(cfg);
        let runner = AttemptRunner::new(ports.clone(), cfg.clone());
        let preview = AttemptRunner::new(
            AttemptPorts {
                entropy: Arc::new(SeqEntropy::new(99)),
                ..ports
            },
            cfg,
        );
        let plan = AttemptPlan {
            attempt_id: attempt,
            node_id: ids.next().unwrap(),
            workflow_id: ids.next().unwrap(),
            role: Role::Single,
            exec: Exec::Sandbox,
            critical: false,
            n: 1,
            base_sha: base,
            pr_base_sha: None,
            tokens_total: 100_000,
            wall_time_s: 900,
            model: ResolvedModel {
                provider_id: "anthropic".into(),
                model: "claude-test".into(),
                reasoning_effort: gantry_core::level::Effort::Medium,
                attempt_wall_s: 900,
            },
            upstream: vec![],
            previous: vec![],
            instructions: TrustedInstructions::compose("single", "fixture"),
            refs: vec![],
            lock: Some(lock.clone()),
            resume: false,
        };
        let cx = AttemptContext {
            project_id: project,
            delivery_id: delivery,
            level_effective: Level::Low,
            graph_rev: 1,
            revision_round: 0,
            required: true,
            upstream: vec![],
        };
        // 无 staging 的替身独立声明一个固定的服务端摘要。
        let job = preview.assemble(&plan, &cx, &ctx).await.unwrap().job;
        let job_sha256 = sha256(b"job.json");
        let result = serde_json::to_vec(&json!({
            "protocol": "gantry-worker/1", "job_id": attempt, "job_sha256": job_sha256,
            "status": "succeeded", "summary": "fixture", "error": null,
            "structured_output": {"kind":"build", "changes_summary":"fixed"},
            "artifacts": {"patch": {"path":"changes.patch", "sha256":digest,
                "bytes":patch.len(), "base_sha":base, "files_changed":1}, "report":null, "proposals":null},
            "flags": {"suspected_injection":[]},
            "usage_reported": {"input_tokens":0, "output_tokens":0, "cache_read_tokens":0,
                "cache_write_tokens":0, "turns":1, "tool_calls":1},
            "agent": {"kind":"claude-code", "version":"1.0.0", "model":"claude-test"},
            "started_at":now, "ended_at":now,
        })).unwrap();
        let result_digest = store
            .cas()
            .put(&result, gantry_store::CasKind::Artifact)
            .await
            .unwrap();
        let mut outcome = FakeSandboxService::success_outcome(attempt, sandbox_id);
        outcome.files.insert(
            ArtifactName::new("changes.patch"),
            FileArtifact {
                sha256: digest,
                bytes: patch.len() as u64,
            },
        );
        outcome.files.insert(
            ArtifactName::new("result.json"),
            FileArtifact {
                sha256: result_digest,
                bytes: result.len() as u64,
            },
        );
        sandbox.script_outcome(attempt, sandbox_id, outcome);
        let finalized = runner.run(&plan, &cx, &ctx).await.unwrap();
        let submitted_job = sandbox
            .calls()
            .into_iter()
            .find_map(|call| match call {
                gantry_fakes::sandbox::SandboxCall::Submit { spec, .. } => Some(spec.job),
                _ => None,
            })
            .unwrap();
        assert_eq!(job, submitted_job);
        let bundle_bytes = store
            .cas()
            .get_bytes(&finalized.result_ref.unwrap(), 1 << 20)
            .await
            .unwrap()
            .unwrap();
        let bundle: ResultBundle = serde_json::from_slice(&bundle_bytes).unwrap();
        bundle.validate().unwrap();
        assert_eq!(bundle.job_id, attempt);
        assert_eq!(bundle.job_sha256, job_sha256);
        assert_eq!(bundle.files[&BundleFileName::ChangesPatch].sha256, digest);
        assert_eq!(
            bundle.files[&BundleFileName::ResultJson].sha256,
            result_digest
        );
        assert_eq!(
            store
                .cas()
                .get_bytes(&digest, patch.len())
                .await
                .unwrap()
                .unwrap(),
            patch.as_bytes()
        );
        assert_eq!(
            store
                .cas()
                .get_bytes(&result_digest, result.len())
                .await
                .unwrap()
                .unwrap(),
            result
        );
        assert_eq!(bundle.classification.failure_class, finalized.failure_class);
        assert_eq!(
            bundle.classification.failure_detail,
            finalized.failure_detail
        );
        let validation = bundle.validation.unwrap().patch.unwrap();
        assert_eq!(validation.sha256, digest);
        assert_eq!(validation.files, 1);
        if path == "README.md" {
            assert_eq!(finalized.status, AttemptStatus::Succeeded);
            let PatchResult::Committed {
                commit,
                tree: result_tree,
                sha256: result_digest,
            } = finalized.patch
            else {
                panic!("expected committed patch")
            };
            assert_eq!((result_tree, result_digest), (tree, digest));
            assert_eq!(
                (validation.verdict, validation.result_tree),
                (PatchVerdict::Allow, Some(tree))
            );
            assert_eq!(repo.branch_head(&wt.id).await.unwrap(), commit);
            let record = repo.provenance(&project, &commit).await.unwrap().unwrap();
            assert_eq!(
                (
                    record.parent,
                    record.tree_oid,
                    record.patch_sha256,
                    record.job_id,
                    record.worktree_id
                ),
                (base, tree, digest, attempt, wt.id)
            );
            let raw = git(&mirror, &["cat-file", "commit", &commit.to_string()]);
            assert!(raw.contains(&format!("tree {tree}\nparent {base}\n")));
            assert!(raw.ends_with(&format!("Gantry-Attempt: {attempt}\n")));
        } else {
            assert_eq!(finalized.status, AttemptStatus::Failed);
            assert_eq!(finalized.failure_class, Some(FailureClass::PolicyDenied));
            assert_eq!(
                finalized.failure_detail.unwrap().as_str(),
                "policy.path_protected:workflows"
            );
            let PatchResult::Denied { patch: denied, .. } = finalized.patch else {
                panic!("expected denied patch")
            };
            assert_eq!(
                (denied.reference, denied.paths),
                (digest, vec![path.to_owned()])
            );
            assert_eq!(
                (validation.verdict, validation.result_tree),
                (PatchVerdict::Deny, None)
            );
            assert!(
                store
                    .cas()
                    .stat(&digest)
                    .await
                    .unwrap()
                    .unwrap()
                    .quarantined
            );
            assert_eq!(repo.branch_head(&wt.id).await.unwrap(), base);
            let refs = git(
                &mirror,
                &["for-each-ref", "--format=%(refname) %(objectname)"],
            );
            assert!(!refs.contains(&format!("refs/gantry/attempt/{attempt}")));
            let denied_events = events(&store, EventType::PolicyDenied).await;
            let rejected_events = events(&store, EventType::PatchRejected).await;
            assert_eq!((denied_events.len(), rejected_events.len()), (1, 1));
            assert_eq!(rejected_events[0].causation_id, Some(denied_events[0].id));
            let rejected: PatchRejectedData =
                serde_json::from_value(rejected_events[0].data.clone()).unwrap();
            assert_eq!(
                (rejected.job_id, rejected.sha256, rejected.reason),
                (attempt, digest, PatchRejectReason::PolicyDenied)
            );
            assert!(rejected
                .reasons
                .iter()
                .any(|r| r.code.as_str() == "policy.path_protected:workflows"));
        }
        sandbox.release(&ctx, &attempt).await.unwrap();
        repo.release_lock(&ctx, &lock).await.unwrap();
    }
}

#[tokio::test]
async fn sandbox_artifact_reaches_real_repo_through_sqlite_cas() {
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
    let delivery = ids.next().unwrap();
    let job_id = ids.next().unwrap();
    let sandbox_id = ids.next().unwrap();
    let slug = RepoSlug::parse("fixture/local").unwrap();
    repo.ensure_mirror(&ctx, &project, &slug).await.unwrap();
    let mirror = tmp.path.join("repo/mirrors/fixture/local.git");
    let upstream = tmp.path.join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    git(
        &upstream,
        &["init", "-q", "-b", "main", "--template=/dev/null"],
    );
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
    let wt = repo
        .create_worktree(
            &ctx,
            CreateWorktree {
                project_id: project,
                delivery_id: delivery,
                branch: BranchName::new("gantry/fixture"),
                base_ref: "main".into(),
                base_sha: base,
            },
        )
        .await
        .unwrap();
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

    const SCHEMA: &[u8] = br#"{"type":"object"}"#;
    let mut job: JobSpec = serde_json::from_str(include_str!(
        "../../gantry-core/fixtures/protocol/worker/job_spec.json"
    ))
    .unwrap();
    job.job_id = job_id;
    job.project_id = project;
    job.delivery_id = delivery;
    job.workspace.repo = slug.clone();
    job.workspace.base_sha = base;
    job.output.structured_schema.sha256 = sha256(SCHEMA);
    job.materials.clear();
    job.upstream.clear();
    job.constraints.paths = PathConstraints {
        allow: vec!["**".into()],
        deny: vec![],
        approval_required: vec![],
    };
    let schemas = BTreeMap::from([(job.output.structured_schema.id.clone(), SCHEMA.to_vec())]);
    let paths = job.constraints.paths.clone();
    let workspace_dir = tmp.path.join("sandboxes");
    std::fs::create_dir(&workspace_dir).unwrap();
    let driver = Arc::new(FakeDriver::new(&workspace_dir));
    driver.script(
        sandbox_id,
        FakeScript {
            out: vec![(
                "changes.patch".into(),
                FakeOutEntry::File(patch.as_bytes().to_vec()),
            )],
            ..FakeScript::default()
        },
    );
    // 固定测试 schema 不是生产注册表（#319）；本切片也不执行引擎结果校验。
    let sandbox = SandboxManager::new(
        SandboxDeps {
            driver: driver.clone(),
            repo: repo.clone(),
            egress: Arc::new(EgressStub::with_default_config()),
            store: store.clone(),
            clock: clock.clone(),
            artifacts: Arc::new(store.cas()),
            schemas: Arc::new(StaticSchemas(schemas)),
        },
        SandboxConfig {
            workspaces_dir: workspace_dir.clone(),
            runtime: Runtime::Runc,
            max_concurrent_sandboxes: 1,
            network: NetworkAttach {
                network: "gantry-sbx".into(),
                add_hosts: Vec::new(),
            },
            log_max_bytes: 16 << 20,
            out_quota_bytes: 12 << 20,
            tmpfs_home_mb: 256,
            tmpfs_tmp_mb: 512,
            progress_sample: Duration::from_secs(1),
            log_drain_timeout: Duration::from_secs(5),
            orphan_grace: Duration::ZERO,
        },
    );
    let spec = SandboxSpec {
        job_id,
        sandbox_id,
        project_id: project,
        delivery_id: Some(delivery),
        conversation_id: None,
        workspace: WorkspaceRequest {
            repo: slug,
            base_sha: base,
            pr_base_sha: None,
            mode: WorkspaceMode::Rw,
        },
        job,
        raw_materials: Vec::new(),
        upstream: Vec::new(),
        exec: ExecSpec {
            image_digest: sha256(b"worker-image"),
            argv: vec!["gantry-worker".into(), "run".into()],
            env: BTreeMap::from([
                ("HOME".into(), "/gantry/home".into()),
                ("ANTHROPIC_API_KEY".into(), "gantry-egress-managed".into()),
            ]),
            token_env: TokenEnvSpec {
                model_base_url: BTreeMap::from([("anthropic".into(), "ANTHROPIC_BASE_URL".into())]),
                proxy: vec!["HTTPS_PROXY".into()],
            },
        },
        driver: SandboxDriver::Podman,
        trust: SandboxTrust::Untrusted,
        limits: SandboxLimits {
            cpu_cores: 1.0,
            mem_mb: 512,
            disk_mb: 512,
            pids: 64,
            wall_time_s: 600,
        },
        timers: Timers {
            idle_timeout_s: 120,
            kill_grace_s: 10,
        },
        io: SandboxIo {
            progress: ProgressIo {
                max_line_bytes: 8192,
                max_lines: 20_000,
                max_rate_per_s: 20,
            },
            artifacts: ArtifactIo {
                result_max_bytes: 1 << 20,
                report_max_bytes: 1 << 18,
                proposals_max_bytes: 1 << 18,
                proposals_max_count: 20,
            },
            materials: MaterialIo {
                max_item_bytes: 64 * 1024,
                max_total_bytes: 512 * 1024,
            },
            logs_tail_bytes: 1 << 20,
        },
        egress: EgressRequest {
            profile: ProfileName::new("default"),
            groups: vec![GroupName::new("model_api")],
            tokens_total: 100_000,
            weights: TokenWeights::default(),
        },
        config_rev: Seq::from_wire(7).unwrap(),
    };
    sandbox.submit(&ctx, spec).await.unwrap();
    let outcome = sandbox.wait(&job_id).await.unwrap();
    assert_eq!((outcome.job_id, outcome.sandbox_id), (job_id, sandbox_id));
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
    assert_eq!(outcome.terminal.exit_code, Some(0));
    assert!(!outcome.partial && outcome.unexpected.is_empty());
    assert_eq!(outcome.files.len(), 1);
    let artifact = &outcome.files[&ArtifactName::new("changes.patch")];
    assert_eq!(
        (artifact.sha256, artifact.bytes),
        (sha256(patch.as_bytes()), patch.len() as u64)
    );
    let stat = store.cas().stat(&artifact.sha256).await.unwrap().unwrap();
    assert_eq!(
        (stat.kind, stat.bytes, stat.quarantined),
        (gantry_store::CasKind::Artifact, artifact.bytes, false)
    );
    assert_eq!(
        store
            .cas()
            .get_bytes(&artifact.sha256, patch.len())
            .await
            .unwrap()
            .unwrap(),
        patch.as_bytes()
    );
    let digest = artifact.sha256;
    let validated = repo
        .validate_patch(
            &ctx,
            ValidatePatch {
                project_id: project,
                delivery_id: delivery,
                job_id,
                worktree_id: Some(wt.id),
                base_sha: base,
                patch: digest,
                declared: DeclaredPatch {
                    sha256: digest,
                    bytes: artifact.bytes,
                    base_sha: base,
                    files_changed: 1,
                },
                job_paths: paths,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(validated, PatchOutcome::Validated(p) if p.verdict == PatchVerdict::Allow && p.result_tree == Some(tree) && p.sha256 == digest)
    );
    let record = repo
        .commit_patch(
            &ctx,
            CommitPatch {
                project_id: project,
                job_id,
                worktree_id: wt.id,
                lock: lock.clone(),
                approval: None,
                message: CommitMessage {
                    subject: "fixture patch".into(),
                    body: String::new(),
                },
                timestamp: Timestamp::from_millis(1_800_000_000_000),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (
            record.parent,
            record.tree_oid,
            record.patch_sha256,
            record.job_id,
            record.worktree_id
        ),
        (base, tree, digest, job_id, wt.id)
    );
    assert_eq!(repo.branch_head(&wt.id).await.unwrap(), record.commit_oid);
    sandbox.release(&ctx, &job_id).await.unwrap();
    repo.release_lock(&ctx, &lock).await.unwrap();
    assert_eq!(driver.container_count(), 0);
    assert_eq!(
        driver.reads(),
        vec![workspace_dir.join(sandbox_id.to_string()).join("out")]
    );
    assert!(!workspace_dir.join(sandbox_id.to_string()).exists());
}

#[tokio::test]
async fn staged_job_digest_flows_through_sandbox_and_runner() {
    let tmp = TempDir::new();
    let clock = Arc::new(FakeClock::new(1_800_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(
        StoreDeps {
            clock: clock.clone(),
            ids: ids.clone(),
        },
        &[],
    )
    .await
    .unwrap();
    let project = ids.next().unwrap();
    let delivery = ids.next().unwrap();
    let base = GitOid::parse(engine_cfg::BASE).unwrap();
    let ctx = context(&ids);
    let repo = Arc::new(FakeRepo::new(clock.now()));
    repo.insert_commit(
        project,
        base,
        gantry_core::ports::repo::CommitInfo {
            parents: vec![],
            tree: GitOid::parse("1111111111111111111111111111111111111111").unwrap(),
            trailers: vec![],
        },
    );
    let policy = Arc::new(FakePolicy::new(clock.now()));
    let forge = Arc::new(FakeForge::new(clock.now()));
    let egress = Arc::new(EgressStub::with_default_config());
    let workspaces = tmp.path.join("sandboxes");
    std::fs::create_dir(&workspaces).unwrap();
    let driver = Arc::new(FakeDriver::new(&workspaces));
    const SCHEMA: &[u8] = br#"{"type":"object"}"#;
    let mut config = engine_cfg::runtime_settings().runner;
    config
        .roles
        .get_mut(&Role::Single)
        .unwrap()
        .structured_schema
        .sha256 = sha256(SCHEMA);
    let sandbox = Arc::new(SandboxManager::new(
        SandboxDeps {
            driver: driver.clone(),
            repo: repo.clone(),
            egress: egress.clone(),
            store: store.clone(),
            clock: clock.clone(),
            artifacts: Arc::new(store.cas()),
            schemas: Arc::new(StaticSchemas(BTreeMap::from([(
                "gantry.worker.output.build/1".into(),
                SCHEMA.to_vec(),
            )]))),
        },
        SandboxConfig {
            workspaces_dir: workspaces.clone(),
            runtime: Runtime::Runc,
            max_concurrent_sandboxes: 1,
            network: NetworkAttach {
                network: "gantry-sbx".into(),
                add_hosts: vec![],
            },
            log_max_bytes: 16 << 20,
            out_quota_bytes: 12 << 20,
            tmpfs_home_mb: 256,
            tmpfs_tmp_mb: 512,
            progress_sample: Duration::from_secs(1),
            log_drain_timeout: Duration::from_secs(5),
            orphan_grace: Duration::ZERO,
        },
    ));
    let runner = AttemptRunner::new(
        AttemptPorts {
            sandbox: sandbox.clone(),
            repo: repo.clone(),
            policy: policy.clone(),
            forge,
            cas: Arc::new(store.cas()),
            adapter: Arc::new(StubWorkerAdapter::new()),
            clock: clock.clone(),
            entropy: Arc::new(SeqEntropy::new(99)),
        },
        Arc::new(config),
    );
    let mut nonces = Vec::new();
    for wrong_declaration in [false, true] {
        let attempt = ids.next().unwrap();
        let sandbox_id = sandbox_id_for(&attempt);
        driver.script(
            sandbox_id,
            FakeScript {
                behavior: FakeBehavior::Forever { honor_term: true },
                ..FakeScript::default()
            },
        );
        let plan = AttemptPlan {
            attempt_id: attempt,
            node_id: ids.next().unwrap(),
            workflow_id: ids.next().unwrap(),
            role: Role::Single,
            exec: Exec::Sandbox,
            critical: false,
            n: 1,
            base_sha: base,
            pr_base_sha: None,
            tokens_total: 100_000,
            wall_time_s: 900,
            model: ResolvedModel {
                provider_id: "anthropic".into(),
                model: "claude-test".into(),
                reasoning_effort: gantry_core::level::Effort::Medium,
                attempt_wall_s: 900,
            },
            upstream: vec![],
            previous: vec![],
            instructions: TrustedInstructions::compose("single", "fixture"),
            refs: vec![],
            lock: None,
            resume: false,
        };
        let cx = AttemptContext {
            project_id: project,
            delivery_id: delivery,
            level_effective: Level::Low,
            graph_rev: 1,
            revision_round: 0,
            required: true,
            upstream: vec![],
        };
        let dir = workspaces.join(sandbox_id.to_string());
        let prior_starts = driver
            .calls()
            .iter()
            .filter(|call| matches!(call, DriverCall::Start(_)))
            .count();
        let worker = async {
            let id = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(id) = driver
                        .calls()
                        .into_iter()
                        .filter_map(|call| match call {
                            DriverCall::Start(id) => Some(id),
                            _ => None,
                        })
                        .nth(prior_starts)
                    {
                        if dir.join("job.json").exists() {
                            break id;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let bytes = std::fs::read(dir.join("job.json")).unwrap();
            let digest = sha256(&bytes);
            let job: JobSpec = serde_json::from_slice(&bytes).unwrap();
            let meta = gantry_sandbox::service::meta::Meta::load(&dir.join("meta.json"))
                .unwrap()
                .unwrap();
            assert_eq!(meta.job_sha256, Some(Some(digest)));
            let declared = if wrong_declaration {
                sha256(b"forged")
            } else {
                digest
            };
            let proposal = serde_json::to_vec(&json!({
                "protocol":"gantry-worker/1", "job_id":attempt, "proposals":[{
                    "id":"p1", "kind":"open_pr", "target":null,
                    "params":{"title":"bad", "body":"bad", "draft":false}, "rationale":"done"
                }]
            }))
            .unwrap();
            let patch = b"diff --git a/src/lib.rs b/src/lib.rs\n".to_vec();
            let result = serde_json::to_vec(&json!({
                "protocol":"gantry-worker/1", "job_id":attempt, "job_sha256":declared,
                "status":"succeeded", "summary":"fixture", "error":null,
                "structured_output":{"kind":"build", "changes_summary":"done"},
                "artifacts":{
                    "patch": if wrong_declaration { Some(json!({"path":"changes.patch", "sha256":sha256(&patch), "bytes":patch.len(), "base_sha":base, "files_changed":1})) } else { None },
                    "report":null,
                    "proposals": if wrong_declaration { Some(json!({"path":"proposals.json", "sha256":sha256(&proposal), "bytes":proposal.len()})) } else { None }
                },
                "flags":{"suspected_injection":[]},
                "usage_reported":{"input_tokens":0, "output_tokens":0, "cache_read_tokens":0,
                    "cache_write_tokens":0, "turns":1, "tool_calls":1},
                "agent":{"kind":"claude-code", "version":"1.0.0", "model":"claude-test"},
                "started_at":clock.now(), "ended_at":clock.now()
            })).unwrap();
            let mut out = vec![("result.json".into(), FakeOutEntry::File(result))];
            if wrong_declaration {
                out.push(("changes.patch".into(), FakeOutEntry::File(patch)));
                out.push(("proposals.json".into(), FakeOutEntry::File(proposal)));
            }
            driver.complete(&id, out).unwrap();
            (digest, job.isolation.nonce)
        };
        let (finalized, (digest, nonce)) = tokio::join!(runner.run(&plan, &cx, &ctx), worker);
        let finalized = finalized.unwrap();
        let outcome = sandbox.wait(&attempt).await.unwrap();
        assert_eq!(outcome.job_sha256, Some(digest));
        let bundle: ResultBundle = serde_json::from_slice(
            &store
                .cas()
                .get_bytes(&finalized.result_ref.unwrap(), 1 << 20)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bundle.job_sha256, digest);
        if wrong_declaration {
            assert_eq!(finalized.status, AttemptStatus::Failed);
            assert_eq!(finalized.failure_detail.unwrap().as_str(), "exec.protocol");
            assert!(bundle.validation.is_none());
            assert!(matches!(finalized.patch, PatchResult::None));
            assert!(finalized.proposals.is_empty());
            assert!(repo.patch_outcome(&attempt).is_none());
            assert_eq!(policy.proposal_calls(), 0);
        } else {
            assert_eq!(finalized.status, AttemptStatus::Succeeded);
        }
        assert!(!egress.calls().iter().any(
            |call| matches!(call, EgressCall::ForgetJob { job_id, .. } if *job_id == attempt)
        ));
        nonces.push(nonce);
    }
    assert_ne!(nonces[0], nonces[1]);

    // 模拟旧记录：原执行仍在驱动中，只能接管，不能重新启动。
    let attempt = ids.next().unwrap();
    let sandbox_id = sandbox_id_for(&attempt);
    driver.script(
        sandbox_id,
        FakeScript {
            behavior: FakeBehavior::Forever { honor_term: true },
            ..FakeScript::default()
        },
    );
    let mut plan = AttemptPlan {
        attempt_id: attempt,
        node_id: ids.next().unwrap(),
        workflow_id: ids.next().unwrap(),
        role: Role::Single,
        exec: Exec::Sandbox,
        critical: false,
        n: 1,
        base_sha: base,
        pr_base_sha: None,
        tokens_total: 100_000,
        wall_time_s: 900,
        model: ResolvedModel {
            provider_id: "anthropic".into(),
            model: "claude-test".into(),
            reasoning_effort: gantry_core::level::Effort::Medium,
            attempt_wall_s: 900,
        },
        upstream: vec![],
        previous: vec![],
        instructions: TrustedInstructions::compose("single", "legacy"),
        refs: vec![],
        lock: None,
        resume: false,
    };
    let cx = AttemptContext {
        project_id: project,
        delivery_id: delivery,
        level_effective: Level::Low,
        graph_rev: 1,
        revision_round: 0,
        required: true,
        upstream: vec![],
    };
    sandbox
        .submit(&ctx, runner.assemble(&plan, &cx, &ctx).await.unwrap())
        .await
        .unwrap();
    let id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(id) = driver
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    DriverCall::Start(id) => Some(id),
                    _ => None,
                })
                .nth(2)
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    sandbox.crash();
    let meta_path = workspaces.join(sandbox_id.to_string()).join("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    meta.as_object_mut().unwrap().remove("job_sha256");
    std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    driver.complete(&id, vec![]).unwrap();
    egress.set_revoke_fault(Some(RevokeFault::NotFound));
    let recovered = Arc::new(SandboxManager::new(
        SandboxDeps {
            driver: driver.clone(),
            repo: repo.clone(),
            egress: egress.clone(),
            store: store.clone(),
            clock: clock.clone(),
            artifacts: Arc::new(store.cas()),
            schemas: Arc::new(StaticSchemas(BTreeMap::from([(
                "gantry.worker.output.build/1".into(),
                SCHEMA.to_vec(),
            )]))),
        },
        SandboxConfig {
            workspaces_dir: workspaces.clone(),
            runtime: Runtime::Runc,
            max_concurrent_sandboxes: 1,
            network: NetworkAttach {
                network: "gantry-sbx".into(),
                add_hosts: vec![],
            },
            log_max_bytes: 16 << 20,
            out_quota_bytes: 12 << 20,
            tmpfs_home_mb: 256,
            tmpfs_tmp_mb: 512,
            progress_sample: Duration::from_secs(1),
            log_drain_timeout: Duration::from_secs(5),
            orphan_grace: Duration::ZERO,
        },
    ));
    let recovered_runner = AttemptRunner::new(
        AttemptPorts {
            sandbox: recovered.clone(),
            repo: repo.clone(),
            policy: policy.clone(),
            forge: Arc::new(FakeForge::new(clock.now())),
            cas: Arc::new(store.cas()),
            adapter: Arc::new(StubWorkerAdapter::new()),
            clock: clock.clone(),
            entropy: Arc::new(SeqEntropy::new(99)),
        },
        runner.config().clone(),
    );
    plan.resume = true;
    let mut running = Box::pin(recovered_runner.run(&plan, &cx, &ctx));
    assert!(
        tokio::time::timeout(Duration::from_millis(400), &mut running)
            .await
            .is_err()
    );
    assert_eq!(recovered.capacity().await.running, 0);
    assert!(recovered.release(&ctx, &attempt).await.is_err());
    egress.set_revoke_fault(Some(RevokeFault::Incomplete));
    assert!(
        tokio::time::timeout(Duration::from_millis(800), &mut running)
            .await
            .is_err()
    );
    assert!(recovered.release(&ctx, &attempt).await.is_err());
    assert!(!egress
        .calls()
        .iter()
        .any(|call| matches!(call, EgressCall::ForgetJob { job_id, .. } if *job_id == attempt)));
    egress.set_revoke_fault(None);
    let finalized = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finalized.failure_detail.unwrap().as_str(), "exec.protocol");
    assert_eq!(
        finalized.retry_blocked,
        Some(gantry_engine::decide::RetryBlocked::JobIdentityUnavailable)
    );
    assert!(finalized.result_ref.is_none());
    assert_eq!(recovered.wait(&attempt).await.unwrap().job_sha256, None);
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(meta_path).unwrap()).unwrap();
    assert!(meta.get("job_sha256").is_none());
    assert_eq!(
        driver
            .calls()
            .iter()
            .filter(|call| matches!(call, DriverCall::Start(_)))
            .count(),
        3
    );
    assert_eq!(policy.proposal_calls(), 0);
    assert!(repo.patch_outcome(&attempt).is_none());
    assert!(!egress
        .calls()
        .iter()
        .any(|call| matches!(call, EgressCall::ForgetJob { job_id, .. } if *job_id == attempt)));
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
        engine_cfg::runtime_settings().runner,
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
