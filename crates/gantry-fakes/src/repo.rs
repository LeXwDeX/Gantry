//! `FakeRepo`：内存版 `RepoService` + `WorktreeLocks`。
//!
//! 不创建真实 Git 仓库；只保留端口调用者能观察到的语义：镜像仓登记、工作树、
//! 写权锁的围栏与租约、补丁校验结果缓存（按 `job_id` 幂等）、提交生成与
//! 溯源。脚本化补丁结论由 [`FakeRepo::script_validate_patch`] 注入。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::{sha256, GitOid, RepoSlug, Sha256Digest};
use gantry_core::entities::{LockHolder, Seq, Worktree, WorktreeLock};
use gantry_core::ids::{AttemptId, DeliveryId, ProjectId, WorktreeId};
use gantry_core::ports::{
    ActorCtx, CommitInfo, CommitPatch, CommitRecord, CreateWorktree, LockError, LockInfo,
    LockToken, MirrorGuard, MirrorHealth, MirrorInfo, MirrorMode, PatchOutcome, PrepareWorkspace,
    PreparedWorkspace, RepoError, RepoService, RevSpec, ValidatePatch, WorkspaceStats,
    WorktreeLocks,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::{PatchVerdict, ValidatedPatch};

/// 人工锁的最长租约；与 api.md §3.7 的 14400 s 一致。
pub const HUMAN_LEASE_MAX: Duration = Duration::from_secs(14_400);

const REFSPEC: &str = "+refs/heads/*:refs/mirror/heads/*";

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ActiveLock {
    holder: LockHolder,
    delivery_id: DeliveryId,
    fence: u64,
    acquired_at: Timestamp,
    lease_until: Timestamp,
}

impl ActiveLock {
    fn info(&self) -> LockInfo {
        LockInfo {
            holder: self.holder,
            delivery_id: self.delivery_id,
            fence: seq(self.fence),
            lease_until: self.lease_until,
        }
    }
}

#[derive(Clone, Debug)]
struct WorktreeState {
    worktree: Worktree,
    head: GitOid,
    last_fence: u64,
    lock: Option<ActiveLock>,
}

#[derive(Clone)]
struct Patched {
    outcome: PatchOutcome,
    patch_sha: Sha256Digest,
    base_sha: GitOid,
}

#[derive(Clone)]
struct RepoSnapshot {
    now: Timestamp,
    counter: u128,
    mirrors: BTreeMap<ProjectId, (RepoSlug, MirrorInfo, MirrorHealth)>,
    commits: BTreeMap<(ProjectId, GitOid), CommitInfo>,
    refs: BTreeMap<(ProjectId, String), GitOid>,
    worktrees: BTreeMap<WorktreeId, WorktreeState>,
    by_delivery: BTreeMap<DeliveryId, WorktreeId>,
    patches: BTreeMap<AttemptId, Patched>,
    commit_by_job: BTreeMap<AttemptId, CommitRecord>,
    provenance: BTreeMap<(ProjectId, GitOid), CommitRecord>,
}

impl RepoSnapshot {
    fn new(now: Timestamp) -> Self {
        Self {
            now,
            counter: 0,
            mirrors: BTreeMap::new(),
            commits: BTreeMap::new(),
            refs: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            by_delivery: BTreeMap::new(),
            patches: BTreeMap::new(),
            commit_by_job: BTreeMap::new(),
            provenance: BTreeMap::new(),
        }
    }

    fn next_id<K: gantry_core::ids::IdKind>(&mut self) -> gantry_core::ids::Id<K> {
        self.counter += 1;
        let ms = self.now.as_millis().max(0) as u64;
        gantry_core::ids::Id::from_parts(ms, self.counter)
            .expect("fake repo counter must fit in 80 bits")
    }

    fn sweep_expired(&mut self) {
        let now = self.now;
        for state in self.worktrees.values_mut() {
            if let Some(lock) = &state.lock {
                if lock.lease_until <= now {
                    state.lock = None;
                    state.worktree.lock = None;
                    state.worktree.updated_at = now;
                }
            }
        }
    }
}

fn seq(value: u64) -> Seq {
    Seq::new(value).expect("fake fence must be at least 1")
}

fn lease_until(now: Timestamp, lease: Duration) -> Timestamp {
    Timestamp::from_millis(now.as_millis().saturating_add(lease.as_millis() as i64))
}

fn deterministic_oid(parts: &[&str]) -> GitOid {
    let hex = sha256(parts.join("\u{0}").as_bytes()).to_hex();
    GitOid::parse(&hex[..40]).expect("sha256 hex is a valid Git oid")
}

fn worktree_lock(lock: &ActiveLock) -> WorktreeLock {
    WorktreeLock {
        holder: lock.holder,
        delivery_id: lock.holder.bound_delivery(lock.delivery_id),
        acquired_at: lock.acquired_at,
        lease_until: lock.lease_until,
    }
}

// ---------------------------------------------------------------------------
// FakeRepo
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct FakeRepo {
    state: Arc<Mutex<RepoSnapshot>>,
    scripts: Arc<Mutex<BTreeMap<AttemptId, PatchOutcome>>>,
}

impl std::fmt::Debug for FakeRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("FakeRepo")
            .field("mirrors", &state.mirrors.len())
            .field("worktrees", &state.worktrees.len())
            .field("patches", &state.patches.len())
            .field("now", &state.now)
            .finish()
    }
}

