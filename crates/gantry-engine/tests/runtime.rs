//! 引擎运行时（engine.md §7.15，#277）：`EngineRuntime::new` + `run(shutdown)` 在进程内用替身
//! 跑通 §12.2「端到端（替身）」主路径 spec → implement → propose → review → merge → closed。
//!
//! 生产装配只换端口：`FakeSandboxService`（按派发的 job 现场脚本化黄金结果）、`FakeRepo`（外包一层
//! 把 `worktree.*` / `patch.validated` 写进 store，与真实 repo 一致）、`FakePolicy`、`FakeForge`
//! （事务内入队）。测试只扮演 forge 执行器与 GitHub 快照来源，不直接调用执行体。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use gantry_core::canon::{sha256, GitOid, RepoSlug, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, ApprovalStatus, DeliveryOrigin, ForgeActionKind, GithubPrRef,
    GithubRef, MergeMethod, NodeStatus, PolicyActionId, Project, PullRequestRef, Role,
    SandboxLimits as Limits, SandboxTrust, Seq, Stage, StageStatus, Tier, TokenUsage, TokenWeights,
};
use gantry_core::events::{
    ApprovalGrantedData, EventType, NewEvent, PatchValidatedData, Subject, WorktreeCreatedData,
    WorktreeLockedData, WorktreeRemovedData, WorktreeUnlockReason, WorktreeUnlockedData,
};
use gantry_core::ids::{
    ApprovalId, AttemptId, DeliveryId, IdGen, MessageId, NodeId, ProjectId, WorktreeId,
};
use gantry_core::level::{Effort, Level};
use gantry_core::ports::context::{ActorCtx, ModuleError};
use gantry_core::ports::engine::DeliveryControl;
use gantry_core::ports::policy::{ActionDecision, ApprovalSpec};
use gantry_core::ports::repo::{
    CommitInfo, CommitPatch, CommitRecord, CreateWorktree, LockToken, MirrorGuard, MirrorHealth,
    MirrorInfo, MirrorMode, PatchOutcome, PrepareWorkspace, PreparedWorkspace, RepoError,
    RepoService, RevSpec, ValidatePatch,
};
use gantry_core::ports::sandbox::{
    ArtifactName, CapacitySnapshot, GroupName, JobHandle, JobOutcome, ProfileName, ProgressSample,
    RecoveredJob, SandboxError, SandboxService, SandboxSpec, SweepReport, Timers,
    UnfinishedAttempt,
};
use gantry_core::settings::resource::{
    ArtifactIo, LevelBudget, LevelEntry, MaterialIo, ProgressIo, SandboxDriver, SandboxIo,
    TierBudget,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::{
    FileArtifact, PatchLimits, PatchMode, ProposalKind, ProposalsMode, ReportMode,
    StructuredSchema, ToolConstraints,
};
use gantry_engine::actor::{ActorConfig, PrSnapshots};
use gantry_engine::attempt::{ResultCas, RoleOutput, RunnerConfig};
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::decide::{
    EngineConsts, EngineSettings, Input, InputIdentity, ModelKey, ModelTable, ProjectFacts,
    ReservationPolicy, ResolvedModel, SandboxLimits, TemplateRegistry, TickReason,
};
use gantry_engine::engine::load_delivery_state;
use gantry_engine::event_router::CURSOR;
use gantry_engine::runtime::{EnginePorts, EngineRuntime, RuntimeSettings};
use gantry_fakes::forge::FakeForge;
use gantry_fakes::inbox::FakeInbox;
use gantry_fakes::policy::FakePolicy;
use gantry_fakes::repo::FakeRepo;
use gantry_fakes::sandbox::FakeSandboxService;
use gantry_fakes::worker::StubWorkerAdapter;
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Cas, ClaimOpts, EventFilter, Store, StoreDeps, TypePattern};
use serde_json::{json, Value};

const T0: i64 = 1_700_000_000_000;
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const BASE: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn repo_slug() -> RepoSlug {
    RepoSlug::parse("acme/web").unwrap()
}

fn project_id() -> ProjectId {
    PROJECT.parse().unwrap()
}

async fn append(
    store: &Store,
    ty: EventType,
    subject: &str,
    delivery: DeliveryId,
    data: Value,
    dedup_key: Option<String>,
) -> Seq {
    let event = NewEvent {
        ty,
        subject: Subject::parse(subject).unwrap(),
        project_id: Some(project_id()),
        actor: Actor::new(ActorKind::System, "repo"),
        causation_id: None,
        correlation_id: Some(delivery.to_string()),
        data,
        dedup_key,
    };
    event.validate().unwrap();
    let mut tx = store.begin_write().await.unwrap();
    let appended = tx.append(event).await.unwrap();
    tx.commit().await.unwrap();
    match appended {
        gantry_store::Appended::New(event) => event.seq,
        gantry_store::Appended::Duplicate { existing_seq } => existing_seq,
    }
}

