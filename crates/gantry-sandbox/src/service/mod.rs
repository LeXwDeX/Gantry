//! 引擎面 `SandboxService` 的实现（sandbox.md §6.1、§7）。
//!
//! 每个 job 一个 tokio 任务（job actor）串行推进状态机；外部命令（取消）经 `mpsc`
//! 送入。`Semaphore(max_concurrent_sandboxes)` 控制同时存在的沙盒数；许可由单一的
//! 分派任务按受理顺序发放（FIFO 以 `submit` 返回的顺序为准，不取决于任务调度）。
//! 每次相位迁移先把 `meta.json` 原子写盘再执行动作。
//!
//! 契约缺口（见 #95 评论与拍板）：CAS 与 schema 注册表经 [`seams`] 注入；`IssueJobToken` 的
//! `tier` 取 `job.execution.tier`、`provider` 取 `exec.token_env.model_base_url` 的唯一键；
//! 运行时取服务配置；`upstream/` 暂不支持（非空 → `InvalidSpec`）。
//!
//! 崩溃恢复（§7.10）：`recover` 按 `meta.json`、带 `gantry.job_id` 标签的容器、作业目录与
//! `store` 里的 `sandbox.created` 逐项分类。尚未启动的相位拿不到 `SandboxSpec`（缺口 10）：
//! 删除残留容器与 staging、把 `meta.json` 重写为 `Accepted` 后返回 `Unknown`，由 engine 以
//! 同一 spec 重新 `submit`（容器从未启动，不会重复执行；令牌不吊销，以便重新签发）。

mod job;
pub mod meta;
pub mod seams;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use gantry_core::ids::{AttemptId, Clock, ProjectId};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::EgressControl;
use gantry_core::ports::repo::RepoService;
use gantry_core::ports::sandbox::{
    CapacitySnapshot, InfraDetail, JobHandle, JobOutcome, ProgressSample, RecoveredJob,
    SandboxError, SandboxService, SandboxSpec, SweepReport, UnfinishedAttempt,
};
use gantry_store::Store;
use tokio::sync::{broadcast, mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};

use crate::driver::{ContainerStatus, ContainerSummary, LabelSelector, SandboxDriver};
use crate::spec::{
    ContainerId, NetworkAttach, Runtime, StagingLayout, LABEL_JOB_ID, LABEL_MANAGED,
    LABEL_SANDBOX_ID,
};
use meta::{Meta, Phase, TerminalReason};
use seams::{ArtifactStore, OutputSchemas};

/// 服务配置（`sandbox.*` 设置与引导配置中建沙盒时固化的部分）。
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    pub workspaces_dir: PathBuf,
    /// `sandbox.runtime`（契约缺口 3：不在 `SandboxSpec` 里）。
    pub runtime: Runtime,
    pub max_concurrent_sandboxes: usize,
    pub network: NetworkAttach,
    pub log_max_bytes: u64,
    pub out_quota_bytes: u64,
    pub tmpfs_home_mb: u64,
    pub tmpfs_tmp_mb: u64,
    pub progress_sample: Duration,
    /// 容器退出后等日志流读完的上限。
    pub log_drain_timeout: Duration,
    /// 孤儿巡检的宽限（§7.10，默认 5 min）：更新的作业目录不清理。
    pub orphan_grace: Duration,
}

/// 依赖（端口与本模块内部的接缝）。
pub struct SandboxDeps<D> {
    pub driver: Arc<D>,
    pub repo: Arc<dyn RepoService>,
    pub egress: Arc<dyn EgressControl>,
    pub store: Store,
    /// 服务端时间（`received_at`、`deadline`、`timings`）。
    pub clock: Arc<dyn Clock>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub schemas: Arc<dyn OutputSchemas>,
}

type Outcome = Result<JobOutcome, SandboxError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Cancel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Occupancy {
    #[default]
    Queued,
    Holding,
    Done,
}

struct Slot {
    /// 恢复出的 job 没有 spec：同一 job 的再次 `submit` 以槽位为准（返回同一句柄）。
    spec: Option<SandboxSpec>,
    handle: JobHandle,
    result: watch::Receiver<Option<Outcome>>,
    commands: mpsc::UnboundedSender<Command>,
    progress: broadcast::Sender<ProgressSample>,
    occupancy: Arc<Mutex<Occupancy>>,
    /// `release` 成功完成后置 `true`；异步锁串行化并发的 `release`。
    released: Arc<tokio::sync::Mutex<bool>>,
}