impl FakeRepo {
    pub fn new(now: Timestamp) -> Self {
        Self {
            state: Arc::new(Mutex::new(RepoSnapshot::new(now))),
            scripts: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RepoSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_now(&self, now: Timestamp) {
        self.lock().now = now;
    }

    pub fn advance_ms(&self, ms: i64) {
        let mut state = self.lock();
        state.now = Timestamp::from_millis(state.now.as_millis().saturating_add(ms));
    }

    /// 注入一个可解析的 ref / 提交。
    pub fn insert_commit(&self, project: ProjectId, oid: GitOid, info: CommitInfo) {
        self.lock().commits.insert((project, oid), info);
    }

    pub fn set_ref(&self, project: ProjectId, name: impl Into<String>, oid: GitOid) {
        self.lock().refs.insert((project, name.into()), oid);
    }

    /// 把项目镜像仓指向一个真实目录（forge 的拉取 / 推送测试在真实 git 仓上跑）。
    pub fn set_mirror(&self, project: ProjectId, remote: RepoSlug, git_dir: PathBuf) {
        let info = MirrorInfo {
            git_dir,
            fetch_refspecs: vec![REFSPEC.to_owned()],
        };
        self.lock()
            .mirrors
            .insert(project, (remote, info, MirrorHealth::Healthy));
    }

    /// 直接登记一条提交溯源（I8），不经 `validate_patch` / `commit_patch`。
    pub fn insert_provenance(&self, project: ProjectId, record: CommitRecord) {
        self.lock()
            .provenance
            .insert((project, record.commit_oid), record);
    }

    pub fn set_mirror_health(&self, project: ProjectId, health: MirrorHealth) {
        let mut state = self.lock();
        if let Some((_, _, current)) = state.mirrors.get_mut(&project) {
            *current = health;
        }
    }

    /// 脚本化某一 `job_id` 的补丁结论；同一 `job_id` 只返回这一次脚本。
    pub fn script_validate_patch(&self, job_id: AttemptId, outcome: PatchOutcome) {
        self.scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(job_id, outcome);
    }

    pub fn patch_outcome(&self, job_id: &AttemptId) -> Option<PatchOutcome> {
        self.lock()
            .patches
            .get(job_id)
            .map(|entry| entry.outcome.clone())
    }

    pub fn commit_record(&self, job_id: &AttemptId) -> Option<CommitRecord> {
        self.lock().commit_by_job.get(job_id).cloned()
    }

    /// 注入一个工作树及其分支头，供一致性用例与上层 fake 使用。
    pub fn insert_worktree(&self, worktree: Worktree, head: GitOid, delivery: DeliveryId) {
        let mut state = self.lock();
        let mut worktree = worktree;
        worktree.lock = None;
        worktree.updated_at = state.now;
        state.by_delivery.insert(delivery, worktree.id);
        state.worktrees.insert(
            worktree.id,
            WorktreeState {
                worktree,
                head,
                last_fence: 0,
                lock: None,
            },
        );
    }

    fn require_worktree<'a>(
        state: &'a RepoSnapshot,
        id: &WorktreeId,
    ) -> Result<&'a WorktreeState, RepoError> {
        state.worktrees.get(id).ok_or(RepoError::WorktreeNotFound)
    }

