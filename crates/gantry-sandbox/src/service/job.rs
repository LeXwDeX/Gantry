//! job actor：一个 job 的完整生命周期（sandbox.md §7.1、§7.4、§7.5、§7.8、§7.9）。
//!
//! `Accepted → Queued → Creating → Created → Staging → Staged → Starting → Running →
//! Stopping → Stopped → Collecting → Collected → Destroying → Destroyed`；前置阶段失败 →
//! `Failed`，清理容器、staging 与令牌后返回 `Err(Infra{…})` / `InvalidSpec`。
//!
//! 崩溃恢复（§7.10）用 [`JobActor::recovered`] 建 actor：没有 `SandboxSpec`，监督与 collect
//! 只依赖 `meta.json` 里的 [`Resume`] 参数。

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::egress::SandboxTier;
use gantry_core::entities::{SandboxStatus, Tier, TokenUsage, WorktreeSnapshot};
use gantry_core::events::{
    AttemptProgressKind, EventType, NewEvent, SandboxCreatedData, SandboxDestroyedData, Subject,
};
use gantry_core::ids::ProjectId;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::{IssueJobToken, JobTokenGrant, ProviderId};
use gantry_core::ports::repo::PrepareWorkspace;
use gantry_core::ports::sandbox::{
    EgressFlags, InfraDetail, JobOutcome, ProgressSample, SandboxError, SandboxSpec, TerminalCause,
    TerminalFacts, TimerKind, Timings, WorkspaceMode,
};
use gantry_core::time::Timestamp;
use gantry_store::Appended;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit};
use tokio::time::Instant;

use super::meta::{Meta, Phase, Resume, TerminalReason};
use super::seams::CasKind;
use super::{infra, Command, Inner, Occupancy, Outcome};
use crate::driver::{DriverError, ExitFacts, LogCursor, SandboxDriver};
use crate::intake::{IntakePlan, IntakeResult};
use crate::invariants::{validate_invariants, InvariantContext};
use crate::progress::{LineOutcome, ProgressCollector, ProgressLimits};
use crate::spec::{
    ContainerId, ContainerSpec, ImageDigest, LogPolicy, Mount, StagingLayout, DST_HOME, DST_INPUTS,
    DST_JOB_JSON, DST_OUT, DST_TMP, DST_WORK, LABEL_JOB_ID, LABEL_MANAGED, LABEL_SANDBOX_ID,
};
use crate::stage::materials::OsNonce;
use crate::stage::secrets::assert_env_clean;
use crate::stage::{stage, StageError, StageRequest};

/// `submit` 时就能发现的引擎缺陷（不进队列，直接 `InvalidSpec`）。
pub(super) fn check_spec(spec: &SandboxSpec) -> Result<(), SandboxError> {
    let invalid = |message: &str| Err(SandboxError::InvalidSpec(message.to_owned()));
    if spec.job.job_id != spec.job_id || spec.job.project_id != spec.project_id {
        return invalid("job.json ids do not match the sandbox spec");
    }
    if spec.delivery_id.is_some() == spec.conversation_id.is_some() {
        return invalid("exactly one of delivery_id or conversation_id is required");
    }
    let limits = &spec.limits;
    if !(limits.cpu_cores.is_finite() && limits.cpu_cores > 0.0)
        || limits.mem_mb == 0
        || limits.disk_mb == 0
        || limits.pids == 0
        || limits.wall_time_s == 0
    {
        return invalid("sandbox limits must be positive");
    }
    if spec.timers.idle_timeout_s == 0 || spec.timers.kill_grace_s == 0 {
        return invalid("sandbox timers must be positive");
    }
    if spec.exec.argv.is_empty() {
        return invalid("exec.argv is empty");
    }
    if spec.exec.token_env.model_base_url.len() != 1 {
        return invalid("exactly one model provider is supported per attempt (contract gap 2)");
    }
    if !spec.upstream.is_empty() {
        return invalid("upstream inputs are not supported yet (contract gap 8)");
    }
    sandbox_tier(spec.job.execution.tier)?;
    assert_env_clean(&spec.exec.env).map_err(stage_error)?;
    Ok(())
}

fn sandbox_tier(tier: Tier) -> Result<SandboxTier, SandboxError> {
    match tier {
        Tier::Planner => Ok(SandboxTier::Planner),
        Tier::Worker => Ok(SandboxTier::Worker),
        Tier::Utility => Err(SandboxError::InvalidSpec(
            "utility tier does not run in a sandbox".into(),
        )),
    }
}

fn stage_error(err: StageError) -> SandboxError {
    match err {
        StageError::InvalidSpec(message) => SandboxError::InvalidSpec(message),
        StageError::Io(message) => infra(InfraDetail::WorkspacePrep, message),
    }
}

fn driver_infra(detail: InfraDetail) -> impl Fn(DriverError) -> SandboxError {
    move |err| infra(detail, err.to_string())
}

