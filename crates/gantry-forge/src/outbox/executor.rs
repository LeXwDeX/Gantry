//! Outbox 执行器（forge.md §7.8；FA §2.3 先查后做、§2.5 审批、§2.6 失败分类）。
//!
//! 一次 [`Executor::run_once`] 处理一行：
//! claim → 前置检查 → 执行前消费审批 → probe → 预写 expected_effects → apply → complete。
//! 每个 store 方法都在自己的短事务里提交，事务内不做外部 IO。崩溃后 `executing` 行的租约
//! 过期，由下一次 `claim` 以 `replay = true` 返回，靠 probe 避免重复创建。
//!
//! 本切片覆盖 `create_pr` / `update_pr` / `close_issue` / `merge_pr`。`push_branch`
//! 需要镜像仓与 git 传输，在推送切片实现；在那之前它被延后而不是失败（见 `defer`）。
//! 限流闸（安装级惩罚期持久化、写间隔、令牌桶）与完整的出站净化在后续切片。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{
    ActorKind, ExpectedEffect, GithubPrRef, GithubRef, OutboxAction, OutboxError,
};
use gantry_core::forge::{
    ActionPayload, CloseIssuePayload, CreatePrPayload, IssueStateReason, MergePrPayload, PrState,
    PushBranchPayload, UpdatePrPayload,
};
use gantry_core::ids::{ActionId, IdGen};
use gantry_core::ports::{
    ActorCtx, ExecutionCheck, ExecutionPermit, ForgeError, GateError, MirrorMode, MirrorWant,
    PolicyPort, RepoService, RevSpec,
};
use gantry_core::secret::SecretString;
use gantry_core::time::Timestamp;
use gantry_store::{AfterFailure, CancelReason, ClaimOpts, Store, StoreError, WriteTx};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{marker, sanitize};

/// 出站文本的长度上限（字符数）。GitHub 的实际上限需核实（FA OQ-8）；正文预留标记与
/// `Closes #N` 的空间。
const TITLE_MAX_CHARS: usize = 256;
const BODY_MAX_CHARS: usize = 60_000;
use crate::auth::{AuthError, TokenProvider, TokenPurpose};
use crate::config::{ForgeConfig, Permission, ProjectBinding};
use crate::git::{ls_remote_sha, GitRunner};
use crate::github::{ApiError, GithubClient, PullView};

/// 执行器参数（forge.md §10 `forge.outbox.*`、`forge.http.timeout`）。
#[derive(Clone, Debug)]
pub struct ExecutorOptions {
    pub lease: Duration,
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    pub http_timeout: Duration,
    /// 二级限流没有给出等待时间时的最短等待（FA §10.3：至少 60 秒）。
    pub min_rate_limit_wait: Duration,
    /// git 子进程超时（forge.md §10 `forge.git.timeout`）。
    pub git_timeout: Duration,
}

impl Default for ExecutorOptions {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(120),
            max_attempts: 5,
            backoff_base: Duration::from_secs(2),
            backoff_cap: Duration::from_secs(300),
            http_timeout: Duration::from_secs(30),
            min_rate_limit_wait: Duration::from_secs(60),
            git_timeout: Duration::from_secs(300),
        }
    }
}

/// FA §2.3 崩溃点矩阵里的注入位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CrashPoint {
    /// claim 已提交，尚未发出任何请求。
    AfterClaim,
    /// 审批已消费，尚未 probe / apply。
    AfterConsume,
    /// `expected_effects` 已提交，变更调用尚未发出。
    AfterRecordEffects,
    /// 变更调用已成功，`complete` 尚未提交。
    AfterApply,
}

/// 故障注入点（forge.md §12 替身"故障注入点"）。返回 `true` 表示在该点"崩溃"：
/// 执行器立刻返回 [`ExecError::Crashed`]，不再写 store。
pub trait FaultInjector: Send + Sync {
    fn crash(&self, action: &ActionId, point: CrashPoint) -> bool;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    Succeeded,
    /// 留在 `queued` 退避（临时错误、限流或延后）。
    Requeued,
    /// 终态失败，已追加 `outbox.failed`。
    Failed {
        code: &'static str,
    },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub action: ActionId,
    pub replay: bool,
    pub outcome: StepOutcome,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("injected crash at {0:?}")]
    Crashed(CrashPoint),
    #[error("store: {0}")]
    Store(#[from] StoreError),
}

/// `forge_rate_state`（forge.md §7.3），键为安装 ID。
pub(crate) const RATE_TABLE: &str = "forge_rate_state";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RateState {
    /// 二级限流惩罚期结束时间；之前该安装零请求。
    penalty_until: Option<Timestamp>,
}

pub struct ExecutorDeps {
    pub policy: Arc<dyn PolicyPort>,
    pub tokens: Arc<dyn TokenProvider>,
    /// GitHub REST 根地址；测试里是 `FakeGitHub::base_url()`。
    pub api_base: String,
    pub ids: Arc<IdGen>,
    pub options: ExecutorOptions,
    pub faults: Option<Arc<dyn FaultInjector>>,
    /// 镜像仓、提交溯源与 `mirror_access`（forge.md §3 `repo` 依赖）。
    pub repo: Arc<dyn RepoService>,
    /// git 远端根地址，拼成 `{git_base}/{owner}/{repo}.git`；生产为 `https://github.com`。
    pub git_base: String,
}

