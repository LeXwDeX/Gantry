//! 引擎运行时测试的共用装配（engine.md §7.15、§12.2）：替身端口、黄金结果、forge 执行器与
//! GitHub / 审批设备的扮演，以及崩溃点闸门（#74）。
//!
//! 跨"进程"持久的部分（store、repo 的锁映射、沙盒与策略状态）放在 [`World`] 里；每次
//! [`World::runtime`] 只新建 `EngineRuntime`，相当于重启引擎进程。

#![allow(dead_code, unused_imports)]

pub use std::collections::{BTreeMap, BTreeSet, HashMap};
pub use std::sync::{Arc, Mutex};
pub use std::time::{Duration, Instant};

pub use async_trait::async_trait;
pub use futures_core::stream::BoxStream;
pub use gantry_core::canon::{sha256, GitOid, RepoSlug, SubjectDigest};
pub use gantry_core::entities::Approval;
pub use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, ApprovalStatus, DeliveryOrigin, ForgeActionKind, GithubPrRef,
    GithubRef, MergeMethod, NodeStatus, PolicyActionId, Project, PullRequestRef, Role,
    SandboxLimits as Limits, SandboxTrust, Seq, Stage, StageStatus, Tier, TokenUsage, TokenWeights,
};
pub use gantry_core::events::{
    ApprovalGrantedData, EventType, NewEvent, PatchValidatedData, Subject, WorktreeCreatedData,
    WorktreeLockedData, WorktreeRemovedData, WorktreeUnlockReason, WorktreeUnlockedData,
};
pub use gantry_core::ids::ActionId;
pub use gantry_core::ids::{
    ApprovalId, AttemptId, DeliveryId, IdGen, MessageId, NodeId, ProjectId, WorktreeId,
};
pub use gantry_core::level::{Effort, Level};
pub use gantry_core::ports::context::TxHandle;
pub use gantry_core::ports::context::{ActorCtx, ModuleError};
pub use gantry_core::ports::engine::DeliveryControl;
pub use gantry_core::ports::policy::{
    ActionDecision, ActionRequest, AddedLineSource, ApprovalSpec, ExecutionCheck, ExecutionPermit,
    GateError, InvalidateReason, PatchApprovalCheck, PatchPermit, PatchPolicyVerdict, PathRuleSet,
    PolicyError, PolicyPort, ProposalContext, ProposalDecision, SecretFinding,
};
pub use gantry_core::ports::repo::PathRulesSnapshot;
pub use gantry_core::ports::repo::{
    CommitInfo, CommitPatch, CommitRecord, CreateWorktree, LockToken, MirrorGuard, MirrorHealth,
    MirrorInfo, MirrorMode, PatchOutcome, PrepareWorkspace, PreparedWorkspace, RepoError,
    RepoService, RevSpec, ValidatePatch,
};
pub use gantry_core::ports::sandbox::{
    ArtifactName, CapacitySnapshot, GroupName, JobHandle, JobOutcome, ProfileName, ProgressSample,
    RecoveredJob, SandboxError, SandboxService, SandboxSpec, SweepReport, Timers,
    UnfinishedAttempt,
};
pub use gantry_core::settings::resource::{
    ArtifactIo, LevelBudget, LevelEntry, MaterialIo, ProgressIo, SandboxDriver, SandboxIo,
    TierBudget,
};
pub use gantry_core::time::Timestamp;
pub use gantry_core::worker::{
    FileArtifact, PatchLimits, PatchMode, ProposalKind, ProposalsMode, ReportMode,
    StructuredSchema, ToolConstraints,
};
pub use gantry_core::worker::{PatchFacts, Proposal};
pub use gantry_engine::actor::{ActorConfig, PrSnapshots};
pub use gantry_engine::attempt::{ResultCas, RoleOutput, RunnerConfig};
pub use gantry_engine::create::{budget_from_level, NewDelivery};
pub use gantry_engine::decide::{
    EngineConsts, EngineSettings, Input, InputIdentity, ModelKey, ModelTable, ProjectFacts,
    ReservationPolicy, ResolvedModel, SandboxLimits, TemplateRegistry, TickReason,
};
pub use gantry_engine::engine::load_delivery_state;
pub use gantry_engine::event_router::CURSOR;
pub use gantry_engine::runtime::{EnginePorts, EngineRuntime, RuntimeSettings};
pub use gantry_fakes::forge::FakeForge;
pub use gantry_fakes::inbox::FakeInbox;
pub use gantry_fakes::policy::FakePolicy;
pub use gantry_fakes::repo::FakeRepo;
pub use gantry_fakes::sandbox::FakeSandboxService;
pub use gantry_fakes::worker::StubWorkerAdapter;
pub use gantry_store::testing::{FakeClock, SeqEntropy};
pub use gantry_store::{Cas, ClaimOpts, EventFilter, Store, StoreDeps, TypePattern};
pub use serde_json::{json, Value};

