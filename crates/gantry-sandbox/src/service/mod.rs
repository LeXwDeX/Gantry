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
//! 崩溃恢复（worker-protocol §14）：只允许新记录在证明从未启动后清理并重入；旧记录和
//! 无法验证的记录走身份拒绝收尾。恢复依据放在 staging 之外，直到终结提交后的 release。

mod job;
pub mod meta;
pub mod seams;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use meta::{Meta, Phase, RecoveryKind, RecoveryRecord, TerminalReason};
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
    project_id: ProjectId,
    result: watch::Receiver<Option<Outcome>>,
    commands: mpsc::UnboundedSender<Command>,
    progress: broadcast::Sender<ProgressSample>,
    occupancy: Arc<Mutex<Occupancy>>,
    /// `release` 成功完成后置 `true`；异步锁串行化并发的 `release`。
    released: Arc<tokio::sync::Mutex<bool>>,
    /// 归属未确认的请求只保留等待通道，不认领沙盒或计算许可。
    blocked: Option<Blocked>,
}

struct Blocked {
    result: watch::Sender<Option<Outcome>>,
    commands: mpsc::UnboundedReceiver<Command>,
    retrying: bool,
    wake: bool,
}

type PermitWaiter = oneshot::Sender<OwnedSemaphorePermit>;

// 最低支持的 Rust 1.85 尚无 `try_update`；提高最低版本后改用 `try_update`。
#[allow(deprecated)]
fn take_debt(debt: &AtomicUsize) -> bool {
    debt.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
        value.checked_sub(1)
    })
    .is_ok()
}

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
    /// 恢复时超出并发上限的容器数（重启后上限变小）。分派任务先拿许可还账，
    /// 新作业要等实际容器数降到上限以下。
    debt: Arc<AtomicUsize>,
}

