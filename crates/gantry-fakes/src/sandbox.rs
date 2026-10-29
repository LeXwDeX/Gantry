//! `FakeSandboxService`：不起容器，按脚本回放 `JobOutcome`。
//!
//! 只模拟引擎面语义：`submit` / `wait` / `cancel` / `release` 幂等，`wait`
//! 对同一 job 恒返回同一结果，`progress` 流式回放脚本样本。`recover` 按
//! 受理集合（成功 `submit` 过的 job）分类：未受理 → `Unknown`；已受理的按
//! 脚本给出的运行时状态返回 `Running` / `Exited` / `Lost`。不重现 sandbox
//! 的生命周期、Egress 调用或 worker 校验。

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use gantry_core::canon::sha256;
use gantry_core::entities::TokenUsage;
use gantry_core::ids::{AttemptId, SandboxId};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::sandbox::{
    CapacitySnapshot, EgressFlags, InfraDetail, JobHandle, JobOutcome, ProgressSample,
    ProgressStats, RecoveredJob, SandboxError, SandboxService, SandboxSpec, SweepReport,
    TerminalCause, TerminalFacts, Timings, UnfinishedAttempt,
};
use gantry_core::worker::FileArtifact;

/// 调用记录；测试用它断言「调用了什么」。
///
/// `Submit` 保留完整 `SandboxSpec` 便于读取，不进入热路径。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum SandboxCall {
    Submit {
        ctx: ActorCtx,
        spec: SandboxSpec,
    },
    Wait {
        job_id: AttemptId,
    },
    Progress {
        job_id: AttemptId,
    },
    Cancel {
        ctx: ActorCtx,
        job_id: AttemptId,
    },
    Release {
        ctx: ActorCtx,
        job_id: AttemptId,
    },
    Recover {
        ctx: ActorCtx,
        unfinished: Vec<UnfinishedAttempt>,
    },
    SweepOrphans {
        ctx: ActorCtx,
        live: BTreeSet<AttemptId>,
    },
    Capacity,
}

impl SandboxCall {
    pub fn op(&self) -> &'static str {
        match self {
            Self::Submit { .. } => "submit",
            Self::Wait { .. } => "wait",
            Self::Progress { .. } => "progress",
            Self::Cancel { .. } => "cancel",
            Self::Release { .. } => "release",
            Self::Recover { .. } => "recover",
            Self::SweepOrphans { .. } => "sweep_orphans",
            Self::Capacity => "capacity",
        }
    }
}

/// 替身里已受理 job 的运行时状态，只影响 `recover` 的分类与丢失后的 `wait`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FakeRuntime {
    /// 已停止，`wait` 返回脚本结果（默认）。
    #[default]
    Exited,
    /// 仍在运行；替身不阻塞，`wait` 仍返回脚本结果。
    Running,
    /// 受理后丢失；`wait` 返回 `Infra { Lost }`。
    Lost,
}

struct JobEntry {
    spec: Option<SandboxSpec>,
    /// 成功 `submit` 过；`script` 预放 spec 不算受理。
    accepted: bool,
    runtime: FakeRuntime,
    outcome: Result<JobOutcome, SandboxError>,
    progress: Vec<ProgressSample>,
    canceled: bool,
    released: bool,
    swept: bool,
}

#[derive(Default)]
struct State {
    jobs: BTreeMap<AttemptId, JobEntry>,
    calls: Vec<SandboxCall>,
    recover_reply: Option<Result<Vec<RecoveredJob>, SandboxError>>,
    recover_replies: std::collections::VecDeque<Result<Vec<RecoveredJob>, SandboxError>>,
    next_wait_error: Option<SandboxError>,
}

struct VecStream {
    items: std::vec::IntoIter<ProgressSample>,
}

impl Stream for VecStream {
    type Item = ProgressSample;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.items.next())
    }
}

/// M0 的沙盒服务替身。
pub struct FakeSandboxService {
    state: Mutex<State>,
}