pub(super) struct JobActor<D> {
    inner: Arc<Inner<D>>,
    ctx: ActorCtx,
    /// 新提交的 job 才有；恢复出的 job 只有 `meta.resume`。
    spec: Option<SandboxSpec>,
    project_id: ProjectId,
    layout: StagingLayout,
    commands: mpsc::UnboundedReceiver<Command>,
    progress: broadcast::Sender<ProgressSample>,
    occupancy: Arc<Mutex<Occupancy>>,
    meta: Meta,
    timings: Timings,
    container: Option<ContainerId>,
    created_event: bool,
    cancel_requested: bool,
    /// `revoke` 的结果只取一次（错误出口的统一清理可能再次调用）。
    revoked: Option<(TokenUsage, u64, EgressFlags)>,
    /// 已执行过 `cleanup_failed`。
    cleaned: bool,
}

/// 日志读取连续失败多少次后放弃（§9：只影响展示，不影响终态）。
const LOG_RECONNECTS: u32 = 5;

/// 日志读取任务发给监督循环的信号。
enum LogSignal {
    /// 一行合法事件：重置空闲计时器。
    Activity,
    /// 读取放弃：空闲计时器失去依据，停用，只剩墙钟与退出监督。
    Gone,
}

/// 恢复出的 job 从哪一步接着走（§7.10）。
pub(super) enum RecoverMode {
    /// 容器还在（运行中或已退出）：继续监督，从头重读日志按 seq 去重，然后 collect。
    Supervise(ContainerId),
    /// 容器已不在、`out/` 已读出：直接 collect。`lost` 为假表示容器是本服务在
    /// `Destroying` 相位自己删的（终止事实取 `meta.json`），否则 `cause = Lost`。
    Collect {
        id: ContainerId,
        intake: IntakeResult,
        lost: bool,
    },
    /// 启动前已请求取消：重出取消结果。
    CancelledBeforeStart,
    /// 受理后丢失：吊销、记 `sandbox.destroyed(lost)`，`wait` 返回 `Infra{Lost}`。
    Lost,
}

/// 运行结束时的事实。
struct Run {
    reason: TerminalReason,
    exit: Option<ExitFacts>,
    lost: bool,
    collector: ProgressCollector,
}

