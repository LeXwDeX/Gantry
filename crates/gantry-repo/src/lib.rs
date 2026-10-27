//! Gantry `repo`：服务端上唯一接触 Git 对象的模块（docs/modules/repo.md）。
//!
//! [`GitRepo`] 实现 `core::ports::RepoService` 与 `WorktreeLocks`：
//!
//! - 每项目一个裸镜像仓（[`mirror`]），向 `forge` 只给路径与 refspec；
//! - 工作树与写权锁的事实在事件日志，本模块折叠出内存投影（[`worktree`]），
//!   锁冲突在单写者事务内用 `core::machines::worktree_lock` 的同一规则仲裁；
//! - 所有 git 子进程只经 [`git::GitCmd`]（repo.md §7.2）。
//!
//! 本模块从不读取沙盒写入过的目录，也不联网（I1、I2、I8）。

pub mod git;
pub mod mirror;
pub mod worktree;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, LockHolder, Worktree};
use gantry_core::events::{
    Event, EventType, NewEvent, Subject, WorktreeCreatedData, WorktreeLockedData,
    WorktreeRemovedData, WorktreeUnlockReason, WorktreeUnlockedData,
};
use gantry_core::ids::{Clock, DeliveryId, IdGen, ProjectId, WorktreeId};
use gantry_core::machines::worktree_lock;
use gantry_core::ports::{
    ActorCtx, CommitInfo, CommitPatch, CommitRecord, CreateWorktree, LockError, LockToken,
    MirrorGuard, MirrorHealth, MirrorInfo, MirrorMode, PatchOutcome, PrepareWorkspace,
    PreparedWorkspace, RepoError, RepoService, RevSpec, StoreError as PortStoreError,
    ValidatePatch, WorktreeLocks,
};
use gantry_store::{Appended, EventFilter, Store, TypePattern};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::git::{GitCmd, GitEnv, GitOutput};
use crate::mirror::{io, io_error, MirrorEntry, MirrorLock};
use crate::worktree::{check_token, lease_until, ActiveLock, WorktreeTable, HUMAN_LEASE_MAX};

pub use crate::mirror::FETCH_REFSPEC;

/// 分支名前缀（repo.md §6.1：分支名由 engine 生成，`gantry/` 前缀）。
pub const BRANCH_PREFIX: &str = "gantry/";

/// 引导配置（repo.md §10，deploy 拥有）。
#[derive(Clone, Debug)]
pub struct RepoConfig {
    /// 镜像仓、`scratch/` 等的根，默认 `/var/lib/gantry`。
    pub data_dir: PathBuf,
    /// 单次 git 调用超时，默认 60 s [建议值，待调优]。
    pub git_timeout: Duration,
    /// 每项目 git 并行度，默认 4 [建议值，待调优]。
    pub git_parallelism: usize,
    /// 单次 git 调用 stdout 上限。
    pub max_git_output: usize,
    /// git 可执行文件；`None` 时在 `PATH` 里查找。
    pub git_bin: Option<PathBuf>,
}

impl RepoConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            git_timeout: Duration::from_secs(60),
            git_parallelism: 4,
            max_git_output: 256 * 1024 * 1024,
            git_bin: None,
        }
    }

    pub fn mirrors_dir(&self) -> PathBuf {
        self.data_dir.join("mirrors")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.data_dir.join("worktrees")
    }

    pub fn scratch_dir(&self) -> PathBuf {
        self.data_dir.join("scratch")
    }

    fn tmp_dir(&self) -> PathBuf {
        self.data_dir.join("repo-tmp")
    }

    fn home_dir(&self) -> PathBuf {
        self.data_dir.join("repo-home")
    }

    fn template_dir(&self) -> PathBuf {
        self.data_dir.join("repo-template")
    }
}

/// 运行期依赖。时钟必须与 `store` 共用同一个。
#[derive(Clone)]
pub struct RepoDeps {
    pub store: Store,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<IdGen>,
}

