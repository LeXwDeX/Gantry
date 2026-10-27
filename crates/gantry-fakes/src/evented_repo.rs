//! 写事件的 repo 替身：把 [`FakeRepo`] 外包一层，像真实 repo 一样在操作成功后写
//! `worktree.created` / `worktree.locked` / `worktree.unlocked` / `worktree.removed` /
//! `patch.validated`（repo.md §7）。引擎从事件折叠工作树与补丁状态，只用 `FakeRepo`
//! 时交付单元会停在 implement 之后。
//!
//! 锁围栏取 `worktree.locked` 的 seq（与真实 repo 一致）；对内仍用 `FakeRepo` 自己的令牌，
//! 两者的对应关系记在 `tokens` 里。真实 repo 在同一事务里写这些事件，这里是操作成功后
//! 各自追加一个事务，足够驱动引擎的端到端路径（engine.md §12.2）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, Seq};
use gantry_core::events::{
    EventType, NewEvent, PatchValidatedData, Subject, WorktreeCreatedData, WorktreeLockedData,
    WorktreeRemovedData, WorktreeUnlockReason, WorktreeUnlockedData,
};
use gantry_core::ids::{DeliveryId, ProjectId, WorktreeId};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::repo::{
    CommitInfo, CommitPatch, CommitRecord, CreateWorktree, LockToken, MirrorGuard, MirrorHealth,
    MirrorInfo, MirrorMode, PatchOutcome, PrepareWorkspace, PreparedWorkspace, RepoError,
    RepoService, RevSpec, ValidatePatch,
};
use gantry_store::{Appended, Store};
use serde_json::Value;

use crate::repo::FakeRepo;

pub struct EventedRepo {
    inner: Arc<FakeRepo>,
    store: Store,
    /// 对外围栏（事件 seq）→ FakeRepo 自己的锁令牌。
    tokens: Mutex<HashMap<(WorktreeId, Seq), LockToken>>,
    /// 交付单元 → 项目；事件的 `project_id` 取自 `create_worktree`。
    projects: Mutex<HashMap<DeliveryId, ProjectId>>,
}

impl EventedRepo {
    pub fn new(inner: Arc<FakeRepo>, store: Store) -> Self {
        Self {
            inner,
            store,
            tokens: Mutex::new(HashMap::new()),
            projects: Mutex::new(HashMap::new()),
        }
    }

    pub fn inner(&self) -> &Arc<FakeRepo> {
        &self.inner
    }

    fn inner_token(&self, token: &LockToken) -> LockToken {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(token.worktree_id, token.fence))
            .cloned()
            .unwrap_or_else(|| token.clone())
    }

    /// 以 `system:repo` 身份追加一条事件，返回其 seq（重复时返回已有的 seq）。
    async fn append(
        &self,
        ty: EventType,
        subject: &str,
        delivery: DeliveryId,
        data: Value,
        dedup_key: Option<String>,
    ) -> Seq {
        let project_id = self
            .projects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&delivery)
            .copied();
        let event = NewEvent {
            ty,
            subject: Subject::parse(subject).expect("event subject is an id"),
            project_id,
            actor: Actor::new(ActorKind::System, "repo"),
            causation_id: None,
            correlation_id: Some(delivery.to_string()),
            data,
            dedup_key,
        };
        event.validate().expect("repo event matches its schema");
        let mut tx = self.store.begin_write().await.expect("begin write");
        let appended = tx.append(event).await.expect("append repo event");
        tx.commit().await.expect("commit repo event");
        match appended {
            Appended::New(event) => event.seq,
            Appended::Duplicate { existing_seq } => existing_seq,
        }
    }
}

#[async_trait]
impl RepoService for EventedRepo {
    async fn ensure_mirror(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
        remote: &RepoSlug,
    ) -> Result<MirrorInfo, RepoError> {
        self.inner.ensure_mirror(ctx, project, remote).await
    }

    async fn mirror_access(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
        mode: MirrorMode,
    ) -> Result<MirrorGuard, RepoError> {
        self.inner.mirror_access(ctx, project, mode).await
    }

