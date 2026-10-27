//! Outbox 入队面：`OutboxPort` 的真实实现（forge.md §6、§7.8"入队时的机械校验"）。

pub(crate) mod executor;
pub(crate) mod key;
pub(crate) mod marker;

use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{ForgeActionKind, OutboxStatus};
use gantry_core::forge::ActionPayload;
use gantry_core::ids::ActionId;
use gantry_core::ports::{
    ActorCtx, CancelOutcome, CancelReason, EnqueueError, EnqueueOutcome, EnqueueRequest,
    ForgeError, OutboxPort, StoreError as PortStoreError, TxHandle,
};
use gantry_store::{OutboxInsert, Store, StoreError, WriteTx};

use crate::config::{ForgeConfig, Permission};
use key::{derive_target_intent, KeyError};

/// M1 交付的动作（forge.md §1.1）。其余动作入队时拒绝，M2 起逐个开放。
pub const M1_ACTIONS: [ForgeActionKind; 5] = [
    ForgeActionKind::PushBranch,
    ForgeActionKind::CreatePr,
    ForgeActionKind::UpdatePr,
    ForgeActionKind::CloseIssue,
    ForgeActionKind::MergePr,
];

/// `forge` 的装配体。当前切片只实现 `OutboxPort`。
#[derive(Clone)]
pub struct Forge {
    store: Store,
    config: Arc<ForgeConfig>,
}

impl Forge {
    pub fn new(store: Store, config: ForgeConfig) -> Self {
        Self {
            store,
            config: Arc::new(config),
        }
    }

    /// 共享本实例的 store 与配置的执行器（forge.md §7.2：每个安装一个写任务）。
    pub fn executor(&self, deps: executor::ExecutorDeps) -> executor::Executor {
        executor::Executor::new(self.store.clone(), self.config.clone(), deps)
    }

    /// 入队时的机械校验 + 落行。不读 `policies.*`（P-62）：是否需要审批由入队方决定，
    /// 这里只拒绝"规则上恒需审批却没带审批"的组合。
    async fn enqueue_on(
        &self,
        tx: &mut WriteTx,
        req: EnqueueRequest,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        let binding = self
            .config
            .projects
            .get(&req.project_id)
            .ok_or(EnqueueError::ProjectUnknown)?;
        let kind = req.payload.kind();
        if !M1_ACTIONS.contains(&kind) {
            return Err(invalid("/kind", KeyError::NotInM1(kind).to_string()));
        }
        if payload_repo(&req.payload) != &binding.project.repo {
            return Err(invalid(
                "/payload/repo",
                "payload repo does not match the project binding".to_owned(),
            ));
        }
        let permission = Permission::required_for(kind);
        if !binding.granted.contains(&permission) {
            return Err(EnqueueError::PermissionMissing {
                kind,
                permission: permission.as_str(),
            });
        }
        match &req.payload {
            ActionPayload::MergePr(_) if req.requires_approval.is_none() => {
                return Err(EnqueueError::ApprovalRequired {
                    reason: "merge_pr always requires an approval",
                });
            }
            ActionPayload::PushBranch(p) if req.requires_approval.is_none() => {
                if p.force {
                    return Err(EnqueueError::ApprovalRequired {
                        reason: "push_branch with force=true requires force_push approval",
                    });
                }
                if !p.branch.starts_with(&self.config.branch_prefix) {
                    return Err(EnqueueError::ApprovalRequired {
                        reason: "push_branch outside the branch prefix requires push_outside_prefix approval",
                    });
                }
            }
            _ => {}
        }

        let key = derive_target_intent(&req.payload)
            .map_err(|error| invalid("/payload", error.to_string()))?;
        let payload = req
            .payload
            .to_value()
            .map_err(|error| invalid("/payload", error.to_string()))?;
        let insert = OutboxInsert {
            project_id: req.project_id,
            delivery_id: req.delivery_id,
            kind,
            target: key.target,
            intent: key.intent,
            payload,
            requires_approval: req.requires_approval,
            depends_on: req.depends_on,
            priority: req.priority,
            not_before: req.not_before,
            epoch: req.epoch,
            // M1 没有 upsert_status_comment，不需要合并键（forge-actions §2.7）。
            coalesce_key: None,
            enqueued_by: gantry_store::EnqueuedBy {
                module: req.enqueued_by.module.as_str().to_owned(),
                causation_id: req.enqueued_by.causation_id,
            },
        };
        // 同键已存在（含 failed / cancelled）时 store 返回已有行、created=false，
        // 不复活；重做必须 epoch + 1（P-53）。
        let enqueued = tx
            .outbox()
            .enqueue(insert)
            .await
            .map_err(|error| EnqueueError::Store(port_store_error(error)))?;
        Ok(EnqueueOutcome {
            action: enqueued.action,
            created: enqueued.created,
        })
    }
}

