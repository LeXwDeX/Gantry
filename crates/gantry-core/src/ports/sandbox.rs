//! `SandboxService`（引擎面）及其请求 / 响应类型。
//!
//! 签名以 `docs/modules/sandbox.md` §6.1 为准。`Ctx` 按 P-78 统一为
//! [`ActorCtx`]，所有写方法都接收它。`core` 不依赖 tokio；进度流使用
//! `futures_core::stream::BoxStream`。
//!
//! 契约缺口：`Timings`、`ProgressStats`、`CapacitySnapshot`、`SweepReport`、
//! `ProgressSample` 与 `UnfinishedAttempt` 在规格书中只有名字或注释；本文件
//! 给出最小类型，不声称完整协议已冻结。

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use futures_core::stream::BoxStream;

use crate::canon::{GitOid, RepoSlug, Sha256Digest};
use crate::entities::{Role, SandboxLimits, SandboxTrust, Seq, TokenUsage, TokenWeights};
use crate::ids::{AttemptId, ConversationId, DeliveryId, NodeId, ProjectId, SandboxId};
use crate::settings::resource::{SandboxDriver, SandboxIo};
use crate::text::UntrustedText;
use crate::time::Timestamp;
use crate::worker::{FileArtifact, JobSpec, MaterialKind, ProgressStage, ResultTerminalCause};

use super::context::{ActorCtx, CasRef, StoreError};

/// 可信的 JobSpec 部分。规格书中的 `JobSpecTrusted` 未给独立形状；M0 以完整
/// `JobSpec` 承载（`materials` 与 `isolation.nonce` 由 sandbox 在 staging 时
/// 处理），标记为契约缺口，不另造一份字段。
pub type JobSpecTrusted = JobSpec;

/// 容器运行时驱动（设置 `sandbox.driver`）。
pub type DriverKind = SandboxDriver;

/// `sandbox.trust`：可信 / 不可信任务。
pub type Trust = SandboxTrust;

/// 沙盒 I/O 上限；直接复用 settings 的 `sandbox.io.*`。
pub type IoLimits = SandboxIo;

/// 引擎面返回的进度样本；不可信自报，只用于展示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressSample {
    pub job_id: AttemptId,
    pub seq: u64,
    pub stage: ProgressStage,
    pub detail: UntrustedText,
    pub at: Timestamp,
}