// ---------------------------------------------------------------------------
// repo：FakeRepo + 事件（真实 repo 在同一事务里写这些事件，锁围栏 = worktree.locked 的 seq）
// ---------------------------------------------------------------------------

struct EventedRepo {
    inner: Arc<FakeRepo>,
    store: Store,
    /// 对外围栏（事件 seq）→ FakeRepo 自己的锁令牌。
    tokens: Mutex<HashMap<(WorktreeId, Seq), LockToken>>,
}

impl EventedRepo {
    fn inner_token(&self, token: &LockToken) -> LockToken {
        self.tokens
            .lock()
            .unwrap()
            .get(&(token.worktree_id, token.fence))
            .cloned()
            .unwrap_or_else(|| token.clone())
    }
}

#[async_trait]
impl RepoService for EventedRepo {
    async fn ensure_mirror(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
        remote: &RepoSlug,
    ) -> Result<MirrorInfo, RepoError> {
        self.inner.ensure_mirror(ctx, project, remote).await
    }

    async fn mirror_access(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
        mode: MirrorMode,
    ) -> Result<MirrorGuard, RepoError> {
        self.inner.mirror_access(ctx, project, mode).await
    }

    async fn commit_info(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitInfo>, RepoError> {
        self.inner.commit_info(project, oid).await
    }

    async fn resolve(&self, project: &ProjectId, rev: &RevSpec) -> Result<GitOid, RepoError> {
        self.inner.resolve(project, rev).await
    }

    async fn has_commit(&self, project: &ProjectId, oid: &GitOid) -> Result<bool, RepoError> {
        self.inner.has_commit(project, oid).await
    }

    async fn check_mirror(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
    ) -> Result<MirrorHealth, RepoError> {
        self.inner.check_mirror(ctx, project).await
    }

    async fn create_worktree(
        &self,
        ctx: &ActorCtx,
        req: CreateWorktree,
    ) -> Result<gantry_core::entities::Worktree, RepoError> {
        let delivery = req.delivery_id;
        let base_sha = req.base_sha;
        let worktree = self.inner.create_worktree(ctx, req).await?;
        let data = WorktreeCreatedData {
            worktree_id: worktree.id,
            project_id: worktree.project_id,
            delivery_id: delivery,
            branch: worktree.branch.clone(),
            base_ref: worktree.base_ref.clone(),
            base_sha,
        };
        append(
            &self.store,
            EventType::WorktreeCreated,
            &worktree.id.to_string(),
            delivery,
            serde_json::to_value(data).unwrap(),
            Some(format!("worktree.created:{}", worktree.id)),
        )
        .await;
        Ok(worktree)
    }

    async fn get_worktree(
        &self,
        id: &WorktreeId,
    ) -> Result<Option<gantry_core::entities::Worktree>, RepoError> {
        self.inner.get_worktree(id).await
    }

    async fn branch_head(&self, id: &WorktreeId) -> Result<GitOid, RepoError> {
        self.inner.branch_head(id).await
    }

    async fn remove_worktree(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        lock: &LockToken,
    ) -> Result<(), RepoError> {
        let branch = self
            .inner
            .get_worktree(id)
            .await?
            .map(|w| w.branch)
            .unwrap_or_default();
        self.inner
            .remove_worktree(ctx, id, &self.inner_token(lock))
            .await?;
        let data = WorktreeRemovedData {
            worktree_id: *id,
            branch,
        };
        append(
            &self.store,
            EventType::WorktreeRemoved,
            &id.to_string(),
            lock.delivery_id,
            serde_json::to_value(data).unwrap(),
            Some(format!("worktree.removed:{id}")),
        )
        .await;
        Ok(())
    }

    async fn advance_head(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        new_head: &GitOid,
        lock: &LockToken,
    ) -> Result<GitOid, RepoError> {
        self.inner
            .advance_head(ctx, id, new_head, &self.inner_token(lock))
            .await
    }

    async fn acquire_lock(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        holder: gantry_core::entities::LockHolder,
        delivery: &DeliveryId,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        let inner = self
            .inner
            .acquire_lock(ctx, id, holder, delivery, lease)
            .await?;
        let data = WorktreeLockedData {
            worktree_id: *id,
            holder,
            delivery_id: Some(*delivery),
            lease_until: inner.lease_until,
        };
        let fence = append(
            &self.store,
            EventType::WorktreeLocked,
            &id.to_string(),
            *delivery,
            serde_json::to_value(data).unwrap(),
            None,
        )
        .await;
        let outer = LockToken {
            fence,
            ..inner.clone()
        };
        self.tokens.lock().unwrap().insert((*id, fence), inner);
        Ok(outer)
    }

    async fn renew_lock(
        &self,
        ctx: &ActorCtx,
        token: &LockToken,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        let inner = self
            .inner
            .renew_lock(ctx, &self.inner_token(token), lease)
            .await?;
        let outer = LockToken {
            fence: token.fence,
            lease_until: inner.lease_until,
            ..token.clone()
        };
        self.tokens
            .lock()
            .unwrap()
            .insert((token.worktree_id, token.fence), inner);
        Ok(outer)
    }

    async fn release_lock(&self, ctx: &ActorCtx, token: &LockToken) -> Result<(), RepoError> {
        self.inner
            .release_lock(ctx, &self.inner_token(token))
            .await?;
        let data = WorktreeUnlockedData {
            worktree_id: token.worktree_id,
            reason: WorktreeUnlockReason::Released,
        };
        append(
            &self.store,
            EventType::WorktreeUnlocked,
            &token.worktree_id.to_string(),
            token.delivery_id,
            serde_json::to_value(data).unwrap(),
            Some(format!(
                "worktree.unlocked:{}:{}",
                token.worktree_id,
                token.fence.as_u64()
            )),
        )
        .await;
        Ok(())
    }

    async fn prepare_workspace(
        &self,
        ctx: &ActorCtx,
        req: PrepareWorkspace,
    ) -> Result<PreparedWorkspace, RepoError> {
        self.inner.prepare_workspace(ctx, req).await
    }

    async fn validate_patch(
        &self,
        ctx: &ActorCtx,
        req: ValidatePatch,
    ) -> Result<PatchOutcome, RepoError> {
        let delivery = req.delivery_id;
        let job = req.job_id;
        let outcome = self.inner.validate_patch(ctx, req).await?;
        if let PatchOutcome::Validated(v) = &outcome {
            let data = PatchValidatedData {
                job_id: job,
                sha256: v.sha256,
                base_sha: v.base_sha,
                result_tree: v.result_tree.expect("fake repo always yields a tree"),
                verdict: v.verdict,
                files: v.files.clone(),
                reasons: v.reasons.clone(),
                config_rev: Seq::new(1).unwrap(),
            };
            append(
                &self.store,
                EventType::PatchValidated,
                &job.to_string(),
                delivery,
                serde_json::to_value(data).unwrap(),
                Some(format!("patch.validated:{job}")),
            )
            .await;
        }
        Ok(outcome)
    }

    async fn commit_patch(
        &self,
        ctx: &ActorCtx,
        mut req: CommitPatch,
    ) -> Result<CommitRecord, RepoError> {
        req.lock = self.inner_token(&req.lock);
        self.inner.commit_patch(ctx, req).await
    }

    async fn provenance(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitRecord>, RepoError> {
        self.inner.provenance(project, oid).await
    }
}

// ---------------------------------------------------------------------------
// sandbox：受理时为该 job 现场脚本化黄金结果（补丁 + open_pr 提议）
// ---------------------------------------------------------------------------

struct GoldenSandbox {
    inner: FakeSandboxService,
    cas: Cas,
    jobs: Mutex<Vec<AttemptId>>,
    /// 为真时 worker 以退出码 10 结束（`exec.bad_job`：不自动重试，节点 failed）。
    fail: Mutex<bool>,
}

impl GoldenSandbox {
    async fn put(&self, bytes: &[u8]) -> FileArtifact {
        FileArtifact {
            sha256: self.cas.put_artifact(bytes).await.unwrap(),
            bytes: bytes.len() as u64,
        }
    }

    async fn golden(&self, spec: &SandboxSpec) -> JobOutcome {
        let job = spec.job_id;
        let patch = self
            .put(b"diff --git a/src/login.rs b/src/login.rs\n")
            .await;
        let proposals = json!({
            "protocol": "gantry-worker/1",
            "job_id": job,
            "proposals": [{
                "id": "p1", "kind": "open_pr", "target": null,
                "params": {"title": "Fix login timeout", "body": "body", "draft": false},
                "rationale": "done"
            }]
        });
        let proposals = self.put(&serde_json::to_vec(&proposals).unwrap()).await;
        let result = json!({
            "protocol": "gantry-worker/1",
            "job_id": job,
            "job_sha256": sha256(b"job.json"),
            "status": "succeeded",
            "summary": "fixed",
            "error": null,
            "structured_output": {"kind": "build", "changes_summary": "fixed"},
            "artifacts": {
                "patch": {"path": "changes.patch", "sha256": patch.sha256, "bytes": patch.bytes,
                          "base_sha": BASE, "files_changed": 1},
                "report": null,
                "proposals": {"path": "proposals.json", "sha256": proposals.sha256,
                              "bytes": proposals.bytes},
            },
            "flags": {"suspected_injection": []},
            "usage_reported": {"input_tokens": 1, "output_tokens": 1, "cache_read_tokens": 0,
                               "cache_write_tokens": 0, "turns": 1, "tool_calls": 1},
            "agent": {"kind": "claude-code", "version": "1.0.0", "model": "claude-test"},
            "started_at": ts(T0 + 1_000),
            "ended_at": ts(T0 + 60_000),
        });
        let result = self.put(&serde_json::to_vec(&result).unwrap()).await;
        let mut outcome = FakeSandboxService::success_outcome(job, spec.sandbox_id);
        outcome.files = BTreeMap::from([
            (ArtifactName::new("result.json"), result),
            (ArtifactName::new("changes.patch"), patch),
            (ArtifactName::new("proposals.json"), proposals),
        ]);
        if *self.fail.lock().unwrap() {
            outcome.terminal.exit_code = Some(10);
        }
        outcome.usage_metered = TokenUsage::new(1_000, 200, 0, 0, 3);
        outcome.weighted_tokens = 2_000;
        outcome
    }
}

#[async_trait]
impl SandboxService for GoldenSandbox {
    async fn submit(&self, ctx: &ActorCtx, spec: SandboxSpec) -> Result<JobHandle, SandboxError> {
        let first = {
            let mut jobs = self.jobs.lock().unwrap();
            let first = !jobs.contains(&spec.job_id);
            if first {
                jobs.push(spec.job_id);
            }
            first
        };
        if first {
            let outcome = self.golden(&spec).await;
            self.inner
                .script_outcome(spec.job_id, spec.sandbox_id, outcome);
        }
        self.inner.submit(ctx, spec).await
    }

    async fn wait(&self, job_id: &AttemptId) -> Result<JobOutcome, SandboxError> {
        self.inner.wait(job_id).await
    }

    fn progress(&self, job_id: &AttemptId) -> BoxStream<'static, ProgressSample> {
        self.inner.progress(job_id)
    }

    async fn cancel(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        self.inner.cancel(ctx, job_id).await
    }

    async fn release(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        self.inner.release(ctx, job_id).await
    }

    async fn recover(
        &self,
        ctx: &ActorCtx,
        unfinished: &[UnfinishedAttempt],
    ) -> Result<Vec<RecoveredJob>, SandboxError> {
        self.inner.recover(ctx, unfinished).await
    }

    async fn sweep_orphans(
        &self,
        ctx: &ActorCtx,
        live: &BTreeSet<AttemptId>,
    ) -> Result<SweepReport, SandboxError> {
        self.inner.sweep_orphans(ctx, live).await
    }

    async fn capacity(&self) -> CapacitySnapshot {
        self.inner.capacity().await
    }
}

// ---------------------------------------------------------------------------
// GitHub 快照来源（生产里由 forge 同步写 store 投影；这里由测试直接给）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Snapshots(Mutex<Option<PullRequestRef>>);

#[async_trait]
impl PrSnapshots for Snapshots {
    async fn pull_request(&self, _: &ProjectId, number: u64) -> Option<PullRequestRef> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .filter(|pr| pr.number == number)
    }
}

