//! 生产用副作用执行器 [`PortEffects`]（engine.md §7.14.5 `Effect` 表，#72）。
//!
//! 把每种 [`Effect`] 接到 `core::ports`：能马上得到结果的返回 `Some(EffectReport)`；长时副作用
//! （`RunAttempt`、`ScheduleTimer`）在 tokio 任务里执行，完成后经调用方提供的 [`Deliver`] 回调
//! 以 `Input::Report` / `Input::Tick` 投回执行体，本次返回 `None`。
//!
//! 端口错误按可否重试映射为 `EffectReport::EffectFailed{key, error}`（[`EffectErrorClass`]）；
//! 幂等的清理类副作用（释放、取消、作废）遇到"对象已不存在 / 已终态"时视为完成。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::entities::LockHolder;
use gantry_core::ids::DeliveryId;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::forge::OutboxPort;
use gantry_core::ports::policy::PolicyError;
use gantry_core::ports::repo::{CreateWorktree, LockError, RepoError, RevSpec};
use gantry_core::ports::sandbox::SandboxError;

use crate::actor::EffectRunner;
use crate::attempt::{
    forge_error_class, policy_error_class, repo_error_class, sandbox_error_class, AttemptContext,
    AttemptPorts, AttemptRunner, RunnerConfig,
};
use crate::decide::{
    Effect, EffectErrorClass, EffectKey, EffectReport, Input, InputIdentity, TickReason,
};
use crate::state::DeliveryState;

/// 长时副作用完成后的投递口：调用方把它接到执行体的输入队列（`DeliveryActor::handle`）。
pub type Deliver = Arc<dyn Fn(DeliveryId, Input, InputIdentity) + Send + Sync>;

