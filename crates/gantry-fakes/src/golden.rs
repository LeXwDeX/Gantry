//! 回放 `single` 黄金结果的沙盒替身（engine.md §12.2「端到端（替身）」）。
//!
//! 每个 job 第一次受理时，现场把一份合法的成功结果写进结果库（CAS）并脚本化给内层
//! [`FakeSandboxService`]：`result.json`、一个补丁和一条 `open_pr` 提议。job_id 由 decide
//! 生成，测试事先拿不到，所以不能提前脚本化。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use gantry_core::canon::{sha256, GitOid};
use gantry_core::entities::TokenUsage;
use gantry_core::ids::{AttemptId, Clock};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::sandbox::{
    ArtifactName, CapacitySnapshot, JobHandle, JobOutcome, ProgressSample, RecoveredJob,
    SandboxError, SandboxService, SandboxSpec, SweepReport, UnfinishedAttempt,
};
use gantry_core::worker::FileArtifact;
use gantry_store::{Cas, CasKind};
use serde_json::json;

use crate::sandbox::FakeSandboxService;

/// 黄金结果里 `open_pr` 提议的标题。
pub const GOLDEN_PR_TITLE: &str = "Fix login timeout";

pub struct GoldenSandbox {
    inner: FakeSandboxService,
    cas: Cas,
    base_sha: GitOid,
    clock: Arc<dyn Clock>,
    jobs: Mutex<Vec<AttemptId>>,
}

impl GoldenSandbox {
    /// `base_sha` 是补丁声明的基线，须与工作树的基线一致。
    pub fn new(cas: Cas, base_sha: GitOid, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: FakeSandboxService::new(),
            cas,
            base_sha,
            clock,
            jobs: Mutex::new(Vec::new()),
        }
    }

    /// 按受理顺序列出已派发的 job。
    pub fn jobs(&self) -> Vec<AttemptId> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn inner(&self) -> &FakeSandboxService {
        &self.inner
    }

    async fn put(&self, bytes: &[u8]) -> FileArtifact {
        FileArtifact {
            sha256: self
                .cas
                .put(bytes, CasKind::Artifact)
                .await
                .expect("in-memory CAS accepts artifacts"),
            bytes: bytes.len() as u64,
        }
    }

    async fn golden(&self, spec: &SandboxSpec) -> JobOutcome {
        let job = spec.job_id;
        let now = self.clock.now();
        let patch = self
            .put(b"diff --git a/src/login.rs b/src/login.rs\n")
            .await;
        let proposals = json!({
            "protocol": "gantry-worker/1",
            "job_id": job,
            "proposals": [{
                "id": "p1", "kind": "open_pr", "target": null,
                "params": {"title": GOLDEN_PR_TITLE, "body": "body", "draft": false},
                "rationale": "done"
            }]
        });
        let proposals = self.put(&to_vec(&proposals)).await;
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
                          "base_sha": self.base_sha, "files_changed": 1},
                "report": null,
                "proposals": {"path": "proposals.json", "sha256": proposals.sha256,
                              "bytes": proposals.bytes},
            },
            "flags": {"suspected_injection": []},
            "usage_reported": {"input_tokens": 1, "output_tokens": 1, "cache_read_tokens": 0,
                               "cache_write_tokens": 0, "turns": 1, "tool_calls": 1},
            "agent": {"kind": "claude-code", "version": "1.0.0", "model": "claude-test"},
            "started_at": now,
            "ended_at": now,
        });
        let result = self.put(&to_vec(&result)).await;
        let mut outcome = FakeSandboxService::success_outcome(job, spec.sandbox_id);
        outcome.files = BTreeMap::from([
            (ArtifactName::new("result.json"), result),
            (ArtifactName::new("changes.patch"), patch),
            (ArtifactName::new("proposals.json"), proposals),
        ]);
        outcome.usage_metered = TokenUsage::new(1_000, 200, 0, 0, 3);
        outcome.weighted_tokens = 2_000;
        outcome
    }
}

fn to_vec(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("JSON value serializes")
}

#[async_trait]
impl SandboxService for GoldenSandbox {
    async fn submit(&self, ctx: &ActorCtx, spec: SandboxSpec) -> Result<JobHandle, SandboxError> {
        let first = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
