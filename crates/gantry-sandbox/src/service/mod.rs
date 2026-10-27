//! 引擎面 `SandboxService` 的实现（sandbox.md §6.1、§7）。
//!
//! 每个 job 一个 tokio 任务（job actor）串行推进状态机；外部命令（取消）经 `mpsc`
//! 送入。`Semaphore(max_concurrent_sandboxes)` 控制同时存在的沙盒数；许可由单一的
//! 分派任务按受理顺序发放（FIFO 以 `submit` 返回的顺序为准，不取决于任务调度）。
//! 每次相位迁移先把 `meta.json` 原子写盘再执行动作。
//!
//! 契约缺口（见 #95 评论）：CAS 与 schema 注册表经 [`seams`] 注入；`IssueJobToken` 的
//! `tier` 取 `job.execution.tier`、`provider` 取 `exec.token_env.model_base_url` 的唯一键；
//! 运行时取服务配置；`upstream/` 暂不支持（非空 → `InvalidSpec`）。`recover` 与
//! `sweep_orphans` 在下一个切片实现，本切片对它们失败闭合。

mod job;
pub mod meta;
pub mod seams;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use gantry_core::ids::{AttemptId, Clock};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::EgressControl;
use gantry_core::ports::repo::RepoService;
use gantry_core::ports::sandbox::{
    CapacitySnapshot, InfraDetail, JobHandle, JobOutcome, ProgressSample, RecoveredJob,
    SandboxError, SandboxService, SandboxSpec, SweepReport, UnfinishedAttempt,
};
use gantry_store::Store;
use tokio::sync::{broadcast, mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};

use crate::driver::SandboxDriver;
use crate::spec::{NetworkAttach, Runtime, StagingLayout};
use meta::Meta;
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
    spec: SandboxSpec,
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
}

impl<D> Inner<D> {
    fn dispatcher(&self) -> &mpsc::UnboundedSender<PermitWaiter> {
        self.dispatch.get_or_init(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<PermitWaiter>();
            let permits = Arc::clone(&self.permits);
            tokio::spawn(async move {
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
            }),
        }
    }

    fn jobs(&self) -> std::sync::MutexGuard<'_, BTreeMap<AttemptId, Slot>> {
        self.inner.jobs.lock().unwrap_or_else(|p| p.into_inner())
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
            if slot.spec != spec {
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
                spec: spec.clone(),
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
        tokio::spawn(async move {
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
            tokio::spawn(async move {
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

    /// 下一个切片实现（§7.10）；在此之前失败闭合，不猜测运行时状态。
    async fn recover(
        &self,
        _ctx: &ActorCtx,
        _unfinished: &[UnfinishedAttempt],
    ) -> Result<Vec<RecoveredJob>, SandboxError> {
        Err(infra(
            InfraDetail::Lost,
            "recover is not implemented in this slice (sandbox.md §7.10)",
        ))
    }

    async fn sweep_orphans(
        &self,
        _ctx: &ActorCtx,
        _live: &BTreeSet<AttemptId>,
    ) -> Result<SweepReport, SandboxError> {
        Err(infra(
            InfraDetail::Lost,
            "sweep_orphans is not implemented in this slice (sandbox.md §7.10)",
        ))
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