/// 持有共享 / 独占 flock 的镜像仓访问守卫。
///
/// `core::ports::MirrorGuard` 只承载字段、不能持锁；需要在 `git fetch` / `push`
/// 期间挡住 gc 的调用方用 [`GitRepo::lease_mirror`] 取得本类型。
#[derive(Debug)]
pub struct MirrorLease {
    pub guard: MirrorGuard,
    _lock: MirrorLock,
}

#[derive(Clone)]
pub struct GitRepo {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: RepoConfig,
    env: GitEnv,
    deps: RepoDeps,
    mirrors: Mutex<BTreeMap<ProjectId, MirrorEntry>>,
    /// 串行化 ensure_mirror。
    mirror_create: tokio::sync::Mutex<()>,
    /// 工作树投影；写路径在 store 事务的整个生命周期内持有它。
    worktrees: tokio::sync::Mutex<WorktreeTable>,
    git_slots: Mutex<BTreeMap<ProjectId, Arc<Semaphore>>>,
}

impl std::fmt::Debug for GitRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRepo")
            .field("data_dir", &self.inner.cfg.data_dir)
            .finish()
    }
}

impl GitRepo {
    /// 启动恢复（repo.md §7.8）：清空 `scratch/` 与临时目录，扫描镜像仓，
    /// 从事件日志重建工作树与锁，补发过期锁的 `worktree.unlocked`，
    /// 对每个镜像仓 `git worktree prune`。
    pub async fn open(cfg: RepoConfig, deps: RepoDeps) -> Result<Self, RepoError> {
        let git_bin = match &cfg.git_bin {
            Some(path) => path.clone(),
            None => git::discover_git().ok_or_else(|| io_error("git executable not found"))?,
        };
        for dir in [cfg.scratch_dir(), cfg.tmp_dir()] {
            mirror::remove_dir_if_exists(&dir)?;
        }
        for dir in [
            cfg.mirrors_dir(),
            cfg.worktrees_dir(),
            cfg.scratch_dir(),
            cfg.tmp_dir(),
            cfg.home_dir(),
            cfg.template_dir(),
        ] {
            std::fs::create_dir_all(&dir).map_err(io)?;
        }
        let env = GitEnv {
            git_bin,
            home: cfg.home_dir(),
            timeout: cfg.git_timeout,
            max_output: cfg.max_git_output,
        };
        let mirrors = mirror::scan(&cfg.mirrors_dir()).into_iter().collect();
        let table = WorktreeTable::new(cfg.worktrees_dir());
        let repo = Self {
            inner: Arc::new(Inner {
                cfg,
                env,
                deps,
                mirrors: Mutex::new(mirrors),
                mirror_create: tokio::sync::Mutex::new(()),
                worktrees: tokio::sync::Mutex::new(table),
                git_slots: Mutex::new(BTreeMap::new()),
            }),
        };
        repo.rebuild_worktrees().await?;
        repo.expire_locks().await?;
        for entry in repo.mirror_entries() {
            GitCmd::new("worktree-prune", &entry.git_dir)
                .args(["worktree", "prune"])
                .run(&repo.inner.env)
                .await?;
        }
        Ok(repo)
    }

    pub fn config(&self) -> &RepoConfig {
        &self.inner.cfg
    }

    /// 取镜像仓的 flock 守卫：`Fetch` / `Push` / `Read` 共享；gc / 重建独占。
    pub async fn lease_mirror(
        &self,
        project: &ProjectId,
        mode: MirrorMode,
    ) -> Result<MirrorLease, RepoError> {
        let entry = self.mirror_entry(project)?;
        let _ = mode; // 三种模式都是共享锁（repo.md §6.1）
        let lock = mirror::lock(&entry.git_dir, false).await?;
        Ok(MirrorLease {
            guard: entry.guard(),
            _lock: lock,
        })
    }

    /// 独占锁下的 `git gc`（repo.md §7.2：`gc.auto=0`，由本模块定时执行）。
    pub async fn gc(&self, project: &ProjectId) -> Result<(), RepoError> {
        let entry = self.mirror_entry(project)?;
        let _lock = mirror::lock(&entry.git_dir, true).await?;
        self.git(
            project,
            GitCmd::new("gc", &entry.git_dir).args(["gc", "--quiet"]),
        )
        .await?;
        Ok(())
    }