fn snapshot(state: &str, head: GitOid) -> PullRequestRef {
    serde_json::from_value(json!({
        "repo": "acme/web", "number": 12, "node_id": "PR_x",
        "title": {"text": "t", "source": "github", "truncated": false},
        "state": state, "head_sha": head, "base_ref": "main", "base_sha": BASE,
        "draft": false, "mergeable_state": "clean", "in_merge_queue": false,
        "checks": {"passed": 1, "failed": 0, "pending": 0, "required_ok": true},
        "review_state": "none", "synced_at": "2026-10-24T12:41:03.120Z"
    }))
    .unwrap()
}

// ---------------------------------------------------------------------------
// 装配
// ---------------------------------------------------------------------------

struct FixedReserve;
impl ReservationPolicy for FixedReserve {
    fn reserve(&self, _: &gantry_core::entities::BudgetLedger, _: Tier, _: usize) -> u64 {
        1000
    }
}

fn actor_config() -> ActorConfig {
    ActorConfig {
        settings: EngineSettings {
            config_rev: Seq::new(1).unwrap(),
            project: ProjectFacts {
                repo: repo_slug(),
                default_branch: "main".to_owned(),
                branch_prefix: "gantry/".to_owned(),
                allowed_merge_methods: vec![MergeMethod::Squash],
            },
            sandbox: SandboxLimits {
                wall_time_s: 1800,
                kill_grace_s: 10,
            },
        },
        models: ModelTable::from([(
            ModelKey {
                sandbox: true,
                level: Level::Low,
                tier: Tier::Worker,
                critical: false,
            },
            ResolvedModel {
                provider_id: "anthropic".to_owned(),
                model: "claude".to_owned(),
                reasoning_effort: Effort::Medium,
                attempt_wall_s: 900,
            },
        )]),
        templates: TemplateRegistry,
        reservation: Arc::new(FixedReserve),
        sandbox_capacity: 4,
        consts: EngineConsts::default(),
    }
}