    fn require_lock(
        state: &RepoSnapshot,
        id: &WorktreeId,
        token: &LockToken,
    ) -> Result<(), RepoError> {
        let worktree = Self::require_worktree(state, id)?;
        let lock = worktree.lock.as_ref().ok_or(RepoError::LockRequired)?;
        // 与真实实现同序（repo.md §12.B）：有效的人工锁优先报 WorktreeLocked，
        // 再判围栏 / 租约。
        if lock.holder == LockHolder::Human && lock.lease_until > state.now {
            return Err(RepoError::WorktreeLocked { lock: lock.info() });
        }
        if lock.lease_until <= state.now || lock.fence != token.fence.as_u64() {
            return Err(RepoError::StaleLock);
        }
        if lock.delivery_id != token.delivery_id || lock.holder != token.holder {
            return Err(RepoError::LockRequired);
        }
        Ok(())
    }
}

// Small helpers that keep the fake independent of any store ID factory.
fn fake_delivery() -> DeliveryId {
    DeliveryId::from_parts(1_700_000_000_000, 1).expect("fixed delivery id")
}

// ---------------------------------------------------------------------------
// RepoService
// ---------------------------------------------------------------------------

#[async_trait]
impl RepoService for FakeRepo {
    async fn ensure_mirror(
        &self,
        _ctx: &ActorCtx,
        project: &ProjectId,
        remote: &RepoSlug,
    ) -> Result<MirrorInfo, RepoError> {
        let mut state = self.lock();
        if let Some((_, info, _)) = state.mirrors.get(project) {
            return Ok(info.clone());
        }
        let info = MirrorInfo {
            git_dir: PathBuf::from(format!("/fake/mirrors/{project}.git")),
            fetch_refspecs: vec![REFSPEC.to_owned()],
        };
        state.mirrors.insert(
            *project,
            (remote.clone(), info.clone(), MirrorHealth::Healthy),
        );
        Ok(info)
    }

    async fn mirror_access(
        &self,
        _ctx: &ActorCtx,
        project: &ProjectId,
        mode: MirrorMode,
    ) -> Result<MirrorGuard, RepoError> {
        let state = self.lock();
        let (_, info, _) = state.mirrors.get(project).ok_or(RepoError::MirrorMissing)?;
        let _ = mode;
        Ok(MirrorGuard::new(
            info.git_dir.clone(),
            info.fetch_refspecs.clone(),
            (),
        ))
    }