    /// 结束所有已过期的锁：追加 `worktree.unlocked(reason=expired)`（repo.md §7.6）。
    /// 返回结束的锁数。由 [`GitRepo::spawn_lock_reaper`] 周期调用，启动时也调用一次。
    pub async fn expire_locks(&self) -> Result<usize, RepoError> {
        let mut table = self.inner.worktrees.lock().await;
        let mut tx = self.begin().await?;
        let now = tx.now();
        let expired: Vec<WorktreeId> = table
            .by_id
            .iter()
            .filter(|(_, state)| state.lock.as_ref().is_some_and(|lock| lock.expired(now)))
            .map(|(id, _)| *id)
            .collect();
        if expired.is_empty() {
            return Ok(0);
        }
        let mut appended = Vec::new();
        for id in &expired {
            let state = table.get(id)?;
            appended.push(
                self.append(
                    &mut tx,
                    system_actor(),
                    None,
                    EventType::WorktreeUnlocked,
                    id,
                    Some(state.worktree.project_id),
                    &WorktreeUnlockedData {
                        worktree_id: *id,
                        reason: WorktreeUnlockReason::Expired,
                    },
                )
                .await?,
            );
        }
        tx.commit().await.map_err(store_error)?;
        apply_all(&mut table, &appended)?;
        Ok(expired.len())
    }