pub struct Executor {
    store: Store,
    config: Arc<ForgeConfig>,
    policy: Arc<dyn PolicyPort>,
    tokens: Arc<dyn TokenProvider>,
    github: GithubClient,
    ids: Arc<IdGen>,
    opts: ExecutorOptions,
    faults: Option<Arc<dyn FaultInjector>>,
    repo: Arc<dyn RepoService>,
    git: GitRunner,
    git_base: String,
}

/// 一次执行的结论，由 `finish` 在一个事务里落库。
enum Disposition {
    Done(GithubRef),
    Terminal {
        code: &'static str,
        detail: String,
    },
    Retry {
        detail: String,
        immediate: bool,
    },
    RateLimited {
        wait: Duration,
    },
    Cancel {
        #[allow(dead_code)] // store 的 cancel 只收原因码；明细留作日志字段（§11）
        detail: String,
    },
    /// 本切片尚未实现的动作：延后，不计尝试。
    Defer {
        wait: Duration,
    },
}

/// probe 的结论（FA §3.2）。
enum Probe {
    Hit(GithubRef),
    Miss,
    Conflict { code: &'static str, detail: String },
}

impl Executor {
    pub(crate) fn new(
        store: Store,
        config: Arc<ForgeConfig>,
        deps: ExecutorDeps,
    ) -> std::io::Result<Self> {
        let git = GitRunner::new(deps.options.git_timeout)?;
        Ok(Self {
            store,
            config,
            policy: deps.policy,
            tokens: deps.tokens,
            github: GithubClient::new(deps.api_base, deps.options.http_timeout),
            ids: deps.ids,
            opts: deps.options,
            faults: deps.faults,
            git,
            repo: deps.repo,
            git_base: deps.git_base.trim_end_matches('/').to_owned(),
        })
    }

    fn remote_url(&self, repo: &RepoSlug) -> String {
        format!("{}/{repo}.git", self.git_base)
    }

    /// 把镜像仓拉到最新（forge.md §7.10"镜像仓拉取"；`ForgeReader::sync_mirror` 的实现体）。
    ///
    /// `ensure_mirror` → `mirror_access(Fetch)` → `git fetch --prune <url> <refspecs>`。
    /// 本地工作在 `refs/gantry/*`，上游快照在 `refs/mirror/*`，`--prune` 不会删未推送的提交。
    pub async fn sync_mirror(
        &self,
        project: &gantry_core::ids::ProjectId,
        want: &MirrorWant,
    ) -> Result<(), ForgeError> {
        let binding =
            self.config
                .projects
                .get(project)
                .ok_or_else(|| ForgeError::InvalidTarget {
                    message: "project is not bound to forge".into(),
                })?;
        let ctx = self.ctx();
        let unavailable = |message: String| ForgeError::Unavailable { message };
        self.repo
            .ensure_mirror(&ctx, project, &binding.project.repo)
            .await
            .map_err(|error| unavailable(format!("ensure_mirror: {error}")))?;
        // 守卫持有镜像仓共享锁（#193），活到本函数结束：fetch 全程挡住 gc / 重建。
        let guard = self
            .repo
            .mirror_access(&ctx, project, MirrorMode::Fetch)
            .await
            .map_err(|error| unavailable(format!("mirror_access: {error}")))?;
        let token = self
            .tokens
            .token(project, TokenPurpose::Read)
            .await
            .map_err(|error| unavailable(format!("token: {error}")))?;
        let url = self.remote_url(&binding.project.repo);
        let mut args = vec!["fetch", "--prune", "--no-tags", url.as_str()];
        args.extend(guard.fetch_refspecs.iter().map(String::as_str));
        let output = self
            .git
            .run(&guard.git_dir, &args, Some(&token))
            .await
            .map_err(|error| unavailable(error.to_string()))?;
        if !output.success {
            return Err(unavailable(format!(
                "git fetch failed: {}",
                first_line(&output.stderr)
            )));
        }
        if let MirrorWant::Oid(oid) = want {
            let present = self
                .repo
                .has_commit(project, oid)
                .await
                .map_err(|error| unavailable(error.to_string()))?;
            if !present {
                return Err(ForgeError::NotFound);
            }
        }
        Ok(())
    }

    /// 领取并执行一行；没有可执行的行时返回 `None`。
    pub async fn run_once(&self) -> Result<Option<Step>, ExecError> {
        let mut tx = self.store.begin_write().await?;
        let claimed = tx
            .outbox()
            .claim(ClaimOpts {
                lease: self.opts.lease,
                // 预算耗尽的交付单元由 engine 发 budget.exhausted；排除列表在限流切片接入。
                exclude_deliveries: BTreeSet::new(),
            })
            .await?;
        let Some(claimed) = claimed else {
            tx.rollback().await?;
            return Ok(None);
        };
        // 惩罚期（FA §10.3）：该安装在到期前零请求；持久化，重启后仍遵守（forge.md §7.3）。
        if let Some(until) = self.penalty_until(&mut tx, &claimed.action)? {
            if until > tx.now() {
                tx.outbox().requeue(&claimed.action.id, until, true).await?;
                tx.commit().await?;
                return Ok(Some(Step {
                    action: claimed.action.id,
                    replay: claimed.replay,
                    outcome: StepOutcome::Requeued,
                }));
            }
        }
        tx.commit().await?;
        let action = claimed.action;
        self.crash_point(&action.id, CrashPoint::AfterClaim)?;

        let disposition = self.execute(&action).await?;
        let outcome = self.finish(&action, disposition).await?;
        Ok(Some(Step {
            action: action.id,
            replay: claimed.replay,
            outcome,
        }))
    }