fn runner_config() -> RunnerConfig {
    let mut roles = HashMap::new();
    roles.insert(
        Role::Single,
        RoleOutput {
            structured_schema: StructuredSchema {
                id: "gantry.worker.output.build/1".into(),
                path: "/gantry/inputs/output.schema.json".into(),
                sha256: sha256(b"build-schema"),
            },
            patch: PatchMode::Optional,
            report: ReportMode::Optional,
            proposals: ProposalsMode::Allowed {
                allowed_kinds: vec![ProposalKind::OpenPr, ProposalKind::Escalate],
                max: 5,
            },
            tools: ToolConstraints {
                edit: true,
                shell: true,
            },
            patch_limits: Some(PatchLimits {
                max_bytes: 5_000_000,
                max_files: 200,
            }),
        },
    );
    RunnerConfig {
        config_rev: Seq::new(1).unwrap(),
        repo: repo_slug(),
        driver: SandboxDriver::Podman,
        trust: SandboxTrust::Trusted,
        limits: Limits {
            cpu_cores: 2.0,
            mem_mb: 4096,
            disk_mb: 10240,
            pids: 512,
            wall_time_s: 3600,
        },
        timers: Timers {
            idle_timeout_s: 300,
            kill_grace_s: 10,
        },
        io: SandboxIo {
            progress: ProgressIo {
                max_line_bytes: 8192,
                max_lines: 20_000,
                max_rate_per_s: 20,
            },
            artifacts: ArtifactIo {
                result_max_bytes: 1_048_576,
                report_max_bytes: 262_144,
                proposals_max_bytes: 262_144,
                proposals_max_count: 20,
            },
            materials: MaterialIo {
                max_item_bytes: 65_536,
                max_total_bytes: 524_288,
            },
            logs_tail_bytes: 1_048_576,
        },
        egress_profile: ProfileName::new("default"),
        egress_groups: vec![GroupName::new("model_api")],
        token_weights: TokenWeights::default(),
        max_turns: 60,
        discussion_max_comments: 50,
        roles,
    }
}