fn valid_default_branch(name: &str) -> bool {
    !name.is_empty()
        && name != "@"
        && !name.starts_with('-')
        && !name.starts_with("refs/")
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.bytes().any(|c| {
            c <= b' ' || c == 0x7f || matches!(c, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

pub struct PortEffects {
    ports: AttemptPorts,
    outbox: Arc<dyn OutboxPort>,
    config: RwLock<Arc<RunnerConfig>>,
    /// `RemoveWorktree` 在锁已释放时先以 engine 身份取锁，这是那把临时锁的租约。
    cleanup_lease: Duration,
    deliver: Deliver,
}

impl PortEffects {
    pub fn new(
        ports: AttemptPorts,
        outbox: Arc<dyn OutboxPort>,
        config: RunnerConfig,
        cleanup_lease: Duration,
        deliver: Deliver,
    ) -> Self {
        Self {
            ports,
            outbox,
            config: RwLock::new(Arc::new(config)),
            cleanup_lease,
            deliver,
        }
    }

    /// 两次派发之间整体切换配置（§7.13）；已开始组装的 spec 用的是开始时的快照。
    pub fn reconfigure(&self, config: RunnerConfig) {
        let mut guard = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Arc::new(config);
    }

    fn snapshot(&self) -> Arc<RunnerConfig> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn failed(key: EffectKey, error: EffectErrorClass) -> Option<EffectReport> {
        Some(EffectReport::EffectFailed { key, error })
    }

    fn done(key: EffectKey) -> Option<EffectReport> {
        Some(EffectReport::Done { key })
    }
}

#[async_trait]
impl EffectRunner for PortEffects {
    async fn run(
        &self,
        state: &DeliveryState,
        effect: Effect,
        ctx: &ActorCtx,
    ) -> Option<EffectReport> {
        let delivery = state.delivery.id;
        let project = state.delivery.project_id;
        let key = effect.key(delivery);
        let repo = &self.ports.repo;
        match effect {
            Effect::EnsureWorktree { branch, base_ref } => {
                if !valid_default_branch(&base_ref) {
                    return Self::failed(
                        key,
                        EffectErrorClass::Defect {
                            what: "default_branch_invalid",
                        },
                    );
                }
                let base_sha = match repo
                    .resolve(
                        &project,
                        &RevSpec::new(format!("refs/mirror/heads/{base_ref}")),
                    )
                    .await
                {
                    Ok(oid) => oid,
                    Err(error) => return Self::failed(key, repo_error_class(&error)),
                };
                let request = CreateWorktree {
                    project_id: project,
                    delivery_id: delivery,
                    branch,
                    base_ref,
                    base_sha,
                };
                match repo.create_worktree(ctx, request).await {
                    Ok(worktree) => Some(EffectReport::WorktreeReady {
                        worktree_id: worktree.id,
                    }),
                    Err(error) => Self::failed(key, repo_error_class(&error)),
                }
            }
            Effect::RemoveWorktree { worktree_id, lock } => {
                let token = match lock {
                    Some(token) => token,
                    None => match repo
                        .acquire_lock(
                            ctx,
                            &worktree_id,
                            LockHolder::Engine,
                            &delivery,
                            self.cleanup_lease,
                        )
                        .await
                    {
                        Ok(token) => token,
                        Err(RepoError::WorktreeNotFound) => return Self::done(key),
                        Err(error) => return Self::failed(key, repo_error_class(&error)),
                    },
                };
                match repo.remove_worktree(ctx, &worktree_id, &token).await {
                    Ok(()) | Err(RepoError::WorktreeNotFound) => Self::done(key),
                    Err(error) => Self::failed(key, repo_error_class(&error)),
                }
            }
            Effect::AcquireLock {
                worktree_id,
                node_id,
                holder,
                lease_s,
            } => {
                match repo
                    .acquire_lock(
                        ctx,
                        &worktree_id,
                        holder,
                        &delivery,
                        Duration::from_secs(lease_s),
                    )
                    .await
                {
                    Ok(token) => Some(EffectReport::LockAcquired { node_id, token }),
                    Err(RepoError::WorktreeLocked { lock })
                    | Err(RepoError::Lock(LockError::Held { lock })) => {
                        Some(EffectReport::LockBusy {
                            node_id,
                            holder: lock.holder,
                            until: Some(lock.lease_until),
                        })
                    }
                    Err(error) => Self::failed(key, repo_error_class(&error)),
                }
            }
            Effect::ReleaseLock { token } => match repo.release_lock(ctx, &token).await {
                // 锁已过期或已被替换：释放本就无事可做。
                Ok(())
                | Err(RepoError::WorktreeNotFound)
                | Err(RepoError::StaleLock)
                | Err(RepoError::LockRequired) => Self::done(key),
                Err(error) => Self::failed(key, repo_error_class(&error)),
            },
            Effect::RenewLock { token, lease_s } => {
                match repo
                    .renew_lock(ctx, &token, Duration::from_secs(lease_s))
                    .await
                {
                    Ok(_) => Self::done(key),
                    Err(error) => Self::failed(key, repo_error_class(&error)),
                }
            }
            Effect::RunAttempt(plan) => {
                let cx = match AttemptContext::from_state(state, &plan) {
                    Ok(cx) => cx,
                    Err(error) => return Self::failed(key, error.class()),
                };
                let runner = AttemptRunner::new(self.ports.clone(), self.snapshot());
                let deliver = self.deliver.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let report = match runner.run(&plan, &cx, &ctx).await {
                        Ok(finalized) => EffectReport::AttemptFinalized(finalized),
                        Err(error) => EffectReport::EffectFailed {
                            key: key.clone(),
                            error: error.class(),
                        },
                    };
                    deliver(delivery, Input::Report(report), InputIdentity::Report(key));
                });
                None
            }
            Effect::CancelAttempt { attempt_id, .. } => {
                match self.ports.sandbox.cancel(ctx, &attempt_id).await {
                    Ok(()) | Err(SandboxError::NotFound) => Self::done(key),
                    Err(error) => Self::failed(key, sandbox_error_class(&error)),
                }
            }
            Effect::ReleaseSandbox { attempt_id } => {
                match self.ports.sandbox.release(ctx, &attempt_id).await {
                    Ok(()) | Err(SandboxError::NotFound) => Self::done(key),
                    Err(error) => Self::failed(key, sandbox_error_class(&error)),
                }
            }
            Effect::JudgeAction {
                key: action,
                request,
            } => match self.ports.policy.decide_action(request, ctx).await {
                Ok(decision) => Some(EffectReport::ActionJudged {
                    key: action,
                    decision,
                }),
                Err(error) => Self::failed(key, policy_error_class(&error)),
            },
            Effect::RequestApproval { key: action, spec } => {
                match self.ports.policy.request_approval(spec, ctx.clone()).await {
                    Ok(approval) => Some(EffectReport::ApprovalRequested {
                        key: action,
                        approval: approval.id,
                    }),
                    Err(error) => Self::failed(key, policy_error_class(&error)),
                }
            }
            Effect::InvalidateApproval {
                approval_id,
                reason,
            } => match self
                .ports
                .policy
                .invalidate(&approval_id, reason, ctx)
                .await
            {
                // 已终态（consumed / invalidated / expired）或不存在：作废的目的已达成。
                Ok(_) | Err(PolicyError::Conflict { .. }) | Err(PolicyError::NotFound) => {
                    Self::done(key)
                }
                Err(error) => Self::failed(key, policy_error_class(&error)),
            },
            Effect::CancelOutbox { action_id, reason } => {
                match self.outbox.cancel(&action_id, reason, ctx).await {
                    Ok(_) => Self::done(key),
                    Err(error) => Self::failed(key, forge_error_class(&error)),
                }
            }
            Effect::ReconcileNow { target } => {
                match self.ports.forge.reconcile_now(target, ctx).await {
                    Ok(_) => Some(EffectReport::ReconcileDone { target }),
                    Err(error) => Self::failed(key, forge_error_class(&error)),
                }
            }
            Effect::ScheduleTimer { at, kind } => {
                let now = self.ports.clock.now();
                let delay = at.as_millis().saturating_sub(now.as_millis()).max(0) as u64;
                let deliver = self.deliver.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let reason = TickReason::Timer { at, kind };
                    deliver(
                        delivery,
                        Input::Tick(reason.clone()),
                        InputIdentity::Tick(reason),
                    );
                });
                None
            }
        }
    }
}