    /// 周期性地结束过期锁。返回的任务句柄 drop 不会停止它；调用方 abort 之。
    pub fn spawn_lock_reaper(&self, period: Duration) -> tokio::task::JoinHandle<()> {
        let repo = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                tick.tick().await;
                let _ = repo.expire_locks().await;
            }
        })
    }

    // -----------------------------------------------------------------------
    // 内部
    // -----------------------------------------------------------------------

    fn mirror_entries(&self) -> Vec<MirrorEntry> {
        self.inner
            .mirrors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn mirror_entry(&self, project: &ProjectId) -> Result<MirrorEntry, RepoError> {
        self.inner
            .mirrors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(project)
            .cloned()
            .ok_or(RepoError::MirrorMissing)
    }

    fn set_health(&self, project: &ProjectId, health: MirrorHealth) {
        if let Some(entry) = self
            .inner
            .mirrors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(project)
        {
            entry.health = health;
        }
    }

    fn slot(&self, project: &ProjectId) -> Arc<Semaphore> {
        self.inner
            .git_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(*project)
            .or_insert_with(|| Arc::new(Semaphore::new(self.inner.cfg.git_parallelism.max(1))))
            .clone()
    }

    /// 在项目的 git 并行度配额内运行一条命令（要求退出码 0）。
    pub(crate) async fn git(
        &self,
        project: &ProjectId,
        cmd: GitCmd,
    ) -> Result<GitOutput, RepoError> {
        let slot = self.slot(project);
        let _permit = slot
            .acquire()
            .await
            .map_err(|_| io_error("git slots closed"))?;
        cmd.run(&self.inner.env).await
    }

    async fn begin(&self) -> Result<gantry_store::WriteTx, RepoError> {
        self.inner
            .deps
            .store
            .begin_write()
            .await
            .map_err(store_error)
    }

    #[allow(clippy::too_many_arguments)]
    async fn append<D: Serialize>(
        &self,
        tx: &mut gantry_store::WriteTx,
        actor: Actor,
        causation: Option<gantry_core::ids::EventId>,
        ty: EventType,
        subject: &impl std::fmt::Display,
        project: Option<ProjectId>,
        data: &D,
    ) -> Result<Event<Value>, RepoError> {
        let subject = Subject::parse(&subject.to_string())
            .map_err(|_| io_error("event subject is not a prefixed id"))?;
        let data = serde_json::to_value(data)
            .map_err(|error| io_error(&format!("event data: {error}")))?;
        let event = NewEvent {
            ty,
            subject,
            project_id: project,
            actor,
            causation_id: causation,
            correlation_id: None,
            data,
            dedup_key: None,
        };
        match tx.append(event).await.map_err(store_error)? {
            Appended::New(event) => Ok(event),
            Appended::Duplicate { .. } => Err(io_error("unexpected duplicate event")),
        }
    }

    async fn rebuild_worktrees(&self) -> Result<(), RepoError> {
        let mut table = self.inner.worktrees.lock().await;
        let log = self.inner.deps.store.events();
        let filter = EventFilter {
            project_ids: None,
            include: vec![TypePattern::Prefix("worktree.".into())],
            exclude: Vec::new(),
        };
        let mut after = gantry_core::entities::Seq::ZERO;
        loop {
            let page = log.scan(after, &filter, 200).await.map_err(store_error)?;
            for event in &page.events {
                table.apply(event)?;
            }
            if page.scanned_to >= page.head || page.scanned_to == after {
                break;
            }
            after = page.scanned_to;
        }
        Ok(())
    }

    async fn branch_ref_head(
        &self,
        project: &ProjectId,
        id: &WorktreeId,
    ) -> Result<GitOid, RepoError> {
        let entry = self.mirror_entry(project)?;
        let out = self
            .git(
                project,
                GitCmd::new("rev-parse", &entry.git_dir)
                    .args(["rev-parse", "--verify", "--end-of-options"])
                    .arg(wt_ref(id)),
            )
            .await?;
        GitOid::parse(out.stdout_str()?.trim()).map_err(|_| io_error("bad branch head oid"))
    }

    async fn check_branch(&self, project: &ProjectId, branch: &str) -> Result<(), RepoError> {
        let entry = self.mirror_entry(project)?;
        let out = GitCmd::new("check-ref-format", &entry.git_dir)
            .arg("check-ref-format")
            .arg(format!("refs/heads/{branch}"))
            .output(&self.inner.env)
            .await?;
        if !branch.starts_with(BRANCH_PREFIX) || !out.success() {
            return Err(RepoError::Git {
                op: "check-ref-format",
                status: out.status,
                stderr_tail: gantry_core::text::UntrustedText::new(
                    format!("branch must be a valid ref under {BRANCH_PREFIX}"),
                    "repo:check-ref-format".into(),
                    false,
                ),
            });
        }
        Ok(())
    }

    /// 写锁事件：冲突判定与 `core::machines::worktree_lock::acquire` 同一规则，
    /// 另加"同 holder + 同 delivery 才算续租"（repo.md §7.6）。
    async fn lock_impl(
        &self,
        ctx: &ActorCtx,
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
        let mut table = self.inner.worktrees.lock().await;
        let mut tx = self.begin().await?;
        let now = tx.now();
        let state = table.get(id)?;
        let project = state.worktree.project_id;
        let until = lease_until(now, lease);
        let mut appended = Vec::new();
        let mut current = state.lock.clone();
        if let Some(lock) = &current {
            if lock.expired(now) {
                appended.push(
                    self.append(
                        &mut tx,
                        system_actor(),
                        ctx.causation_id,
                        EventType::WorktreeUnlocked,
                        id,
                        Some(project),
                        &WorktreeUnlockedData {
                            worktree_id: *id,
                            reason: WorktreeUnlockReason::Expired,
                        },
                    )
                    .await?,
                );
                current = None;
            } else if lock.holder != holder || lock.delivery_id != *delivery {
                return Err(RepoError::WorktreeLocked { lock: lock.info() });
            }
        }
        let request = worktree_lock::LockRequest {
            holder,
            delivery_id: *delivery,
            lease_until: until,
            max_lease: None,
        };
        let core_current = current
            .as_ref()
            .map(|lock| gantry_core::entities::WorktreeLock {
                holder: lock.holder,
                delivery_id: lock.delivery_id,
                acquired_at: lock.acquired_at,
                lease_until: lock.lease_until,
            });
        worktree_lock::acquire(core_current.as_ref(), &request, now).map_err(machine_error)?;
        appended.push(
            self.append(
                &mut tx,
                ctx.actor.clone(),
                ctx.causation_id,
                EventType::WorktreeLocked,
                id,
                Some(project),
                &WorktreeLockedData {
                    worktree_id: *id,
                    holder,
                    delivery_id: *delivery,
                    lease_until: until,
                },
            )
            .await?,
        );
        tx.commit().await.map_err(store_error)?;
        apply_all(&mut table, &appended)?;
        let lock = table.get(id)?.lock.clone().expect("lock was just appended");
        Ok(lock.token(*id))
    }

    async fn renew_impl(
        &self,
        ctx: &ActorCtx,
        token: &LockToken,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        if token.holder == LockHolder::Human && lease > HUMAN_LEASE_MAX {
            return Err(RepoError::LeaseTooLong {
                max: HUMAN_LEASE_MAX,
            });
        }
        let mut table = self.inner.worktrees.lock().await;
        let mut tx = self.begin().await?;
        let now = tx.now();
        let state = table.get(&token.worktree_id)?;
        let lock = state.lock.as_ref().ok_or(RepoError::LockRequired)?;
        if !lock.matches(token) || lock.expired(now) {
            return Err(RepoError::StaleLock);
        }
        let until = lease_until(now, lease);
        let core_lock = gantry_core::entities::WorktreeLock {
            holder: lock.holder,
            delivery_id: lock.delivery_id,
            acquired_at: lock.acquired_at,
            lease_until: lock.lease_until,
        };
        worktree_lock::renew(&core_lock, token.holder, until, now).map_err(machine_error)?;
        let event = self
            .append(
                &mut tx,
                ctx.actor.clone(),
                ctx.causation_id,
                EventType::WorktreeLocked,
                &token.worktree_id,
                Some(state.worktree.project_id),
                &WorktreeLockedData {
                    worktree_id: token.worktree_id,
                    holder: token.holder,
                    delivery_id: token.delivery_id,
                    lease_until: until,
                },
            )
            .await?;
        tx.commit().await.map_err(store_error)?;
        table.apply(&event)?;
        let lock = table
            .get(&token.worktree_id)?
            .lock
            .clone()
            .expect("lock was just renewed");
        Ok(lock.token(token.worktree_id))
    }

    async fn release_impl(&self, ctx: &ActorCtx, token: &LockToken) -> Result<(), RepoError> {
        let mut table = self.inner.worktrees.lock().await;
        let state = table.get(&token.worktree_id)?;
        let Some(lock) = &state.lock else {
            return Ok(());
        };
        if !lock.matches(token) {
            return Ok(());
        }
        let project = state.worktree.project_id;
        let mut tx = self.begin().await?;
        let event = self
            .append(
                &mut tx,
                ctx.actor.clone(),
                ctx.causation_id,
                EventType::WorktreeUnlocked,
                &token.worktree_id,
                Some(project),
                &WorktreeUnlockedData {
                    worktree_id: token.worktree_id,
                    reason: WorktreeUnlockReason::Released,
                },
            )
            .await?;
        tx.commit().await.map_err(store_error)?;
        table.apply(&event)?;
        Ok(())
    }

    fn current_lock(&self, table: &WorktreeTable, id: &WorktreeId) -> Option<ActiveLock> {
        table.by_id.get(id).and_then(|state| state.lock.clone())
    }
}