type PermitWaiter = oneshot::Sender<OwnedSemaphorePermit>;

pub(crate) struct Inner<D> {
    deps: SandboxDeps<D>,
    config: SandboxConfig,
    permits: Arc<Semaphore>,
    jobs: Mutex<BTreeMap<AttemptId, Slot>>,
    /// 串行化 `submit`：受理记录写盘与入队在同一临界区内，同一 job 的并发提交不会
    /// 同时写 `meta.json`，入队顺序即受理顺序。
    submit_lock: tokio::sync::Mutex<()>,
    /// 许可分派队列（首次 `submit` 时在当前运行时上启动分派任务）。
    dispatch: OnceLock<mpsc::UnboundedSender<PermitWaiter>>,
    /// 本服务启动的全部任务（测试用的 `crash` 一次性中止它们）。
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl<D> Inner<D> {
    pub(crate) fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = tokio::spawn(future);
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle.abort_handle());
        handle
    }

    fn dispatcher(&self) -> &mpsc::UnboundedSender<PermitWaiter> {
        self.dispatch.get_or_init(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<PermitWaiter>();
            let permits = Arc::clone(&self.permits);
            self.spawn(async move {
                while let Some(mut waiter) = rx.recv().await {
                    // 排队中被取消的 job 已丢弃接收端：跳过，不占许可。
                    let permit = tokio::select! {
                        biased;
                        () = waiter.closed() => continue,
                        permit = Arc::clone(&permits).acquire_owned() => match permit {
                            Ok(permit) => permit,
                            Err(_) => break,
                        },
                    };
                    let _ = waiter.send(permit);
                }
            });
            tx
        })
    }
}

/// `SandboxService` 的实现，泛型于容器驱动。
pub struct SandboxManager<D> {
    inner: Arc<Inner<D>>,
}

