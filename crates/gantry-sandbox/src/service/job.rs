//! job actor：一个 job 的完整生命周期（sandbox.md §7.1、§7.4、§7.5、§7.8、§7.9）。
//!
//! `Accepted → Queued → Creating → Created → Staging → Staged → Starting → Running →
//! Stopping → Stopped → Collecting → Collected → Destroying → Destroyed`；前置阶段失败 →
//! `Failed`，清理容器、staging 与令牌后返回 `Err(Infra{…})` / `InvalidSpec`。

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
use tokio::sync::{broadcast, mpsc, OwnedSemaphorePermit};
use tokio::time::Instant;

use super::meta::{Meta, Phase, TerminalReason};
use super::seams::CasKind;
use super::{infra, Command, Inner, Occupancy, Outcome};
use crate::driver::{DriverError, ExitFacts, SandboxDriver};
use crate::intake::IntakePlan;
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
    spec: SandboxSpec,
    layout: StagingLayout,
    commands: mpsc::UnboundedReceiver<Command>,
    progress: broadcast::Sender<ProgressSample>,
    occupancy: Arc<Mutex<Occupancy>>,
    meta: Meta,
    timings: Timings,
    container: Option<ContainerId>,
    created_event: bool,
    cancel_requested: bool,
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
        let meta = Meta::new(spec.job_id, spec.sandbox_id);
        Self {
            inner,
            ctx,
            spec,
            layout,
            commands,
            progress,
            occupancy,
            meta,
            timings: Timings::default(),
            container: None,
            created_event: false,
            cancel_requested: false,
        }
    }

    fn driver(&self) -> &D {
        &self.inner.deps.driver
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

    pub(super) async fn run(mut self) -> Outcome {
        let result = self.run_inner().await;
        self.set_occupancy(Occupancy::Done);
        result
    }

    async fn run_inner(&mut self) -> Outcome {
        self.phase(Phase::Queued)?;
        let permits = Arc::clone(&self.inner.permits);
        let permit: OwnedSemaphorePermit = tokio::select! {
            biased;
            command = self.commands.recv() => {
                if command == Some(Command::Cancel) {
                    self.cancel_requested = true;
                }
                return self.finish_without_start().await;
            }
            permit = permits.acquire_owned() => permit.map_err(|_| SandboxError::ShuttingDown)?,
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
        let grant = match self.prepare().await {
            Ok(grant) => grant,
            Err(err) => {
                self.cleanup_failed(&err).await;
                return Err(err);
            }
        };
        drop(grant);
        if self.poll_cancel() {
            // create / stage 阶段被取消：不 start，直接销毁，不取产物（§7.5）。
            return self.finish_without_start().await;
        }
        let id = self.container.clone().expect("prepared container");
        self.phase(Phase::Starting)?;
        if let Err(err) = self.driver().start(&id).await {
            let err = infra(InfraDetail::SandboxCreate, err.to_string());
            self.cleanup_failed(&err).await;
            return Err(err);
        }
        self.timings.started = Some(self.now());
        let wall = Duration::from_secs(self.spec.limits.wall_time_s);
        self.meta.deadline_ms = Some(self.now().as_millis() + wall.as_millis() as i64);
        self.phase(Phase::Running)?;
        let run = self.supervise(&id, wall).await?;
        self.collect_and_destroy(&id, run).await
    }

    /// 签发令牌 → 建目录 → 准备工作区 → 建容器（未启动）→ staging。
    async fn prepare(&mut self) -> Result<JobTokenGrant, SandboxError> {
        let spec = self.spec.clone();
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
        let spec = &self.spec;
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
                project_id: Some(self.spec.project_id),
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
        let spec = &self.spec;
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
            sandbox_id: self.spec.sandbox_id,
            final_status,
            reason: reason.to_owned(),
        };
        // 失败只影响投影展示，不影响终态；孤儿巡检兜底。
        let _ = self
            .append_event(
                EventType::SandboxDestroyed,
                self.spec.sandbox_id.to_string(),
                &data,
                format!("sandbox.destroyed:{}", self.spec.sandbox_id),
            )
            .await;
    }

    /// 监督运行：容器退出、墙钟、空闲、取消，先写入者胜。
    async fn supervise(&mut self, id: &ContainerId, wall: Duration) -> Result<Run, SandboxError> {
        let limits = ProgressLimits::new(
            &self.spec.io.progress,
            self.spec.io.logs_tail_bytes,
            self.inner.config.progress_sample,
        );
        let collector = Arc::new(Mutex::new(ProgressCollector::new(self.spec.job_id, limits)));
        let (activity_tx, mut activity_rx) = mpsc::unbounded_channel::<()>();
        let reader = self.spawn_log_reader(id, Arc::clone(&collector), activity_tx);

        let idle = Duration::from_secs(self.spec.timers.idle_timeout_s);
        let started = Instant::now();
        let deadline = started + wall;
        let mut idle_deadline = started + idle;
        let driver = Arc::clone(&self.inner.deps.driver);
        let wait_id = id.clone();
        let mut exit_wait = tokio::spawn(async move { driver.wait(&wait_id).await });
        let mut exit = None;
        let mut lost = false;
        let reason = loop {
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
                _ = tokio::time::sleep_until(idle_deadline) => break TerminalReason::IdleTimer,
                Some(()) = activity_rx.recv() => {
                    idle_deadline = Instant::now() + idle;
                }
            }
        };
        // 原因先写盘（fsync）再发信号：崩溃重启不会丢「为什么杀它」。
        let reason = self.meta.record_reason(reason);
        self.phase(Phase::Stopping)?;
        if reason != TerminalReason::RuntimeExit {
            let grace = Duration::from_secs(self.spec.timers.kill_grace_s);
            if self.driver().stop(id, grace).await.is_err() {
                let _ = self.driver().kill(id).await;
            }
            match exit_wait.await {
                Ok(Ok(facts)) => exit = Some(facts),
                _ => lost = true,
            }
        }
        // 停止期间到达的取消只记 cancel_requested，不改原因。
        self.poll_cancel();
        let mut reader = reader;
        if tokio::time::timeout(self.inner.config.log_drain_timeout, &mut reader)
            .await
            .is_err()
        {
            reader.abort();
        }
        self.timings.stopped = Some(self.now());
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
        activity: mpsc::UnboundedSender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let mut stream = self.driver().logs(id, None);
        let progress = self.progress.clone();
        let spool_path = self.layout.progress_spool();
        let store = self.inner.deps.store.clone();
        let clock = Arc::clone(&self.inner.deps.clock);
        let job_id = self.spec.job_id;
        let project_id = self.spec.project_id;
        let actor = self.ctx.actor.clone();
        tokio::spawn(async move {
            let mut spool = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&spool_path)
                .ok();
            while let Some(item) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                // 读取中断只影响展示，不影响终态（§9）。
                let Ok(chunk) = item else { break };
                let received_at = clock.now();
                let (outcome, samples) = {
                    let mut guard = collector.lock().unwrap_or_else(|p| p.into_inner());
                    let outcome =
                        guard.push_line(chunk.stream, &chunk.line, Instant::now(), received_at);
                    let samples = guard.sample(Instant::now());
                    (outcome, samples)
                };
                if outcome.activity() {
                    let _ = activity.send(());
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
            if let Some(file) = spool.as_mut() {
                let _ = file.sync_all();
            }
        })
    }

    async fn collect_and_destroy(&mut self, id: &ContainerId, run: Run) -> Outcome {
        self.phase(Phase::Collecting)?;
        let plan = IntakePlan::new(
            &self.spec.io.artifacts,
            self.spec.job.output.patch,
            self.spec.job.constraints.patch.as_ref(),
            self.inner.config.out_quota_bytes,
        );
        let intake = match self.driver().read_out(id, &self.layout, &plan).await {
            Ok(intake) => intake,
            Err(err) => {
                let err = infra(InfraDetail::Collect, err.to_string());
                self.cleanup_failed(&err).await;
                return Err(err);
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
            job_id: self.spec.job_id,
            sandbox_id: self.spec.sandbox_id,
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
        match self
            .inner
            .deps
            .egress
            .revoke_job_token(&self.spec.job_id, &self.ctx)
            .await
        {
            Ok(usage) => (
                usage.usage,
                usage.weighted_tokens,
                flags(usage.capped, usage.incomplete),
            ),
            // Egress 不可达：用量未知，标记为不完整（下界为 0），不让沙盒失败。
            Err(_) => (TokenUsage::default(), 0, flags(false, true)),
        }
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
            job_id: self.spec.job_id,
            sandbox_id: self.spec.sandbox_id,
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
        self.meta.phase = Phase::Failed;
        let _ = self.meta.save(&self.layout.meta_json());
        if let Some(id) = self.container.clone() {
            let _ = self.driver().kill(&id).await;
            let _ = self.driver().remove(&id).await;
        }
        let _ = self.revoke().await;
        let _ = self.driver().purge_staging(&self.layout).await;
        self.append_destroyed(SandboxStatus::Destroyed, &format!("failed: {err}"))
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
