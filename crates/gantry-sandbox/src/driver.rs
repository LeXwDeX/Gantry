//! 容器面 `SandboxDriver`（sandbox.md §6.2，P-52）：本模块内部接口，不对 `engine` 暴露。
//!
//! 驱动可替换：[`crate::podman::PodmanDriver`] 经 `gantry-sbx` 的 Podman 套接字调用
//! rootless Podman；`FakeDriver`（`testing` feature）用内存模拟容器，供驱动之上的
//! 逻辑做 A 级测试。两者跑同一份驱动一致性套件（§12.A）。

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use gantry_core::time::Timestamp;

use crate::intake::{IntakePlan, IntakeResult};
use crate::invariants::InvariantViolation;
use crate::spec::{ContainerId, ContainerSpec, ImageDigest, Runtime, StagingLayout};

/// 驱动如实报告的能力；`gantryd` 启动自检据此决定能否启动（§8）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverCaps {
    pub runtime: Runtime,
    pub oom_reporting: bool,
    pub disk_quota: bool,
    pub supports_pool: bool,
    /// cgroup v2 是否委派了 `memory` / `pids`（缺失时拒绝启动，§8 [提议]）。
    pub memory_limit: bool,
    pub pids_limit: bool,
    pub cpu_limit: bool,
}

/// 运行时观察到的退出事实（W3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitFacts {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub oom_killed: bool,
    pub finished_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerStatus {
    Created,
    Running,
    Exited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerState {
    pub id: ContainerId,
    pub status: ContainerStatus,
    pub exit: Option<ExitFacts>,
    pub labels: BTreeMap<String, String>,
}

/// 日志位置：已读过的行数。可重复读，崩溃恢复靠进度事件的 `seq` 去重。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogCursor(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// 一行日志（不含换行符），不可信。`cursor` 是读完这一行之后的位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogChunk {
    pub cursor: LogCursor,
    pub stream: LogStream,
    pub line: Vec<u8>,
}

/// 按标签选择容器（全部匹配）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelSelector(pub BTreeMap<String, String>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSummary {
    pub id: ContainerId,
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub status: ContainerStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    #[error("container not found")]
    NotFound,
    #[error("container spec violates an invariant: {0}")]
    InvariantViolated(#[from] InvariantViolation),
    #[error("image {0} is not present locally")]
    ImageMissing(String),
    #[error("container is still running")]
    StillRunning,
    #[error("container is in the wrong state for {0}")]
    WrongState(&'static str),
    #[error("runtime call {op} failed: {message}")]
    Runtime { op: &'static str, message: String },
    #[error("host file operation failed: {0}")]
    Io(String),
}

#[async_trait]
pub trait SandboxDriver: Send + Sync {
    fn capabilities(&self) -> DriverCaps;

    async fn image_present(&self, digest: &ImageDigest) -> Result<bool, DriverError>;

    /// 未启动的容器。驱动按自己的 `workspaces_dir` 与标签 `gantry.sandbox_id` 推出作业
    /// 目录，`create` 之前跑 `validate_invariants`，不通过就不创建；同名容器已存在时
    /// 先删除再建（崩溃恢复时重走 Creating 相位的幂等保证，§7.10）。`job_token` 只出现在
    /// `token_env` 变量里这一条由服务层在签发后用同一个参数闸再校验一次。
    async fn create(&self, spec: &ContainerSpec) -> Result<ContainerId, DriverError>;

    async fn start(&self, id: &ContainerId) -> Result<(), DriverError>;

    /// SIGTERM → 等待 grace → SIGKILL（容器级）。返回时容器必须已处于 exited，且其 PID
    /// 命名空间内没有残留进程。
    async fn stop(&self, id: &ContainerId, grace: Duration) -> Result<(), DriverError>;

    /// 立即 SIGKILL。
    async fn kill(&self, id: &ContainerId) -> Result<(), DriverError>;

    async fn wait(&self, id: &ContainerId) -> Result<ExitFacts, DriverError>;

    async fn inspect(&self, id: &ContainerId) -> Result<ContainerState, DriverError>;

    /// 主机侧日志接口：从 `since` 起读，可重复读，顺序稳定；容器停止且读完后结束。
    fn logs(
        &self,
        id: &ContainerId,
        since: Option<LogCursor>,
    ) -> BoxStream<'static, Result<LogChunk, DriverError>>;

    /// 幂等：不存在也返回 `Ok`。
    async fn remove(&self, id: &ContainerId) -> Result<(), DriverError>;

    async fn list(&self, selector: &LabelSelector) -> Result<Vec<ContainerSummary>, DriverError>;

    /// 建作业目录与 `inputs/` `work/` `out/`（属主适配见 §7.7，OQ-S2）。
    async fn prepare_dirs(&self, layout: &StagingLayout) -> Result<(), DriverError>;

    /// 先停后取：容器仍在运行时必须拒绝（`StillRunning`）。只读 `out/`。
    async fn read_out(
        &self,
        id: &ContainerId,
        layout: &StagingLayout,
        plan: &IntakePlan,
    ) -> Result<IntakeResult, DriverError>;

    /// 幂等：删除整个作业目录。
    async fn purge_staging(&self, layout: &StagingLayout) -> Result<(), DriverError>;
}