impl<D: SandboxDriver + 'static> SandboxManager<D> {
    pub fn new(deps: SandboxDeps<D>, config: SandboxConfig) -> Self {
        let permits = Arc::new(Semaphore::new(config.max_concurrent_sandboxes));
        Self {
            inner: Arc::new(Inner {
                deps,
                config,
                permits,
                jobs: Mutex::new(BTreeMap::new()),
                submit_lock: tokio::sync::Mutex::new(()),
                dispatch: OnceLock::new(),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    fn jobs(&self) -> std::sync::MutexGuard<'_, BTreeMap<AttemptId, Slot>> {
        self.inner.jobs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 模拟 `gantryd` 崩溃：中止本服务的全部任务，不做任何清理。容器、`meta.json`、
    /// staging 与令牌原样留下，交给同一根目录上新建的服务 `recover`。
    #[cfg(any(test, feature = "testing"))]
    pub fn crash(&self) {
        let tasks =
            std::mem::take(&mut *self.inner.tasks.lock().unwrap_or_else(|p| p.into_inner()));
        for task in tasks {
            task.abort();
        }
    }

    fn layout(
        &self,
        sandbox_id: gantry_core::ids::SandboxId,
    ) -> Result<StagingLayout, SandboxError> {
        StagingLayout::new(&self.inner.config.workspaces_dir, sandbox_id)
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))
    }

    async fn created_event(
        &self,
        sandbox_id: gantry_core::ids::SandboxId,
    ) -> Result<bool, SandboxError> {
        self.inner
            .deps
            .store
            .events()
            .by_dedup_key(&format!("sandbox.created:{sandbox_id}"))
            .await
            .map(|seq| seq.is_some())
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))
    }

    /// 本进程里已有该 job（同一服务上重复 `recover`）：按它的状态作答。
    fn known(&self, job_id: &AttemptId) -> Option<RecoveredJob> {
        let jobs = self.jobs();
        let slot = jobs.get(job_id)?;
        let handle = slot.handle;
        let result = slot.result.borrow();
        let recovered = match &*result {
            None => RecoveredJob::Running(handle),
            Some(Err(SandboxError::Infra {
                detail: InfraDetail::Lost,
                ..
            })) => RecoveredJob::Lost(handle),
            Some(_) => RecoveredJob::Exited(handle),
        };
        Some(recovered)
    }

    /// 为恢复出的 job 建槽位并起 actor。
    #[allow(clippy::too_many_arguments)]
    fn spawn_recovered(
        &self,
        ctx: &ActorCtx,
        project_id: ProjectId,
        layout: StagingLayout,
        meta: Meta,
        container: Option<ContainerId>,
        created_event: bool,
        mode: job::RecoverMode,
    ) -> JobHandle {
        let handle = JobHandle {
            job_id: meta.job_id,
            sandbox_id: meta.sandbox_id,
        };
        let (result_tx, result_rx) = watch::channel(None);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (progress_tx, _) = broadcast::channel(256);
        let occupancy = Arc::new(Mutex::new(Occupancy::Queued));
        self.jobs().insert(
            meta.job_id,
            Slot {
                spec: None,
                handle,
                result: result_rx,
                commands: command_tx,
                progress: progress_tx.clone(),
                occupancy: Arc::clone(&occupancy),
                released: Arc::default(),
            },
        );
        let actor = job::JobActor::recovered(
            Arc::clone(&self.inner),
            ctx.clone(),
            project_id,
            layout,
            meta,
            container,
            created_event,
            command_rx,
            progress_tx,
            occupancy,
        );
        self.inner.spawn(async move {
            let outcome = actor.run_recovered(mode).await;
            let _ = result_tx.send(Some(outcome));
        });
        handle
    }

    async fn recover_one(
        &self,
        ctx: &ActorCtx,
        unfinished: &UnfinishedAttempt,
    ) -> Result<RecoveredJob, SandboxError> {
        if let Some(known) = self.known(&unfinished.job_id) {
            return Ok(known);
        }
        let driver = &self.inner.deps.driver;
        let handle = JobHandle {
            job_id: unfinished.job_id,
            sandbox_id: unfinished.sandbox_id,
        };
        let layout = self.layout(unfinished.sandbox_id)?;
        // 损坏的 meta.json 按「不存在」处理：只靠容器、目录与事件判断。
        let meta = Meta::load(&layout.meta_json()).ok().flatten();
        let mut selector = BTreeMap::new();
        selector.insert(LABEL_JOB_ID.to_owned(), unfinished.job_id.to_string());
        let containers = driver
            .list(&LabelSelector(selector))
            .await
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
        let container = pick_container(&containers, meta.as_ref());
        let dir_exists = layout.dir().exists();
        let created = self.created_event(unfinished.sandbox_id).await?;

        let Some(mut meta) = meta.filter(|meta| meta.resume.is_some()) else {
            // 没有可用的受理记录：残留的只可能是受理途中崩溃的半成品（容器从未启动）。
            if let Some(summary) = &container {
                let _ = driver.kill(&summary.id).await;
                let _ = driver.remove(&summary.id).await;
            }
            if dir_exists {
                let _ = driver.purge_staging(&layout).await;
            }
            return Ok(if created {
                self.spawn_lost(ctx, layout, unfinished, created);
                RecoveredJob::Lost(handle)
            } else {
                RecoveredJob::Unknown {
                    job_id: unfinished.job_id,
                }
            });
        };
        if meta.job_id != unfinished.job_id {
            return Err(infra(
                InfraDetail::Lost,
                "meta.json belongs to a different job",
            ));
        }

        let running_or_exited = container
            .as_ref()
            .filter(|summary| summary.status != ContainerStatus::Created);
        let started = meta.started() || running_or_exited.is_some();
        if meta.phase == Phase::Failed {
            let container = container.map(|summary| summary.id);
            self.spawn_recovered(
                ctx,
                unfinished.project_id,
                layout,
                meta,
                container,
                created,
                job::RecoverMode::Lost,
            );
            return Ok(RecoveredJob::Lost(handle));
        }
        if !started {
            if meta.terminal_reason == Some(TerminalReason::Cancel) {
                let container = container.map(|summary| summary.id);
                let handle = self.spawn_recovered(
                    ctx,
                    unfinished.project_id,
                    layout,
                    meta,
                    container,
                    created,
                    job::RecoverMode::CancelledBeforeStart,
                );
                return Ok(RecoveredJob::Exited(handle));
            }
            // 缺口 10：拿不到 SandboxSpec，无法重走该阶段——清掉残留，让 engine 重新 submit。
            if let Some(summary) = &container {
                let _ = driver.kill(&summary.id).await;
                driver
                    .remove(&summary.id)
                    .await
                    .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
            }
            driver
                .purge_staging(&layout)
                .await
                .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
            std::fs::create_dir_all(layout.dir())
                .and_then(|()| Meta::new(meta.job_id, meta.sandbox_id).save(&layout.meta_json()))
                .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
            return Ok(RecoveredJob::Unknown {
                job_id: unfinished.job_id,
            });
        }
        if let Some(summary) = container {
            let running = summary.status == ContainerStatus::Running;
            let handle = self.spawn_recovered(
                ctx,
                unfinished.project_id,
                layout,
                meta,
                Some(summary.id.clone()),
                created,
                job::RecoverMode::Supervise(summary.id),
            );
            return Ok(if running {
                RecoveredJob::Running(handle)
            } else {
                RecoveredJob::Exited(handle)
            });
        }
        // 容器已不在：没有进程能再写 out/，主机直读还能取就 collect（cause = Lost）。
        if dir_exists {
            let id = meta
                .container_id
                .clone()
                .map(ContainerId::new)
                .unwrap_or_else(|| ContainerId::new(layout.container_name()));
            let plan = meta.resume.as_ref().expect("filtered above").intake;
            if let Ok(intake) = driver.read_out(&id, &layout, &plan).await {
                // 已过 Destroying：容器是本服务删的，终止事实以 meta.json 为准。
                let lost = meta.lost || !matches!(meta.phase, Phase::Destroying | Phase::Destroyed);
                meta.lost = lost;
                let handle = self.spawn_recovered(
                    ctx,
                    unfinished.project_id,
                    layout,
                    meta,
                    Some(id.clone()),
                    created,
                    job::RecoverMode::Collect { id, intake, lost },
                );
                return Ok(RecoveredJob::Exited(handle));
            }
        }
        let container = meta.container_id.clone().map(ContainerId::new);
        self.spawn_recovered(
            ctx,
            unfinished.project_id,
            layout,
            meta,
            container,
            created,
            job::RecoverMode::Lost,
        );
        Ok(RecoveredJob::Lost(handle))
    }

    /// 没有可用 `meta.json` 但有 `sandbox.created`：受理后丢失。
    fn spawn_lost(
        &self,
        ctx: &ActorCtx,
        layout: StagingLayout,
        unfinished: &UnfinishedAttempt,
        created: bool,
    ) {
        let mut meta = Meta::new(unfinished.job_id, unfinished.sandbox_id);
        meta.phase = Phase::Failed;
        // 是否签发过令牌已不可知：按已签发处理（吊销幂等）。
        meta.token_issued = true;
        meta.resume = None;
        self.spawn_recovered(
            ctx,
            unfinished.project_id,
            layout,
            meta,
            None,
            created,
            job::RecoverMode::Lost,
        );
    }
}