pub const T0: i64 = 1_700_000_000_000;
pub const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
pub const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
pub const BASE: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";

pub fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}

pub fn repo_slug() -> RepoSlug {
    RepoSlug::parse("acme/web").unwrap()
}

pub fn project_id() -> ProjectId {
    PROJECT.parse().unwrap()
}

pub async fn append(
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

pub struct EventedRepo {
    pub inner: Arc<FakeRepo>,
    pub store: Store,
    /// 对外围栏（事件 seq）→ FakeRepo 自己的锁令牌。
    pub tokens: Mutex<HashMap<(WorktreeId, Seq), LockToken>>,
    pub gate: Arc<Gate>,
}

impl EventedRepo {
    pub fn inner_token(&self, token: &LockToken) -> LockToken {
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
        self.gate.pass(Point::BeforeCommit).await;
        req.lock = self.inner_token(&req.lock);
        let record = self.inner.commit_patch(ctx, req).await?;
        self.gate.pass(Point::AfterCommit).await;
        Ok(record)
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

pub struct GoldenSandbox {
    pub inner: FakeSandboxService,
    pub cas: Cas,
    pub jobs: Mutex<Vec<AttemptId>>,
    /// 为真时 worker 以退出码 10 结束（`exec.bad_job`：不自动重试，节点 failed）。
    pub fail: Mutex<bool>,
    pub gate: Arc<Gate>,
}

impl GoldenSandbox {
    pub async fn put(&self, bytes: &[u8]) -> FileArtifact {
        FileArtifact {
            sha256: self.cas.put_artifact(bytes).await.unwrap(),
            bytes: bytes.len() as u64,
        }
    }

    pub async fn golden(&self, spec: &SandboxSpec) -> JobOutcome {
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
        self.gate.pass(Point::AttemptStarted).await;
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
pub struct Snapshots(pub Mutex<Option<PullRequestRef>>);

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

pub fn snapshot(state: &str, head: GitOid) -> PullRequestRef {
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

pub struct FixedReserve;
impl ReservationPolicy for FixedReserve {
    fn reserve(&self, _: &gantry_core::entities::BudgetLedger, _: Tier, _: usize) -> u64 {
        1000
    }
}

pub fn actor_config() -> ActorConfig {
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

pub fn runner_config() -> RunnerConfig {
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

// ---------------------------------------------------------------------------
// 崩溃点闸门（#74）：端口调用到达已布防的点时挂起，测试随即中止运行时任务
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// `attempt.started` 已提交，沙盒尚未受理。
    AttemptStarted,
    /// 结果已收集，本地提交之前。
    BeforeCommit,
    /// 本地提交之后，`attempt.finished` 之前。
    AfterCommit,
    /// `JudgeAction` 已发出，`ActionJudged` 回流之前。
    Judged,
}

#[derive(Default)]
pub struct Gate {
    armed: Mutex<Option<Point>>,
    hit: tokio::sync::Notify,
}

impl Gate {
    /// 布防：下一次到达 `point` 的调用挂起（只触发一次）。
    pub fn arm(&self, point: Point) {
        *self.armed.lock().unwrap() = Some(point);
    }

    /// 等到布防点被触发。
    pub async fn hit(&self) {
        self.hit.notified().await;
    }

    pub async fn pass(&self, point: Point) {
        let hit = {
            let mut armed = self.armed.lock().unwrap();
            if *armed == Some(point) {
                *armed = None;
                true
            } else {
                false
            }
        };
        if hit {
            self.hit.notify_one();
            // 模拟进程在此处被杀：这次调用永不返回。
            std::future::pending::<()>().await;
        }
    }
}

pub struct GatedPolicy {
    pub inner: Arc<FakePolicy>,
    pub gate: Arc<Gate>,
}

#[async_trait]
impl PolicyPort for GatedPolicy {
    async fn decide_action(
        &self,
        req: ActionRequest,
        ctx: &ActorCtx,
    ) -> Result<ActionDecision, PolicyError> {
        self.gate.pass(Point::Judged).await;
        self.inner.decide_action(req, ctx).await
    }

    async fn request_approval(
        &self,
        spec: ApprovalSpec,
        ctx: ActorCtx,
    ) -> Result<Approval, PolicyError> {
        self.inner.request_approval(spec, ctx).await
    }

    async fn invalidate(
        &self,
        id: &ApprovalId,
        reason: InvalidateReason,
        ctx: &ActorCtx,
    ) -> Result<Approval, PolicyError> {
        self.inner.invalidate(id, reason, ctx).await
    }

    async fn live_approvals_for_pr(
        &self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Vec<Approval>, PolicyError> {
        self.inner.live_approvals_for_pr(repo, number).await
    }

    async fn decide_proposals(
        &self,
        proposal_ctx: ProposalContext,
        items: &[Proposal],
        ctx: &ActorCtx,
    ) -> Result<Vec<ProposalDecision>, PolicyError> {
        self.inner.decide_proposals(proposal_ctx, items, ctx).await
    }

    async fn evaluate_patch(
        &self,
        facts: &PatchFacts,
        added: &dyn AddedLineSource,
        ctx: &ActorCtx,
    ) -> Result<PatchPolicyVerdict, PolicyError> {
        self.inner.evaluate_patch(facts, added, ctx).await
    }

    async fn consume_patch_approval(
        &self,
        tx: &mut dyn TxHandle,
        approval: &ApprovalId,
        check: &PatchApprovalCheck,
        ctx: &ActorCtx,
    ) -> Result<PatchPermit, GateError> {
        self.inner
            .consume_patch_approval(tx, approval, check, ctx)
            .await
    }

    fn path_rules(&self, project: &ProjectId) -> Arc<dyn PathRuleSet> {
        self.inner.path_rules(project)
    }

    fn path_rules_snapshot(&self, project: &ProjectId) -> PathRulesSnapshot {
        self.inner.path_rules_snapshot(project)
    }

    fn scan_text(&self, text: &str) -> Vec<SecretFinding> {
        self.inner.scan_text(text)
    }

    async fn consume_for_execution(
        &self,
        approval: &ApprovalId,
        check: &ExecutionCheck,
        act: &ActionId,
        ctx: &ActorCtx,
    ) -> Result<ExecutionPermit, GateError> {
        self.inner
            .consume_for_execution(approval, check, act, ctx)
            .await
    }
}

pub struct World {
    pub store: Store,
    pub repo: Arc<FakeRepo>,
    pub policy: Arc<FakePolicy>,
    pub sandbox: Arc<GoldenSandbox>,
    pub snapshots: Arc<Snapshots>,
    pub gate: Arc<Gate>,
    /// 跨重启持久的端口包装（锁围栏映射、策略闸门），相当于 repo / policy 进程。
    pub repo_port: Arc<EventedRepo>,
    pub policy_port: Arc<GatedPolicy>,
}

impl World {
    pub async fn new() -> Self {
        let clock = Arc::new(FakeClock::new(T0));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(11))));
        let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
            .await
            .unwrap();
        let repo = Arc::new(FakeRepo::new(ts(T0)));
        repo.set_ref(
            project_id(),
            "refs/mirror/heads/main",
            GitOid::parse(BASE).unwrap(),
        );
        let gate = Arc::new(Gate::default());
        let policy = Arc::new(FakePolicy::new_in(store.clone()));
        Self {
            policy_port: Arc::new(GatedPolicy {
                inner: policy.clone(),
                gate: gate.clone(),
            }),
            repo_port: Arc::new(EventedRepo {
                inner: repo.clone(),
                store: store.clone(),
                tokens: Mutex::new(HashMap::new()),
                gate: gate.clone(),
            }),
            policy,
            sandbox: Arc::new(GoldenSandbox {
                inner: FakeSandboxService::new(),
                cas: store.cas(),
                jobs: Mutex::new(Vec::new()),
                fail: Mutex::new(false),
                gate: gate.clone(),
            }),
            snapshots: Arc::new(Snapshots::default()),
            gate,
            repo,
            store,
        }
    }

    /// 与 gantryd 的装配同形：组端口 → `EngineRuntime::new`。
    pub async fn runtime(&self) -> Arc<EngineRuntime> {
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
            policy: self.policy_port.clone(),
            forge,
            repo: self.repo_port.clone(),
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
    pub fn committed(&self) -> Option<GitOid> {
        let jobs = self.sandbox.jobs.lock().unwrap().clone();
        jobs.iter()
            .find_map(|job| self.repo.commit_record(job))
            .map(|record| record.commit_oid)
    }

    /// forge 执行器：认领并完成排队动作；create_pr 绑定 PR 12。返回完成的动作种类。
    pub async fn execute_outbox(&self, head: Option<GitOid>) -> Vec<ForgeActionKind> {
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

    pub async fn stages(&self, delivery: DeliveryId) -> Vec<(String, String)> {
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

pub fn new_delivery() -> NewDelivery {
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
pub fn script_merge(
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
pub async fn grant(world: &World, delivery: DeliveryId, id: ApprovalId) {
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
pub async fn drive_to_closed(world: &World, runtime: &EngineRuntime, delivery: DeliveryId) {
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