    async fn commit_info(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitInfo>, RepoError> {
        self.inner.commit_info(project, oid).await
    }

    async fn resolve(&self, project: &ProjectId, rev: &RevSpec) -> Result<GitOid, RepoError> {
        self.inner.resolve(project, rev).await
    }

    async fn has_commit(&self, project: &ProjectId, oid: &GitOid) -> Result<bool, RepoError> {
        self.inner.has_commit(project, oid).await
    }

    async fn check_mirror(
        &self,
        ctx: &ActorCtx,
        project: &ProjectId,
    ) -> Result<MirrorHealth, RepoError> {
        self.inner.check_mirror(ctx, project).await
    }

    async fn create_worktree(
        &self,
        ctx: &ActorCtx,
        req: CreateWorktree,
    ) -> Result<gantry_core::entities::Worktree, RepoError> {
        let delivery = req.delivery_id;
        let base_sha = req.base_sha;
        let worktree = self.inner.create_worktree(ctx, req).await?;
        self.projects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(delivery, worktree.project_id);
        let data = WorktreeCreatedData {
            worktree_id: worktree.id,
            project_id: worktree.project_id,
            delivery_id: delivery,
            branch: worktree.branch.clone(),
            base_ref: worktree.base_ref.clone(),
            base_sha,
        };
        self.append(
            EventType::WorktreeCreated,
            &worktree.id.to_string(),
            delivery,
            serde_json::to_value(data).expect("event data serializes"),
            Some(format!("worktree.created:{}", worktree.id)),
        )
        .await;
        Ok(worktree)
    }

    async fn get_worktree(
        &self,
        id: &WorktreeId,
    ) -> Result<Option<gantry_core::entities::Worktree>, RepoError> {
        self.inner.get_worktree(id).await
    }

    async fn branch_head(&self, id: &WorktreeId) -> Result<GitOid, RepoError> {
        self.inner.branch_head(id).await
    }

    async fn remove_worktree(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        lock: &LockToken,
    ) -> Result<(), RepoError> {
        let branch = self
            .inner
            .get_worktree(id)
            .await?
            .map(|w| w.branch)
            .unwrap_or_default();
        self.inner
            .remove_worktree(ctx, id, &self.inner_token(lock))
            .await?;
        let data = WorktreeRemovedData {
            worktree_id: *id,
            branch,
        };
        self.append(
            EventType::WorktreeRemoved,
            &id.to_string(),
            lock.delivery_id,
            serde_json::to_value(data).expect("event data serializes"),
            Some(format!("worktree.removed:{id}")),
        )
        .await;
        Ok(())
    }

    async fn advance_head(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        new_head: &GitOid,
        lock: &LockToken,
    ) -> Result<GitOid, RepoError> {
        self.inner
            .advance_head(ctx, id, new_head, &self.inner_token(lock))
            .await
    }

    async fn acquire_lock(
        &self,
        ctx: &ActorCtx,
        id: &WorktreeId,
        holder: gantry_core::entities::LockHolder,
        delivery: &DeliveryId,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        let inner = self
            .inner
            .acquire_lock(ctx, id, holder, delivery, lease)
            .await?;
        let data = WorktreeLockedData {
            worktree_id: *id,
            holder,
            delivery_id: Some(*delivery),
            lease_until: inner.lease_until,
        };
        let fence = self
            .append(
                EventType::WorktreeLocked,
                &id.to_string(),
                *delivery,
                serde_json::to_value(data).expect("event data serializes"),
                None,
            )
            .await;
        let outer = LockToken {
            fence,
            ..inner.clone()
        };
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((*id, fence), inner);
        Ok(outer)
    }

    async fn renew_lock(
        &self,
        ctx: &ActorCtx,
        token: &LockToken,
        lease: Duration,
    ) -> Result<LockToken, RepoError> {
        let inner = self
            .inner
            .renew_lock(ctx, &self.inner_token(token), lease)
            .await?;
        let outer = LockToken {
            fence: token.fence,
            lease_until: inner.lease_until,
            ..token.clone()
        };
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((token.worktree_id, token.fence), inner);
        Ok(outer)
    }

    async fn release_lock(&self, ctx: &ActorCtx, token: &LockToken) -> Result<(), RepoError> {
        self.inner
            .release_lock(ctx, &self.inner_token(token))
            .await?;
        let data = WorktreeUnlockedData {
            worktree_id: token.worktree_id,
            reason: WorktreeUnlockReason::Released,
        };
        self.append(
            EventType::WorktreeUnlocked,
            &token.worktree_id.to_string(),
            token.delivery_id,
            serde_json::to_value(data).expect("event data serializes"),
            Some(format!(
                "worktree.unlocked:{}:{}",
                token.worktree_id,
                token.fence.as_u64()
            )),
        )
        .await;
        Ok(())
    }

    async fn prepare_workspace(
        &self,
        ctx: &ActorCtx,
        req: PrepareWorkspace,
    ) -> Result<PreparedWorkspace, RepoError> {
        self.inner.prepare_workspace(ctx, req).await
    }

    async fn validate_patch(
        &self,
        ctx: &ActorCtx,
        req: ValidatePatch,
    ) -> Result<PatchOutcome, RepoError> {
        let delivery = req.delivery_id;
        let job = req.job_id;
        let outcome = self.inner.validate_patch(ctx, req).await?;
        if let PatchOutcome::Validated(v) = &outcome {
            let data = PatchValidatedData {
                job_id: job,
                sha256: v.sha256,
                base_sha: v.base_sha,
                result_tree: v.result_tree.expect("FakeRepo always yields a tree"),
                verdict: v.verdict,
                files: v.files.clone(),
                reasons: v.reasons.clone(),
                config_rev: Seq::new(1).expect("seq 1"),
            };
            self.append(
                EventType::PatchValidated,
                &job.to_string(),
                delivery,
                serde_json::to_value(data).expect("event data serializes"),
                Some(format!("patch.validated:{job}")),
            )
            .await;
        }
        Ok(outcome)
    }

    async fn commit_patch(
        &self,
        ctx: &ActorCtx,
        mut req: CommitPatch,
    ) -> Result<CommitRecord, RepoError> {
        req.lock = self.inner_token(&req.lock);
        self.inner.commit_patch(ctx, req).await
    }

    async fn provenance(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitRecord>, RepoError> {
        self.inner.provenance(project, oid).await
    }
}