    async fn commit_info(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitInfo>, RepoError> {
        Ok(self.lock().commits.get(&(*project, *oid)).cloned())
    }

    async fn resolve(&self, project: &ProjectId, rev: &RevSpec) -> Result<GitOid, RepoError> {
        let state = self.lock();
        if let Some(oid) = state
            .refs
            .get(&(*project, rev.as_str().to_owned()))
            .copied()
        {
            return Ok(oid);
        }
        if let Ok(oid) = GitOid::parse(rev.as_str()) {
            if state.commits.contains_key(&(*project, oid)) {
                return Ok(oid);
            }
        }
        Err(RepoError::BaseNotInMirror {
            oid: deterministic_oid(&["missing", rev.as_str()]),
        })
    }

    async fn has_commit(&self, project: &ProjectId, oid: &GitOid) -> Result<bool, RepoError> {
        Ok(self.lock().commits.contains_key(&(*project, *oid)))
    }

    async fn check_mirror(
        &self,
        _ctx: &ActorCtx,
        project: &ProjectId,
    ) -> Result<MirrorHealth, RepoError> {
        let state = self.lock();
        let (_, _, health) = state.mirrors.get(project).ok_or(RepoError::MirrorMissing)?;
        Ok(health.clone())
    }

    async fn create_worktree(
        &self,
        _ctx: &ActorCtx,
        req: CreateWorktree,
    ) -> Result<Worktree, RepoError> {
        let mut state = self.lock();
        if let Some(id) = state.by_delivery.get(&req.delivery_id) {
            if let Some(existing) = state.worktrees.get(id) {
                return Ok(existing.worktree.clone());
            }
        }
        let id: WorktreeId = state.next_id();
        let now = state.now;
        let worktree = Worktree {
            id,
            project_id: req.project_id,
            branch: req.branch.as_str().to_owned(),
            base_ref: req.base_ref,
            path: format!("/fake/worktrees/{id}"),
            lock: None,
            created_at: now,
            updated_at: now,
        };
        state.by_delivery.insert(req.delivery_id, id);
        state.worktrees.insert(
            id,
            WorktreeState {
                worktree: worktree.clone(),
                head: req.base_sha,
                last_fence: 0,
                lock: None,
            },
        );
        Ok(worktree)
    }

    async fn get_worktree(&self, id: &WorktreeId) -> Result<Option<Worktree>, RepoError> {
        Ok(self
            .lock()
            .worktrees
            .get(id)
            .map(|state| state.worktree.clone()))
    }

    async fn branch_head(&self, id: &WorktreeId) -> Result<GitOid, RepoError> {
        let state = self.lock();
        Ok(Self::require_worktree(&state, id)?.head)
    }

    async fn remove_worktree(
        &self,
        _ctx: &ActorCtx,
        id: &WorktreeId,
        lock: &LockToken,
    ) -> Result<(), RepoError> {
        let mut state = self.lock();
        Self::require_lock(&state, id, lock)?;
        state
            .worktrees
            .remove(id)
            .ok_or(RepoError::WorktreeNotFound)?;
        let delivery = state.by_delivery.iter().find_map(|(delivery, worktree)| {
            if *worktree == *id {
                Some(*delivery)
            } else {
                None
            }
        });
        if let Some(delivery) = delivery {
            state.by_delivery.remove(&delivery);
        }
        Ok(())
    }

    async fn advance_head(
        &self,
        _ctx: &ActorCtx,
        id: &WorktreeId,
        new_head: &GitOid,
        lock: &LockToken,
    ) -> Result<GitOid, RepoError> {
        let mut state = self.lock();
        let now = state.now;
        Self::require_lock(&state, id, lock)?;
        let project = {
            let worktree = Self::require_worktree(&state, id)?;
            worktree.worktree.project_id
        };
        if !state.commits.contains_key(&(project, *new_head)) {
            return Err(RepoError::BaseNotInMirror { oid: *new_head });
        }
        let worktree = state
            .worktrees
            .get_mut(id)
            .ok_or(RepoError::WorktreeNotFound)?;
        worktree.head = *new_head;
        worktree.worktree.updated_at = now;
        Ok(*new_head)
    }

    async fn acquire_lock(
        &self,
        _ctx: &ActorCtx,
        id: &WorktreeId,
        holder: LockHolder,
        delivery: &DeliveryId,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        if holder == LockHolder::Human && lease > HUMAN_LEASE_MAX {
            return Err(RepoError::LeaseTooLong {
                max: HUMAN_LEASE_MAX,
            });
        }
        let mut state = self.lock();
        state.sweep_expired();
        let now = state.now;
        let worktree = state
            .worktrees
            .get_mut(id)
            .ok_or(RepoError::WorktreeNotFound)?;
        if let Some(lock) = &worktree.lock {
            if lock.holder == holder && lock.delivery_id == *delivery {
                let renewed = ActiveLock {
                    holder,
                    delivery_id: *delivery,
                    fence: lock.fence,
                    acquired_at: lock.acquired_at,
                    lease_until: lease_until(now, lease),
                };
                worktree.worktree.lock = Some(worktree_lock(&renewed));
                worktree.worktree.updated_at = now;
                worktree.lock = Some(renewed);
                let lock = worktree.lock.as_ref().expect("lock was just set");
                return Ok(LockToken {
                    worktree_id: *id,
                    holder: lock.holder,
                    delivery_id: lock.delivery_id,
                    fence: seq(lock.fence),
                    lease_until: lock.lease_until,
                });
            }
            return Err(RepoError::WorktreeLocked { lock: lock.info() });
        }
        let fence = worktree.last_fence + 1;
        worktree.last_fence = fence;
        let lock = ActiveLock {
            holder,
            delivery_id: *delivery,
            fence,
            acquired_at: now,
            lease_until: lease_until(now, lease),
        };
        worktree.worktree.lock = Some(worktree_lock(&lock));
        worktree.worktree.updated_at = now;
        worktree.lock = Some(lock.clone());
        Ok(LockToken {
            worktree_id: *id,
            holder,
            delivery_id: *delivery,
            fence: seq(fence),
            lease_until: lock.lease_until,
        })
    }

    async fn renew_lock(
        &self,
        _ctx: &ActorCtx,
        token: &LockToken,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        if token.holder == LockHolder::Human && lease > HUMAN_LEASE_MAX {
            return Err(RepoError::LeaseTooLong {
                max: HUMAN_LEASE_MAX,
            });
        }
        let mut state = self.lock();
        let now = state.now;
        let worktree = state
            .worktrees
            .get_mut(&token.worktree_id)
            .ok_or(RepoError::WorktreeNotFound)?;
        let lock = worktree.lock.as_mut().ok_or(RepoError::LockRequired)?;
        if lock.fence != token.fence.as_u64()
            || lock.holder != token.holder
            || lock.delivery_id != token.delivery_id
            || lock.lease_until <= now
        {
            return Err(RepoError::StaleLock);
        }
        lock.lease_until = lease_until(now, lease);
        worktree.worktree.lock = Some(worktree_lock(lock));
        worktree.worktree.updated_at = now;
        Ok(LockToken {
            worktree_id: token.worktree_id,
            holder: lock.holder,
            delivery_id: lock.delivery_id,
            fence: seq(lock.fence),
            lease_until: lock.lease_until,
        })
    }

    async fn release_lock(&self, _ctx: &ActorCtx, token: &LockToken) -> Result<(), RepoError> {
        let mut state = self.lock();
        let now = state.now;
        let worktree = state
            .worktrees
            .get_mut(&token.worktree_id)
            .ok_or(RepoError::WorktreeNotFound)?;
        if let Some(lock) = &worktree.lock {
            if lock.fence == token.fence.as_u64()
                && lock.holder == token.holder
                && lock.delivery_id == token.delivery_id
            {
                worktree.lock = None;
                worktree.worktree.lock = None;
                worktree.worktree.updated_at = now;
            }
        }
        Ok(())
    }

    async fn prepare_workspace(
        &self,
        _ctx: &ActorCtx,
        req: PrepareWorkspace,
    ) -> Result<PreparedWorkspace, RepoError> {
        let state = self.lock();
        let info = state
            .commits
            .get(&(req.project_id, req.base_sha))
            .ok_or(RepoError::BaseNotInMirror { oid: req.base_sha })?;
        Ok(PreparedWorkspace {
            work_dir: req.dest,
            head: req.base_sha,
            tree: info.tree,
            diff_patch: None,
            stats: WorkspaceStats {
                bytes: 0,
                files: 0,
                elapsed_ms: 0,
            },
        })
    }

    async fn validate_patch(
        &self,
        _ctx: &ActorCtx,
        req: ValidatePatch,
    ) -> Result<PatchOutcome, RepoError> {
        let cached = self
            .lock()
            .patches
            .get(&req.job_id)
            .map(|cached| cached.outcome.clone());
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let mut state = self.lock();
        if let Some(worktree_id) = req.worktree_id {
            let worktree = Self::require_worktree(&state, &worktree_id)?;
            if worktree.head != req.base_sha {
                return Ok(PatchOutcome::BaseConflict {
                    base_sha: req.base_sha,
                    branch_head: worktree.head,
                });
            }
        }
        let scripted = self
            .scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&req.job_id);
        let outcome = scripted.unwrap_or_else(|| {
            let tree = deterministic_oid(&["tree", &req.job_id.to_string()]);
            PatchOutcome::Validated(ValidatedPatch {
                sha256: req.declared.sha256,
                base_sha: req.base_sha,
                result_tree: Some(tree),
                files: Vec::new(),
                verdict: PatchVerdict::Allow,
                reasons: Vec::new(),
                matched_approval_rules: Vec::new(),
                config_rev: Seq::ZERO,
            })
        });
        state.patches.insert(
            req.job_id,
            Patched {
                outcome: outcome.clone(),
                patch_sha: req.declared.sha256,
                base_sha: req.base_sha,
            },
        );
        Ok(outcome)
    }

