//! `RepoService` 与 `WorktreeLocks`。
//!
//! 签名以 `docs/modules/repo.md` §6 为准。文档里的 `Ctx` 按 P-78 统一为
//! [`ActorCtx`]；未在文档中带上下文的写方法（`mirror_access`、`check_mirror`、
//! `prepare_workspace`）也补上了它，以保证所有跨模块写接口都有统一上下文。

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::canon::{GitOid, RepoSlug, Sha256Digest};
use crate::entities::{ApprovalError, LockHolder, Seq, Worktree};
use crate::ids::{AttemptId, DeliveryId, ProjectId, WorktreeId};
use crate::text::UntrustedText;
use crate::time::Timestamp;
use crate::worker::{PatchRejectReason, PatchVerdict, ValidatedPatch};

use super::context::{ActorCtx, CasRef, StoreError};

/// 镜像仓布局。规格书只给 `MirrorInfo` 名字；这里返回 document 推荐的
/// `git_dir` 与 fetch refspecs。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorInfo {
    pub git_dir: PathBuf,
    pub fetch_refspecs: Vec<String>,
}

/// 受控访问守卫（repo.md §6.1）：持有实现方的锁句柄，守卫 drop 即释放。
///
/// 真实实现的句柄是镜像仓的共享 flock：`Fetch` / `Push` / `Read` 期间 `repo` 的
/// `gc` / 重建（独占锁）必须等待，所以 `forge` 在 `git fetch` / `git push` 全程持有
/// 守卫。守卫因此不可 Clone。`core` 不做 IO，只保存一个不透明的句柄。
pub struct MirrorGuard {
    pub git_dir: PathBuf,
    pub fetch_refspecs: Vec<String>,
    _lease: Box<dyn std::any::Any + Send + Sync>,
}

impl MirrorGuard {
    /// `lease` 在守卫 drop 时随之 drop；不需要锁的实现（替身）传 `()`。
    pub fn new(
        git_dir: PathBuf,
        fetch_refspecs: Vec<String>,
        lease: impl std::any::Any + Send + Sync,
    ) -> Self {
        Self {
            git_dir,
            fetch_refspecs,
            _lease: Box::new(lease),
        }
    }
}

impl std::fmt::Debug for MirrorGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorGuard")
            .field("git_dir", &self.git_dir)
            .field("fetch_refspecs", &self.fetch_refspecs)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorMode {
    Fetch,
    Push,
    Read,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub parents: Vec<GitOid>,
    pub tree: GitOid,
    pub trailers: Vec<(String, String)>,
}

/// 仅解析镜像仓内已有的 ref / OID，不联网。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RevSpec(String);