fn invalid(pointer: &str, message: String) -> EnqueueError {
    EnqueueError::InvalidPayload {
        pointer: pointer.to_owned(),
        message,
    }
}

fn payload_repo(payload: &ActionPayload) -> &gantry_core::canon::RepoSlug {
    use ActionPayload::*;
    match payload {
        PushBranch(p) => &p.repo,
        CreatePr(p) => &p.repo,
        UpdatePr(p) => &p.repo,
        UpsertStatusComment(p) => &p.repo,
        CreateComment(p) => &p.repo,
        ReplyReviewThread(p) => &p.repo,
        ResolveReviewThread(p) => &p.repo,
        CreateIssue(p) => &p.repo,
        UpdateIssue(p) => &p.repo,
        CloseIssue(p) => &p.repo,
        AddLabels(p) => &p.repo,
        RequestReviewers(p) => &p.repo,
        MergePr(p) => &p.repo,
        RerunFailedJobs(p) => &p.repo,
    }
}

pub(crate) fn port_store_error(error: StoreError) -> PortStoreError {
    match error {
        StoreError::NotFound => PortStoreError::NotFound,
        StoreError::Poisoned => PortStoreError::Unavailable { retry_after: None },
        StoreError::Unsupported(message) => PortStoreError::Rejected {
            message: message.to_owned(),
        },
        other => PortStoreError::InvalidState {
            message: other.to_string(),
        },
    }
}

fn store_cancel_reason(reason: &CancelReason) -> gantry_store::CancelReason {
    match reason {
        CancelReason::Superseded => gantry_store::CancelReason::Superseded,
        CancelReason::ApprovalInvalidated => gantry_store::CancelReason::ApprovalInvalid,
        CancelReason::DeliveryAbandoned
        | CancelReason::DeliveryClosed
        | CancelReason::Shutdown
        | CancelReason::Other(_) => gantry_store::CancelReason::Manual,
    }
}

#[async_trait]
impl OutboxPort for Forge {
    async fn enqueue_in(
        &self,
        tx: &mut dyn TxHandle,
        req: EnqueueRequest,
        _ctx: &ActorCtx,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        let tx = tx.as_any_mut().downcast_mut::<WriteTx>().ok_or_else(|| {
            EnqueueError::Store(PortStoreError::Rejected {
                message: "Forge::enqueue_in requires a store WriteTx".into(),
            })
        })?;
        self.enqueue_on(tx, req).await
    }

    async fn enqueue(
        &self,
        req: EnqueueRequest,
        _ctx: &ActorCtx,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        let mut tx = self
            .store
            .begin_write()
            .await
            .map_err(|error| EnqueueError::Store(port_store_error(error)))?;
        let outcome = self.enqueue_on(&mut tx, req).await?;
        tx.commit()
            .await
            .map_err(|error| EnqueueError::Store(port_store_error(error)))?;
        Ok(outcome)
    }

    async fn cancel(
        &self,
        id: &ActionId,
        reason: CancelReason,
        _ctx: &ActorCtx,
    ) -> Result<CancelOutcome, ForgeError> {
        let store_error = |error| ForgeError::Store(port_store_error(error));
        // 状态判定与取消在同一个写事务里，避免与执行器的 claim 竞态。
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let action = tx
            .read()
            .outbox_action(id)
            .ok_or_else(|| ForgeError::InvalidTarget {
                message: format!("outbox action {id} not found"),
            })?;
        match action.status {
            OutboxStatus::Queued => {
                tx.outbox()
                    .cancel(id, store_cancel_reason(&reason))
                    .await
                    .map_err(store_error)?;
                tx.commit().await.map_err(store_error)?;
                Ok(CancelOutcome::Cancelled)
            }
            // 执行中的动作不可取消，由执行器自行收尾（forge.md §6）。
            OutboxStatus::Executing => Ok(CancelOutcome::NotCancellable),
            other => Ok(CancelOutcome::AlreadyTerminal(other)),
        }
    }
}