impl<D> Inner<D> {
    /// 没拿到许可的恢复容器结束：欠账还没还就销账，已经由分派任务拿许可还过就把那个许可还回去。
    pub(crate) fn settle_overdraft(&self) {
        if !take_debt(&self.debt) {
            self.permits.add_permits(1);
        }
    }

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
            let inner_debt = Arc::clone(&self.debt);
            self.spawn(async move {
                'waiters: while let Some(mut waiter) = rx.recv().await {
                    let permit = loop {
                        // 排队中被取消的 job 已丢弃接收端：跳过，不占许可。
                        let permit = tokio::select! {
                            biased;
                            () = waiter.closed() => continue 'waiters,
                            permit = Arc::clone(&permits).acquire_owned() => match permit {
                                Ok(permit) => permit,
                                Err(_) => break 'waiters,
                            },
                        };
                        // 先还恢复时的欠账：这个许可代表一个早已在跑的容器。
                        if take_debt(&inner_debt) {
                            permit.forget();
                            continue;
                        }
                        break permit;
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
                debt: Arc::default(),
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
    ) -> Result<Option<(AttemptId, ProjectId)>, SandboxError> {
        let events = self.inner.deps.store.events();
        let seq = events
            .by_dedup_key(&format!("sandbox.created:{sandbox_id}"))
            .await
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
        let Some(seq) = seq else { return Ok(None) };
        let event = events
            .get_by_seq(seq)
            .await
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?
            .ok_or_else(|| infra(InfraDetail::Lost, "sandbox.created event missing"))?;
        let data: gantry_core::events::SandboxCreatedData = serde_json::from_value(event.data)
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
        let project = event
            .project_id
            .ok_or_else(|| infra(InfraDetail::Lost, "sandbox.created project missing"))?;
        if data.sandbox_id != sandbox_id {
            return Err(infra(
                InfraDetail::Lost,
                "sandbox.created identity mismatch",
            ));
        }
        Ok(Some((data.attempt_id, project)))
    }

    /// 本进程里已有该 job（同一服务上重复 `recover`）：按它的状态作答。
    fn known(&self, attempt: &UnfinishedAttempt) -> Result<Option<RecoveredJob>, SandboxError> {
        let mut jobs = self.jobs();
        let Some(slot) = jobs.get(&attempt.job_id) else {
            return Ok(None);
        };
        if slot.blocked.is_some() && slot.result.borrow().is_some() {
            jobs.remove(&attempt.job_id);
            return Ok(None);
        }
        if slot.handle.sandbox_id != attempt.sandbox_id || slot.project_id != attempt.project_id {
            if slot.blocked.is_some() {
                jobs.remove(&attempt.job_id);
                return Ok(None);
            }
            return Err(SandboxError::InvalidSpec(
                "conflicting identity for an in-process job".into(),
            ));
        }
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
        Ok(Some(recovered))
    }

    fn block_recovery(&self, attempt: &UnfinishedAttempt, reason: &str) -> RecoveredJob {
        let handle = JobHandle {
            job_id: attempt.job_id,
            sandbox_id: attempt.sandbox_id,
        };
        eprintln!(
            "sandbox recovery blocked: job={} sandbox={} project={}: {reason}",
            attempt.job_id, attempt.sandbox_id, attempt.project_id
        );
        let mut jobs = self.jobs();
        if let Some(slot) = jobs.get(&attempt.job_id) {
            assert!(
                slot.blocked.is_some()
                    && slot.handle == handle
                    && slot.project_id == attempt.project_id
            );
            return RecoveredJob::Running(handle);
        }
        let (result_tx, result_rx) = watch::channel(None);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (progress, _) = broadcast::channel(256);
        jobs.insert(
            attempt.job_id,
            Slot {
                spec: None,
                handle,
                project_id: attempt.project_id,
                result: result_rx,
                commands,
                progress,
                occupancy: Arc::new(Mutex::new(Occupancy::Done)),
                released: Arc::default(),
                blocked: Some(Blocked {
                    result: result_tx,
                    commands: command_rx,
                    retrying: false,
                    wake: false,
                }),
            },
        );
        RecoveredJob::Running(handle)
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
        let (result_tx, command_rx, progress_tx, occupancy) = {
            let mut jobs = self.jobs();
            if let Some(slot) = jobs.get_mut(&meta.job_id) {
                assert_eq!((slot.handle, slot.project_id), (handle, project_id));
                let blocked = slot
                    .blocked
                    .take()
                    .expect("only blocked slots can be resumed");
                *slot.occupancy.lock().unwrap_or_else(|p| p.into_inner()) = Occupancy::Queued;
                (
                    blocked.result,
                    blocked.commands,
                    slot.progress.clone(),
                    Arc::clone(&slot.occupancy),
                )
            } else {
                let (result_tx, result_rx) = watch::channel(None);
                let (command_tx, command_rx) = mpsc::unbounded_channel();
                let (progress_tx, _) = broadcast::channel(256);
                let occupancy = Arc::new(Mutex::new(Occupancy::Queued));
                jobs.insert(
                    meta.job_id,
                    Slot {
                        spec: None,
                        handle,
                        project_id,
                        result: result_rx,
                        commands: command_tx,
                        progress: progress_tx.clone(),
                        occupancy: Arc::clone(&occupancy),
                        released: Arc::default(),
                        blocked: None,
                    },
                );
                (result_tx, command_rx, progress_tx, occupancy)
            }
        };
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

    fn foreign_job_evidence(&self, attempt: &UnfinishedAttempt) -> Result<bool, SandboxError> {
        let job_id = attempt.job_id.to_string();
        let entries = match std::fs::read_dir(&self.inner.config.workspaces_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(infra(InfraDetail::Lost, err.to_string())),
        };
        for entry in entries {
            let entry = entry.map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let ty = entry
                .file_type()
                .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
            let (sandbox, path) = if ty.is_dir() {
                (
                    name.parse::<gantry_core::ids::SandboxId>().ok(),
                    entry.path().join("meta.json"),
                )
            } else if ty.is_file() {
                (
                    name.strip_suffix(".recovery.json")
                        .and_then(|s| s.parse::<gantry_core::ids::SandboxId>().ok()),
                    entry.path(),
                )
            } else {
                continue;
            };
            let Some(sandbox) = sandbox else { continue };
            if sandbox == attempt.sandbox_id {
                continue;
            }
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(infra(InfraDetail::Lost, err.to_string())),
            };
            let evidence = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
            if evidence
                .as_ref()
                .and_then(|value| value.get("job_id"))
                .and_then(|id| id.as_str())
                .is_some_and(|id| id == job_id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn retry_recovery(&self, ctx: ActorCtx, attempt: UnfinishedAttempt) {
        let generation = {
            let mut jobs = self.jobs();
            let Some(slot) = jobs.get_mut(&attempt.job_id) else {
                return;
            };
            let Some(blocked) = slot.blocked.as_mut() else {
                return;
            };
            if blocked.retrying {
                return;
            }
            blocked.retrying = true;
            Arc::clone(&slot.occupancy)
        };
        let manager = Self {
            inner: Arc::clone(&self.inner),
        };
        self.inner.spawn(async move {
            let mut delay = Duration::from_millis(250);
            loop {
                tokio::time::sleep(delay).await;
                let _guard = manager.inner.submit_lock.lock().await;
                let still_blocked = {
                    let mut jobs = manager.jobs();
                    let owns_slot = jobs.get(&attempt.job_id).is_some_and(|slot| {
                        slot.blocked.is_some()
                            && slot.handle.sandbox_id == attempt.sandbox_id
                            && slot.project_id == attempt.project_id
                            && Arc::ptr_eq(&slot.occupancy, &generation)
                    });
                    let occupied = jobs.iter().any(|(job_id, slot)| {
                        *job_id != attempt.job_id
                            && slot.blocked.is_none()
                            && slot.handle.sandbox_id == attempt.sandbox_id
                    });
                    if owns_slot {
                        let blocked = jobs
                            .get_mut(&attempt.job_id)
                            .and_then(|slot| slot.blocked.as_mut())
                            .expect("checked blocked slot");
                        blocked.wake = false;
                        if occupied {
                            blocked.retrying = false;
                        }
                    }
                    owns_slot && !occupied
                };
                if !still_blocked {
                    return;
                }
                if manager.recover_one(&ctx, &attempt, true).await.is_ok() {
                    let wake = {
                        let mut jobs = manager.jobs();
                        jobs.get_mut(&attempt.job_id)
                            .filter(|slot| Arc::ptr_eq(&slot.occupancy, &generation))
                            .and_then(|slot| slot.blocked.as_mut())
                            .is_some_and(|blocked| {
                                if !blocked.wake {
                                    blocked.retrying = false;
                                }
                                blocked.wake
                            })
                    };
                    if !wake {
                        return;
                    }
                }
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        });
    }

    async fn recover_one(
        &self,
        ctx: &ActorCtx,
        unfinished: &UnfinishedAttempt,
        retrying: bool,
    ) -> Result<RecoveredJob, SandboxError> {
        if !retrying {
            if let Some(known) = self.known(unfinished)? {
                return Ok(known);
            }
        }
        if self.foreign_job_evidence(unfinished)? {
            return Ok(self.block_recovery(unfinished, "job belongs to another sandbox"));
        }
        let driver = &self.inner.deps.driver;
        let handle = JobHandle {
            job_id: unfinished.job_id,
            sandbox_id: unfinished.sandbox_id,
        };
        let layout = self.layout(unfinished.sandbox_id)?;
        let meta = Meta::load(&layout.meta_json());
        let guard = match RecoveryRecord::load(&recovery_path(&layout)) {
            Ok(guard) => guard,
            Err(_) => return Ok(self.block_recovery(unfinished, "damaged recovery record")),
        };
        if guard.as_ref().is_some_and(|record| {
            !record.matches_identity(
                unfinished.job_id,
                unfinished.sandbox_id,
                unfinished.project_id,
            )
        }) || meta
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|meta| {
                !meta.matches_identity(
                    unfinished.job_id,
                    unfinished.sandbox_id,
                    unfinished.project_id,
                )
            })
        {
            return Ok(self.block_recovery(unfinished, "persisted ownership cannot be verified"));
        }
        let mut selector = BTreeMap::new();
        selector.insert(LABEL_JOB_ID.to_owned(), unfinished.job_id.to_string());
        let mut containers = tokio::time::timeout(
            Duration::from_secs(5),
            driver.list(&LabelSelector(selector)),
        )
        .await
        .map_err(|_| infra(InfraDetail::Lost, "runtime list timed out"))?
        .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
        let mut selector = BTreeMap::new();
        selector.insert(
            LABEL_SANDBOX_ID.to_owned(),
            unfinished.sandbox_id.to_string(),
        );
        for summary in tokio::time::timeout(
            Duration::from_secs(5),
            driver.list(&LabelSelector(selector)),
        )
        .await
        .map_err(|_| infra(InfraDetail::Lost, "runtime list timed out"))?
        .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?
        {
            if !containers.iter().any(|other| other.id == summary.id) {
                containers.push(summary);
            }
        }
        let dir_exists = layout.dir().exists();
        let created_owner = tokio::time::timeout(
            Duration::from_secs(5),
            self.created_event(unfinished.sandbox_id),
        )
        .await
        .map_err(|_| infra(InfraDetail::Lost, "sandbox.created query timed out"))??;
        if created_owner.is_some_and(|owner| owner != (unfinished.job_id, unfinished.project_id)) {
            return Ok(self.block_recovery(unfinished, "sandbox.created ownership mismatch"));
        }
        let created = created_owner.is_some();
        let ownership_ok = containers.len() <= 1
            && containers.iter().all(|summary| {
                summary.labels.get(LABEL_MANAGED).is_some_and(|v| v == "1")
                    && summary.labels.get(LABEL_JOB_ID) == Some(&unfinished.job_id.to_string())
                    && summary.labels.get(LABEL_SANDBOX_ID)
                        == Some(&unfinished.sandbox_id.to_string())
            });
        let container = containers.first().cloned();
        let reject = |message: &str, no_usage: bool| RecoveryRecord {
            job_id: unfinished.job_id,
            sandbox_id: unfinished.sandbox_id,
            project_id: unfinished.project_id,
            kind: RecoveryKind::Reject {
                message: message.to_owned(),
                no_usage,
                delivered: false,
            },
        };
        let reject_job = |record: RecoveryRecord, meta: Option<Meta>| {
            let _ = record.save(&recovery_path(&layout));
            let mut actor_meta =
                meta.unwrap_or_else(|| Meta::new(unfinished.job_id, unfinished.sandbox_id));
            actor_meta.job_id = unfinished.job_id;
            actor_meta.sandbox_id = unfinished.sandbox_id;
            let occupied = containers.iter().any(|c| {
                c.labels.get(LABEL_MANAGED).is_some_and(|v| v == "1")
                    && c.labels.get(LABEL_JOB_ID) == Some(&unfinished.job_id.to_string())
                    && c.labels.get(LABEL_SANDBOX_ID) == Some(&unfinished.sandbox_id.to_string())
            });
            let permit = if occupied {
                let permit = Arc::clone(&self.inner.permits).try_acquire_owned().ok();
                if permit.is_none() {
                    self.inner.debt.fetch_add(1, Ordering::SeqCst);
                }
                Some(permit)
            } else {
                None
            };
            self.spawn_recovered(
                ctx,
                unfinished.project_id,
                layout.clone(),
                actor_meta,
                None,
                created,
                job::RecoverMode::Reject { record, permit },
            );
            RecoveredJob::Running(handle)
        };

        if !ownership_ok {
            return Ok(self.block_recovery(unfinished, "container ownership mismatch or duplicate"));
        }
        if (meta.is_err() || matches!(meta, Ok(None)) && dir_exists)
            && guard.is_none()
            && containers.is_empty()
            && !created
        {
            return Ok(self.block_recovery(unfinished, "damaged metadata has no verifiable owner"));
        }
        if let Some(record) = guard {
            if matches!(record.kind, RecoveryKind::Reject { .. }) {
                return Ok(reject_job(record, None));
            }
            if container
                .as_ref()
                .is_some_and(|c| c.status != ContainerStatus::Created)
            {
                return Ok(reject_job(
                    reject("reentry container unexpectedly executed", false),
                    None,
                ));
            }
            // Once Running was returned, wait cannot communicate Unknown to the engine.
            if retrying {
                return Ok(self.block_recovery(unfinished, "preparation requires redispatch"));
            }
            return self
                .finish_reentry(ctx, unfinished, layout, container, created, record)
                .await;
        }
        let mut meta = match meta {
            Ok(Some(meta)) => meta,
            Ok(None) if !dir_exists && containers.is_empty() && !created => {
                if retrying {
                    return Ok(self.block_recovery(unfinished, "job requires redispatch"));
                }
                return Ok(RecoveredJob::Unknown {
                    job_id: unfinished.job_id,
                });
            }
            Ok(None) => {
                return Ok(reject_job(
                    reject("accepted job has no metadata", false),
                    None,
                ))
            }
            Err(_) => {
                return Ok(reject_job(
                    reject("damaged metadata or digest", false),
                    None,
                ))
            }
        };
        if container.as_ref().is_some_and(|c| {
            meta.container_id
                .as_deref()
                .is_some_and(|id| id != c.id.as_str())
        }) {
            return Ok(self.block_recovery(unfinished, "metadata ownership mismatch"));
        }
        let before_start = matches!(
            meta.phase,
            Phase::Accepted
                | Phase::Queued
                | Phase::Creating
                | Phase::Created
                | Phase::Staging
                | Phase::Staged
        ) && container
            .as_ref()
            .is_none_or(|c| c.status == ContainerStatus::Created)
            && meta.timings.started.is_none()
            && meta.exit.is_none()
            && meta.deadline_ms.is_none();
        let starting_created = meta.phase == Phase::Starting
            && container
                .as_ref()
                .is_some_and(|c| c.status == ContainerStatus::Created)
            && meta.timings.started.is_none()
            && meta.exit.is_none();
        let cancelled_destroyed = matches!(meta.phase, Phase::Destroying | Phase::Destroyed)
            && meta.terminal_reason == Some(TerminalReason::Cancel)
            && meta.timings.started.is_none()
            && meta.exit.is_none()
            && container
                .as_ref()
                .is_none_or(|c| c.status == ContainerStatus::Created);
        let never_started = before_start || starting_created || cancelled_destroyed;
        let past_creation = matches!(
            meta.phase,
            Phase::Created
                | Phase::Staging
                | Phase::Staged
                | Phase::Starting
                | Phase::Running
                | Phase::Stopping
                | Phase::Stopped
                | Phase::Collecting
                | Phase::Collected
        ) || (matches!(meta.phase, Phase::Destroying | Phase::Destroyed)
            && !never_started);
        let stopped_phase = matches!(
            meta.phase,
            Phase::Stopped
                | Phase::Collecting
                | Phase::Collected
                | Phase::Destroying
                | Phase::Destroyed
        );
        let impossible = (matches!(
            meta.phase,
            Phase::Accepted
                | Phase::Queued
                | Phase::Creating
                | Phase::Created
                | Phase::Staging
                | Phase::Staged
        ) && !before_start)
            || (meta.phase == Phase::Starting && container.is_none())
            || (matches!(
                meta.phase,
                Phase::Staged
                    | Phase::Starting
                    | Phase::Running
                    | Phase::Stopping
                    | Phase::Stopped
                    | Phase::Collecting
                    | Phase::Collected
            ) && meta.job_sha256 == Some(None))
            || (!never_started && meta.job_sha256 == Some(None))
            || (meta.phase == Phase::Failed)
            || (past_creation
                && (!meta.token_issued
                    || meta.resume.is_none()
                    || meta.container_id.is_none()
                    || meta.timings.created.is_none()))
            || (matches!(
                meta.phase,
                Phase::Staged
                    | Phase::Starting
                    | Phase::Running
                    | Phase::Stopping
                    | Phase::Stopped
                    | Phase::Collecting
                    | Phase::Collected
            ) && meta.timings.staged.is_none())
            || (meta.phase == Phase::Starting && meta.deadline_ms.is_none())
            || (matches!(
                meta.phase,
                Phase::Running
                    | Phase::Stopping
                    | Phase::Stopped
                    | Phase::Collecting
                    | Phase::Collected
            ) && (meta.timings.started.is_none() || meta.deadline_ms.is_none()))
            || (stopped_phase
                && container
                    .as_ref()
                    .is_some_and(|c| c.status == ContainerStatus::Running))
            || (matches!(meta.phase, Phase::Running | Phase::Starting) && meta.exit.is_some())
            || (!never_started && meta.resume.is_none())
            || (!never_started
                && container
                    .as_ref()
                    .is_some_and(|c| c.status == ContainerStatus::Created));
        if impossible {
            return Ok(reject_job(
                reject("inconsistent metadata phase and runtime state", false),
                Some(meta),
            ));
        }
        if never_started {
            let cancel_pending = retrying
                && meta.terminal_reason.is_none()
                && self
                    .jobs()
                    .get_mut(&unfinished.job_id)
                    .and_then(|slot| slot.blocked.as_mut())
                    .is_some_and(|blocked| blocked.commands.try_recv().is_ok());
            if meta.terminal_reason == Some(TerminalReason::Cancel) || cancel_pending {
                let id = container.map(|summary| summary.id);
                let handle = self.spawn_recovered(
                    ctx,
                    unfinished.project_id,
                    layout,
                    meta,
                    id,
                    created,
                    job::RecoverMode::CancelledBeforeStart,
                );
                return Ok(RecoveredJob::Running(handle));
            }
            if meta.job_sha256.is_none() {
                let no_usage = !meta.token_issued;
                return Ok(reject_job(
                    reject("legacy preparation has no trusted digest", no_usage),
                    Some(meta),
                ));
            }
            if retrying {
                return Ok(self.block_recovery(unfinished, "preparation requires redispatch"));
            }
            let record = RecoveryRecord {
                job_id: unfinished.job_id,
                sandbox_id: unfinished.sandbox_id,
                project_id: unfinished.project_id,
                kind: RecoveryKind::Reenter {
                    token_issued: meta.token_issued,
                },
            };
            if record.save(&recovery_path(&layout)).is_err() {
                return Ok(reject_job(
                    reject("could not persist preparation recovery", !meta.token_issued),
                    Some(meta),
                ));
            }
            return self
                .finish_reentry(ctx, unfinished, layout, container, created, record)
                .await;
        }
        let legacy = meta.job_sha256.is_none();
        if let Some(summary) = container {
            let running = summary.status == ContainerStatus::Running;
            // 同步占许可（在任何新提交之前）；超出上限就记欠账。
            let permit = Arc::clone(&self.inner.permits).try_acquire_owned().ok();
            if permit.is_none() {
                self.inner.debt.fetch_add(1, Ordering::SeqCst);
            }
            let handle = self.spawn_recovered(
                ctx,
                unfinished.project_id,
                layout,
                meta,
                Some(summary.id.clone()),
                created,
                job::RecoverMode::Supervise {
                    id: summary.id,
                    exited: !running,
                    permit,
                },
            );
            return Ok(if running || legacy {
                RecoveredJob::Running(handle)
            } else {
                RecoveredJob::Exited(handle)
            });
        }
        // 旧任务必须有真实退出事实，不能把丢失伪装成可交付 outcome。
        if legacy && (meta.exit.is_none() || meta.lost) {
            return Ok(reject_job(
                reject("legacy job has no reliable exit facts", false),
                Some(meta),
            ));
        }
        // 容器已不在：没有进程能再写 out/，主机直读还能取就 collect（cause = Lost）。
        if dir_exists {
            let id = meta
                .container_id
                .clone()
                .map(ContainerId::new)
                .unwrap_or_else(|| ContainerId::new(layout.container_name()));
            let plan = meta.resume.as_ref().expect("filtered above").intake;
            let intake = if legacy || retrying {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    driver.read_out(&id, &layout, &plan),
                )
                .await;
                if retrying && result.is_err() {
                    return Err(infra(InfraDetail::Lost, "recovery intake timed out"));
                }
                result.ok().and_then(Result::ok)
            } else {
                driver.read_out(&id, &layout, &plan).await.ok()
            };
            if let Some(intake) = intake {
                // 已过 Destroying：容器是本服务删的，终止事实以 meta.json 为准。
                let lost = if legacy {
                    false
                } else {
                    meta.lost || !matches!(meta.phase, Phase::Destroying | Phase::Destroyed)
                };
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
                return Ok(if legacy {
                    RecoveredJob::Running(handle)
                } else {
                    RecoveredJob::Exited(handle)
                });
            }
        }
        if meta.job_sha256.is_none() {
            return Ok(reject_job(
                reject("legacy job artifacts are unavailable", false),
                Some(meta),
            ));
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

    async fn finish_reentry(
        &self,
        ctx: &ActorCtx,
        unfinished: &UnfinishedAttempt,
        layout: StagingLayout,
        container: Option<ContainerSummary>,
        created: bool,
        record: RecoveryRecord,
    ) -> Result<RecoveredJob, SandboxError> {
        let RecoveryKind::Reenter { token_issued } = record.kind else {
            unreachable!()
        };
        let driver = &self.inner.deps.driver;
        let cleanup = tokio::time::timeout(Duration::from_secs(5), async {
            if let Some(summary) = &container {
                driver.remove(&summary.id).await?;
            }
            driver.purge_staging(&layout).await
        })
        .await;
        if matches!(cleanup, Ok(Ok(()))) {
            let mut accepted = Meta::new(unfinished.job_id, unfinished.sandbox_id);
            accepted.project_id = Some(unfinished.project_id);
            accepted.token_issued = token_issued;
            if std::fs::create_dir_all(layout.dir())
                .and_then(|()| accepted.save(&layout.meta_json()))
                .is_ok()
                && std::fs::remove_file(recovery_path(&layout)).is_ok()
            {
                return Ok(RecoveredJob::Unknown {
                    job_id: unfinished.job_id,
                });
            }
        }
        let record = RecoveryRecord {
            job_id: unfinished.job_id,
            sandbox_id: unfinished.sandbox_id,
            project_id: unfinished.project_id,
            kind: RecoveryKind::Reject {
                message: "preparation cleanup failed".into(),
                no_usage: !token_issued,
                delivered: false,
            },
        };
        let _ = record.save(&recovery_path(&layout));
        let meta = Meta::new(unfinished.job_id, unfinished.sandbox_id);
        let permit = if container.is_some() {
            let permit = Arc::clone(&self.inner.permits).try_acquire_owned().ok();
            if permit.is_none() {
                self.inner.debt.fetch_add(1, Ordering::SeqCst);
            }
            Some(permit)
        } else {
            None
        };
        let handle = self.spawn_recovered(
            ctx,
            unfinished.project_id,
            layout,
            meta,
            None,
            created,
            job::RecoverMode::Reject { record, permit },
        );
        Ok(RecoveredJob::Running(handle))
    }

    /// 已提交终结后的重启可能没有内存槽位；只认持久化的可释放依据。
    async fn release_evidence(
        &self,
        job_id: &AttemptId,
    ) -> Result<(gantry_core::ids::SandboxId, ProjectId, bool), SandboxError> {
        let entries = std::fs::read_dir(&self.inner.config.workspaces_dir)
            .map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
        let mut sandboxes = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Ok(sandbox) = name.strip_suffix(".recovery.json").unwrap_or(name).parse() {
                sandboxes.insert(sandbox);
            }
        }
        let mut found = BTreeMap::new();
        let mut unidentifiable = false;
        let job_text = job_id.to_string();
        for sandbox in sandboxes {
            let layout = self.layout(sandbox)?;
            let guard = RecoveryRecord::load(&recovery_path(&layout));
            let meta = Meta::load(&layout.meta_json());
            let event = self.created_event(sandbox).await?;
            let claimed_job = |path: &std::path::Path| {
                std::fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .and_then(|value| {
                        value
                            .get("job_id")
                            .and_then(|id| id.as_str())
                            .map(str::to_owned)
                    })
            };
            let guard_claim = guard
                .is_err()
                .then(|| claimed_job(&recovery_path(&layout)))
                .flatten();
            let meta_claim = meta
                .is_err()
                .then(|| claimed_job(&layout.meta_json()))
                .flatten();
            // 损坏记录里的 job_id 只能用于阻断，不能证明它与其他 job 无关。
            unidentifiable |= guard.is_err() || meta.is_err();
            if !guard
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .is_some_and(|record| record.job_id == *job_id)
                && !meta
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .is_some_and(|meta| meta.job_id == *job_id)
                && !event.is_some_and(|(owner, _)| owner == *job_id)
                && guard_claim.as_deref() != Some(job_text.as_str())
                && meta_claim.as_deref() != Some(job_text.as_str())
            {
                continue;
            }
            let pending = || infra(InfraDetail::Collect, "unverified release evidence");
            let guard = guard.map_err(|_| pending())?;
            if let Some(record) = guard {
                if record.job_id != *job_id
                    || record.sandbox_id != sandbox
                    || event.is_some_and(|owner| owner != (*job_id, record.project_id))
                    || meta
                        .as_ref()
                        .ok()
                        .and_then(Option::as_ref)
                        .is_some_and(|meta| {
                            !meta.matches_identity(*job_id, sandbox, record.project_id)
                        })
                {
                    return Err(pending());
                }
                if let RecoveryKind::Reject {
                    no_usage,
                    delivered: true,
                    ..
                } = record.kind
                {
                    found.insert(sandbox, (record.project_id, !no_usage));
                    continue;
                }
                return Err(pending());
            }
            let meta = meta.map_err(|_| pending())?.ok_or_else(pending)?;
            let cancelled_without_token = meta.terminal_reason == Some(TerminalReason::Cancel)
                && !meta.token_issued
                && meta.timings.started.is_none()
                && meta.exit.is_none();
            let prestart_cancel = meta.terminal_reason == Some(TerminalReason::Cancel)
                && meta.timings.started.is_none()
                && meta.exit.is_none();
            let outcome_recorded = match meta.job_sha256 {
                Some(Some(_)) => {
                    (meta.timings.started.is_some() || prestart_cancel)
                        && (meta.identity_diagnostic.is_none() || meta.identity_finalized)
                }
                Some(None) => {
                    prestart_cancel
                        && (meta.identity_diagnostic.is_none() || meta.identity_finalized)
                }
                None => meta.identity_finalized || cancelled_without_token,
            };
            let project = meta
                .project_id
                .or_else(|| meta.resume.as_ref().map(|r| r.project_id))
                .ok_or_else(pending)?;
            if !meta.matches_identity(*job_id, sandbox, project)
                || event.is_some_and(|owner| owner != (*job_id, project))
                || !matches!(meta.phase, Phase::Destroyed | Phase::Failed)
                || !outcome_recorded
            {
                return Err(pending());
            }
            found.insert(sandbox, (project, meta.token_issued));
        }
        if found.is_empty() {
            if unidentifiable {
                return Err(infra(
                    InfraDetail::Collect,
                    "unidentifiable release evidence",
                ));
            }
            let mut labels = BTreeMap::new();
            labels.insert(LABEL_MANAGED.to_owned(), "1".to_owned());
            labels.insert(LABEL_JOB_ID.to_owned(), job_id.to_string());
            let containers = self
                .inner
                .deps
                .driver
                .list(&LabelSelector(labels))
                .await
                .map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
            if !containers.is_empty() {
                return Err(infra(
                    InfraDetail::Collect,
                    "container without release evidence",
                ));
            }
            return match self.inner.deps.egress.job_usage(job_id).await {
                Ok(_) => Err(infra(
                    InfraDetail::Collect,
                    "egress job without release evidence",
                )),
                Err(gantry_core::ports::egress::EgressError::NotFound) => {
                    Err(SandboxError::NotFound)
                }
                Err(err) => Err(infra(InfraDetail::Egress, format!("job usage: {err}"))),
            };
        }
        if found.len() != 1 {
            return Err(infra(InfraDetail::Collect, "ambiguous release evidence"));
        }
        let (sandbox, (project, forget)) = found.into_iter().next().expect("one entry");
        Ok((sandbox, project, forget))
    }

    async fn preserve_sweep_evidence(
        &self,
        layout: &StagingLayout,
        container_job: Option<Option<AttemptId>>,
    ) -> Result<bool, SandboxError> {
        if recovery_path(layout).exists() {
            return Ok(true);
        }
        let meta = match Meta::load(&layout.meta_json()) {
            Err(_) => return Ok(true),
            Ok(None) if !layout.dir().exists() => return Ok(false),
            Ok(None) => return Ok(self.created_event(layout.sandbox_id()).await?.is_some()),
            Ok(Some(meta)) => meta,
        };
        let project = meta
            .project_id
            .or_else(|| meta.resume.as_ref().map(|resume| resume.project_id));
        if project
            .is_none_or(|project| !meta.matches_identity(meta.job_id, layout.sandbox_id(), project))
            || container_job.is_some_and(|job| job != Some(meta.job_id))
            || meta.job_sha256.is_none()
            || meta.identity_diagnostic.is_some()
            || meta.phase == Phase::Destroyed
            // 已签发令牌的普通失败保留 release 依据（D-22），巡检不得清除。
            || (meta.phase == Phase::Failed && meta.token_issued)
        {
            return Ok(true);
        }
        Ok(self
            .created_event(layout.sandbox_id())
            .await?
            .is_some_and(|(job, project)| {
                !meta.matches_identity(job, layout.sandbox_id(), project)
            }))
    }

    async fn release_files(
        &self,
        ctx: &ActorCtx,
        job_id: &AttemptId,
        sandbox_id: gantry_core::ids::SandboxId,
        project_id: ProjectId,
        forget: bool,
    ) -> Result<(), SandboxError> {
        let layout = StagingLayout::new(&self.inner.config.workspaces_dir, sandbox_id)
            .map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
        let guard = RecoveryRecord::load(&recovery_path(&layout))
            .map_err(|err| infra(InfraDetail::Collect, format!("recovery record: {err}")))?;
        if guard
            .as_ref()
            .is_some_and(|record| !record.matches_identity(*job_id, sandbox_id, project_id))
        {
            return Err(infra(
                InfraDetail::Collect,
                "recovery record ownership prevents release",
            ));
        }
        match Meta::load(&layout.meta_json()) {
            Ok(Some(meta)) if meta.matches_identity(*job_id, sandbox_id, project_id) => {}
            Ok(None) if guard.is_some() => {}
            Err(_) if guard.is_some() => {
                let bytes = std::fs::read(layout.meta_json())
                    .map_err(|err| infra(InfraDetail::Collect, format!("metadata: {err}")))?;
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    // 损坏记录的字段只能用于阻断冲突，不能用于证明归属。
                    let claims = [
                        (value.get("job_id"), job_id.to_string()),
                        (value.get("sandbox_id"), sandbox_id.to_string()),
                        (
                            value.get("project_id").filter(|claim| !claim.is_null()),
                            project_id.to_string(),
                        ),
                        (
                            value
                                .get("resume")
                                .and_then(|resume| resume.get("project_id")),
                            project_id.to_string(),
                        ),
                    ];
                    if claims.iter().any(|(claim, owner)| {
                        claim.is_some_and(|claim| claim.as_str() != Some(owner.as_str()))
                    }) {
                        return Err(infra(
                            InfraDetail::Collect,
                            "damaged metadata ownership prevents release",
                        ));
                    }
                }
            }
            _ => {
                return Err(infra(
                    InfraDetail::Collect,
                    "metadata ownership prevents release",
                ))
            }
        }
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_SANDBOX_ID.to_owned(), sandbox_id.to_string());
        let containers = tokio::time::timeout(
            Duration::from_secs(5),
            self.inner.deps.driver.list(&LabelSelector(labels)),
        )
        .await
        .map_err(|_| {
            infra(
                InfraDetail::Collect,
                "runtime list timed out during release",
            )
        })?
        .map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
        if containers.iter().any(|c| {
            c.labels.get(LABEL_JOB_ID) != Some(&job_id.to_string())
                || c.labels.get(LABEL_MANAGED).is_none_or(|v| v != "1")
                || c.status == ContainerStatus::Running
        }) {
            return Err(infra(
                InfraDetail::Collect,
                "container ownership or state prevents release",
            ));
        }
        if forget {
            self.inner
                .deps
                .egress
                .forget_job(job_id, ctx)
                .await
                .map_err(|err| infra(InfraDetail::Egress, format!("forget: {err}")))?;
        }
        self.inner
            .deps
            .driver
            .purge_staging(&layout)
            .await
            .map_err(|err| infra(InfraDetail::Collect, err.to_string()))?;
        match std::fs::remove_file(recovery_path(&layout)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(infra(
                    InfraDetail::Collect,
                    format!("recovery record: {err}"),
                ))
            }
        }
        Ok(())
    }
}

/// 吊销 job 令牌；Egress 不认识它（从未签发或已遗忘）也算已吊销。
async fn revoke(
    egress: &dyn EgressControl,
    job_id: &AttemptId,
    ctx: &ActorCtx,
) -> Result<(), gantry_core::ports::egress::EgressError> {
    match egress.revoke_job_token(job_id, ctx).await {
        Ok(_) | Err(gantry_core::ports::egress::EgressError::NotFound) => Ok(()),
        Err(err) => Err(err),
    }
}

fn recovery_path(layout: &StagingLayout) -> PathBuf {
    layout.dir().with_extension("recovery.json")
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
            if slot.handle.sandbox_id != spec.sandbox_id
                || slot.project_id != spec.project_id
                || slot.spec.as_ref().is_some_and(|known| *known != spec)
            {
                return Err(SandboxError::InvalidSpec(
                    "a different sandbox spec was already submitted for this job".into(),
                ));
            }
            return Ok(slot.handle);
        }
        job::check_spec(&spec)?;
        let layout = StagingLayout::new(&self.inner.config.workspaces_dir, spec.sandbox_id)
            .map_err(|err| infra(InfraDetail::SandboxCreate, err.to_string()))?;
        if recovery_path(&layout).exists() {
            return Err(SandboxError::InvalidSpec(
                "job has unresolved recovery evidence".into(),
            ));
        }
        let mut accepted = match Meta::load(&layout.meta_json()) {
            Ok(Some(meta))
                if meta.job_id == spec.job_id
                    && meta.sandbox_id == spec.sandbox_id
                    && meta.phase == Phase::Accepted
                    && meta.job_sha256 == Some(None)
                    && meta.project_id == Some(spec.project_id)
                    && meta
                        .resume
                        .as_ref()
                        .is_none_or(|r| r.project_id == spec.project_id) =>
            {
                meta
            }
            Ok(None) if !layout.dir().exists() => Meta::new(spec.job_id, spec.sandbox_id),
            _ => {
                return Err(SandboxError::InvalidSpec(
                    "job has existing unverified metadata".into(),
                ))
            }
        };
        accepted.resume = None;
        accepted.project_id = Some(spec.project_id);
        // 返回 Ok 之前受理记录已原子写盘（E4）：重启后 recover 不会判成 Unknown。
        std::fs::create_dir_all(layout.dir())
            .and_then(|()| accepted.save(&layout.meta_json()))
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
                project_id: spec.project_id,
                result: result_rx,
                commands: command_tx,
                progress: progress_tx.clone(),
                occupancy: Arc::clone(&occupancy),
                released: Arc::default(),
                blocked: None,
            },
        );
        // 分派任务没了，接收端会立刻报错，actor 以 ShuttingDown 结束。
        let _ = self.inner.dispatcher().send(permit_tx);
        let actor = job::JobActor::new(
            Arc::clone(&self.inner),
            ctx.clone(),
            spec,
            layout,
            accepted,
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

    async fn cancel(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        let pending = {
            let mut jobs = self.jobs();
            let slot = jobs.get_mut(job_id).ok_or(SandboxError::NotFound)?;
            // 已结束的 job 收不到命令，取消是空操作（幂等）。
            let _ = slot.commands.send(Command::Cancel);
            slot.blocked.as_mut().map(|blocked| {
                blocked.wake = true;
                UnfinishedAttempt {
                    job_id: *job_id,
                    sandbox_id: slot.handle.sandbox_id,
                    project_id: slot.project_id,
                }
            })
        };
        if let Some(attempt) = pending {
            self.retry_recovery(ctx.clone(), attempt);
        }
        Ok(())
    }

    async fn release(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        let slot = {
            let jobs = self.jobs();
            jobs.get(job_id).map(|slot| {
                (
                    slot.handle.sandbox_id,
                    slot.project_id,
                    Arc::clone(&slot.released),
                    slot.result.borrow().clone(),
                    slot.blocked.is_some(),
                )
            })
        };
        let Some((sandbox_id, project_id, released, outcome, blocked)) = slot else {
            let _guard = self.inner.submit_lock.lock().await;
            return tokio::time::timeout(Duration::from_secs(10), async {
                let (sandbox_id, project_id, forget) = self.release_evidence(job_id).await?;
                self.release_files(ctx, job_id, sandbox_id, project_id, forget)
                    .await
            })
            .await
            .map_err(|_| infra(InfraDetail::Collect, "release timed out"))?;
        };
        if blocked {
            return Err(SandboxError::InvalidSpec(
                "release of unverified job".into(),
            ));
        }
        let outcome = outcome.ok_or_else(|| {
            SandboxError::InvalidSpec("release before the job reached a terminal state".into())
        })?;
        // 并发的 release 排队；失败不置位，调用方可以重试。
        let mut released = released.lock().await;
        if *released {
            return Ok(());
        }
        let _guard = self.inner.submit_lock.lock().await;
        let layout = self.layout(sandbox_id)?;
        let forget = match &outcome {
            Err(SandboxError::JobIdentityUnavailable { usage, .. }) => usage.is_some(),
            _ => Meta::load(&layout.meta_json())
                .ok()
                .flatten()
                .is_some_and(|meta| meta.token_issued),
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            self.release_files(ctx, job_id, sandbox_id, project_id, forget),
        )
        .await
        .map_err(|_| infra(InfraDetail::Collect, "release timed out"))??;
        *released = true;
        Ok(())
    }

    /// §7.10：先验证整批绑定，再对每个未终结尝试恰好返回一项（E4）。
    async fn recover(
        &self,
        ctx: &ActorCtx,
        unfinished: &[UnfinishedAttempt],
    ) -> Result<Vec<RecoveredJob>, SandboxError> {
        let _recovering = self.inner.submit_lock.lock().await;
        {
            let jobs = self.jobs();
            let mut bindings: BTreeMap<_, _> = jobs
                .iter()
                .filter(|(_, slot)| slot.blocked.is_none())
                .map(|(job_id, slot)| (*job_id, (slot.handle.sandbox_id, slot.project_id)))
                .collect();
            let mut owners = BTreeMap::new();
            for (job_id, slot) in jobs.iter() {
                if slot.blocked.is_some() {
                    continue;
                }
                let owner = owners
                    .entry(slot.handle.sandbox_id)
                    .or_insert(Some(*job_id));
                if *owner != Some(*job_id) {
                    *owner = None;
                }
            }
            for attempt in unfinished {
                let binding = (attempt.sandbox_id, attempt.project_id);
                if bindings
                    .get(&attempt.job_id)
                    .is_some_and(|known| *known != binding)
                    || owners
                        .get(&attempt.sandbox_id)
                        .is_some_and(|owner| *owner != Some(attempt.job_id))
                {
                    return Err(SandboxError::InvalidSpec(
                        "conflicting identity in recovery request".into(),
                    ));
                }
                bindings.insert(attempt.job_id, binding);
                owners.insert(attempt.sandbox_id, Some(attempt.job_id));
            }
        }
        let mut recovered = Vec::with_capacity(unfinished.len());
        let mut seen = BTreeMap::new();
        for attempt in unfinished {
            if let Some(item) = seen.get(&attempt.job_id) {
                recovered.push(*item);
                continue;
            }
            let item = match self.recover_one(ctx, attempt, false).await {
                Ok(item) => item,
                Err(err @ SandboxError::InvalidSpec(_)) => return Err(err),
                Err(_) => {
                    let item = self.block_recovery(attempt, "runtime or event query failed");
                    self.retry_recovery(ctx.clone(), attempt.clone());
                    item
                }
            };
            seen.insert(attempt.job_id, item);
            recovered.push(item);
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
        let _guard = self.inner.submit_lock.lock().await;
        let driver = &self.inner.deps.driver;
        let egress = &self.inner.deps.egress;
        let grace = self.inner.config.orphan_grace;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        // 只保护仍在推进、或结果尚未 release 的 job；已 release 的残留照样清理。
        let active: BTreeSet<AttemptId> = self
            .jobs()
            .iter()
            .filter(|(_, slot)| {
                slot.result.borrow().is_none()
                    || slot.released.try_lock().map_or(true, |released| !*released)
            })
            .map(|(job_id, _)| *job_id)
            .collect();
        let mut protected_sandboxes: BTreeSet<_> = self
            .jobs()
            .values()
            .filter(|slot| slot.blocked.is_some())
            .map(|slot| slot.handle.sandbox_id)
            .collect();
        let keep = |job_id: &AttemptId| live.contains(job_id) || active.contains(job_id);
        let mut report = SweepReport::default();
        let mut revoked = BTreeSet::new();

        let mut selector = BTreeMap::new();
        selector.insert(LABEL_MANAGED.to_owned(), "1".to_owned());
        let containers = tokio::time::timeout_at(deadline, driver.list(&LabelSelector(selector)))
            .await
            .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))?
            .map_err(|err| infra(InfraDetail::Lost, err.to_string()))?;
        let mut seen_sandboxes = BTreeSet::new();
        for summary in &containers {
            let Some(layout) = summary
                .labels
                .get(LABEL_SANDBOX_ID)
                .and_then(|value| value.parse().ok())
                .and_then(|sandbox_id| self.layout(sandbox_id).ok())
            else {
                continue;
            };
            let job_id = summary
                .labels
                .get(LABEL_JOB_ID)
                .and_then(|value| value.parse::<AttemptId>().ok());
            if !seen_sandboxes.insert(layout.sandbox_id())
                || tokio::time::timeout_at(
                    deadline,
                    self.preserve_sweep_evidence(&layout, Some(job_id)),
                )
                .await
                .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))??
            {
                protected_sandboxes.insert(layout.sandbox_id());
            }
        }
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
            if summary
                .labels
                .get(LABEL_SANDBOX_ID)
                .and_then(|value| value.parse().ok())
                .is_some_and(|id| protected_sandboxes.contains(&id))
            {
                continue;
            }
            if layout
                .as_ref()
                .is_some_and(|layout| !older_than(layout, grace))
            {
                continue;
            }
            if let Some(job_id) = job_id {
                // 先吊销：容器删除前它的令牌已作废。吊销失败就不删，留到下一轮重试。
                if let Err(err) =
                    tokio::time::timeout_at(deadline, revoke(egress.as_ref(), &job_id, ctx))
                        .await
                        .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))?
                {
                    report.errors.push(format!("revoke {job_id}: {err}"));
                    continue;
                }
                revoked.insert(job_id);
            }
            let _ = tokio::time::timeout_at(deadline, driver.kill(&summary.id))
                .await
                .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))?;
            match tokio::time::timeout_at(deadline, driver.remove(&summary.id))
                .await
                .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))?
            {
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
            if protected_sandboxes.contains(&sandbox_id) {
                continue;
            }
            if tokio::time::timeout_at(deadline, self.preserve_sweep_evidence(&layout, None))
                .await
                .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))??
            {
                continue;
            }
            let meta = Meta::load(&layout.meta_json()).ok().flatten();
            let job_id = meta.as_ref().map(|meta| meta.job_id);
            if job_id.as_ref().is_some_and(keep) || !older_than(&layout, grace) {
                continue;
            }
            if let Some(job_id) = job_id {
                let issued = meta.as_ref().is_some_and(|meta| meta.token_issued);
                if issued && !revoked.contains(&job_id) {
                    if let Err(err) =
                        tokio::time::timeout_at(deadline, revoke(egress.as_ref(), &job_id, ctx))
                            .await
                            .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))?
                    {
                        // meta.json 还在，下一轮还能找到它。
                        report.errors.push(format!("revoke {job_id}: {err}"));
                        continue;
                    }
                    revoked.insert(job_id);
                }
            }
            match tokio::time::timeout_at(deadline, driver.purge_staging(&layout))
                .await
                .map_err(|_| infra(InfraDetail::Lost, "orphan sweep timed out"))?
            {
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
