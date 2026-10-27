//! `meta.json`：每次相位迁移先原子写盘再执行动作（sandbox.md §7.1、§7.13）。
//!
//! 只记相位、容器 ID、deadline、终止原因与「令牌是否已签发」；不存令牌本身。

use std::fs;
use std::io::Write;
use std::path::Path;

use gantry_core::ids::{AttemptId, SandboxId};
use serde::{Deserialize, Serialize};

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
        }
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