    /// 反复 `run_once` 直到没有可执行的行。返回处理过的步骤。
    pub async fn drain(&self) -> Result<Vec<Step>, ExecError> {
        let mut steps = Vec::new();
        while let Some(step) = self.run_once().await? {
            steps.push(step);
        }
        Ok(steps)
    }

    fn installation_of(&self, action: &OutboxAction) -> Option<i64> {
        self.config
            .projects
            .get(&action.project_id)
            .map(|binding| binding.project.installation_id)
    }

    fn penalty_until(
        &self,
        tx: &mut WriteTx,
        action: &OutboxAction,
    ) -> Result<Option<Timestamp>, StoreError> {
        let Some(installation) = self.installation_of(action) else {
            return Ok(None);
        };
        let row: Option<RateState> = tx
            .module_conn(crate::webhook::MODULE)
            .get(RATE_TABLE, &installation.to_string())?;
        Ok(row.and_then(|row| row.penalty_until))
    }

    fn extend_penalty(
        &self,
        tx: &mut WriteTx,
        action: &OutboxAction,
        until: Timestamp,
    ) -> Result<(), StoreError> {
        let Some(installation) = self.installation_of(action) else {
            return Ok(());
        };
        let current = self.penalty_until(tx, action)?;
        if current.is_some_and(|current| current >= until) {
            return Ok(());
        }
        tx.module_conn(crate::webhook::MODULE).put(
            RATE_TABLE,
            &installation.to_string(),
            &RateState {
                penalty_until: Some(until),
            },
        )?;
        Ok(())
    }

    fn crash_point(&self, action: &ActionId, point: CrashPoint) -> Result<(), ExecError> {
        match &self.faults {
            Some(faults) if faults.crash(action, point) => Err(ExecError::Crashed(point)),
            _ => Ok(()),
        }
    }

    async fn execute(&self, action: &OutboxAction) -> Result<Disposition, ExecError> {
        let Some(binding) = self.config.projects.get(&action.project_id) else {
            return Ok(terminal(
                "permission_denied",
                "project is not bound to forge",
            ));
        };
        // 前置检查：权限仍满足（FA §9.5）。
        if !binding
            .granted
            .contains(&Permission::required_for(action.kind))
        {
            return Ok(terminal(
                "permission_denied",
                "required permission is missing",
            ));
        }
        let token = match self
            .tokens
            .token(&action.project_id, TokenPurpose::Write)
            .await
        {
            Ok(token) => token,
            // 换令牌被限流：等到期再领取，不扣尝试次数（FA §10.3）。
            Err(AuthError::RateLimited { retry_after }) => {
                return Ok(Disposition::RateLimited {
                    wait: retry_after
                        .unwrap_or(self.opts.min_rate_limit_wait)
                        .max(Duration::from_secs(1)),
                })
            }
            Err(error) => {
                return Ok(Disposition::Retry {
                    detail: format!("token: {error}"),
                    immediate: false,
                })
            }
        };
        let run = Run {
            exec: self,
            action,
            binding,
            token: &token,
        };
        let result = match &action.payload {
            ActionPayload::CreatePr(payload) => run.create_pr(payload).await,
            ActionPayload::UpdatePr(payload) => run.update_pr(payload).await,
            ActionPayload::CloseIssue(payload) => run.close_issue(payload).await,
            ActionPayload::MergePr(payload) => run.merge_pr(payload).await,
            ActionPayload::PushBranch(payload) => run.push_branch(payload).await,
            // 入队面只放行 M1 动作；其余动作出现在行里说明数据被改过，延后而不是执行。
            _ => Ok(Disposition::Defer {
                wait: Duration::from_secs(300),
            }),
        };
        match result {
            Ok(disposition) => Ok(disposition),
            Err(Flow::Crash(point)) => Err(ExecError::Crashed(point)),
            Err(Flow::Store(error)) => Err(ExecError::Store(error)),
            Err(Flow::Api(error)) => Ok(self.classify(action, error)),
        }
    }

    /// FA §2.6 失败分类。
    fn classify(&self, action: &OutboxAction, error: ApiError) -> Disposition {
        match error {
            ApiError::Transport(error) => Disposition::Retry {
                detail: format!("transport: {error}"),
                immediate: false,
            },
            ApiError::Decode(detail) => Disposition::Retry {
                detail: format!("decode: {detail}"),
                immediate: false,
            },
            ApiError::Status {
                rate_limited: true,
                retry_after,
                ..
            } => Disposition::RateLimited {
                wait: retry_after
                    .unwrap_or(self.opts.min_rate_limit_wait)
                    .max(Duration::from_secs(1)),
            },
            ApiError::Status {
                status, message, ..
            } => match status {
                401 => {
                    self.tokens.invalidate(&action.project_id);
                    Disposition::Retry {
                        detail: "token_expired".into(),
                        immediate: true,
                    }
                }
                403 => terminal("permission_denied", &message),
                404 => terminal("target_missing", &message),
                405 => terminal("not_mergeable", &message),
                409 => terminal("precondition_failed", "head_changed"),
                422 => terminal("validation_failed", &message),
                _ => Disposition::Retry {
                    detail: format!("status {status}"),
                    immediate: false,
                },
            },
        }
    }