impl Default for FakeSandboxService {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSandboxService {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    /// 预放一个结果；之后 `submit` 不要求先持有 spec，`wait` 直接返回它。
    pub fn script_outcome(&self, job_id: AttemptId, sandbox_id: SandboxId, outcome: JobOutcome) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.jobs.insert(
            job_id,
            JobEntry {
                spec: None,
                accepted: false,
                runtime: FakeRuntime::default(),
                outcome: Ok(JobOutcome {
                    job_id,
                    sandbox_id,
                    ..outcome
                }),
                progress: Vec::new(),
                canceled: false,
                released: false,
                swept: false,
            },
        );
    }

    /// 给已脚本化的 job 注入稳定的 wait 错误。
    pub fn script_wait_error(&self, job_id: AttemptId, error: SandboxError) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .jobs
            .get_mut(&job_id)
            .expect("script the job first")
            .outcome = Err(error);
    }

    /// 测试端口违约时注入 recover 返回值。
    pub fn script_recover_reply(&self, reply: Vec<RecoveredJob>) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .recover_reply = Some(Ok(reply));
    }

    /// 测试恢复请求被拒绝时注入端口错误。
    pub fn script_recover_error(&self, error: SandboxError) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .recover_reply = Some(Err(error));
    }

    /// 按调用顺序注入多次 `recover` 回复；用完后回退到 `script_recover_reply` / 默认。
    pub fn script_recover_replies(
        &self,
        replies: impl IntoIterator<Item = Result<Vec<RecoveredJob>, SandboxError>>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.recover_replies = replies.into_iter().collect();
    }

    /// 下一次 `wait` 返回一次指定错误，之后恢复正常。
    pub fn fail_next_wait(&self, error: SandboxError) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_wait_error = Some(error);
    }

    pub fn script(&self, spec: SandboxSpec, outcome: JobOutcome) {
        let job_id = spec.job_id;
        let sandbox_id = spec.sandbox_id;
        self.script_outcome(job_id, sandbox_id, outcome);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = state.jobs.get_mut(&job_id) {
            entry.spec = Some(spec);
        }
    }

    /// 设定已脚本化 job 的运行时状态（模拟重启时运行时里看到的情形）。
    pub fn set_runtime(&self, job_id: AttemptId, runtime: FakeRuntime) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = state.jobs.get_mut(&job_id) {
            entry.runtime = runtime;
        }
    }

    pub fn push_progress(&self, sample: ProgressSample) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = state.jobs.get_mut(&sample.job_id) {
            entry.progress.push(sample);
        }
    }

    pub fn calls(&self) -> Vec<SandboxCall> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .calls
            .clone()
    }

    /// 一个合法的成功结果：退出码 0、无产物、零用量。测试可在此基础上改字段。
    pub fn success_outcome(job_id: AttemptId, sandbox_id: SandboxId) -> JobOutcome {
        let empty = FileArtifact {
            sha256: sha256(b""),
            bytes: 0,
        };
        JobOutcome {
            job_id,
            sandbox_id,
            // 服务端替身声明，独立于 worker 结果文件。
            job_sha256: Some(sha256(b"job.json")),
            terminal: TerminalFacts {
                cause: TerminalCause::Exited,
                exit_code: Some(0),
                signal: None,
                oom: false,
                timer: None,
                cancel_requested: false,
            },
            partial: false,
            files: BTreeMap::new(),
            unexpected: Vec::new(),
            progress: None,
            progress_stats: ProgressStats::default(),
            stdout_tail: empty.clone(),
            stderr_tail: empty,
            usage_metered: TokenUsage::default(),
            weighted_tokens: 0,
            egress: EgressFlags {
                capped: false,
                upstream_error_summary: None,
                incomplete: false,
            },
            timings: Timings::default(),
        }
    }
}