    async fn commit_patch(
        &self,
        _ctx: &ActorCtx,
        req: CommitPatch,
    ) -> Result<CommitRecord, RepoError> {
        let mut state = self.lock();
        Self::require_lock(&state, &req.worktree_id, &req.lock)?;
        if let Some(record) = state.commit_by_job.get(&req.job_id) {
            return Ok(record.clone());
        }
        let record = state
            .patches
            .get(&req.job_id)
            .cloned()
            .ok_or(RepoError::PatchRecordMissing)?;
        match &record.outcome {
            PatchOutcome::Validated(validated) => {
                if validated.verdict == PatchVerdict::Deny {
                    return Err(RepoError::VerdictNotCommittable(validated.verdict));
                }
                if validated.verdict == PatchVerdict::NeedsApproval && req.approval.is_none() {
                    return Err(RepoError::VerdictNotCommittable(validated.verdict));
                }
                let worktree = state
                    .worktrees
                    .get(&req.worktree_id)
                    .ok_or(RepoError::WorktreeNotFound)?;
                if worktree.head != record.base_sha {
                    return Err(RepoError::BaseMoved {
                        base_sha: record.base_sha,
                        branch_head: worktree.head,
                    });
                }
                let tree = validated
                    .result_tree
                    .unwrap_or_else(|| deterministic_oid(&["tree", &req.job_id.to_string()]));
                let commit_oid = deterministic_oid(&[
                    "commit",
                    &req.job_id.to_string(),
                    &tree.to_string(),
                    &record.base_sha.to_string(),
                ]);
                let commit = CommitRecord {
                    commit_oid,
                    tree_oid: tree,
                    parent: record.base_sha,
                    patch_sha256: record.patch_sha,
                    job_id: req.job_id,
                    worktree_id: req.worktree_id,
                };
                state.commits.insert(
                    (req.project_id, commit_oid),
                    CommitInfo {
                        parents: vec![record.base_sha],
                        tree,
                        trailers: vec![("Gantry-Attempt".into(), req.job_id.to_string())],
                    },
                );
                state.refs.insert(
                    (req.project_id, format!("refs/gantry/attempt/{commit_oid}")),
                    commit_oid,
                );
                state
                    .provenance
                    .insert((req.project_id, commit_oid), commit.clone());
                state.commit_by_job.insert(req.job_id, commit.clone());
                let now = state.now;
                if let Some(worktree) = state.worktrees.get_mut(&req.worktree_id) {
                    worktree.head = commit_oid;
                    worktree.worktree.updated_at = now;
                }
                Ok(commit)
            }
            PatchOutcome::Rejected { .. } => Err(RepoError::PatchRecordMissing),
            PatchOutcome::BaseConflict { .. } => Err(RepoError::PatchRecordMissing),
            PatchOutcome::NoChange => Err(RepoError::PatchRecordMissing),
        }
    }