/// `submit` 的返回值；不包含运行时类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobHandle {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SandboxSpec {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
    pub project_id: ProjectId,
    pub delivery_id: Option<DeliveryId>,
    pub conversation_id: Option<ConversationId>,
    pub job: JobSpecTrusted,
    pub raw_materials: Vec<RawMaterial>,
    pub upstream: Vec<UpstreamInput>,
    pub exec: ExecSpec,
    pub driver: DriverKind,
    pub trust: Trust,
    pub limits: SandboxLimits,
    pub timers: Timers,
    pub io: IoLimits,
    pub egress: EgressRequest,
    pub workspace: WorkspaceRequest,
    /// 引擎组装本 spec 时所用 `ConfigView` 的修订（`egress.weights` 等配置快照所属）。
    /// sandbox 原样填入 `IssueJobToken.config_rev`，不读签发时刻的「当前生效修订」（#220）。
    pub config_rev: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawMaterial {
    pub id: String,
    pub kind: MaterialKind,
    pub source: String,
    pub text: UntrustedText,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamInput {
    pub node_id: NodeId,
    pub role: Role,
    pub attempt_id: AttemptId,
    /// 文档字段名为 `ref`；Rust 字段用 `reference`。
    pub reference: CasRef,
    pub files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecSpec {
    pub image_digest: Sha256Digest,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub token_env: TokenEnvSpec,
}

/// 由 sandbox 在签发 `job_token` 之后填值的环境变量名。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenEnvSpec {
    /// provider_id → 模型 base URL 的环境变量名。
    pub model_base_url: BTreeMap<String, String>,
    /// 例如 `HTTPS_PROXY` / `https_proxy`。
    pub proxy: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timers {
    pub idle_timeout_s: u64,
    pub kill_grace_s: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EgressRequest {
    pub profile: ProfileName,
    pub groups: Vec<GroupName>,
    pub tokens_total: u64,
    pub weights: TokenWeights,
}

/// 配置里的 egress profile 名。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileName(String);

impl ProfileName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 配置里的 egress group 名。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupName(String);

impl GroupName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GroupName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRequest {
    pub repo: RepoSlug,
    pub base_sha: GitOid,
    pub pr_base_sha: Option<GitOid>,
    pub mode: WorkspaceMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceMode {
    Rw,
    Ro,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobOutcome {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
    /// 最终 job.json 原始字节的可信摘要，来自 stage 并经服务端元数据保存。
    /// None 表示没有可信摘要，包括正常的启动前取消。
    pub job_sha256: Option<Sha256Digest>,
    pub terminal: TerminalFacts,
    pub partial: bool,
    pub files: BTreeMap<ArtifactName, CasObject>,
    pub unexpected: Vec<UnexpectedArtifact>,
    pub progress: Option<CasObject>,
    pub progress_stats: ProgressStats,
    pub stdout_tail: CasObject,
    pub stderr_tail: CasObject,
    pub usage_metered: TokenUsage,
    pub weighted_tokens: u64,
    pub egress: EgressFlags,
    pub timings: Timings,
}

/// 白名单产物的名字（`result.json` / `changes.patch` / `report.md` /
/// `proposals.json`）。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactName(String);

impl ArtifactName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// CAS 对象的元数据；worker 协议已有同形类型，直接复用。
pub type CasObject = FileArtifact;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalFacts {
    pub cause: TerminalCause,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub oom: bool,
    pub timer: Option<TimerKind>,
    pub cancel_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalCause {
    Exited,
    KilledTimer,
    KilledCancel,
    KilledRuntime,
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    Wall,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnexpectedArtifact {
    pub name: String,
    pub reason: String,
}

/// 进度流统计。规格书只给字段名 `progress_stats`；这里保留丢弃计数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressStats {
    pub samples: u64,
    pub dropped: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    pub created: Option<Timestamp>,
    pub staged: Option<Timestamp>,
    pub started: Option<Timestamp>,
    pub stopped: Option<Timestamp>,
    pub collected: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressFlags {
    pub capped: bool,
    pub upstream_error_summary: Option<UntrustedText>,
    pub incomplete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnfinishedAttempt {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
    pub project_id: ProjectId,
}

/// `recover` 对单个未终结尝试的判定（engine.md §7.14.6 E4）。
///
/// 区分允许重新准备与受理后丢失：前者由引擎以同一 `SandboxSpec` 重新
/// `submit`，不计为失败；后者终结为 `infra.lost`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveredJob {
    /// 已受理且仍在推进（运行中，或尚未启动、由 sandbox 重新走排队 / 创建 /
    /// 注入阶段，或拒绝任务的收尾正在推进）；引擎继续 `wait`。
    Running(JobHandle),
    /// 已受理且已停止；`wait` 稳定返回 `JobOutcome` 或 `JobIdentityUnavailable`。
    Exited(JobHandle),
    /// 受理后丢失：容器不在且产物不可提取；`wait` 返回 `Infra { Lost }`，
    /// 引擎终结为 `infra.lost`。
    Lost(JobHandle),
    /// 从未受理，或新格式记录经相位与运行时事实确认从未启动，清理准备
    /// 残留并持久记录 Accepted 后请求重入；引擎以相同 ID 重新提交完整 spec。
    Unknown { job_id: AttemptId },
}

impl RecoveredJob {
    pub fn job_id(&self) -> AttemptId {
        match self {
            Self::Running(handle) | Self::Exited(handle) | Self::Lost(handle) => handle.job_id,
            Self::Unknown { job_id } => *job_id,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub destroyed: u32,
    pub staging_removed: u32,
    pub errors: Vec<String>,
}

/// 运行中 / 排队中 / 上限 / 预热池。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapacitySnapshot {
    pub running: u32,
    pub queued: u32,
    pub limit: u32,
    pub prewarmed: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox infrastructure failure ({detail:?}): {message}")]
    Infra {
        detail: InfraDetail,
        message: String,
    },
    /// 已受理任务无法可信恢复；稳定终态，不属于普通基础设施自动重试。
    /// usage 为 Some 时须是已确认吊销、job_id 匹配且 incomplete=false 的 Egress 最终计量。
    /// None 仅限可信事实证明从未启动且未签发 job token，即无须结算用量。
    /// 计量未获取、不完整或吊销未确认时不得公开终态。
    #[error("sandbox job identity unavailable: {message}")]
    JobIdentityUnavailable {
        message: String,
        usage: Option<Box<crate::egress::FinalUsage>>,
    },
    #[error("invalid sandbox spec: {0}")]
    InvalidSpec(String),
    #[error("sandbox job not found")]
    NotFound,
    #[error("sandbox service is shutting down")]
    ShuttingDown,
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfraDetail {
    SandboxCreate,
    WorkspacePrep,
    Egress,
    Collect,
    Lost,
}

/// 引擎面的沙盒服务。`engine` 只依赖它，不接触容器驱动。
#[async_trait]
pub trait SandboxService: Send + Sync {
    /// 受理一个 job（按 `job_id` 幂等）。返回 `Ok` 之前受理记录已持久化，
    /// 重启后只有确认从未启动的新格式记录可按准备重入例外返回 Unknown。
    async fn submit(&self, ctx: &ActorCtx, spec: SandboxSpec) -> Result<JobHandle, SandboxError>;

    /// 等待终态；可重复调用，同一 job 恒返回同一结果。
    async fn wait(&self, job_id: &AttemptId) -> Result<JobOutcome, SandboxError>;

    /// 采样节流后的展示数据；丢包无影响。
    fn progress(&self, job_id: &AttemptId) -> BoxStream<'static, ProgressSample>;

    async fn cancel(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError>;

    async fn release(&self, ctx: &ActorCtx, job_id: &AttemptId) -> Result<(), SandboxError>;

    /// `gantryd` 重启后接管未终结的尝试。按输入顺序为每个
    /// `UnfinishedAttempt` 恰好返回一项（E4）；新格式记录经相位及运行时
    /// 事实确认从未启动时可清理残留、保存 Accepted 并返回 Unknown，要求
    /// 同 ID 重提完整 spec。不可验证的已受理任务不能返回 Unknown 或 Lost，
    /// 也不能中断其他任务的恢复结果。
    async fn recover(
        &self,
        ctx: &ActorCtx,
        unfinished: &[UnfinishedAttempt],
    ) -> Result<Vec<RecoveredJob>, SandboxError>;

    async fn sweep_orphans(
        &self,
        ctx: &ActorCtx,
        live: &BTreeSet<AttemptId>,
    ) -> Result<SweepReport, SandboxError>;

    async fn capacity(&self) -> CapacitySnapshot;
}

/// 保留 `ResultTerminalCause` 的显式映射，避免两个终态词表漂移。
pub fn terminal_cause_from_worker(cause: ResultTerminalCause) -> TerminalCause {
    match cause {
        ResultTerminalCause::Exited => TerminalCause::Exited,
        ResultTerminalCause::KilledTimer => TerminalCause::KilledTimer,
        ResultTerminalCause::KilledCancel => TerminalCause::KilledCancel,
        ResultTerminalCause::KilledRuntime => TerminalCause::KilledRuntime,
        ResultTerminalCause::Lost => TerminalCause::Lost,
    }
}