impl<D: SandboxDriver + 'static> JobActor<D> {
    pub(super) fn new(
        inner: Arc<Inner<D>>,
        ctx: ActorCtx,
        spec: SandboxSpec,
        layout: StagingLayout,
        commands: mpsc::UnboundedReceiver<Command>,
        progress: broadcast::Sender<ProgressSample>,
        occupancy: Arc<Mutex<Occupancy>>,
    ) -> Self {
        let mut meta = Meta::new(spec.job_id, spec.sandbox_id);
        meta.resume = Some(Resume {
            project_id: spec.project_id,
            io: spec.io.clone(),
            intake: IntakePlan::new(
                &spec.io.artifacts,
                spec.job.output.patch,
                spec.job.constraints.patch.as_ref(),
                inner.config.out_quota_bytes,
            ),
            wall_time_s: spec.limits.wall_time_s,
            idle_timeout_s: spec.timers.idle_timeout_s,
            kill_grace_s: spec.timers.kill_grace_s,
        });
        Self {
            inner,
            ctx,
            project_id: spec.project_id,
            spec: Some(spec),
            layout,
            commands,
            progress,
            occupancy,
            meta,
            timings: Timings::default(),
            container: None,
            created_event: false,
            cancel_requested: false,
            revoked: None,
            cleaned: false,
        }
    }

    /// 恢复出的 job（没有 `SandboxSpec`）。`meta.resume` 必须存在。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn recovered(
        inner: Arc<Inner<D>>,
        ctx: ActorCtx,
        project_id: ProjectId,
        layout: StagingLayout,
        meta: Meta,
        container: Option<ContainerId>,
        created_event: bool,
        commands: mpsc::UnboundedReceiver<Command>,
        progress: broadcast::Sender<ProgressSample>,
        occupancy: Arc<Mutex<Occupancy>>,
    ) -> Self {
        Self {
            inner,
            ctx,
            spec: None,
            project_id,
            layout,
            commands,
            progress,
            occupancy,
            timings: meta.timings.into(),
            cancel_requested: meta.cancel_requested,
            meta,
            container,
            created_event,
            revoked: None,
            cleaned: false,
        }
    }

    fn driver(&self) -> &D {
        &self.inner.deps.driver
    }

    fn resume(&self) -> &Resume {
        self.meta
            .resume
            .as_ref()
            .expect("resume parameters are written at accept")
    }

    fn fresh_spec(&self) -> &SandboxSpec {
        self.spec
            .as_ref()
            .expect("only freshly submitted jobs prepare")
    }

    fn now(&self) -> Timestamp {
        self.inner.deps.clock.now()
    }

    fn set_occupancy(&self, value: Occupancy) {
        *self.occupancy.lock().unwrap_or_else(|p| p.into_inner()) = value;
    }

    fn phase(&mut self, phase: Phase) -> Result<(), SandboxError> {
        self.meta.phase = phase;
        self.meta.cancel_requested = self.cancel_requested;
        self.meta.timings = self.timings.into();
        self.meta
            .save(&self.layout.meta_json())
            .map_err(|err| infra(InfraDetail::WorkspacePrep, format!("meta.json: {err}")))
    }

    /// 非阻塞地取走已到达的取消命令。
    fn poll_cancel(&mut self) -> bool {
        while let Ok(Command::Cancel) = self.commands.try_recv() {
            self.cancel_requested = true;
        }
        self.cancel_requested
    }

    pub(super) async fn run(mut self, permit: oneshot::Receiver<OwnedSemaphorePermit>) -> Outcome {
        let result = self.run_inner(permit).await;
        // 所有错误出口统一清理：容器、令牌、staging 不随许可一起泄漏。
        if let Err(err) = &result {
            self.cleanup_failed(err).await;
        }
        self.set_occupancy(Occupancy::Done);
        result
    }

    /// 恢复出的 job 的入口（§7.10）。
    pub(super) async fn run_recovered(mut self, mode: RecoverMode) -> Outcome {
        let result = self.recover_inner(mode).await;
        if let Err(err) = &result {
            self.cleanup_failed(err).await;
        }
        self.set_occupancy(Occupancy::Done);
        result
    }

    async fn recover_inner(&mut self, mode: RecoverMode) -> Outcome {
        match mode {
            RecoverMode::Supervise(id) => {
                // 重启后的新服务：占一个许可（不排队，容器早已存在）。
                let permit = Arc::clone(&self.inner.permits).try_acquire_owned().ok();
                self.set_occupancy(Occupancy::Holding);
                let wall_time = Duration::from_secs(self.resume().wall_time_s);
                let now = self.now().as_millis();
                let deadline_ms = *self
                    .meta
                    .deadline_ms
                    .get_or_insert(now + wall_time.as_millis() as i64);
                if self.timings.started.is_none() {
                    self.timings.started = Some(self.now());
                }
                self.phase(Phase::Running)?;
                // deadline 按持久化的 UTC 时刻重建，不因重启而延长。
                let wall = Duration::from_millis(deadline_ms.saturating_sub(now).max(0) as u64);
                let preset = self.meta.terminal_reason;
                let run = self.supervise(&id, wall, preset).await?;
                let outcome = self.collect_and_destroy(&id, run, None).await;
                drop(permit);
                outcome
            }
            RecoverMode::Collect { id, intake, lost } => {
                let reason = self.meta.record_reason(TerminalReason::RuntimeExit);
                let collector = self.resumed_collector();
                let run = Run {
                    reason,
                    exit: self.meta.exit,
                    lost,
                    collector,
                };
                self.collect_and_destroy(&id, run, Some(intake)).await
            }
            RecoverMode::CancelledBeforeStart => self.finish_without_start().await,
            RecoverMode::Lost => Err(infra(
                InfraDetail::Lost,
                "sandbox lost across a restart (container and out/ unavailable)",
            )),
        }
    }

    fn progress_limits(&self) -> ProgressLimits {
        let io = &self.resume().io;
        ProgressLimits::new(
            &io.progress,
            io.logs_tail_bytes,
            self.inner.config.progress_sample,
        )
    }

    /// 从 `progress.spool.jsonl` 重建 seq 与计数；新 job 的 spool 为空，等同新建。
    fn resumed_collector(&self) -> ProgressCollector {
        let spool = std::fs::read(self.layout.progress_spool()).unwrap_or_default();
        ProgressCollector::resume_from_spool(self.meta.job_id, self.progress_limits(), &spool)
    }

    async fn run_inner(&mut self, permit: oneshot::Receiver<OwnedSemaphorePermit>) -> Outcome {
        self.phase(Phase::Queued)?;
        let permit: OwnedSemaphorePermit = tokio::select! {
            biased;
            command = self.commands.recv() => {
                if command == Some(Command::Cancel) {
                    self.cancel_requested = true;
                }
                return self.finish_without_start().await;
            }
            permit = permit => permit.map_err(|_| SandboxError::ShuttingDown)?,
        };
        self.set_occupancy(Occupancy::Holding);
        let outcome = self.run_with_permit().await;
        drop(permit);
        outcome
    }

    async fn run_with_permit(&mut self) -> Outcome {
        if self.poll_cancel() {
            return self.finish_without_start().await;
        }
        let grant = self.prepare().await?;
        drop(grant);
        if self.poll_cancel() {
            // create / stage 阶段被取消：不 start，直接销毁，不取产物（§7.5）。
            return self.finish_without_start().await;
        }
        let id = self.container.clone().expect("prepared container");
        self.phase(Phase::Starting)?;
        self.driver()
            .start(&id)
            .await
            .map_err(driver_infra(InfraDetail::SandboxCreate))?;
        self.timings.started = Some(self.now());
        let wall = Duration::from_secs(self.resume().wall_time_s);
        self.meta.deadline_ms = Some(self.now().as_millis() + wall.as_millis() as i64);
        self.phase(Phase::Running)?;
        let run = self.supervise(&id, wall, None).await?;
        self.collect_and_destroy(&id, run, None).await
    }

    /// 签发令牌 → 建目录 → 准备工作区 → 建容器（未启动）→ staging。
    async fn prepare(&mut self) -> Result<JobTokenGrant, SandboxError> {
        let spec = self.fresh_spec().clone();
        self.phase(Phase::Creating)?;
        let image = ImageDigest::local_id(spec.exec.image_digest);
        let present = self
            .driver()
            .image_present(&image)
            .await
            .map_err(driver_infra(InfraDetail::SandboxCreate))?;
        if !present {
            return Err(infra(
                InfraDetail::SandboxCreate,
                format!("image {image} is not present (never pulled at run time)"),
            ));
        }

        // 环境变量只能在创建时设定，所以令牌先于容器（§7.4、§7.9）。拿不到就不启动。
        let provider = spec
            .exec
            .token_env
            .model_base_url
            .keys()
            .next()
            .cloned()
            .expect("checked in check_spec");
        // 契约缺口 11：`SandboxSpec` 不带权重快照所属的配置修订，暂取 Egress 当前已生效的
        // 修订（`weights` 由同一份配置推出）；Egress 不可达即不启动（失败闭合）。
        let config_rev = self
            .inner
            .deps
            .egress
            .status()
            .await
            .map_err(|err| infra(InfraDetail::Egress, err.to_string()))?
            .applied_rev;
        let request = IssueJobToken {
            job_id: spec.job_id,
            sandbox_id: spec.sandbox_id,
            project_id: spec.project_id,
            delivery_id: spec.delivery_id,
            conversation_id: spec.conversation_id,
            tier: sandbox_tier(spec.job.execution.tier)?,
            provider: ProviderId::new(provider.clone()),
            profile: spec.egress.profile.clone(),
            groups: spec.egress.groups.clone(),
            tokens_total: spec.egress.tokens_total,
            weights: spec.egress.weights,
            wall_time_s: spec.limits.wall_time_s,
            config_rev,
        };
        // 先记「已签发」：签发请求发出后崩溃也会在恢复时吊销（吊销是幂等的）。
        self.meta.token_issued = true;
        self.phase(Phase::Creating)?;
        let grant = self
            .inner
            .deps
            .egress
            .issue_job_token(request, &self.ctx)
            .await
            .map_err(|err| infra(InfraDetail::Egress, err.to_string()))?;

        self.driver()
            .prepare_dirs(&self.layout)
            .await
            .map_err(driver_infra(InfraDetail::WorkspacePrep))?;
        let prepared = self
            .inner
            .deps
            .repo
            .prepare_workspace(
                &self.ctx,
                PrepareWorkspace {
                    project_id: spec.project_id,
                    job_id: spec.job_id,
                    base_sha: spec.workspace.base_sha,
                    pr_base_sha: spec.workspace.pr_base_sha,
                    dest: self.layout.work(),
                    max_bytes: spec.limits.disk_mb.saturating_mul(1024 * 1024),
                    want_diff: spec.workspace.pr_base_sha.is_some(),
                },
            )
            .await
            .map_err(|err| infra(InfraDetail::WorkspacePrep, err.to_string()))?;

        let container = self.container_spec(&grant, &provider)?;
        validate_invariants(
            &container,
            InvariantContext {
                layout: &self.layout,
                job_token: Some(grant.job_token.expose_secret()),
            },
        )
        .map_err(|err| SandboxError::InvalidSpec(format!("container spec: {err}")))?;
        let id = self
            .driver()
            .create(&container)
            .await
            .map_err(driver_infra(InfraDetail::SandboxCreate))?;
        self.container = Some(id.clone());
        self.meta.container_id = Some(id.to_string());
        self.timings.created = Some(self.now());
        self.phase(Phase::Created)?;
        self.append_created().await?;

        self.phase(Phase::Staging)?;
        let schema = self
            .inner
            .deps
            .schemas
            .get(&spec.job.output.structured_schema.id)
            .ok_or_else(|| {
                SandboxError::InvalidSpec(format!(
                    "unknown structured output schema {}",
                    spec.job.output.structured_schema.id
                ))
            })?;
        let diff = match prepared.diff_patch {
            Some(digest) => Some(
                self.inner
                    .deps
                    .artifacts
                    .get(&digest)
                    .map_err(|err| infra(InfraDetail::WorkspacePrep, err))?
                    .ok_or_else(|| {
                        infra(InfraDetail::WorkspacePrep, "diff.patch missing from CAS")
                    })?,
            ),
            None => None,
        };
        let request = StageRequest {
            job: &spec.job,
            raw_materials: &spec.raw_materials,
            materials_io: &spec.io.materials,
            output_schema: &schema,
            diff_patch: diff.as_deref(),
            upstream: &[],
        };
        stage(&self.layout, &request, &mut OsNonce).map_err(stage_error)?;
        self.timings.staged = Some(self.now());
        self.phase(Phase::Staged)?;
        Ok(grant)
    }

    fn container_spec(
        &self,
        grant: &JobTokenGrant,
        provider: &str,
    ) -> Result<ContainerSpec, SandboxError> {
        let spec = self.fresh_spec();
        let config = &self.inner.config;
        let mut env = spec.exec.env.clone();
        for (provider_id, var) in &spec.exec.token_env.model_base_url {
            let url = grant.model_base_urls.get(provider_id).ok_or_else(|| {
                infra(
                    InfraDetail::Egress,
                    format!("egress returned no base URL for provider {provider}"),
                )
            })?;
            env.insert(var.clone(), url.to_owned());
        }
        for var in &spec.exec.token_env.proxy {
            env.insert(
                var.clone(),
                grant.connect_proxy_url.expose_secret().to_owned(),
            );
        }
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_MANAGED.to_owned(), "1".to_owned());
        labels.insert(LABEL_JOB_ID.to_owned(), spec.job_id.to_string());
        labels.insert(LABEL_SANDBOX_ID.to_owned(), spec.sandbox_id.to_string());
        let layout = &self.layout;
        Ok(ContainerSpec {
            name: layout.container_name(),
            labels,
            image: ImageDigest::local_id(spec.exec.image_digest),
            argv: spec.exec.argv.clone(),
            env,
            token_env: spec.exec.token_env.clone(),
            mounts: vec![
                Mount::Bind {
                    src: layout.job_json(),
                    dst: DST_JOB_JSON.into(),
                    ro: true,
                },
                Mount::Bind {
                    src: layout.inputs(),
                    dst: DST_INPUTS.into(),
                    ro: true,
                },
                Mount::Bind {
                    src: layout.work(),
                    dst: DST_WORK.into(),
                    ro: spec.workspace.mode == WorkspaceMode::Ro,
                },
                Mount::Bind {
                    src: layout.out(),
                    dst: DST_OUT.into(),
                    ro: false,
                },
                Mount::Tmpfs {
                    dst: DST_HOME.into(),
                    size_mb: config.tmpfs_home_mb,
                },
                Mount::Tmpfs {
                    dst: DST_TMP.into(),
                    size_mb: config.tmpfs_tmp_mb,
                },
            ],
            limits: spec.limits.clone(),
            network: config.network.clone(),
            runtime: config.runtime,
            stop_grace: Duration::from_secs(spec.timers.kill_grace_s),
            log: LogPolicy {
                max_bytes: config.log_max_bytes,
            },
        })
    }

    async fn append_event<T: Serialize>(
        &self,
        ty: EventType,
        subject: String,
        data: &T,
        dedup_key: String,
    ) -> Result<(), SandboxError> {
        let store_err =
            |err: gantry_store::StoreError| infra(InfraDetail::SandboxCreate, err.to_string());
        let subject = Subject::parse(&subject).map_err(|_| {
            infra(
                InfraDetail::SandboxCreate,
                "event subject is not a prefixed id",
            )
        })?;
        let data = serde_json::to_value(data)
            .map_err(|err| infra(InfraDetail::SandboxCreate, err.to_string()))?;
        let mut tx = self
            .inner
            .deps
            .store
            .begin_write()
            .await
            .map_err(store_err)?;
        let appended = tx
            .append(NewEvent {
                ty,
                subject,
                project_id: Some(self.project_id),
                actor: self.ctx.actor.clone(),
                causation_id: self.ctx.causation_id,
                correlation_id: None,
                data,
                dedup_key: Some(dedup_key),
            })
            .await
            .map_err(store_err)?;
        if let Appended::New(_) = appended {
            tx.commit().await.map_err(store_err)?;
        }
        Ok(())
    }

    async fn append_created(&mut self) -> Result<(), SandboxError> {
        let spec = self.fresh_spec();
        let data = SandboxCreatedData {
            sandbox_id: spec.sandbox_id,
            attempt_id: spec.job_id,
            driver: "podman".into(),
            runtime: self.inner.config.runtime.as_str().into(),
            image_digest: spec.exec.image_digest,
            limits: spec.limits.clone(),
            egress_profile: spec.egress.profile.to_string(),
            worktree_snapshot: WorktreeSnapshot {
                mirror: spec.workspace.repo.to_string(),
                base_sha: spec.workspace.base_sha,
                pr_base_sha: spec.workspace.pr_base_sha,
            },
            trust: spec.trust,
        };
        self.append_event(
            EventType::SandboxCreated,
            spec.sandbox_id.to_string(),
            &data,
            format!("sandbox.created:{}", spec.sandbox_id),
        )
        .await?;
        self.created_event = true;
        Ok(())
    }

    async fn append_destroyed(&self, final_status: SandboxStatus, reason: &str) {
        if !self.created_event {
            return;
        }
        let data = SandboxDestroyedData {
            sandbox_id: self.meta.sandbox_id,
            final_status,
            reason: reason.to_owned(),
        };
        // 失败只影响投影展示，不影响终态；孤儿巡检兜底。
        let _ = self
            .append_event(
                EventType::SandboxDestroyed,
                self.meta.sandbox_id.to_string(),
                &data,
                format!("sandbox.destroyed:{}", self.meta.sandbox_id),
            )
            .await;
    }

    /// 监督运行：容器退出、墙钟、空闲、取消，先写入者胜。`preset` 是崩溃前已写盘的
    /// 终止原因：直接按它停（补发信号），不再等计时器。
    async fn supervise(
        &mut self,
        id: &ContainerId,
        wall: Duration,
        preset: Option<TerminalReason>,
    ) -> Result<Run, SandboxError> {
        let collector = Arc::new(Mutex::new(self.resumed_collector()));
        let (activity_tx, mut activity_rx) = mpsc::unbounded_channel::<LogSignal>();
        let reader = self.spawn_log_reader(id, Arc::clone(&collector), activity_tx);

        let idle = Duration::from_secs(self.resume().idle_timeout_s);
        let started = Instant::now();
        let deadline = started + wall;
        let mut idle_deadline = started + idle;
        let mut idle_enabled = true;
        let driver = Arc::clone(&self.inner.deps.driver);
        let wait_id = id.clone();
        let mut exit_wait = self.inner.spawn(async move { driver.wait(&wait_id).await });
        let mut exit = None;
        let mut lost = false;
        let reason = if let Some(reason) = preset {
            reason
        } else {
            loop {
                tokio::select! {
                    biased;
                    result = &mut exit_wait => {
                        match result {
                            Ok(Ok(facts)) => exit = Some(facts),
                            _ => lost = true,
                        }
                        break TerminalReason::RuntimeExit;
                    }
                    command = self.commands.recv() => {
                        if command == Some(Command::Cancel) {
                            self.cancel_requested = true;
                            break TerminalReason::Cancel;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => break TerminalReason::WallTimer,
                    _ = tokio::time::sleep_until(idle_deadline), if idle_enabled => {
                        break TerminalReason::IdleTimer
                    }
                    Some(signal) = activity_rx.recv() => match signal {
                        LogSignal::Activity => idle_deadline = Instant::now() + idle,
                        LogSignal::Gone => idle_enabled = false,
                    },
                }
            }
        };
        // 原因先写盘（fsync）再发信号：崩溃重启不会丢「为什么杀它」。
        let reason = self.meta.record_reason(reason);
        self.phase(Phase::Stopping)?;
        if reason != TerminalReason::RuntimeExit {
            let grace = Duration::from_secs(self.resume().kill_grace_s);
            if self.driver().stop(id, grace).await.is_err() {
                let _ = self.driver().kill(id).await;
            }
        }
        // 循环里没等到退出（计时器 / 取消，或恢复时沿用已写盘的原因）：现在等。
        if exit.is_none() && !lost {
            match exit_wait.await {
                Ok(Ok(facts)) => exit = Some(facts),
                _ => lost = true,
            }
        }
        let mut reader = reader;
        if tokio::time::timeout(self.inner.config.log_drain_timeout, &mut reader)
            .await
            .is_err()
        {
            reader.abort();
        }
        // 停止期间（含等日志排空）到达的取消只记 cancel_requested，不改原因。
        self.poll_cancel();
        self.timings.stopped = Some(self.now());
        self.meta.exit = exit;
        self.meta.lost = lost;
        self.phase(Phase::Stopped)?;
        let collector = collector.lock().unwrap_or_else(|p| p.into_inner()).clone();
        Ok(Run {
            reason,
            exit,
            lost,
            collector,
        })
    }

    fn spawn_log_reader(
        &self,
        id: &ContainerId,
        collector: Arc<Mutex<ProgressCollector>>,
        activity: mpsc::UnboundedSender<LogSignal>,
    ) -> tokio::task::JoinHandle<()> {
        let driver = Arc::clone(&self.inner.deps.driver);
        let id = id.clone();
        let progress = self.progress.clone();
        let spool_path = self.layout.progress_spool();
        let store = self.inner.deps.store.clone();
        let clock = Arc::clone(&self.inner.deps.clock);
        let job_id = self.meta.job_id;
        let project_id = self.project_id;
        let actor = self.ctx.actor.clone();
        self.inner.spawn(async move {
            let mut spool = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&spool_path)
                .ok();
            let mut cursor: Option<LogCursor> = None;
            let mut failures = 0u32;
            'connect: loop {
                let mut stream = driver.logs(&id, cursor);
                loop {
                    let chunk = match std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await
                    {
                        // 容器停止且读完；容器不在了由退出监督处理。
                        None | Some(Err(DriverError::NotFound)) => break 'connect,
                        // 读取中断：从上次 cursor 之后重连（§9）。
                        Some(Err(_)) => break,
                        Some(Ok(chunk)) => chunk,
                    };
                    failures = 0;
                    cursor = Some(chunk.cursor);
                    let received_at = clock.now();
                    let (outcome, samples) = {
                        let mut guard = collector.lock().unwrap_or_else(|p| p.into_inner());
                        let outcome =
                            guard.push_line(chunk.stream, &chunk.line, Instant::now(), received_at);
                        let samples = guard.sample(Instant::now());
                        (outcome, samples)
                    };
                    if outcome.activity() {
                        let _ = activity.send(LogSignal::Activity);
                    }
                    if let LineOutcome::Accepted(event) = &outcome {
                        if let Some(file) = spool.as_mut() {
                            let _ = file.write_all(&event.spool_line());
                        }
                    }
                    for sample in samples {
                        let _ = progress.send(sample.clone());
                        let _ = append_progress(&store, &actor, project_id, job_id, &sample).await;
                    }
                }
                failures += 1;
                if failures > LOG_RECONNECTS {
                    let _ = activity.send(LogSignal::Gone);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100 << (failures - 1))).await;
            }
            if let Some(file) = spool.as_mut() {
                let _ = file.sync_all();
            }
        })
    }

    async fn collect_and_destroy(
        &mut self,
        id: &ContainerId,
        run: Run,
        intake: Option<IntakeResult>,
    ) -> Outcome {
        self.phase(Phase::Collecting)?;
        let intake = match intake {
            Some(intake) => intake,
            None => {
                let plan = self.resume().intake;
                self.driver()
                    .read_out(id, &self.layout, &plan)
                    .await
                    .map_err(driver_infra(InfraDetail::Collect))?
            }
        };
        let put = |bytes: &[u8], kind: CasKind| {
            self.inner
                .deps
                .artifacts
                .put(bytes, kind)
                .map_err(|err| infra(InfraDetail::Collect, err))
        };
        let mut files = BTreeMap::new();
        for (name, bytes) in &intake.files {
            files.insert(name.clone(), put(bytes, CasKind::Artifact)?);
        }
        let spool = std::fs::read(self.layout.progress_spool()).unwrap_or_default();
        let progress = if spool.is_empty() {
            None
        } else {
            Some(put(&spool, CasKind::Progress)?)
        };
        let stdout_tail = put(&run.collector.stdout_tail().contents(), CasKind::Artifact)?;
        let stderr_tail = put(&run.collector.stderr_tail().contents(), CasKind::Artifact)?;
        self.timings.collected = Some(self.now());
        self.phase(Phase::Collected)?;

        let (usage, weighted, egress) = self.revoke().await;
        let terminal = terminal_facts(&run, self.cancel_requested);
        let outcome = JobOutcome {
            job_id: self.meta.job_id,
            sandbox_id: self.meta.sandbox_id,
            partial: terminal.cause != TerminalCause::Exited,
            terminal,
            files,
            unexpected: intake.unexpected,
            progress,
            progress_stats: run.collector.stats(),
            stdout_tail,
            stderr_tail,
            usage_metered: usage,
            weighted_tokens: weighted,
            egress,
            timings: self.timings,
        };

        self.phase(Phase::Destroying)?;
        // 销毁失败留给孤儿巡检，不影响分类（§9）。
        let _ = self.driver().remove(id).await;
        let final_status = if run.lost {
            SandboxStatus::Lost
        } else {
            SandboxStatus::Destroyed
        };
        self.append_destroyed(final_status, cause_text(outcome.terminal.cause))
            .await;
        self.phase(Phase::Destroyed)?;
        Ok(outcome)
    }

    /// 吊销令牌并取最终计量；在所有路径上都执行（§7.9）。
    async fn revoke(&mut self) -> (TokenUsage, u64, EgressFlags) {
        if !self.meta.token_issued {
            return (TokenUsage::default(), 0, flags(false, true));
        }
        if let Some(revoked) = &self.revoked {
            return revoked.clone();
        }
        let revoked = match self
            .inner
            .deps
            .egress
            .revoke_job_token(&self.meta.job_id, &self.ctx)
            .await
        {
            Ok(usage) => (
                usage.usage,
                usage.weighted_tokens,
                flags(usage.capped, usage.incomplete),
            ),
            // Egress 不可达：用量未知，标记为不完整（下界为 0），不让沙盒失败。
            Err(_) => (TokenUsage::default(), 0, flags(false, true)),
        };
        self.revoked = Some(revoked.clone());
        revoked
    }

    /// 尚未启动（排队中或 create / stage 阶段被取消）：销毁、吊销、不取产物。
    async fn finish_without_start(&mut self) -> Outcome {
        self.cancel_requested = true;
        let _ = self.meta.record_reason(TerminalReason::Cancel);
        self.phase(Phase::Destroying)?;
        if let Some(id) = self.container.clone() {
            let _ = self.driver().remove(&id).await;
        }
        let (usage, weighted, mut egress) = self.revoke().await;
        if !self.meta.token_issued {
            egress.incomplete = false;
        }
        let _ = self.driver().purge_staging(&self.layout).await;
        // purge 会删掉 meta.json；重建它，release 之前仍能判断受理过。
        let _ = std::fs::create_dir_all(self.layout.dir());
        self.append_destroyed(SandboxStatus::Destroyed, "cancelled_before_start")
            .await;
        self.phase(Phase::Destroyed)?;
        let empty = self
            .inner
            .deps
            .artifacts
            .put(b"", CasKind::Artifact)
            .map_err(|err| infra(InfraDetail::Collect, err))?;
        Ok(JobOutcome {
            job_id: self.meta.job_id,
            sandbox_id: self.meta.sandbox_id,
            terminal: TerminalFacts {
                cause: TerminalCause::KilledCancel,
                exit_code: None,
                signal: None,
                oom: false,
                timer: None,
                cancel_requested: true,
            },
            partial: true,
            files: BTreeMap::new(),
            unexpected: Vec::new(),
            progress: None,
            progress_stats: Default::default(),
            stdout_tail: empty.clone(),
            stderr_tail: empty,
            usage_metered: usage,
            weighted_tokens: weighted,
            egress,
            timings: self.timings,
        })
    }

    /// 前置阶段失败：容器、staging、令牌全部清理（§12.D 最后一条）。
    async fn cleanup_failed(&mut self, err: &SandboxError) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        self.meta.phase = Phase::Failed;
        let _ = self.meta.save(&self.layout.meta_json());
        if let Some(id) = self.container.clone() {
            let _ = self.driver().kill(&id).await;
            let _ = self.driver().remove(&id).await;
        }
        let _ = self.revoke().await;
        let _ = self.driver().purge_staging(&self.layout).await;
        let final_status = match err {
            SandboxError::Infra {
                detail: InfraDetail::Lost,
                ..
            } => SandboxStatus::Lost,
            _ => SandboxStatus::Destroyed,
        };
        self.append_destroyed(final_status, &format!("failed: {err}"))
            .await;
    }
}