struct World {
    store: Store,
    repo: Arc<FakeRepo>,
    policy: Arc<FakePolicy>,
    sandbox: Arc<GoldenSandbox>,
    snapshots: Arc<Snapshots>,
}

impl World {
    async fn new() -> Self {
        let clock = Arc::new(FakeClock::new(T0));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(11))));
        let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
            .await
            .unwrap();
        let repo = Arc::new(FakeRepo::new(ts(T0)));
        repo.set_ref(project_id(), "main", GitOid::parse(BASE).unwrap());
        Self {
            policy: Arc::new(FakePolicy::new_in(store.clone())),
            sandbox: Arc::new(GoldenSandbox {
                inner: FakeSandboxService::new(),
                cas: store.cas(),
                jobs: Mutex::new(Vec::new()),
                fail: Mutex::new(false),
            }),
            snapshots: Arc::new(Snapshots::default()),
            repo,
            store,
        }
    }

    /// 与 gantryd 的装配同形：组端口 → `EngineRuntime::new`。
    async fn runtime(&self) -> Arc<EngineRuntime> {
        let forge = Arc::new(FakeForge::new_in(self.store.clone()));
        forge.insert_project(Project {
            id: project_id(),
            repo: repo_slug(),
            installation_id: 1,
            bot_user_id: 2,
            default_branch: "main".into(),
            allowed_merge_methods: vec![MergeMethod::Squash],
            merge_queue_enabled: false,
            created_at: ts(T0),
            updated_at: ts(T0),
        });
        let clock = Arc::new(FakeClock::new(T0));
        let ports = EnginePorts {
            inbox: Arc::new(FakeInbox::new_in(self.store.clone())),
            outbox: forge.clone(),
            policy: self.policy.clone(),
            forge,
            repo: Arc::new(EventedRepo {
                inner: self.repo.clone(),
                store: self.store.clone(),
                tokens: Mutex::new(HashMap::new()),
            }),
            sandbox: self.sandbox.clone(),
            worker: Arc::new(StubWorkerAdapter::new()),
            cas: Arc::new(self.store.cas()),
            clock,
            entropy: Arc::new(SeqEntropy::new(99)),
            snapshots: Some(self.snapshots.clone()),
        };
        let settings = RuntimeSettings {
            actor: actor_config(),
            runner: runner_config(),
            poll_interval: Duration::from_millis(5),
            cleanup_lease: Duration::from_secs(60),
        };
        EngineRuntime::new(self.store.clone(), ports, settings)
            .await
            .unwrap()
    }

    /// 已提交的补丁（单节点，最多一个尝试成功）。
    fn committed(&self) -> Option<GitOid> {
        let jobs = self.sandbox.jobs.lock().unwrap().clone();
        jobs.iter()
            .find_map(|job| self.repo.commit_record(job))
            .map(|record| record.commit_oid)
    }

    /// forge 执行器：认领并完成排队动作；create_pr 绑定 PR 12。返回完成的动作种类。
    async fn execute_outbox(&self, head: Option<GitOid>) -> Vec<ForgeActionKind> {
        let mut done = Vec::new();
        loop {
            let mut tx = self.store.begin_write().await.unwrap();
            let claimed = tx
                .outbox()
                .claim(ClaimOpts {
                    lease: Duration::from_secs(60),
                    exclude_deliveries: BTreeSet::new(),
                })
                .await
                .unwrap();
            let Some(claimed) = claimed else {
                tx.rollback().await.unwrap();
                return done;
            };
            let action = claimed.action;
            let pr = (action.kind == ForgeActionKind::CreatePr).then(|| GithubPrRef {
                number: 12,
                node_id: "PR_x".into(),
                head_sha: head.expect("create_pr only after the patch is committed"),
            });
            let github_ref = GithubRef {
                bot: None,
                pr,
                marker: None,
                recorded_at: None,
            };
            tx.outbox().complete(&action.id, github_ref).await.unwrap();
            tx.commit().await.unwrap();
            done.push(action.kind);
        }
    }

    async fn stages(&self, delivery: DeliveryId) -> Vec<(String, String)> {
        let filter = EventFilter {
            project_ids: None,
            include: vec![TypePattern::Exact(EventType::DeliveryStageChanged)],
            exclude: Vec::new(),
        };
        let page = self
            .store
            .events()
            .scan(Seq::ZERO, &filter, 200)
            .await
            .unwrap();
        page.events
            .iter()
            .filter(|e| e.subject.as_str() == delivery.to_string())
            .map(|e| {
                (
                    e.data["to"].as_str().unwrap_or_default().to_owned(),
                    e.data["reason"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }
}

fn new_delivery() -> NewDelivery {
    NewDelivery {
        id: DELIVERY.parse().unwrap(),
        project_id: project_id(),
        title: "fix login timeout".to_owned(),
        user_text: "修复登录超时".to_owned(),
        level_cap: Level::Low,
        level_effective: Level::Low,
        origin: DeliveryOrigin::Conversation,
        source_message_id: "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD"
            .parse::<MessageId>()
            .unwrap(),
        issues: vec![],
        budget: budget_from_level(&LevelEntry {
            graph_template: "low.single".to_owned(),
            budget: LevelBudget {
                tokens_by_tier: TierBudget {
                    utility: 50_000,
                    planner: 100_000,
                    worker: 1_000_000,
                },
                nodes: 8,
                rounds: 0,
                wall_time_s: 3600,
                concurrency: 1,
                forge_writes: 20,
                forge_writes_per_hour: 30,
            },
        }),
        config_rev: Seq::new(1).unwrap(),
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
    }
}

/// merge_pr 需要审批（I8）：未授予时 `NeedsApproval`，授予后 `Allow` 带审批 ID。
fn script_merge(
    policy: &FakePolicy,
    delivery: DeliveryId,
    head: GitOid,
    granted: Option<ApprovalId>,
) {
    let decision = match granted {
        None => ActionDecision::NeedsApproval {
            spec: ApprovalSpec {
                action: ApprovalAction::Forge(ForgeActionKind::MergePr),
                target: serde_json::from_value(
                    json!({"kind": "pr", "repo": "acme/web", "number": 12}),
                )
                .unwrap(),
                subject_digest: SubjectDigest::git(head),
                delivery_id: Some(delivery),
                ttl: Duration::from_secs(3600),
            },
            pending: None,
            config_rev: Seq::new(1).unwrap(),
        },
        Some(id) => ActionDecision::Allow {
            action_id: PolicyActionId::from(ForgeActionKind::MergePr),
            approval: Some(id),
            config_rev: Seq::new(1).unwrap(),
        },
    };
    policy.script_decision(ForgeActionKind::MergePr, decision);
}

/// 设备签名授予：policy 状态置 granted，并写 `approval.granted`（生产里由 policy 在同一事务写）。
async fn grant(world: &World, delivery: DeliveryId, id: ApprovalId) {
    world.policy.grant(&id).unwrap();
    let digest = sha256(b"approval");
    let data = ApprovalGrantedData {
        approval_id: id,
        device_id: "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().unwrap(),
        approval_pubkey_fpr: digest,
        payload_sha256: digest,
    };
    append(
        &world.store,
        EventType::ApprovalGranted,
        &id.to_string(),
        delivery,
        serde_json::to_value(data).unwrap(),
        None,
    )
    .await;
}

/// 扮演 forge 执行器、GitHub 与审批设备，直到交付单元 closed 且工作树已移除。
async fn drive_to_closed(world: &World, runtime: &EngineRuntime, delivery: DeliveryId) {
    let tick = runtime.mailbox();
    let mut merged = false;
    let mut granted = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let head = world.committed();
        if let Some(head) = head {
            script_merge(&world.policy, delivery, head, granted);
        }
        let done = world.execute_outbox(head).await;
        merged |= done.contains(&ForgeActionKind::MergePr);
        let state = load_delivery_state(&world.store, &delivery)
            .await
            .unwrap()
            .unwrap();
        if state.delivery.stage == Stage::Closed && state.worktree.is_none() {
            return;
        }
        if granted.is_none() {
            let pending = state
                .approvals
                .iter()
                .find(|(_, a)| a.status == ApprovalStatus::Pending)
                .map(|(id, _)| *id);
            if let Some(id) = pending {
                if let Some(head) = head {
                    script_merge(&world.policy, delivery, head, Some(id));
                }
                grant(world, delivery, id).await;
                granted = Some(id);
            }
        }
        if state.pr.is_some() {
            if let Some(head) = head {
                let pr = snapshot(if merged { "merged" } else { "open" }, head);
                *world.snapshots.0.lock().unwrap() = Some(pr);
                // 生产里快照变化随 forge.* 事件路由进来；这里直接给执行体一个 Settle。
                tick(
                    delivery,
                    Input::Tick(TickReason::Settle),
                    InputIdentity::Tick(TickReason::Settle),
                );
            }
        }
        if Instant::now() >= deadline {
            let page = world
                .store
                .events()
                .scan(Seq::ZERO, &EventFilter::default(), 200)
                .await
                .unwrap();
            for e in &page.events {
                eprintln!(
                    "{} {} {} {}",
                    e.seq.as_u64(),
                    e.ty,
                    e.subject.as_str(),
                    e.data
                );
            }
            eprintln!("pr={:?} outbox={:?}", state.pr, state.outbox);
        }
        assert!(
            Instant::now() < deadline,
            "stuck at {:?}: {:?}",
            state.delivery.stage,
            world.stages(delivery).await
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn low_delivery_reaches_closed_with_new_and_run_only() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));

    // 建单走现有写入口；之后不再直接碰执行体。
    let delivery = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    let driven = tokio::time::timeout(
        Duration::from_secs(5),
        drive_to_closed(&world, &runtime, delivery),
    )
    .await;
    if driven.is_err() && main.is_finished() {
        panic!("run exited: {:?}", main.await);
    }

    let stages = world.stages(delivery).await;
    let order: Vec<&str> = stages.iter().map(|(to, _)| to.as_str()).collect();
    assert_eq!(order, ["implement", "propose", "review", "merge", "closed"]);
    assert_eq!(stages[0].1, "admission_skipped");
    assert_eq!(world.sandbox.jobs.lock().unwrap().len(), 1);
    assert!(runtime.degraded().await.is_empty());

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), main)
        .await
        .expect("run returns after shutdown")
        .unwrap()
        .unwrap();
    assert_eq!(
        world.store.cursors().load(CURSOR).await.unwrap(),
        world.store.events().head().await.unwrap(),
        "cursor catches up with the log head"
    );
}