#[async_trait]
impl SandboxService for FakeSandboxService {
    async fn submit(&self, ctx: &ActorCtx, spec: SandboxSpec) -> Result<JobHandle, SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Submit {
            ctx: ctx.clone(),
            spec: spec.clone(),
        });
        let job_id = spec.job_id;
        let sandbox_id = spec.sandbox_id;
        let Some(entry) = state.jobs.get_mut(&job_id) else {
            return Err(SandboxError::InvalidSpec(
                "fake sandbox has no scripted outcome for this job".into(),
            ));
        };
        if let Some(existing) = &entry.spec {
            if existing.sandbox_id != sandbox_id || existing != &spec {
                return Err(SandboxError::InvalidSpec(
                    "a different sandbox spec was already submitted for this job".into(),
                ));
            }
            entry.accepted = true;
            return Ok(JobHandle { job_id, sandbox_id });
        }
        entry.spec = Some(spec);
        entry.accepted = true;
        Ok(JobHandle { job_id, sandbox_id })
    }

    async fn wait(&self, job_id: &AttemptId) -> Result<JobOutcome, SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Wait { job_id: *job_id });
        if let Some(error) = state.next_wait_error.take() {
            return Err(error);
        }
        let entry = state.jobs.get(job_id).ok_or(SandboxError::NotFound)?;
        if entry.accepted && (entry.runtime == FakeRuntime::Lost || entry.swept) {
            return Err(SandboxError::Infra {
                detail: InfraDetail::Lost,
                message: "fake sandbox job was lost".into(),
            });
        }
        entry.outcome.clone()
    }

    fn progress(&self, job_id: &AttemptId) -> BoxStream<'static, ProgressSample> {
        let items = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.calls.push(SandboxCall::Progress { job_id: *job_id });
            state
                .jobs
                .get(job_id)
                .map(|entry| entry.progress.clone())
                .unwrap_or_default()
        };
        Box::pin(VecStream {
            items: items.into_iter(),
        })
    }

    async fn cancel(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Cancel {
            ctx: ctx.clone(),
            job_id: *job_id,
        });
        let Some(entry) = state.jobs.get_mut(job_id) else {
            return Err(SandboxError::NotFound);
        };
        entry.canceled = true;
        Ok(())
    }

    async fn release(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Release {
            ctx: ctx.clone(),
            job_id: *job_id,
        });
        let Some(entry) = state.jobs.get_mut(job_id) else {
            return Err(SandboxError::NotFound);
        };
        entry.released = true;
        Ok(())
    }

    async fn recover(
        &self,
        ctx: &ActorCtx,
        unfinished: &[UnfinishedAttempt],
    ) -> Result<Vec<RecoveredJob>, SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Recover {
            ctx: ctx.clone(),
            unfinished: unfinished.to_vec(),
        });
        if let Some(reply) = state.recover_replies.pop_front() {
            return reply;
        }
        if let Some(reply) = &state.recover_reply {
            return reply.clone();
        }
        let recovered = unfinished
            .iter()
            .map(|attempt| {
                let handle = JobHandle {
                    job_id: attempt.job_id,
                    sandbox_id: attempt.sandbox_id,
                };
                match state.jobs.get(&attempt.job_id) {
                    Some(entry) if entry.accepted => {
                        if entry.swept {
                            return RecoveredJob::Lost(handle);
                        }
                        match entry.runtime {
                            FakeRuntime::Running => RecoveredJob::Running(handle),
                            FakeRuntime::Exited => RecoveredJob::Exited(handle),
                            FakeRuntime::Lost => RecoveredJob::Lost(handle),
                        }
                    }
                    _ => RecoveredJob::Unknown {
                        job_id: attempt.job_id,
                    },
                }
            })
            .collect();
        Ok(recovered)
    }

    async fn sweep_orphans(
        &self,
        ctx: &ActorCtx,
        live: &BTreeSet<AttemptId>,
    ) -> Result<SweepReport, SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::SweepOrphans {
            ctx: ctx.clone(),
            live: live.clone(),
        });
        let mut report = SweepReport::default();
        for (job_id, entry) in &mut state.jobs {
            if live.contains(job_id) || entry.released || entry.swept {
                continue;
            }
            entry.swept = true;
            report.destroyed += 1;
        }
        Ok(report)
    }

    async fn capacity(&self) -> CapacitySnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Capacity);
        let running = state
            .jobs
            .values()
            .filter(|entry| !entry.released && !entry.swept)
            .count() as u32;
        CapacitySnapshot {
            running,
            queued: 0,
            limit: 1,
            prewarmed: 0,
        }
    }
}