fn flags(capped: bool, incomplete: bool) -> EgressFlags {
    EgressFlags {
        capped,
        upstream_error_summary: None,
        incomplete,
    }
}

fn cause_text(cause: TerminalCause) -> &'static str {
    match cause {
        TerminalCause::Exited => "exited",
        TerminalCause::KilledTimer => "killed_timer",
        TerminalCause::KilledCancel => "killed_cancel",
        TerminalCause::KilledRuntime => "killed_runtime",
        TerminalCause::Lost => "lost",
    }
}

/// 服务端观察到的事实 → `TerminalFacts`（不判 failure_class，那是 engine 的事）。
fn terminal_facts(run: &Run, cancel_requested: bool) -> TerminalFacts {
    let exit = run.exit.unwrap_or(ExitFacts {
        exit_code: None,
        signal: None,
        oom_killed: false,
        finished_at: None,
    });
    let (cause, timer) = match run.reason {
        _ if run.lost => (TerminalCause::Lost, None),
        TerminalReason::Cancel => (TerminalCause::KilledCancel, None),
        TerminalReason::WallTimer => (TerminalCause::KilledTimer, Some(TimerKind::Wall)),
        TerminalReason::IdleTimer => (TerminalCause::KilledTimer, Some(TimerKind::Idle)),
        TerminalReason::RuntimeExit if exit.oom_killed || exit.signal.is_some() => {
            (TerminalCause::KilledRuntime, None)
        }
        TerminalReason::RuntimeExit => (TerminalCause::Exited, None),
    };
    TerminalFacts {
        cause,
        exit_code: exit.exit_code,
        signal: exit.signal,
        oom: exit.oom_killed,
        timer,
        cancel_requested,
    }
}

async fn append_progress(
    store: &gantry_store::Store,
    actor: &gantry_core::entities::Actor,
    project_id: gantry_core::ids::ProjectId,
    job_id: gantry_core::ids::AttemptId,
    sample: &ProgressSample,
) -> Result<(), gantry_store::StoreError> {
    // `attempt.progress` 按 type 白名单校验字段：只写 stage 需要的键，不带 null。
    let data = serde_json::json!({
        "v": 1,
        "seq": sample.seq,
        "type": AttemptProgressKind::Stage,
        "stage": sample.stage,
        "detail": sample.detail.as_untrusted_str(),
    });
    let Ok(subject) = Subject::parse(&job_id.to_string()) else {
        return Ok(());
    };
    let mut tx = store.begin_write().await?;
    let appended = tx
        .append(NewEvent {
            ty: EventType::AttemptProgress,
            subject,
            project_id: Some(project_id),
            actor: actor.clone(),
            causation_id: None,
            correlation_id: None,
            data,
            dedup_key: Some(format!("attempt.progress:{job_id}:{}", sample.seq)),
        })
        .await?;
    if let Appended::New(_) = appended {
        tx.commit().await?;
    }
    Ok(())
}
