//! `meta.json`：每次相位迁移先原子写盘再执行动作（sandbox.md §7.1、§7.13）。
//!
//! 记相位、容器 ID、deadline、终止原因、「令牌是否已签发」，以及崩溃恢复要用的
//! 运行参数（[`Resume`]）、退出事实与各相位时刻；不存令牌本身，也不存 `SandboxSpec`。

use std::fs;
use std::io::Write;
use std::path::Path;

use gantry_core::canon::Sha256Digest;
use gantry_core::ids::{AttemptId, ProjectId, SandboxId};
use gantry_core::ports::sandbox::Timings;
use gantry_core::settings::resource::SandboxIo;
use gantry_core::time::Timestamp;
use serde::{Deserialize, Serialize};

use crate::driver::ExitFacts;
use crate::intake::IntakePlan;

/// 内部状态机（§7.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Accepted,
    Queued,
    Creating,
    Created,
    Staging,
    Staged,
    Starting,
    Running,
    Stopping,
    Stopped,
    Collecting,
    Collected,
    Destroying,
    Destroyed,
    Failed,
}

/// 只写一次的终止原因（§7.5，先写入者胜）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    Cancel,
    WallTimer,
    IdleTimer,
    RuntimeExit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub phase: Phase,
    /// 缺失为旧记录，显式 null 为新记录尚未 stage。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "digest_field"
    )]
    pub job_sha256: Option<Option<Sha256Digest>>,
    #[serde(default)]
    pub container_id: Option<String>,
    /// `start` 时刻的 UTC 毫秒 + `wall_time`。
    #[serde(default)]
    pub deadline_ms: Option<i64>,
    #[serde(default)]
    pub terminal_reason: Option<TerminalReason>,
    pub token_issued: bool,
    #[serde(default)]
    pub cancel_requested: bool,
    /// 恢复监督与 collect 所需参数（受理后、排队前写入）。
    #[serde(default)]
    pub resume: Option<Resume>,
    /// `Stopped` 时记录：容器删除后恢复仍能给出终止事实。
    #[serde(default)]
    pub exit: Option<ExitFacts>,
    #[serde(default)]
    pub lost: bool,
    #[serde(default)]
    pub timings: TimingsRecord,
    /// `Collected` 时记录的日志尾部 CAS 引用：容器删除后恢复，尾部仍与原结果相同。
    #[serde(default)]
    pub tails: Option<TailsRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_diagnostic: Option<String>,
    #[serde(default)]
    pub identity_finalized: bool,
}

fn digest_field<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<Sha256Digest>>, D::Error> {
    Option::<Sha256Digest>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasRef {
    pub sha256: Sha256Digest,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailsRecord {
    pub stdout: CasRef,
    pub stderr: CasRef,
}

/// 崩溃恢复要用、但 `UnfinishedAttempt` 不带的运行参数。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub project_id: ProjectId,
    pub io: SandboxIo,
    pub intake: IntakePlan,
    pub wall_time_s: u64,
    pub idle_timeout_s: u64,
    pub kill_grace_s: u64,
}

/// [`Timings`] 的落盘形状。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingsRecord {
    #[serde(default)]
    pub created: Option<Timestamp>,
    #[serde(default)]
    pub staged: Option<Timestamp>,
    #[serde(default)]
    pub started: Option<Timestamp>,
    #[serde(default)]
    pub stopped: Option<Timestamp>,
    #[serde(default)]
    pub collected: Option<Timestamp>,
}

impl From<Timings> for TimingsRecord {
    fn from(t: Timings) -> Self {
        Self {
            created: t.created,
            staged: t.staged,
            started: t.started,
            stopped: t.stopped,
            collected: t.collected,
        }
    }
}

impl From<TimingsRecord> for Timings {
    fn from(t: TimingsRecord) -> Self {
        Self {
            created: t.created,
            staged: t.staged,
            started: t.started,
            stopped: t.stopped,
            collected: t.collected,
        }
    }
}

impl Meta {
    pub fn new(job_id: AttemptId, sandbox_id: SandboxId) -> Self {
        Self {
            job_id,
            sandbox_id,
            project_id: None,
            phase: Phase::Accepted,
            job_sha256: Some(None),
            container_id: None,
            deadline_ms: None,
            terminal_reason: None,
            token_issued: false,
            cancel_requested: false,
            resume: None,
            exit: None,
            lost: false,
            timings: TimingsRecord::default(),
            tails: None,
            identity_diagnostic: None,
            identity_finalized: false,
        }
    }

    /// 相位已过 `Starting`（`start` 确定成功过）。`Starting` 本身不算：deadline 在
    /// `start` 之前就落盘，那一刻容器可能还没启动。
    pub fn past_start(&self) -> bool {
        matches!(
            self.phase,
            Phase::Running
                | Phase::Stopping
                | Phase::Stopped
                | Phase::Collecting
                | Phase::Collected
                | Phase::Destroying
                | Phase::Destroyed
        )
    }

    /// 写入原因；已有原因时不覆盖（先写入者胜），返回最终生效的原因。
    pub fn record_reason(&mut self, reason: TerminalReason) -> TerminalReason {
        *self.terminal_reason.get_or_insert(reason)
    }

    /// 临时文件 + `fsync` + `rename`。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(std::io::Error::other),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

/// 在清理 staging 时仍保留的恢复依据。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RecoveryRecord {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
    pub project_id: ProjectId,
    pub kind: RecoveryKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum RecoveryKind {
    Reenter {
        token_issued: bool,
    },
    Reject {
        message: String,
        no_usage: bool,
        #[serde(default)]
        delivered: bool,
    },
}

impl RecoveryRecord {
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(tmp, path)
    }

    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(std::io::Error::other),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}