/// 同一 job 至多一个容器；有多个时优先 `meta.json` 记的那个。
fn pick_container(
    containers: &[ContainerSummary],
    meta: Option<&Meta>,
) -> Option<ContainerSummary> {
    let recorded = meta.and_then(|meta| meta.container_id.as_deref());
    containers
        .iter()
        .find(|summary| Some(summary.id.as_str()) == recorded)
        .or_else(|| containers.first())
        .cloned()
}

/// 作业目录是否已过孤儿宽限（以 `meta.json`，没有时以目录本身的修改时间为准）。
fn older_than(layout: &StagingLayout, grace: Duration) -> bool {
    let modified = std::fs::metadata(layout.meta_json())
        .or_else(|_| std::fs::metadata(layout.dir()))
        .and_then(|meta| meta.modified());
    match modified {
        Ok(at) => SystemTime::now()
            .duration_since(at)
            .is_ok_and(|age| age >= grace),
        // 读不到时间（目录已没了）：视为过期。
        Err(_) => true,
    }
}

fn infra(detail: InfraDetail, message: impl Into<String>) -> SandboxError {
    SandboxError::Infra {
        detail,
        message: message.into(),
    }
}

struct ChannelStream<T>(mpsc::UnboundedReceiver<T>);

impl<T> Stream for ChannelStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}