// ---------------------------------------------------------------------------
// RepoService
// ---------------------------------------------------------------------------

#[async_trait]
impl RepoService for GitRepo {
    async fn ensure_mirror(
        &self,
        _ctx: &ActorCtx,
        project: &ProjectId,
        remote: &RepoSlug,
    ) -> Result<MirrorInfo, RepoError> {
        let _serial = self.inner.mirror_create.lock().await;
        if let Ok(entry) = self.mirror_entry(project) {
            return Ok(entry.info());
        }
        let dest = mirror::mirror_path(&self.inner.cfg.mirrors_dir(), remote)?;
        if dest.exists() {
            match mirror::read_project_file(&dest) {
                Some((owner, _)) if owner == *project => {}
                _ => return Err(io_error("mirror path is owned by another project")),
            }
        } else {
            mirror::create(
                &self.inner.env,
                &self.inner.cfg.tmp_dir(),
                &self.inner.cfg.template_dir(),
                project,
                remote,
                &dest,
            )
            .await?;
        }
        let entry = MirrorEntry {
            slug: remote.clone(),
            git_dir: dest,
            health: MirrorHealth::Healthy,
        };
        let info = entry.info();
        self.inner
            .mirrors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(*project, entry);
        Ok(info)
    }

    /// 返回路径与 refspec。`core::ports::MirrorGuard` 不能持锁；需要挡住 gc 的
    /// 调用方改用 [`GitRepo::lease_mirror`]。
    async fn mirror_access(
        &self,
        _ctx: &ActorCtx,
        project: &ProjectId,
        mode: MirrorMode,
    ) -> Result<MirrorGuard, RepoError> {
        let entry = self.mirror_entry(project)?;
        if mode != MirrorMode::Fetch {
            if let MirrorHealth::NeedsRebuild { .. } = entry.health {
                return Err(RepoError::MirrorNeedsRebuild);
            }
        }
        Ok(entry.guard())
    }

