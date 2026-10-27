//! `meta.json`：每次相位迁移先原子写盘再执行动作（sandbox.md §7.1、§7.13）。
//!
//! 记相位、容器 ID、deadline、终止原因、「令牌是否已签发」，以及崩溃恢复要用的
//! 运行参数（[`Resume`]）、退出事实与各相位时刻；不存令牌本身，也不存 `SandboxSpec`。

use std::fs;
use std::io::Write;
use std::path::Path;

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
    pub phase: Phase,
    #[serde(default)]
    pub container_id: Option<String>,
    /// `start` 时刻的 UTC 毫秒 + `wall_time`。
    #[serde(default)]
    pub deadline_ms: Option<i64>,
    #[serde(default)]
    pub terminal_reason: Option<TerminalReason>,
    #[serde(default)]
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
            phase: Phase::Accepted,
            container_id: None,
            deadline_ms: None,
            terminal_reason: None,
            token_issued: false,
            cancel_requested: false,
            resume: None,
            exit: None,
            lost: false,
            timings: TimingsRecord::default(),
        }
    }

    /// 容器是否已经启动过（`start` 成功后才有 deadline）。
    pub fn started(&self) -> bool {
        self.deadline_ms.is_some()
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