impl RevSpec {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RevSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 由 engine 生成的分支名（`gantry/` 前缀）；完整 ref 校验归 repo 的 git 层。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(String);

impl BranchName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BranchName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateWorktree {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub branch: BranchName,
    pub base_ref: String,
    pub base_sha: GitOid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockToken {
    pub worktree_id: WorktreeId,
    pub holder: LockHolder,
    pub delivery_id: DeliveryId,
    pub fence: Seq,
    pub lease_until: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockInfo {
    pub holder: LockHolder,
    pub delivery_id: DeliveryId,
    pub fence: Seq,
    pub lease_until: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareWorkspace {
    pub project_id: ProjectId,
    pub job_id: AttemptId,
    pub base_sha: GitOid,
    pub pr_base_sha: Option<GitOid>,
    pub dest: PathBuf,
    pub max_bytes: u64,
    pub want_diff: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedWorkspace {
    pub work_dir: PathBuf,
    pub head: GitOid,
    pub tree: GitOid,
    pub diff_patch: Option<CasRef>,
    pub stats: WorkspaceStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceStats {
    pub bytes: u64,
    pub files: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatePatch {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub job_id: AttemptId,
    pub worktree_id: Option<WorktreeId>,
    pub base_sha: GitOid,
    pub patch: CasRef,
    pub declared: DeclaredPatch,
    pub job_paths: crate::worker::PathConstraints,
}

/// `result.json` 的补丁自报。字段与 worker `PatchArtifact` 对应，但端口只
/// 需要这四列；不复用带 `path` 的协议类型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredPatch {
    pub sha256: Sha256Digest,
    pub bytes: u64,
    pub base_sha: GitOid,
    pub files_changed: u32,
}

/// V5 的四种结论。机械拒绝只在这里出现；策略拒绝由 `policy` 的输出驱动。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchOutcome {
    Validated(ValidatedPatch),
    Rejected {
        reason: PatchRejectReason,
        detail: String,
    },
    BaseConflict {
        base_sha: GitOid,
        branch_head: GitOid,
    },
    NoChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMessage {
    pub subject: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPatch {
    pub project_id: ProjectId,
    pub job_id: AttemptId,
    pub worktree_id: WorktreeId,
    pub lock: LockToken,
    pub approval: Option<crate::ids::ApprovalId>,
    pub message: CommitMessage,
    pub timestamp: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRecord {
    pub commit_oid: GitOid,
    pub tree_oid: GitOid,
    pub parent: GitOid,
    pub patch_sha256: Sha256Digest,
    pub job_id: AttemptId,
    pub worktree_id: WorktreeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorHealth {
    Healthy,
    NeedsRebuild { reason: String },
}

/// `JobSpec.constraints.paths` 的快照。复用 worker 协议类型，避免两套路径
/// 规则。
pub type PathRulesSnapshot = crate::worker::PathConstraints;

/// 工作树锁错误。`api` 的 `worktree_locked` 映射使用 `Held`。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LockError {
    #[error("worktree is locked")]
    Held { lock: LockInfo },
    #[error("worktree lock is required")]
    LockRequired,
    #[error("worktree lock is stale")]
    StaleLock,
    #[error("human lease is too long")]
    LeaseTooLong { max: Duration },
    #[error("worktree not found")]
    NotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RepoError {
    #[error("mirror missing")]
    MirrorMissing,
    #[error("mirror needs rebuild")]
    MirrorNeedsRebuild,
    #[error("base commit is not in the mirror: {oid}")]
    BaseNotInMirror { oid: GitOid },
    #[error("worktree not found")]
    WorktreeNotFound,
    #[error("worktree locked")]
    WorktreeLocked { lock: LockInfo },
    #[error("worktree lock required")]
    LockRequired,
    #[error("worktree lock is stale")]
    StaleLock,
    #[error("base moved")]
    BaseMoved {
        base_sha: GitOid,
        branch_head: GitOid,
    },
    #[error("lease too long")]
    LeaseTooLong { max: Duration },
    #[error("workspace too large: {bytes} > {max}")]
    WorkspaceTooLarge { bytes: u64, max: u64 },
    #[error("patch record missing")]
    PatchRecordMissing,
    #[error("patch digest mismatch")]
    PatchDigestMismatch,
    #[error(transparent)]
    ApprovalInvalid(#[from] ApprovalError),
    #[error("patch verdict is not committable: {0:?}")]
    VerdictNotCommittable(PatchVerdict),
    #[error("commit identity is not configured")]
    CommitIdentityMissing,
    #[error("git {op} failed (status {status:?})")]
    Git {
        op: &'static str,
        status: Option<i32>,
        stderr_tail: UntrustedText,
    },
    #[error("repo operation timed out: {op}")]
    Timeout { op: &'static str },
    #[error("repo I/O error: {message}")]
    Io { message: String },
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("policy error: {0}")]
    Policy(String),
}

/// 镜像仓、工作树、补丁闸门与提交。`FakeRepo` 实现同一个 trait。
#[async_trait]
pub trait RepoService: Send + Sync {
    async fn ensure_mirror(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
        remote: &RepoSlug,
    ) -> Result<MirrorInfo, RepoError>;

    async fn mirror_access(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
        mode: MirrorMode,
    ) -> Result<MirrorGuard, RepoError>;

    async fn commit_info(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitInfo>, RepoError>;

    async fn resolve(&self, project: &ProjectId, rev: &RevSpec) -> Result<GitOid, RepoError>;

    async fn has_commit(&self, project: &ProjectId, oid: &GitOid) -> Result<bool, RepoError>;

    async fn check_mirror(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
    ) -> Result<MirrorHealth, RepoError>;

    async fn create_worktree(
        &self,
        ctx: &ActorCtx,
        req: CreateWorktree,
    ) -> Result<Worktree, RepoError>;

    async fn get_worktree(&self, id: &WorktreeId) -> Result<Option<Worktree>, RepoError>;

    async fn branch_head(&self, id: &WorktreeId) -> Result<GitOid, RepoError>;

    async fn remove_worktree(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        lock: &LockToken,
    ) -> Result<(), RepoError>;

    async fn advance_head(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        new_head: &GitOid,
        lock: &LockToken,
    ) -> Result<GitOid, RepoError>;

    async fn acquire_lock(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        holder: LockHolder,
        delivery: &DeliveryId,
        lease: Duration,
    ) -> Result<LockToken, RepoError>;

    async fn renew_lock(
        &self,
        ctx: &ActorCtx,
        token: &LockToken,
        lease: Duration,
    ) -> Result<LockToken, RepoError>;

    async fn release_lock(&self, ctx: &ActorCtx, token: &LockToken) -> Result<(), RepoError>;

    /// 由 `sandbox.stage` 调用；`dest` 是 sandbox 建好的空目录。
    async fn prepare_workspace(
        &self,
        ctx: &ActorCtx,
        req: PrepareWorkspace,
    ) -> Result<PreparedWorkspace, RepoError>;

    /// V5；同一 `job_id` 幂等。
    async fn validate_patch(
        &self,
        ctx: &ActorCtx,
        req: ValidatePatch,
    ) -> Result<PatchOutcome, RepoError>;

    async fn commit_patch(
        &self,
        ctx: &ActorCtx,
        req: CommitPatch,
    ) -> Result<CommitRecord, RepoError>;

    async fn provenance(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitRecord>, RepoError>;
}

/// `api` 的人工锁入口。
#[async_trait]
pub trait WorktreeLocks: Send + Sync {
    async fn acquire_human(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        lease: Duration,
    ) -> Result<Worktree, LockError>;

    async fn release_human(&self, ctx: &ActorCtx, id: &WorktreeId) -> Result<Worktree, LockError>;
}