#[async_trait]
impl<D: SandboxDriver + 'static> SandboxService for SandboxManager<D> {
    async fn submit(&self, ctx: &ActorCtx, spec: SandboxSpec) -> Result<JobHandle, SandboxError> {
        let handle = JobHandle {
            job_id: spec.job_id,
            sandbox_id: spec.sandbox_id,
        };
        let _submitting = self.inner.submit_lock.lock().await;
        if let Some(slot) = self.jobs().get(&spec.job_id) {
            if slot.spec.as_ref().is_some_and(|known| *known != spec) {
                return Err(SandboxError::InvalidSpec(
                    "a different sandbox spec was already submitted for this job".into(),
                ));
            }
            return Ok(slot.handle);
        }
        job::check_spec(&spec)?;
        let layout = StagingLayout::new(&self.inner.config.workspaces_dir, spec.sandbox_id)
            .map_err(|err| infra(InfraDetail::SandboxCreate, err.to_string()))?;
        // 返回 Ok 之前受理记录已原子写盘（E4）：重启后 recover 不会判成 Unknown。
        std::fs::create_dir_all(layout.dir())
            .and_then(|()| Meta::new(spec.job_id, spec.sandbox_id).save(&layout.meta_json()))
            .map_err(|err| infra(InfraDetail::WorkspacePrep, format!("accept: {err}")))?;

        let (result_tx, result_rx) = watch::channel(None);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (progress_tx, _) = broadcast::channel(256);
        let (permit_tx, permit_rx) = oneshot::channel();
        let occupancy = Arc::new(Mutex::new(Occupancy::Queued));
        self.jobs().insert(
            spec.job_id,
            Slot {
                spec: Some(spec.clone()),
                handle,
                result: result_rx,
                commands: command_tx,
                progress: progress_tx.clone(),
                occupancy: Arc::clone(&occupancy),
                released: Arc::default(),
            },
        );
        // 分派任务没了，接收端会立刻报错，actor 以 ShuttingDown 结束。
        let _ = self.inner.dispatcher().send(permit_tx);
        let actor = job::JobActor::new(
            Arc::clone(&self.inner),
            ctx.clone(),
            spec,
            layout,
            command_rx,
            progress_tx,
            occupancy,
        );
        self.inner.spawn(async move {
            let outcome = actor.run(permit_rx).await;
            let _ = result_tx.send(Some(outcome));
        });
        Ok(handle)
    }

    async fn wait(&self, job_id: &AttemptId) -> Result<JobOutcome, SandboxError> {
        let mut rx = self
            .jobs()
            .get(job_id)
            .map(|slot| slot.result.clone())
            .ok_or(SandboxError::NotFound)?;
        let outcome = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| SandboxError::ShuttingDown)?;
        outcome.clone().expect("checked by wait_for")
    }

    fn progress(&self, job_id: &AttemptId) -> BoxStream<'static, ProgressSample> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(slot) = self.jobs().get(job_id) {
            let mut sub = slot.progress.subscribe();
            let mut done = slot.result.clone();
            // 订阅方丢弃流或 job 结束时退出转发，不留下常驻任务。
            self.inner.spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        received = sub.recv() => match received {
                            Ok(sample) => {
                                if tx.send(sample).is_err() {
                                    break;
                                }
                            }
                            // 订阅慢了丢样本没关系（仅展示）。
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        },
                        () = tx.closed() => break,
                        () = async { let _ = done.wait_for(Option::is_some).await; } => {
                            loop {
                                match sub.try_recv() {
                                    Ok(sample) => {
                                        let _ = tx.send(sample);
                                    }
                                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                                    Err(_) => break,
                                }
                            }
                            break;
                        }
                    }
                }
            });
        }
        Box::pin(ChannelStream(rx))
    }

    async fn cancel(&self, _ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        let jobs = self.jobs();
        let slot = jobs.get(job_id).ok_or(SandboxError::NotFound)?;
        // 已结束的 job 收不到命令，取消是空操作（幂等）。
        let _ = slot.commands.send(Command::Cancel);
        Ok(())
    }

    async fn release(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        let (sandbox_id, released) = {
            let jobs = self.jobs();
            let slot = jobs.get(job_id).ok_or(SandboxError::NotFound)?;
            if slot.result.borrow().is_none() {
                return Err(SandboxError::InvalidSpec(
                    "release before the job reached a terminal state".into(),
                ));
            }
            (slot.handle.sandbox_id, Arc::clone(&slot.released))
        };
        // 并发的 release 排队；失败不置位，调用方可以重试。
        let mut released = released.lock().await;
        if *released {
            return Ok(());
        }
        let layout = StagingLayout::new(&self.inner.config.workspaces_dir, sandbox_id)
            .map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
        self.inner
            .deps
            .driver
            .purge_staging(&layout)
            .await
            .map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
        // 让 egress 忘记最终用量；它不在了也不影响本地释放。
        let _ = self.inner.deps.egress.forget_job(job_id, ctx).await;
        *released = true;
        Ok(())
    }

    /// §7.10：对每个未终结尝试恰好返回一项（E4）。
    async fn recover(
        &self,
        ctx: &ActorCtx,
        unfinished: &[UnfinishedAttempt],
    ) -> Result<Vec<RecoveredJob>, SandboxError> {
        let mut recovered = Vec::with_capacity(unfinished.len());
        for attempt in unfinished {
            recovered.push(self.recover_one(ctx, attempt).await?);
        }
        Ok(recovered)
    }

    /// §7.10：带 `gantry.managed=1` 的容器与 `<workspaces_dir>/*`，不在 `live`、不在本进程、
    /// 且超过宽限的，先吊销令牌再删除。
    async fn sweep_orphans(
        &self,
        ctx: &ActorCtx,
        live: &BTreeSet<AttemptId>,
    ) -> Result<SweepReport, SandboxError> {
        let driver = &self.inner.deps.driver;
        let egress = &self.inner.deps.egress;
        let grace = self.inner.config.orphan_grace;
        let active: BTreeSet<AttemptId> = self.jobs().keys().copied().collect();
        let keep = |job_id: &AttemptId| live.contains(job_id) || active.contains(job_id);
        let mut report = SweepReport::default();
        let mut revoked = BTreeSet::new();

        let mut selector = BTreeMap::new();
        selector.insert(LABEL_MANAGED.to_owned(), "1".to_owned());
        let containers = driver
            .list(&LabelSelector(selector))
            .await
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
        for summary in containers {
            let job_id = summary
                .labels
                .get(LABEL_JOB_ID)
                .and_then(|value| value.parse::<AttemptId>().ok());
            if job_id.as_ref().is_some_and(keep) {
                continue;
            }
            let layout = summary
                .labels
                .get(LABEL_SANDBOX_ID)
                .and_then(|value| value.parse().ok())
                .and_then(|sandbox_id| self.layout(sandbox_id).ok());
            if layout
                .as_ref()
                .is_some_and(|layout| !older_than(layout, grace))
            {
                continue;
            }
            if let Some(job_id) = job_id {
                // 先吊销：容器删除前它的令牌已作废（吊销幂等，未签发的报错忽略）。
                let _ = egress.revoke_job_token(&job_id, ctx).await;
                revoked.insert(job_id);
            }
            let _ = driver.kill(&summary.id).await;
            match driver.remove(&summary.id).await {
                Ok(()) => report.destroyed += 1,
                Err(err) => report.errors.push(format!("remove {}: {err}", summary.id)),
            }
        }

        let entries = match std::fs::read_dir(&self.inner.config.workspaces_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(err) => return Err(infra(InfraDetail::Lost, err.to_string())),
        };
        for entry in entries.flatten() {
            // 只碰形如 `<sandbox_id>` 的目录；其他东西不是本模块的。
            let Some(sandbox_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<gantry_core::ids::SandboxId>().ok())
            else {
                continue;
            };
            let Ok(layout) = self.layout(sandbox_id) else {
                continue;
            };
            let meta = Meta::load(&layout.meta_json()).ok().flatten();
            let job_id = meta.as_ref().map(|meta| meta.job_id);
            if job_id.as_ref().is_some_and(keep) || !older_than(&layout, grace) {
                continue;
            }
            if let Some(job_id) = job_id {
                if meta.as_ref().is_some_and(|meta| meta.token_issued) && revoked.insert(job_id) {
                    let _ = egress.revoke_job_token(&job_id, ctx).await;
                }
            }
            match driver.purge_staging(&layout).await {
                Ok(()) => report.staging_removed += 1,
                Err(err) => report.errors.push(format!("purge {sandbox_id}: {err}")),
            }
        }
        Ok(report)
    }

    async fn capacity(&self) -> CapacitySnapshot {
        let jobs = self.jobs();
        let mut snapshot = CapacitySnapshot {
            limit: u32::try_from(self.inner.config.max_concurrent_sandboxes).unwrap_or(u32::MAX),
            ..CapacitySnapshot::default()
        };
        for slot in jobs.values() {
            match *slot.occupancy.lock().unwrap_or_else(|p| p.into_inner()) {
                Occupancy::Queued => snapshot.queued += 1,
                Occupancy::Holding => snapshot.running += 1,
                Occupancy::Done => {}
            }
        }
        snapshot
    }
}