    async fn finish(
        &self,
        action: &OutboxAction,
        disposition: Disposition,
    ) -> Result<StepOutcome, ExecError> {
        let mut tx = self.store.begin_write().await?;
        let now = tx.now();
        let outcome = match disposition {
            Disposition::Done(github_ref) => {
                // Gantry 创建 / 操作的 PR：之后的事件经此绑定到交付单元（forge.md §7.5 第 5 点）。
                if let Some(pr) = &github_ref.pr {
                    tx.module_conn(crate::webhook::MODULE).put(
                        crate::effects::EFFECT_TABLE,
                        &crate::effects::pr_key(super::payload_repo(&action.payload), pr.number),
                        &crate::effects::EffectRow {
                            action_id: action.id,
                            delivery_id: action.delivery_id,
                        },
                    )?;
                }
                if let ActionPayload::PushBranch(push) = &action.payload {
                    tx.module_conn(crate::webhook::MODULE).put(
                        crate::effects::EFFECT_TABLE,
                        &crate::effects::push_key(&push.repo, &push.branch),
                        &crate::effects::LastPush {
                            action_id: action.id,
                            delivery_id: action.delivery_id,
                            head_sha: push.local_head_oid,
                        },
                    )?;
                }
                tx.outbox().complete(&action.id, github_ref).await?;
                StepOutcome::Succeeded
            }
            Disposition::Terminal { code, detail } => {
                tx.outbox()
                    .fail(&action.id, error(code, detail), AfterFailure::Terminal)
                    .await?;
                StepOutcome::Failed { code }
            }
            Disposition::Retry { detail, immediate } => {
                if action.attempts >= self.opts.max_attempts {
                    // 重试用尽 → 终态（P-53）。
                    let code = if detail == "token_expired" {
                        "token_expired"
                    } else {
                        "transient"
                    };
                    tx.outbox()
                        .fail(&action.id, error(code, detail), AfterFailure::Terminal)
                        .await?;
                    StepOutcome::Failed { code }
                } else {
                    let delay = if immediate {
                        Duration::ZERO
                    } else {
                        self.backoff(action)
                    };
                    tx.outbox()
                        .fail(
                            &action.id,
                            error("transient", detail),
                            AfterFailure::Retry {
                                not_before: after(now, delay),
                            },
                        )
                        .await?;
                    StepOutcome::Requeued
                }
            }
            Disposition::RateLimited { wait } => {
                // 限流等待不消耗尝试次数（FA §2.6）；惩罚期落盘，作用于整个安装。
                self.extend_penalty(&mut tx, action, after(now, wait))?;
                tx.outbox()
                    .requeue(&action.id, after(now, wait), true)
                    .await?;
                StepOutcome::Requeued
            }
            Disposition::Defer { wait } => {
                tx.outbox()
                    .requeue(&action.id, after(now, wait), true)
                    .await?;
                StepOutcome::Requeued
            }
            Disposition::Cancel { detail: _ } => {
                tx.outbox()
                    .cancel(&action.id, CancelReason::ApprovalInvalid)
                    .await?;
                StepOutcome::Cancelled
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// `min(cap, base × 2^(n-1))` 加全抖动（FA §2.6）。抖动取自动作 ID 与尝试次数的哈希，
    /// 保证测试可复现。
    fn backoff(&self, action: &OutboxAction) -> Duration {
        let exp = action.attempts.saturating_sub(1).min(16);
        let ceiling = self
            .opts
            .backoff_base
            .saturating_mul(1u32 << exp)
            .min(self.opts.backoff_cap);
        let seed =
            gantry_core::canon::sha256(format!("{}:{}", action.id, action.attempts).as_bytes());
        let fraction = f64::from(seed.as_bytes()[0]) / 255.0;
        ceiling.mul_f64(fraction).max(Duration::from_millis(1))
    }

    fn ctx(&self) -> ActorCtx {
        ActorCtx {
            actor: gantry_core::entities::Actor::new(ActorKind::Forge, "forge"),
            device: None,
            role: None,
            client_profile: None,
            request_id: self
                .ids
                .next_ulid()
                .unwrap_or_else(|_| gantry_core::ids::Ulid::from_parts(0, 0).expect("zero ulid")),
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        }
    }

    async fn record_effects(
        &self,
        action: &OutboxAction,
        effects: Vec<ExpectedEffect>,
    ) -> Result<(), Flow> {
        // write-ahead：早于变更调用（FA §4.2）；归因索引与预期效果同事务（forge.md §7.3）。
        let mut tx = self.store.begin_write().await.map_err(Flow::Store)?;
        crate::effects::put_effects(
            &mut tx.module_conn(crate::webhook::MODULE),
            super::payload_repo(&action.payload),
            &effects,
            &crate::effects::EffectRow {
                action_id: action.id,
                delivery_id: action.delivery_id,
            },
        )
        .map_err(Flow::Store)?;
        tx.outbox()
            .record_effects(&action.id, effects)
            .await
            .map_err(Flow::Store)?;
        tx.commit().await.map_err(Flow::Store)?;
        self.crash(action, CrashPoint::AfterRecordEffects)
    }

    fn crash(&self, action: &OutboxAction, point: CrashPoint) -> Result<(), Flow> {
        self.crash_point(&action.id, point)
            .map_err(|_| Flow::Crash(point))
    }

    fn github_ref(&self, action: &OutboxAction, pr: Option<&PullView>) -> GithubRef {
        GithubRef {
            // App 机器人的 login 尚无来源（FA OQ-10）；只记 PR 与标记。
            bot: None,
            pr: pr.and_then(|pr| {
                Some(GithubPrRef {
                    number: pr.number,
                    node_id: pr.node_id.clone(),
                    head_sha: GitOid::parse(&pr.head_sha).ok()?,
                })
            }),
            marker: Some(format!("gantry:v1 act={}", action.id)),
            recorded_at: None,
        }
    }
}

/// 执行中的内部控制流：API 错误交给 `classify`，崩溃与 store 错误直接上抛。
enum Flow {
    Api(ApiError),
    Crash(CrashPoint),
    Store(StoreError),
}

impl From<ApiError> for Flow {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

struct Run<'a> {
    exec: &'a Executor,
    action: &'a OutboxAction,
    binding: &'a ProjectBinding,
    token: &'a SecretString,
}

impl Run<'_> {
    fn bot_id(&self) -> i64 {
        self.binding.project.bot_user_id
    }

    /// 出站净化 + 密钥扫描（FA §2.4）。命中密钥 → `validation_failed(secret_detected)`，
    /// 不写出、不重试。扫描的是净化后真正要写出的文本。
    /// 返回 `None` 表示命中密钥，调用方以 [`secret_detected`] 结束。
    fn outbound(&self, raw: &str, max_chars: usize) -> Option<String> {
        let text = sanitize::outbound(raw, self.binding.project.repo.as_str(), max_chars);
        self.exec.policy.scan_text(&text).is_empty().then_some(text)
    }

    /// 执行前消费审批（P-54）。返回 `Some` 表示不得继续。
    async fn consume(
        &self,
        approval: &gantry_core::ids::ApprovalId,
        check: &ExecutionCheck,
    ) -> Option<Disposition> {
        match self
            .exec
            .policy
            .consume_for_execution(approval, check, &self.action.id, &self.exec.ctx())
            .await
        {
            Ok(ExecutionPermit::Consumed | ExecutionPermit::AlreadyConsumedBySameAction) => None,
            Err(GateError::HeadChanged { .. }) => {
                Some(terminal("precondition_failed", "head_changed"))
            }
            Err(GateError::ApprovalInvalid { reason }) => Some(Disposition::Cancel {
                detail: format!("approval_invalid: {reason:?}"),
            }),
            Err(GateError::ConsumedByOther { by }) => Some(Disposition::Cancel {
                detail: format!("approval consumed by {by}"),
            }),
            Err(GateError::Store(error)) => Some(Disposition::Retry {
                detail: format!("policy store: {error}"),
                immediate: false,
            }),
        }
    }

    fn is_ours(&self, pr: &PullView) -> bool {
        // 标记只是索引：必须叠加作者 == bot（FA §2.4，I5）。
        pr.user_id == Some(self.bot_id()) && marker::carries_act(&pr.body, &self.action.id)
    }

    // ---- create_pr ----------------------------------------------------------

    async fn probe_create_pr(&self, p: &CreatePrPayload) -> Result<Probe, Flow> {
        let github = &self.exec.github;
        let pulls = github
            .list_pulls(self.token, &p.repo, &p.head, &p.base)
            .await?;
        if let Some(pr) = pulls.iter().find(|pr| self.is_ours(pr)) {
            return Ok(Probe::Hit(self.exec.github_ref(self.action, Some(pr))));
        }
        // 人手开的 open PR → existing_pr；旧周期遗留的 closed / merged 忽略（FA §3.2）。
        if pulls.iter().any(|pr| pr.state == "open") {
            return Ok(Probe::Conflict {
                code: "precondition_failed",
                detail: "existing_pr".into(),
            });
        }
        Ok(Probe::Miss)
    }

    async fn create_pr(&self, p: &CreatePrPayload) -> Result<Disposition, Flow> {
        if let Some(done) = settle(self.probe_create_pr(p).await?) {
            return Ok(done);
        }
        // 机械前置：expected_head_oid 等于远端分支 head（FA §3.1 #2、forge.md §7.8 表）。
        let remote = self
            .exec
            .github
            .branch_head(self.token, &p.repo, &p.head)
            .await?;
        match remote {
            None => {
                // 刚推送的分支可能有传播延迟：短重试（FA §2.6 target_missing）。
                return Ok(Disposition::Retry {
                    detail: "head branch not found".into(),
                    immediate: false,
                });
            }
            Some(sha) if sha != p.expected_head_oid.to_string() => {
                return Ok(terminal("precondition_failed", "remote_diverged"));
            }
            Some(_) => {}
        }
        let effects = vec![effect(
            "pull_request.opened",
            [("head_ref", json!(p.head)), ("base_ref", json!(p.base))],
        )];
        self.exec.record_effects(self.action, effects).await?;

        let mut body = match self.outbound(&p.body, BODY_MAX_CHARS) {
            Some(body) => body.trim_end().to_owned(),
            None => return Ok(secret_detected()),
        };
        let title = match self.outbound(&p.title, TITLE_MAX_CHARS) {
            Some(title) => title,
            None => return Ok(secret_detected()),
        };
        // `Closes #N` 只在 base 为默认分支时写，且只出现在 forge 生成的位置（FA §2.4、§3.3）。
        if p.base == self.binding.project.default_branch && !p.closes.is_empty() {
            body.push_str("\n\n");
            let lines: Vec<String> = p.closes.iter().map(|n| format!("Closes #{n}")).collect();
            body.push_str(&lines.join("\n"));
        }
        body.push_str("\n\n");
        body.push_str(&marker::render(
            &self.action.id,
            &self.action.delivery_id,
            self.action.kind,
        ));
        let request = json!({
            "title": title,
            "head": p.head,
            "base": p.base,
            "body": body,
            "draft": p.draft,
            "maintainer_can_modify": p.maintainer_can_modify,
        });
        match self
            .exec
            .github
            .create_pull(self.token, &p.repo, &request)
            .await
        {
            Ok(pr) => {
                self.exec.crash(self.action, CrashPoint::AfterApply)?;
                Ok(Disposition::Done(
                    self.exec.github_ref(self.action, Some(&pr)),
                ))
            }
            // 422"已存在"视为触发再探测（FA §3.2）。
            Err(ApiError::Status {
                status: 422,
                message,
                ..
            }) => match self.probe_create_pr(p).await? {
                Probe::Hit(github_ref) => Ok(Disposition::Done(github_ref)),
                Probe::Conflict { code, detail } => Ok(Disposition::Terminal { code, detail }),
                Probe::Miss => Ok(terminal("validation_failed", &message)),
            },
            Err(error) => Err(error.into()),
        }
    }

    // ---- push_branch --------------------------------------------------------

    async fn push_branch(&self, p: &PushBranchPayload) -> Result<Disposition, Flow> {
        let exec = self.exec;
        let project = self.action.project_id;
        let retry = |detail: String| Disposition::Retry {
            detail,
            immediate: false,
        };
        // 分支名先过 git 的 ref 规则，避免拼进 refspec 时被当成选项或多段 refspec。
        if p.branch.starts_with('-') || p.branch.contains(':') {
            return Ok(terminal("validation_failed", "invalid branch name"));
        }

        // I8 机械复核：推送的必须是 repo 在补丁校验后生成的那个提交（forge.md §7.8）。
        let record = match exec.repo.provenance(&project, &p.local_head_oid).await {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(terminal("precondition_failed", "provenance_missing")),
            Err(error) => return Ok(retry(format!("provenance: {error}"))),
        };
        if record.tree_oid != p.expected_tree_oid {
            return Ok(terminal("precondition_failed", "tree_mismatch"));
        }
        if record.patch_sha256 != p.validation_ref {
            return Ok(terminal("precondition_failed", "validation_ref_mismatch"));
        }
        let base = match p.expected_remote_oid {
            Some(expected) => expected,
            // 首次推送：父提交必须是镜像仓里的分支基线。
            None => {
                let rev = RevSpec::new(format!("refs/mirror/heads/{}", p.base_ref));
                match exec.repo.resolve(&project, &rev).await {
                    Ok(oid) => oid,
                    Err(error) => return Ok(retry(format!("resolve base: {error}"))),
                }
            }
        };
        if !p.force && record.parent != base {
            return Ok(terminal("precondition_failed", "parent_mismatch"));
        }

        // 守卫持有镜像仓共享锁（#193），活到本函数结束：diff / ls-remote / push 全程挡住 gc / 重建。
        let guard = match exec
            .repo
            .mirror_access(&exec.ctx(), &project, MirrorMode::Push)
            .await
        {
            Ok(guard) => guard,
            Err(error) => return Ok(retry(format!("mirror_access: {error}"))),
        };
        let git = |args: Vec<String>| {
            let git_dir = guard.git_dir.clone();
            async move {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                exec.git.run(&git_dir, &args, Some(self.token)).await
            }
        };
        let full_ref = format!("refs/heads/{}", p.branch);
        match git(vec!["check-ref-format".into(), full_ref.clone()]).await {
            Ok(output) if output.success => {}
            Ok(_) => return Ok(terminal("validation_failed", "invalid branch name")),
            Err(error) => return Ok(retry(error.to_string())),
        }

        // workflow 路径的机械拒绝（P-48，第二道防线）。
        let diff = match git(vec![
            "diff".into(),
            "--name-only".into(),
            "--no-renames".into(),
            base.to_string(),
            p.local_head_oid.to_string(),
        ])
        .await
        {
            Ok(output) if output.success => output.stdout,
            Ok(output) => return Ok(retry(format!("git diff: {}", first_line(&output.stderr)))),
            Err(error) => return Ok(retry(error.to_string())),
        };
        if diff
            .lines()
            .any(|path| path.starts_with(".github/workflows/"))
        {
            return Ok(terminal("validation_failed", "workflows_path"));
        }

        if let Some(approval) = &self.action.requires_approval {
            let check = ExecutionCheck::PushBranch {
                repo: p.repo.clone(),
                branch: p.branch.clone(),
                force: p.force,
                local_head_oid: p.local_head_oid,
                expected_remote_oid: p.expected_remote_oid,
                validation_ref: Some(p.validation_ref),
            };
            if let Some(stop) = self.consume(approval, &check).await {
                return Ok(stop);
            }
            exec.crash(self.action, CrashPoint::AfterConsume)?;
        }

        // probe：读远端 ref（FA §3.2）。
        let url = exec.remote_url(&p.repo);
        let remote = match git(vec!["ls-remote".into(), url.clone(), full_ref.clone()]).await {
            Ok(output) if output.success => ls_remote_sha(&output.stdout, &full_ref),
            Ok(output) => return Ok(retry(format!("ls-remote: {}", first_line(&output.stderr)))),
            Err(error) => return Ok(retry(error.to_string())),
        };
        let local = p.local_head_oid.to_string();
        let expected = p.expected_remote_oid.map(|oid| oid.to_string());
        match remote.as_deref() {
            Some(sha) if sha == local => {
                return Ok(Disposition::Done(exec.github_ref(self.action, None)))
            }
            current if current.map(str::to_owned) == expected => {}
            // 强推只核对审批里的 expected_remote_oid；不一致同样是 remote_diverged。
            _ => return Ok(terminal("precondition_failed", "remote_diverged")),
        }

        let effects = vec![effect(
            "pull_request.synchronize",
            [("branch", json!(p.branch)), ("head_sha", json!(local))],
        )];
        exec.record_effects(self.action, effects).await?;

        let mut args = vec!["push".to_owned(), "--porcelain".to_owned()];
        if p.force {
            let lease = expected.clone().unwrap_or_default();
            args.push(format!("--force-with-lease={full_ref}:{lease}"));
        }
        args.push(url);
        args.push(format!("{local}:{full_ref}"));
        let output = match git(args).await {
            Ok(output) => output,
            // 超时是未知结局：transient，重放时先 probe。
            Err(error) => return Ok(retry(error.to_string())),
        };
        if !output.success {
            let stderr = output.stderr.to_ascii_lowercase();
            if stderr.contains("non-fast-forward")
                || stderr.contains("stale info")
                || stderr.contains("fetch first")
            {
                return Ok(terminal("precondition_failed", "remote_diverged"));
            }
            if stderr.contains("signed") || stderr.contains("signature") {
                // M2 才有 api 传输的降级；M1 记为终态（FA §8）。
                return Ok(terminal(
                    "signature_required",
                    "push rejected by signed-commit rule",
                ));
            }
            return Ok(retry(format!("git push: {}", first_line(&output.stderr))));
        }
        exec.crash(self.action, CrashPoint::AfterApply)?;
        Ok(Disposition::Done(exec.github_ref(self.action, None)))
    }

    // ---- update_pr ----------------------------------------------------------

    async fn update_pr(&self, p: &UpdatePrPayload) -> Result<Disposition, Flow> {
        let github = &self.exec.github;
        let current = github.get_pull(self.token, &p.repo, p.pull_number).await?;
        let mut patch = serde_json::Map::new();
        if let Some(title) = &p.title {
            let title = match self.outbound(title, TITLE_MAX_CHARS) {
                Some(title) => title,
                None => return Ok(secret_detected()),
            };
            if current.title != title {
                patch.insert("title".into(), json!(title));
            }
        }
        if let Some(body) = &p.body {
            let wanted = match self.outbound(body, BODY_MAX_CHARS) {
                Some(body) => body.trim_end().to_owned(),
                None => return Ok(secret_detected()),
            };
            if marker::without_markers(&current.body) != wanted {
                // 保留创建时的标记，使 create_pr 的 probe 仍能认出对象。
                let mut next = wanted;
                if let Some(existing) = marker::existing_marker(&current.body) {
                    next.push_str("\n\n");
                    next.push_str(existing);
                }
                patch.insert("body".into(), json!(next));
            }
        }
        if let Some(base) = &p.base {
            if &current.base_ref != base {
                patch.insert("base".into(), json!(base));
            }
        }
        if let Some(state) = &p.state {
            let wanted = match state {
                PrState::Open => "open",
                PrState::Closed => "closed",
            };
            if current.state != wanted {
                patch.insert("state".into(), json!(wanted));
            }
        }
        // probe：逐字段相等 → 已完成（FA §3.2）。
        if patch.is_empty() {
            return Ok(Disposition::Done(
                self.exec.github_ref(self.action, Some(&current)),
            ));
        }
        let effects = vec![effect(
            "pull_request.edited",
            [
                ("pr", json!(p.pull_number)),
                ("sender", json!(self.bot_id())),
            ],
        )];
        self.exec.record_effects(self.action, effects).await?;
        let pr = github
            .update_pull(self.token, &p.repo, p.pull_number, &Value::Object(patch))
            .await?;
        self.exec.crash(self.action, CrashPoint::AfterApply)?;
        Ok(Disposition::Done(
            self.exec.github_ref(self.action, Some(&pr)),
        ))
    }

    // ---- close_issue --------------------------------------------------------

    async fn close_issue(&self, p: &CloseIssuePayload) -> Result<Disposition, Flow> {
        let github = &self.exec.github;
        let issue = github.get_issue(self.token, &p.repo, p.number).await?;
        if issue.state == "closed" {
            // 被别人关的也算已完成（FA §3.2）。`GithubRef` 还没有 closed_by_other 字段，
            // 见 #92 评论"契约缺口"。
            return Ok(Disposition::Done(self.exec.github_ref(self.action, None)));
        }
        if let Some(pr) = p.require_merged_pr {
            let pull = github.get_pull(self.token, &p.repo, pr).await?;
            if !pull.merged {
                return Ok(terminal(
                    "precondition_failed",
                    "require_merged_pr is not merged",
                ));
            }
        }
        let effects = vec![effect(
            "issues.closed",
            [("issue", json!(p.number)), ("sender", json!(self.bot_id()))],
        )];
        self.exec.record_effects(self.action, effects).await?;
        let reason = match p.state_reason {
            IssueStateReason::Completed => "completed",
            IssueStateReason::NotPlanned => "not_planned",
        };
        github
            .update_issue(
                self.token,
                &p.repo,
                p.number,
                &json!({ "state": "closed", "state_reason": reason }),
            )
            .await?;
        self.exec.crash(self.action, CrashPoint::AfterApply)?;
        Ok(Disposition::Done(self.exec.github_ref(self.action, None)))
    }

    // ---- merge_pr -----------------------------------------------------------

    async fn merge_pr(&self, p: &MergePrPayload) -> Result<Disposition, Flow> {
        let project = &self.binding.project;
        // F-OQ-18：M1 不支持启用 merge queue 的仓库。
        if project.merge_queue_enabled {
            return Ok(terminal("validation_failed", "merge_queue_unsupported"));
        }
        if !project.allowed_merge_methods.contains(&p.merge_method) {
            return Ok(terminal("validation_failed", "merge method not allowed"));
        }
        // 出站文本在消费审批之前净化并扫描：命中密钥时审批保持未消费。
        let method = serde_json::to_value(&p.merge_method).unwrap_or(Value::Null);
        let mut request = json!({
            "sha": p.expected_head_sha.to_string(),
            "merge_method": method,
        });
        if let Some(title) = &p.commit_title {
            match self.outbound(title, TITLE_MAX_CHARS) {
                Some(title) => request["commit_title"] = json!(title),
                None => return Ok(secret_detected()),
            }
        }
        if let Some(message) = &p.commit_message {
            match self.outbound(message, BODY_MAX_CHARS) {
                Some(message) => request["commit_message"] = json!(message),
                None => return Ok(secret_detected()),
            }
        }
        let github = &self.exec.github;
        // 执行前强制回读 GitHub 实时 head（forge.md §7.7、FA §2.5 #3）。
        let live = github.get_pull(self.token, &p.repo, p.pull_number).await?;
        let Ok(live_head) = GitOid::parse(&live.head_sha) else {
            return Ok(terminal(
                "validation_failed",
                "live head sha is not a git oid",
            ));
        };
        if live_head != p.expected_head_sha {
            // 不消费审批、不发变更请求。
            return Ok(terminal("precondition_failed", "head_changed"));
        }

        let Some(approval) = &self.action.requires_approval else {
            // 入队时已强制要求；行里缺审批说明数据被改过，fail-closed。
            return Ok(Disposition::Cancel {
                detail: "merge_pr without approval".into(),
            });
        };
        let check = ExecutionCheck::MergePr {
            repo: p.repo.clone(),
            number: p.pull_number,
            expected_head_sha: p.expected_head_sha,
            live_head_sha: live_head,
        };
        if let Some(stop) = self.consume(approval, &check).await {
            return Ok(stop);
        }
        self.exec.crash(self.action, CrashPoint::AfterConsume)?;

        // probe：已合并且合并前 head 等于期望 → 已完成；head 不符 → precondition_failed。
        if live.merged {
            return Ok(Disposition::Done(
                self.exec.github_ref(self.action, Some(&live)),
            ));
        }
        let effects = vec![effect(
            "pull_request.closed",
            [
                ("pr", json!(p.pull_number)),
                ("head_sha", json!(p.expected_head_sha.to_string())),
            ],
        )];
        self.exec.record_effects(self.action, effects).await?;
        // 超时是未知结局：classify 成 transient，重放时先重读 PR（上面的 probe）再决定。
        github
            .merge_pull(self.token, &p.repo, p.pull_number, &request)
            .await?;
        self.exec.crash(self.action, CrashPoint::AfterApply)?;
        Ok(Disposition::Done(
            self.exec.github_ref(self.action, Some(&live)),
        ))
    }
}

/// 只取 stderr 的第一行写进 `last_error`：git 的输出里不会有令牌（只经 askpass 注入），
/// 但仍不整段转存。
fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

fn settle(probe: Probe) -> Option<Disposition> {
    match probe {
        Probe::Hit(github_ref) => Some(Disposition::Done(github_ref)),
        Probe::Conflict { code, detail } => Some(Disposition::Terminal { code, detail }),
        Probe::Miss => None,
    }
}

fn secret_detected() -> Disposition {
    terminal("validation_failed", "secret_detected")
}

fn terminal(code: &'static str, detail: &str) -> Disposition {
    Disposition::Terminal {
        code,
        detail: detail.to_owned(),
    }
}

fn error(code: &str, message: String) -> OutboxError {
    OutboxError {
        code: code.to_owned(),
        message,
    }
}

fn after(now: Timestamp, delay: Duration) -> Timestamp {
    Timestamp::from_millis(
        now.as_millis()
            .saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX)),
    )
}

fn effect<const N: usize>(event: &str, matches: [(&str, Value); N]) -> ExpectedEffect {
    ExpectedEffect {
        event: event.to_owned(),
        match_: matches
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
        matched_webhook_deliveries: Vec::new(),
    }
}