#[tokio::test]
async fn restart_resumes_from_the_cursor_without_redispatching() {
    let world = World::new().await;
    let first = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(first.clone().run(async {
        let _ = stopped.await;
    }));
    let delivery = first
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();

    // 跑到 propose（补丁已提交、PR 动作已入队）后停机。
    let deadline = Instant::now() + Duration::from_secs(20);
    while load_delivery_state(&world.store, &delivery)
        .await
        .unwrap()
        .unwrap()
        .delivery
        .stage
        != Stage::Propose
    {
        assert!(Instant::now() < deadline, "never reached propose");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();

    // 新进程：同一 store，新的运行时；Tick(Recover) 接管，不重新派发尝试。
    let second = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(second.clone().run(async {
        let _ = stopped.await;
    }));
    drive_to_closed(&world, &second, delivery).await;
    assert_eq!(world.sandbox.jobs.lock().unwrap().len(), 1);
    let order: Vec<String> = world
        .stages(delivery)
        .await
        .into_iter()
        .map(|(to, _)| to)
        .collect();
    assert_eq!(order, ["implement", "propose", "review", "merge", "closed"]);
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();
}

/// 端口拒绝事务内决策（这里：policy 放行了不带审批的 merge_pr，forge 按 I8 拒绝入队）是引擎缺陷：
/// 只让该交付单元停止决策，主循环继续运行（§7.14.1）。
#[tokio::test]
async fn a_rejected_enqueue_degrades_only_that_delivery() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));
    let delivery = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();
    let tick = runtime.mailbox();
    let deadline = Instant::now() + Duration::from_secs(20);
    while runtime.degraded().await.is_empty() {
        let head = world.committed();
        world.execute_outbox(head).await;
        if let Some(head) = head {
            *world.snapshots.0.lock().unwrap() = Some(snapshot("open", head));
            tick(
                delivery,
                Input::Tick(TickReason::Settle),
                InputIdentity::Tick(TickReason::Settle),
            );
        }
        assert!(Instant::now() < deadline, "never degraded");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let degraded = runtime.degraded().await;
    assert!(degraded[&delivery].contains("approval"), "{degraded:?}");
    assert!(!main.is_finished(), "run keeps going");
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// DeliveryControl（#73）：命令收 ActorCtx，经执行体由 decide 求值
// ---------------------------------------------------------------------------

fn ctx(key: &str, n: u128) -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        device: None,
        role: None,
        client_profile: None,
        request_id: gantry_core::ids::Ulid::from_parts(T0 as u64, n).unwrap(),
        idempotency_key: None,
        dedup_key: Some(key.to_owned()),
        causation_id: None,
    }
}

