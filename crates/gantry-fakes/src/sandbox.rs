//! `FakeSandboxService`：不起容器，按脚本回放 `JobOutcome`。
//!
//! 只模拟引擎面语义：`submit` / `wait` / `cancel` / `release` 幂等，`wait`
//! 对同一 job 恒返回同一结果，`progress` 流式回放脚本样本。不重现 sandbox
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
    CapacitySnapshot, EgressFlags, JobHandle, JobOutcome, ProgressSample, ProgressStats,
    SandboxError, SandboxService, SandboxSpec, SweepReport, TerminalCause, TerminalFacts, Timings,
    UnfinishedAttempt,
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

struct JobEntry {
    spec: Option<SandboxSpec>,
    outcome: JobOutcome,
    progress: Vec<ProgressSample>,
    canceled: bool,
    released: bool,
    swept: bool,
}

#[derive(Default)]
struct State {
    jobs: BTreeMap<AttemptId, JobEntry>,
    calls: Vec<SandboxCall>,
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
                outcome: JobOutcome {
                    job_id,
                    sandbox_id,
                    ..outcome
                },
                progress: Vec::new(),
                canceled: false,
                released: false,
                swept: false,
            },
        );
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
            return Ok(JobHandle { job_id, sandbox_id });
        }
        entry.spec = Some(spec);
        Ok(JobHandle { job_id, sandbox_id })
    }

    async fn wait(&self, job_id: &AttemptId) -> Result<JobOutcome, SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Wait { job_id: *job_id });
        state
            .jobs
            .get(job_id)
            .map(|entry| entry.outcome.clone())
            .ok_or(SandboxError::NotFound)
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
    ) -> Result<Vec<JobHandle>, SandboxError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(SandboxCall::Recover {
            ctx: ctx.clone(),
            unfinished: unfinished.to_vec(),
        });
        let mut handles = Vec::new();
        for attempt in unfinished {
            if state.jobs.contains_key(&attempt.job_id) {
                handles.push(JobHandle {
                    job_id: attempt.job_id,
                    sandbox_id: attempt.sandbox_id,
                });
            }
        }
        Ok(handles)
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