    async fn provenance(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitRecord>, RepoError> {
        Ok(self.lock().provenance.get(&(*project, *oid)).cloned())
    }
}

// ---------------------------------------------------------------------------
// WorktreeLocks
// ---------------------------------------------------------------------------

#[async_trait]
impl WorktreeLocks for FakeRepo {
    async fn acquire_human(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        lease: Duration,
    ) -> Result<Worktree, LockError> {
        let delivery = fake_delivery();
        self.acquire_lock(ctx, id, LockHolder::Human, &delivery, lease)
            .await
            .map_err(map_lock_error)?;
        self.get_worktree(id)
            .await
            .map_err(map_repo_error)?
            .ok_or(LockError::NotFound)
    }

    async fn release_human(&self, ctx: &ActorCtx, id: &WorktreeId) -> Result<Worktree, LockError> {
        let token = {
            let state = self.lock();
            let worktree = state.worktrees.get(id).ok_or(LockError::NotFound)?;
            let Some(lock) = &worktree.lock else {
                return Err(LockError::LockRequired);
            };
            if lock.holder != LockHolder::Human {
                return Err(LockError::Held { lock: lock.info() });
            }
            LockToken {
                worktree_id: *id,
                holder: lock.holder,
                delivery_id: lock.delivery_id,
                fence: seq(lock.fence),
                lease_until: lock.lease_until,
            }
        };
        self.release_lock(ctx, &token)
            .await
            .map_err(map_repo_error)?;
        self.get_worktree(id)
            .await
            .map_err(map_repo_error)?
            .ok_or(LockError::NotFound)
    }
}

fn map_lock_error(error: RepoError) -> LockError {
    match error {
        RepoError::WorktreeLocked { lock } => LockError::Held { lock },
        RepoError::WorktreeNotFound => LockError::NotFound,
        RepoError::LockRequired => LockError::LockRequired,
        RepoError::StaleLock => LockError::StaleLock,
        RepoError::LeaseTooLong { max } => LockError::LeaseTooLong { max },
        other => LockError::Store(gantry_core::ports::StoreError::Rejected {
            message: other.to_string(),
        }),
    }
}

fn map_repo_error(error: RepoError) -> LockError {
    match error {
        RepoError::WorktreeNotFound => LockError::NotFound,
        RepoError::LockRequired => LockError::LockRequired,
        RepoError::StaleLock => LockError::StaleLock,
        RepoError::Lock(lock) => lock,
        other => LockError::Store(gantry_core::ports::StoreError::Rejected {
            message: other.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// 一致性用例
// ---------------------------------------------------------------------------

/// 写权锁的基本语义：冲突、续租、释放幂等。
///
/// 前置条件：`id` 已存在，且当前没有有效锁。
pub async fn lock_conformance(
    service: &dyn RepoService,
    ctx: &ActorCtx,
    id: &WorktreeId,
    delivery: &DeliveryId,
    lease: Duration,
) -> Result<(), String> {
    let first = service
        .acquire_lock(ctx, id, LockHolder::Engine, delivery, lease)
        .await
        .map_err(|error| format!("acquire_lock failed: {error}"))?;
    let conflict = service
        .acquire_lock(ctx, id, LockHolder::Human, delivery, lease)
        .await;
    if !matches!(conflict, Err(RepoError::WorktreeLocked { .. })) {
        return Err("a second holder must see WorktreeLocked".into());
    }
    let renewed = service
        .renew_lock(ctx, &first, lease)
        .await
        .map_err(|error| format!("renew_lock failed: {error}"))?;
    if renewed.fence != first.fence || renewed.delivery_id != first.delivery_id {
        return Err("renew_lock must keep the fence and delivery".into());
    }
    service
        .release_lock(ctx, &renewed)
        .await
        .map_err(|error| format!("release_lock failed: {error}"))?;
    service
        .release_lock(ctx, &renewed)
        .await
        .map_err(|error| format!("release_lock must be idempotent: {error}"))?;
    Ok(())
}

/// 人工锁不可被抢占，且有效期内 engine 的写入得到 `WorktreeLocked`
/// （repo.md §12.B；写入用 `remove_worktree` 代表，它与 `commit_patch` 走同一锁校验）。
///
/// 前置条件：`id` 已存在，且当前没有有效锁。结束时人工锁已释放。
pub async fn human_lock_conformance(
    service: &dyn RepoService,
    ctx: &ActorCtx,
    id: &WorktreeId,
    delivery: &DeliveryId,
    lease: Duration,
) -> Result<(), String> {
    let engine = service
        .acquire_lock(ctx, id, LockHolder::Engine, delivery, lease)
        .await
        .map_err(|error| format!("engine acquire_lock failed: {error}"))?;
    service
        .release_lock(ctx, &engine)
        .await
        .map_err(|error| format!("release_lock failed: {error}"))?;
    let human = service
        .acquire_lock(ctx, id, LockHolder::Human, delivery, lease)
        .await
        .map_err(|error| format!("human acquire_lock failed: {error}"))?;
    if !matches!(
        service
            .acquire_lock(ctx, id, LockHolder::Engine, delivery, lease)
            .await,
        Err(RepoError::WorktreeLocked { .. })
    ) {
        return Err("engine must not preempt a human lock".into());
    }
    match service.remove_worktree(ctx, id, &engine).await {
        Err(RepoError::WorktreeLocked { lock }) if lock.holder == LockHolder::Human => {}
        other => {
            return Err(format!(
                "engine write under a human lock must be WorktreeLocked, got {other:?}"
            ))
        }
    }
    service
        .release_lock(ctx, &human)
        .await
        .map_err(|error| format!("release human lock failed: {error}"))?;
    Ok(())
}

/// 同一 `job_id` 的补丁校验必须返回同一结果。
///
/// 前置条件：若 `req.worktree_id` 存在，该工作树的分支头等于 `req.base_sha`。
pub async fn patch_idempotency_conformance(
    service: &dyn RepoService,
    ctx: &ActorCtx,
    req: ValidatePatch,
) -> Result<(), String> {
    let first = service
        .validate_patch(ctx, req.clone())
        .await
        .map_err(|error| format!("first validate_patch failed: {error}"))?;
    let second = service
        .validate_patch(ctx, req)
        .await
        .map_err(|error| format!("second validate_patch failed: {error}"))?;
    if first != second {
        return Err("validate_patch must be idempotent for one job_id".into());
    }
    Ok(())
}

// 编译期检查。
const _: fn() = || {
    fn assert_repo<T: RepoService>() {}
    fn assert_locks<T: WorktreeLocks>() {}
    assert_repo::<FakeRepo>();
    assert_locks::<FakeRepo>();
};