async fn head(world: &World) -> Seq {
    world.store.events().head().await.unwrap()
}

#[tokio::test]
async fn delivery_control_goes_through_the_actor() {
    let world = World::new().await;
    let runtime = world.runtime().await;
    let id = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();

    // pause：写 delivery.paused（dedup_key = ctx.dedup_key，actor 取调用方）。
    let paused = runtime.pause(ctx("p1", 1), id).await.unwrap();
    assert_eq!(paused.stage_status, StageStatus::Paused);
    assert_eq!(paused.id, id);
    let seq = world
        .store
        .events()
        .by_dedup_key("p1")
        .await
        .unwrap()
        .unwrap();
    let event = world.store.events().get_by_seq(seq).await.unwrap().unwrap();
    assert_eq!(event.ty, EventType::DeliveryPaused);
    assert_eq!(event.actor.kind, ActorKind::Device);
    // 同一请求重放 → 首次结果，不写事件。
    let before = head(&world).await;
    assert_eq!(
        runtime.pause(ctx("p1", 2), id).await.unwrap().stage_status,
        StageStatus::Paused
    );
    assert_eq!(head(&world).await, before);
    // 暂停期间不派发。
    assert!(world.sandbox.jobs.lock().unwrap().is_empty());

    // resume；新请求在未暂停时 → InvalidState。
    let resumed = runtime.resume(ctx("r1", 3), id).await.unwrap();
    assert_eq!(resumed.stage, Stage::Implement);
    assert!(matches!(
        runtime.resume(ctx("r2", 4), id).await,
        Err(ModuleError::InvalidState { .. })
    ));

    // set_level_cap(None) → Validation，指向 /level_cap。
    assert!(matches!(
        runtime.set_level_cap(ctx("c1", 5), id, Level::None).await,
        Err(ModuleError::Validation { pointer, .. }) if pointer == "/level_cap"
    ));

    // abandon → 终态；之后的新命令 → InvalidState。
    let abandoned = runtime
        .abandon(ctx("a1", 6), id, Some("not needed".to_owned()))
        .await
        .unwrap();
    assert_eq!(abandoned.stage, Stage::Abandoned);
    assert!(matches!(
        runtime.pause(ctx("p2", 7), id).await,
        Err(ModuleError::InvalidState { .. })
    ));

    // 未知交付单元 → NotFound，且不留下执行体。
    let missing: DeliveryId = "dlv_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap();
    assert!(matches!(
        runtime.pause(ctx("p3", 8), missing).await,
        Err(ModuleError::NotFound)
    ));
    assert!(runtime.degraded().await.is_empty());
}