    async fn commit_info(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitInfo>, RepoError> {
        let entry = self.mirror_entry(project)?;
        let slot = self.slot(project);
        let _permit = slot
            .acquire()
            .await
            .map_err(|_| io_error("git slots closed"))?;
        mirror::commit_info(&self.inner.env, &entry.git_dir, oid).await
    }

    async fn resolve(&self, project: &ProjectId, rev: &RevSpec) -> Result<GitOid, RepoError> {
        let entry = self.mirror_entry(project)?;
        match mirror::resolve(&self.inner.env, &entry.git_dir, rev.as_str()).await? {
            Some(oid) => Ok(oid),
            None => Err(RepoError::BaseNotInMirror {
                oid: GitOid::parse(rev.as_str()).unwrap_or_else(|_| zero_oid()),
            }),
        }
    }

    async fn has_commit(&self, project: &ProjectId, oid: &GitOid) -> Result<bool, RepoError> {
        let entry = self.mirror_entry(project)?;
        mirror::has_commit(&self.inner.env, &entry.git_dir, oid).await
    }

    async fn check_mirror(
        &self,
        _ctx: &ActorCtx,
        project: &ProjectId,
    ) -> Result<MirrorHealth, RepoError> {
        let entry = self.mirror_entry(project)?;
        let _lock = mirror::lock(&entry.git_dir, false).await?;
        let health = mirror::check(&self.inner.env, &entry.git_dir).await?;
        self.set_health(project, health.clone());
        Ok(health)
    }

    async fn create_worktree(
        &self,
        ctx: &ActorCtx,
        req: CreateWorktree,
    ) -> Result<Worktree, RepoError> {
        let mut table = self.inner.worktrees.lock().await;
        if let Some(id) = table.by_delivery.get(&req.delivery_id) {
            return Ok(table.get(id)?.worktree.clone());
        }
        let entry = self.mirror_entry(&req.project_id)?;
        self.check_branch(&req.project_id, req.branch.as_str())
            .await?;
        if !mirror::has_commit(&self.inner.env, &entry.git_dir, &req.base_sha).await? {
            return Err(RepoError::BaseNotInMirror { oid: req.base_sha });
        }
        let id: WorktreeId = self
            .inner
            .deps
            .ids
            .next()
            .map_err(|error| io_error(&format!("worktree id: {error}")))?;
        self.git(
            &req.project_id,
            GitCmd::new("update-ref", &entry.git_dir)
                .args(["update-ref", "--no-deref"])
                .arg(wt_ref(&id))
                .arg(req.base_sha.to_string())
                .arg(zero_oid().to_string()),
        )
        .await?;
        let mut tx = self.begin().await?;
        let event = self
            .append(
                &mut tx,
                ctx.actor.clone(),
                ctx.causation_id,
                EventType::WorktreeCreated,
                &id,
                Some(req.project_id),
                &WorktreeCreatedData {
                    worktree_id: id,
                    project_id: req.project_id,
                    delivery_id: req.delivery_id,
                    branch: req.branch.as_str().to_owned(),
                    base_ref: req.base_ref.clone(),
                    base_sha: req.base_sha,
                },
            )
            .await?;
        tx.commit().await.map_err(store_error)?;
        table.apply(&event)?;
        Ok(table.get(&id)?.worktree.clone())
    }

    async fn get_worktree(&self, id: &WorktreeId) -> Result<Option<Worktree>, RepoError> {
        let table = self.inner.worktrees.lock().await;
        Ok(table.by_id.get(id).map(|state| state.worktree.clone()))
    }

    async fn branch_head(&self, id: &WorktreeId) -> Result<GitOid, RepoError> {
        let project = {
            let table = self.inner.worktrees.lock().await;
            table.get(id)?.worktree.project_id
        };
        self.branch_ref_head(&project, id).await
    }

    async fn remove_worktree(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        lock: &LockToken,
    ) -> Result<(), RepoError> {
        let mut table = self.inner.worktrees.lock().await;
        let mut tx = self.begin().await?;
        let now = tx.now();
        let state = table.get(id)?;
        check_token(state, lock, now)?;
        let project = state.worktree.project_id;
        let branch = state.worktree.branch.clone();
        let unlocked = self
            .append(
                &mut tx,
                ctx.actor.clone(),
                ctx.causation_id,
                EventType::WorktreeUnlocked,
                id,
                Some(project),
                &WorktreeUnlockedData {
                    worktree_id: *id,
                    reason: WorktreeUnlockReason::Released,
                },
            )
            .await?;
        let removed = self
            .append(
                &mut tx,
                ctx.actor.clone(),
                ctx.causation_id,
                EventType::WorktreeRemoved,
                id,
                Some(project),
                &WorktreeRemovedData {
                    worktree_id: *id,
                    branch,
                },
            )
            .await?;
        tx.commit().await.map_err(store_error)?;
        apply_all(&mut table, &[unlocked, removed])?;
        drop(table);
        // 事件已提交；ref 删除失败只留下无害的孤儿 ref。
        if let Ok(entry) = self.mirror_entry(&project) {
            let _ = self
                .git(
                    &project,
                    GitCmd::new("update-ref", &entry.git_dir)
                        .args(["update-ref", "-d", "--no-deref"])
                        .arg(wt_ref(id)),
                )
                .await;
        }
        Ok(())
    }

    /// M2（OQ-R10）。M1 不提供。
    async fn advance_head(
        &self,
        _ctx: &ActorCtx,
        _id: &WorktreeId,
        _new_head: &GitOid,
        _lock: &LockToken,
    ) -> Result<GitOid, RepoError> {
        Err(io_error("advance_head is an M2 capability (repo.md §1.1)"))
    }

    async fn acquire_lock(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        holder: LockHolder,
        delivery: &DeliveryId,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        self.lock_impl(ctx, id, holder, delivery, lease).await
    }

    async fn renew_lock(
        &self,
        ctx: &ActorCtx,
        token: &LockToken,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        self.renew_impl(ctx, token, lease).await
    }

    async fn release_lock(&self, ctx: &ActorCtx, token: &LockToken) -> Result<(), RepoError> {
        self.release_impl(ctx, token).await
    }

    async fn prepare_workspace(
        &self,
        _ctx: &ActorCtx,
        _req: PrepareWorkspace,
    ) -> Result<PreparedWorkspace, RepoError> {
        Err(io_error("prepare_workspace lands in a later #91 slice"))
    }

    async fn validate_patch(
        &self,
        _ctx: &ActorCtx,
        _req: ValidatePatch,
    ) -> Result<PatchOutcome, RepoError> {
        Err(io_error("validate_patch lands in a later #91 slice"))
    }

    async fn commit_patch(
        &self,
        _ctx: &ActorCtx,
        _req: CommitPatch,
    ) -> Result<CommitRecord, RepoError> {
        Err(io_error("commit_patch lands in a later #91 slice"))
    }

    async fn provenance(
        &self,
        _project: &ProjectId,
        _oid: &GitOid,
    ) -> Result<Option<CommitRecord>, RepoError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// WorktreeLocks（api 的人工锁）
// ---------------------------------------------------------------------------

#[async_trait]
impl WorktreeLocks for GitRepo {
    async fn acquire_human(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        lease: Duration,
    ) -> Result<Worktree, LockError> {
        let delivery = {
            let table = self.inner.worktrees.lock().await;
            table.get(id).map_err(lock_error)?.delivery_id
        };
        self.lock_impl(ctx, id, LockHolder::Human, &delivery, lease)
            .await
            .map_err(lock_error)?;
        self.get_worktree(id)
            .await
            .map_err(lock_error)?
            .ok_or(LockError::NotFound)
    }

    async fn release_human(&self, ctx: &ActorCtx, id: &WorktreeId) -> Result<Worktree, LockError> {
        let token = {
            let table = self.inner.worktrees.lock().await;
            table.get(id).map_err(lock_error)?;
            let lock = self
                .current_lock(&table, id)
                .ok_or(LockError::LockRequired)?;
            if lock.holder != LockHolder::Human {
                return Err(LockError::Held { lock: lock.info() });
            }
            lock.token(*id)
        };
        self.release_impl(ctx, &token).await.map_err(lock_error)?;
        self.get_worktree(id)
            .await
            .map_err(lock_error)?
            .ok_or(LockError::NotFound)
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 工作树分支头的 ref（repo.md §7.1）。
pub fn wt_ref(id: &WorktreeId) -> String {
    format!("refs/gantry/wt/{id}")
}

fn zero_oid() -> GitOid {
    GitOid::parse("0000000000000000000000000000000000000000").expect("zero oid is valid")
}

fn system_actor() -> Actor {
    Actor::new(ActorKind::System, "repo")
}

fn apply_all(table: &mut WorktreeTable, events: &[Event<Value>]) -> Result<(), RepoError> {
    for event in events {
        table.apply(event)?;
    }
    Ok(())
}

fn machine_error(error: worktree_lock::LockError) -> RepoError {
    match error {
        worktree_lock::LockError::LeaseTooLong => RepoError::LeaseTooLong {
            max: HUMAN_LEASE_MAX,
        },
        worktree_lock::LockError::NotHolder => RepoError::StaleLock,
        worktree_lock::LockError::LockConflict | worktree_lock::LockError::InvalidLease => {
            RepoError::Lock(LockError::Store(PortStoreError::InvalidState {
                message: error.to_string(),
            }))
        }
    }
}

pub(crate) fn store_error(error: gantry_store::StoreError) -> RepoError {
    let port = match error {
        gantry_store::StoreError::NotFound => PortStoreError::NotFound,
        gantry_store::StoreError::Poisoned => PortStoreError::Unavailable { retry_after: None },
        gantry_store::StoreError::PreconditionFailed(message) => {
            PortStoreError::InvalidState { message }
        }
        other => PortStoreError::Rejected {
            message: other.to_string(),
        },
    };
    RepoError::Store(port)
}

fn lock_error(error: RepoError) -> LockError {
    match error {
        RepoError::WorktreeLocked { lock } => LockError::Held { lock },
        RepoError::WorktreeNotFound => LockError::NotFound,
        RepoError::LockRequired => LockError::LockRequired,
        RepoError::StaleLock => LockError::StaleLock,
        RepoError::LeaseTooLong { max } => LockError::LeaseTooLong { max },
        RepoError::Lock(lock) => lock,
        RepoError::Store(store) => LockError::Store(store),
        other => LockError::Store(PortStoreError::Rejected {
            message: other.to_string(),
        }),
    }
}