#[tokio::test]
async fn retry_node_through_delivery_control() {
    let world = World::new().await;
    *world.sandbox.fail.lock().unwrap() = true;
    let runtime = world.runtime().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let main = tokio::spawn(runtime.clone().run(async {
        let _ = stopped.await;
    }));
    let id = runtime
        .engine()
        .create_delivery(new_delivery())
        .await
        .unwrap();

    let failed_node = || async {
        let state = load_delivery_state(&world.store, &id)
            .await
            .unwrap()
            .unwrap();
        state
            .workflows
            .values()
            .flat_map(|w| w.nodes.values())
            .find(|n| n.status == NodeStatus::Failed)
            .map(|n| n.id)
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let node = loop {
        if let Some(node) = failed_node().await {
            break node;
        }
        assert!(Instant::now() < deadline, "node never failed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    // 正例：failed → queued，随后重新派发（这次成功）。
    *world.sandbox.fail.lock().unwrap() = false;
    let retried = runtime.retry_node(ctx("retry-1", 1), node).await.unwrap();
    assert_eq!(retried.id, node);
    assert_ne!(retried.status, NodeStatus::Failed);
    // 同一请求重放 → Ok；新请求（节点已不是 failed）→ InvalidState。
    assert!(runtime.retry_node(ctx("retry-1", 2), node).await.is_ok());
    assert!(matches!(
        runtime.retry_node(ctx("retry-2", 3), node).await,
        Err(ModuleError::InvalidState { .. })
    ));
    // 未知节点 → NotFound。
    let missing: NodeId = "nod_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap();
    assert!(matches!(
        runtime.retry_node(ctx("retry-3", 4), missing).await,
        Err(ModuleError::NotFound)
    ));
    let deadline = Instant::now() + Duration::from_secs(20);
    while world.sandbox.jobs.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "retry never dispatched");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.send(()).unwrap();
    main.await.unwrap().unwrap();
}
