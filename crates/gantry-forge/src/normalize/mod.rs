//! 归一化与自身事件归因（forge.md §7.5、§7.6，FA §4.4、§6）。
//!
//! 分发器从 `forge_webhook_inbox` 取一行：标 `processing` → **回读** GitHub 当前状态 →
//! 与 forge 快照做差 → 归因 → 一个事务里追加 `forge.<kind>`、随后的 `forge.synced`、
//! 写快照、标 `done`。载荷只当提示：事件由"回读值 − 快照"产生，所以重复投递、不同 GUID
//! 的同一事实、乱序（`closed` 先于 `opened`）都收敛到同一组事件；`dedup_key` 唯一索引兜底。
//!
//! 归因（FA §4.4）：规则 1–2 查 `forge_effect_index`（不看 sender）；`sender == bot` 而
//! 索引未命中 → 停放至多 `self_event_grace`，到期记 `self_unattributed` 并追加
//! `forge.drift_detected(orphan_effect)`；其余 `external`。`check_*` 恒为 `external`（§4.6）。
//! 绑定（§7.5 第 5 点）：经 `forge_effect_index` 的 PR 绑定行或预期效果行解析 `delivery_id`；
//! 解析不到 → 停放至多 `self_event_grace`，仍未绑定则丢弃（`pass_unbound = false`）。
//!
//! shortcut（后续切片）：
//! - 回读用 REST，没有 GraphQL 的 reviews / threads / checks 汇总；快照里 `checks`、
//!   `review_state`、`in_merge_queue` 先取保守值。
//! - 只处理 `pull_request`、`pull_request_review`（触发回读）、`check_suite.completed`、
//!   `issues.closed` / `reopened`、默认分支 `push`；评审 / 评论 / `check_run` /
//!   `installation*` / `repository` 的事件在后续切片补，现在落 `done` 不产事件。
//! - Issue 只能经 `close_issue` 的预期效果绑定；`Delivery.issues` 的反查要等 store 索引。
//! - `check_suite` 的 `pull_requests` 为空时（fork 推送）不按 `head_sha` 反查 PR。

pub mod snapshot;

use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, ForgeEventKind, ForgeOrigin};
use gantry_core::events::{
    EventType, ForgeSyncedData, ForgeSyncedState, ForgeSyncedTarget, ForgeSyncedTargetKind,
    NewEvent, Subject,
};
use gantry_core::forge::{
    BaseAdvancedData, CheckConclusion, CheckSuiteCompletedData, DriftDetectedBy, DriftPayload,
    DriftResolution, DriftSynthesizedEvent, DriftTarget, ForgeActor, ForgeActorKind, ForgeApp,
    ForgeEventData, ForgeEventEnvelope, ForgeEventSchema, ForgeEventSource, ForgeEventTarget,
    ForgeTargetType, ForgeText, HeadDivergedActual, HeadDivergedDrift, HeadDivergedExpected,
    IssueClosedData, IssueReopenedData, IssueStateReason, MaybeDeliveryId, NullOnly,
    OrphanEffectActual, OrphanEffectDrift, OrphanEffectExpected, OrphanObject, PrBase,
    PrChangedField, PrClosedData, PrHead, PrMergedData, PrOpenedData, PrReopenedData,
    PrSynchronizedData, PrUpdatedData, RequiredNullable,
};
use gantry_core::ids::{ActionId, Clock, DeliveryId, EventId, ProjectId};
use gantry_core::ports::{DriftKind, ReconcileReport, ReconcileTarget};
use gantry_core::time::Timestamp;
use gantry_store::{Store, StoreError, WriteTx};
use serde_json::{json, Value};

use crate::auth::{TokenProvider, TokenPurpose};
use crate::config::{ForgeConfig, ProjectBinding};
use crate::effects::{effect_key, pr_key, push_key, EffectRow, LastPush, EFFECT_TABLE};
use crate::github::GithubClient;
use crate::webhook::{InboxRow, InboxStatus, INBOX_TABLE, MODULE};
use snapshot::{branch_key, issue_key, BranchSnap, IssueSnap, PrSnap, SNAPSHOT_TABLE};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatcherOptions {
    /// `forge.self_event_grace`：归因 / 绑定的停放上限。
    pub self_event_grace: Duration,
    /// 回读失败的重试上限；超过后 `failed`，交给对账兜底（FA §5.4）。
    pub max_attempts: u32,
    pub retry_backoff: Duration,
    pub http_timeout: Duration,
    /// `forge.events.max_text_bytes`。
    pub max_text_bytes: usize,
}

impl Default for DispatcherOptions {
    fn default() -> Self {
        Self {
            self_event_grace: Duration::from_secs(30),
            max_attempts: 5,
            retry_backoff: Duration::from_secs(2),
            http_timeout: Duration::from_secs(30),
            max_text_bytes: 65_536,
        }
    }
}

pub struct DispatcherDeps {
    pub tokens: Arc<dyn TokenProvider>,
    /// 与 store 共用的时钟（宽限期、退避）。
    pub clock: Arc<dyn Clock>,
    pub api_base: String,
    pub options: DispatcherOptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// 处理完成；`events` 为本次追加的 `forge.*` 条数（含 `forge.synced` 与 drift）。
    Done { delivery: String, events: usize },
    /// 停放等待归因 / 绑定，`retry_at` 后再处理。
    Parked { delivery: String },
    /// 丢弃（未绑定超时、项目未知等），不产事件。
    Dropped { delivery: String },
    /// 回读失败，按退避重试；用尽后由对账兜底。
    Failed { delivery: String },
}

pub struct Dispatcher {
    store: Store,
    config: Arc<ForgeConfig>,
    tokens: Arc<dyn TokenProvider>,
    clock: Arc<dyn Clock>,
    github: GithubClient,
    opts: DispatcherOptions,
    /// 对账串行化：同一时刻只有一轮在跑（`reconcile_now` 的 single-flight 近似）。
    reconcile_lock: tokio::sync::Mutex<()>,
}

/// 一条待追加的 `forge.<kind>`。
struct Candidate {
    kind: ForgeEventKind,
    dedup_key: String,
    target: ForgeEventTarget,
    actor: ForgeActor,
    occurred_at: Timestamp,
    stale: bool,
    data: ForgeEventData,
    /// 归因：规则 1–2 的效果键；`None` 表示只按 sender 判断。
    effect: Option<String>,
    /// `check_*`：恒为 external。
    always_external: bool,
}

/// 一次处理的结论（事务内决定）。只在栈上短暂存在，变体大小差异无所谓。
#[allow(clippy::large_enum_variant)]
enum Plan {
    Commit {
        events: Vec<Candidate>,
        synced: Option<ForgeSyncedData>,
        delivery: Option<DeliveryId>,
        snapshot: Option<(String, Value)>,
        orphan: Option<OrphanEffectActual>,
        drift_target: Option<DriftTarget>,
        /// head 变化且该分支有成功的 push_branch 记录（head_diverged 的依据）。
        head_basis: Option<HeadBasis>,
    },
    Park,
    Drop,
}

#[derive(Clone)]
struct HeadBasis {
    action_id: ActionId,
    pushed: GitOid,
    observed: GitOid,
}

/// 一次提交的结果：webhook 路径的处置，以及对账报告需要的事件 ID 与 drift。
struct Committed {
    outcome: Option<DispatchOutcome>,
    emitted: Vec<EventId>,
    drifts: Vec<DriftKind>,
}

/// 回读结果。
enum Readback {
    Pr(Value),
    Issue(Value),
    Branch { branch: String, sha: GitOid },
    Nothing,
}

impl Dispatcher {
    pub(crate) fn new(store: Store, config: Arc<ForgeConfig>, deps: DispatcherDeps) -> Self {
        Self {
            store,
            config,
            tokens: deps.tokens,
            clock: deps.clock,
            github: GithubClient::new(deps.api_base, deps.options.http_timeout),
            opts: deps.options,
            reconcile_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 对账（forge.md §7.7）：对目标 PR / Issue 回读，与快照做差，按 `source = reconcile`
    /// 追加事件与 `forge.synced`；外部推送导致的 head 变化追加 `drift(head_diverged)`。
    /// 同一事实与 webhook 路径的 `dedup_key` 相同，所以先对账、后到的原 webhook 不再产事件。
    ///
    /// shortcut: 巡检周期、启动全量与投递恢复（`GET /app/hook/deliveries`）的调度由装配方
    /// 按 `guardian.patrol_interval_s` 调本方法；批量与低配额降级随感知限流的 Outbox（M2）。
    pub async fn reconcile(
        &self,
        target: ReconcileTarget,
        by: DriftDetectedBy,
    ) -> Result<ReconcileReport, StoreError> {
        let _flight = self.reconcile_lock.lock().await;
        let mut report = ReconcileReport {
            emitted: Vec::new(),
            drifts: Vec::new(),
            synced_at: self.clock.now(),
        };
        for (project, number, pull) in self.reconcile_targets(&target).await? {
            let Some(binding) = self.config.projects.get(&project) else {
                continue;
            };
            let Ok(token) = self.tokens.token(&project, TokenPurpose::Read).await else {
                continue;
            };
            let readback = match self
                .github
                .issue_or_pull(&token, &binding.project.repo, number, pull)
                .await
            {
                Ok(value) if pull => Readback::Pr(value),
                Ok(value) => Readback::Issue(value),
                // 回读失败留给下一轮；不可访问的安装由 installation_unreachable 路径处理（后续）。
                Err(_) => continue,
            };
            let now = self.clock.now();
            let row = InboxRow {
                delivery_id: format!("reconcile:{project}:{number}:{}", now.as_millis()),
                event: if pull { "pull_request" } else { "issues" }.into(),
                action: None,
                installation_id: Some(binding.project.installation_id),
                repo: Some(binding.project.repo.to_string()),
                // 合成行不停放：视为宽限期已过。
                received_at: Timestamp::from_millis(
                    now.as_millis().saturating_sub(
                        i64::try_from(self.opts.self_event_grace.as_millis())
                            .unwrap_or(i64::MAX)
                            .saturating_add(1),
                    ),
                ),
                body: None,
                status: InboxStatus::Processing,
                attempts: 0,
                retry_at: None,
                last_error: None,
            };
            let committed = self
                .commit(row, Value::Null, binding, readback, Some(by))
                .await?;
            report.emitted.extend(committed.emitted);
            report.drifts.extend(committed.drifts);
        }
        report.synced_at = self.clock.now();
        Ok(report)
    }

    async fn reconcile_targets(
        &self,
        target: &ReconcileTarget,
    ) -> Result<Vec<(ProjectId, u64, bool)>, StoreError> {
        Ok(match target {
            ReconcileTarget::Pr { project, number } => vec![(*project, *number, true)],
            ReconcileTarget::Issue { project, number } => vec![(*project, *number, false)],
            ReconcileTarget::Project(project) => {
                let snapshot = self.store.snapshot().await?;
                let reader = snapshot.module_conn(MODULE);
                let mut out = Vec::new();
                for pr in scan_all::<PrSnap>(&reader, &snapshot::pr_prefix(project))? {
                    if pr.state == ForgeSyncedState::Open {
                        out.push((*project, pr.number, true));
                    }
                }
                for issue in scan_all::<IssueSnap>(&reader, &format!("issue|{project}|"))? {
                    if issue.state == ForgeSyncedState::Open {
                        out.push((*project, issue.number, false));
                    }
                }
                out
            }
            ReconcileTarget::Delivery(delivery) => {
                // 交付单元绑定的 PR：经 PR 绑定行反查（Issue 绑定要等 store 的 Delivery.issues 索引）。
                let snapshot = self.store.snapshot().await?;
                let reader = snapshot.module_conn(MODULE);
                let mut out = Vec::new();
                for binding in self.config.projects.values() {
                    let prefix = format!("pr|{}#", binding.project.repo);
                    let mut after: Option<String> = None;
                    loop {
                        let page = reader.scan::<EffectRow>(
                            EFFECT_TABLE,
                            &prefix,
                            after.as_deref(),
                            gantry_store::MODULE_SCAN_MAX,
                        )?;
                        let full = page.len() == gantry_store::MODULE_SCAN_MAX as usize;
                        for (key, row) in page {
                            if row.delivery_id == *delivery {
                                if let Some(number) =
                                    key.rsplit('#').next().and_then(|n| n.parse::<u64>().ok())
                                {
                                    out.push((binding.project.id, number, true));
                                }
                            }
                            after = Some(key);
                        }
                        if !full {
                            break;
                        }
                    }
                }
                out
            }
        })
    }

    /// 处理到没有可处理的行为止。
    pub async fn drain(&self) -> Result<Vec<DispatchOutcome>, StoreError> {
        let mut out = Vec::new();
        while let Some(outcome) = self.run_once().await? {
            out.push(outcome);
        }
        Ok(out)
    }

    /// 取一行可处理的投递（`received`，或到期的 `parked` / `failed`）并处理。
    pub async fn run_once(&self) -> Result<Option<DispatchOutcome>, StoreError> {
        let Some(mut row) = self.next_row().await? else {
            return Ok(None);
        };
        // 标 processing（崩溃后由启动扫描重新入队，§7.4 末段）。
        row.status = InboxStatus::Processing;
        row.attempts += 1;
        self.save_row(&row).await?;

        let payload: Value = row
            .body
            .as_deref()
            .and_then(|body| serde_json::from_str(body).ok())
            .unwrap_or(Value::Null);
        let Some(binding) = self.project_for(&row) else {
            return self.finish(row, InboxStatus::Dropped, None).await;
        };
        let readback = match self.readback(&row, &payload, binding).await {
            Ok(readback) => readback,
            Err(()) => return self.retry(row).await,
        };
        Ok(self
            .commit(row, payload, binding, readback, None)
            .await?
            .outcome)
    }

    async fn next_row(&self) -> Result<Option<InboxRow>, StoreError> {
        let snapshot = self.store.snapshot().await?;
        let now = self.clock.now();
        let reader = snapshot.module_conn(MODULE);
        let mut after: Option<String> = None;
        let mut best: Option<InboxRow> = None;
        loop {
            let page = reader.scan::<InboxRow>(
                INBOX_TABLE,
                "",
                after.as_deref(),
                gantry_store::MODULE_SCAN_MAX,
            )?;
            let full = page.len() == gantry_store::MODULE_SCAN_MAX as usize;
            for (key, row) in page {
                after = Some(key);
                let due = match row.status {
                    InboxStatus::Received => true,
                    InboxStatus::Parked | InboxStatus::Failed => {
                        row.retry_at.is_some_and(|at| at <= now)
                    }
                    _ => false,
                };
                if due
                    && best
                        .as_ref()
                        .is_none_or(|b| row.received_at < b.received_at)
                {
                    best = Some(row);
                }
            }
            if !full {
                break;
            }
        }
        Ok(best)
    }

    async fn save_row(&self, row: &InboxRow) -> Result<(), StoreError> {
        let mut tx = self.store.begin_write().await?;
        tx.module_conn(MODULE)
            .put(INBOX_TABLE, &row.delivery_id, row)?;
        tx.commit().await.map(|_| ())
    }

    async fn finish(
        &self,
        mut row: InboxRow,
        status: InboxStatus,
        retry_at: Option<Timestamp>,
    ) -> Result<Option<DispatchOutcome>, StoreError> {
        row.status = status;
        row.retry_at = retry_at;
        self.save_row(&row).await?;
        let delivery = row.delivery_id;
        Ok(Some(match status {
            InboxStatus::Parked => DispatchOutcome::Parked { delivery },
            InboxStatus::Failed => DispatchOutcome::Failed { delivery },
            InboxStatus::Done => DispatchOutcome::Done {
                delivery,
                events: 0,
            },
            _ => DispatchOutcome::Dropped { delivery },
        }))
    }

    async fn retry(&self, mut row: InboxRow) -> Result<Option<DispatchOutcome>, StoreError> {
        row.last_error = Some("readback_failed".into());
        let retry_at = (row.attempts < self.opts.max_attempts).then(|| {
            after(
                self.clock.now(),
                self.opts.retry_backoff * 2u32.saturating_pow(row.attempts.saturating_sub(1)),
            )
        });
        self.finish(row, InboxStatus::Failed, retry_at).await
    }

    fn project_for(&self, row: &InboxRow) -> Option<&ProjectBinding> {
        let installation = row.installation_id?;
        self.config.projects.values().find(|binding| {
            binding.project.installation_id == installation
                && row
                    .repo
                    .as_deref()
                    .is_none_or(|repo| repo == binding.project.repo.as_str())
        })
    }

    async fn readback(
        &self,
        row: &InboxRow,
        payload: &Value,
        binding: &ProjectBinding,
    ) -> Result<Readback, ()> {
        let repo = &binding.project.repo;
        let project = &binding.project.id;
        let token = self
            .tokens
            .token(project, TokenPurpose::Read)
            .await
            .map_err(|_| ())?;
        let target = match row.event.as_str() {
            "pull_request" | "pull_request_review" => payload
                .pointer("/pull_request/number")
                .and_then(Value::as_u64)
                .map(|n| (n, true)),
            "check_suite" if row.action.as_deref() == Some("completed") => payload
                .pointer("/check_suite/pull_requests/0/number")
                .and_then(Value::as_u64)
                .map(|n| (n, true)),
            "issues" if payload.pointer("/issue/pull_request").is_none() => payload
                .pointer("/issue/number")
                .and_then(Value::as_u64)
                .map(|n| (n, false)),
            "push" => {
                let branch = binding.project.default_branch.clone();
                return match self.github.branch_head(&token, repo, &branch).await {
                    Ok(Some(sha)) => GitOid::parse(&sha)
                        .map(|sha| Readback::Branch { branch, sha })
                        .map_err(|_| ()),
                    Ok(None) => Ok(Readback::Nothing),
                    Err(_) => Err(()),
                };
            }
            _ => None,
        };
        let Some((number, pull)) = target else {
            return Ok(Readback::Nothing);
        };
        match self.github.issue_or_pull(&token, repo, number, pull).await {
            Ok(value) if pull => Ok(Readback::Pr(value)),
            Ok(value) => Ok(Readback::Issue(value)),
            Err(crate::github::ApiError::Status { status: 404, .. }) => Ok(Readback::Nothing),
            Err(_) => Err(()),
        }
    }

    /// `by = None`：webhook 路径（落盘收件箱行状态）；`Some`：对账路径（行是合成的，不落盘）。
    async fn commit(
        &self,
        row: InboxRow,
        payload: Value,
        binding: &ProjectBinding,
        readback: Readback,
        by: Option<DriftDetectedBy>,
    ) -> Result<Committed, StoreError> {
        let mut tx = self.store.begin_write().await?;
        let now = tx.now();
        let source = if by.is_some() {
            ForgeEventSource::Reconcile
        } else {
            ForgeEventSource::Webhook
        };
        let mut committed = Committed {
            outcome: None,
            emitted: Vec::new(),
            drifts: Vec::new(),
        };
        let plan = match readback {
            Readback::Pr(pr) => self.plan_pr(&mut tx, &row, &payload, binding, &pr, now)?,
            Readback::Issue(issue) => {
                self.plan_issue(&mut tx, &row, &payload, binding, &issue, now)?
            }
            Readback::Branch { branch, sha } => {
                self.plan_push(&mut tx, &payload, binding, &branch, sha)?
            }
            Readback::Nothing => Plan::Commit {
                events: Vec::new(),
                synced: None,
                delivery: None,
                snapshot: None,
                orphan: None,
                drift_target: None,
                head_basis: None,
            },
        };
        let (events, synced, delivery, snapshot, orphan, drift_target, head_basis) = match plan {
            Plan::Commit {
                events,
                synced,
                delivery,
                snapshot,
                orphan,
                drift_target,
                head_basis,
            } => (
                events,
                synced,
                delivery,
                snapshot,
                orphan,
                drift_target,
                head_basis,
            ),
            Plan::Park if by.is_none() => {
                tx.rollback().await?;
                let retry_at = after(row.received_at, self.opts.self_event_grace);
                committed.outcome = self
                    .finish(row, InboxStatus::Parked, Some(retry_at))
                    .await?;
                return Ok(committed);
            }
            Plan::Park | Plan::Drop if by.is_some() => {
                tx.rollback().await?;
                return Ok(committed);
            }
            Plan::Park | Plan::Drop => {
                tx.rollback().await?;
                committed.outcome = self.finish(row, InboxStatus::Dropped, None).await?;
                return Ok(committed);
            }
        };

        let project = binding.project.id;
        let mut appended = 0;
        let mut needs_drift = false;
        let mut head_drift = None;
        for candidate in events {
            let (origin, caused_by) = self.attribute(&mut tx, &candidate, &payload, binding)?;
            needs_drift |= origin == ForgeOrigin::OwnUnattributed;
            if candidate.kind == ForgeEventKind::PrSynchronized && origin == ForgeOrigin::External {
                head_drift = head_basis.clone();
            }
            let envelope = ForgeEventEnvelope {
                schema: ForgeEventSchema,
                kind: candidate.kind,
                dedup_key: candidate.dedup_key.clone(),
                source,
                delivery_ids: if by.is_some() {
                    Vec::new()
                } else {
                    vec![row.delivery_id.clone()]
                },
                project_id: project,
                repo: binding.project.repo.clone(),
                installation_id: u64::try_from(binding.project.installation_id).unwrap_or(0),
                delivery_id: delivery,
                target: candidate.target,
                actor: candidate.actor,
                origin,
                caused_by_action: caused_by,
                occurred_at: candidate.occurred_at,
                observed_at: now,
                stale: candidate.stale,
                data: candidate.data,
            };
            let appended_event = tx
                .append(NewEvent {
                    ty: EventType::Forge(candidate.kind),
                    subject: project_subject(&project)?,
                    project_id: Some(project),
                    actor: forge_actor(),
                    causation_id: None,
                    correlation_id: delivery.map(|id| id.to_string()),
                    data: serde_json::to_value(&envelope)
                        .map_err(|error| StoreError::ModuleValue(error.to_string()))?,
                    dedup_key: Some(candidate.dedup_key),
                })
                .await?;
            if let gantry_store::Appended::New(event) = appended_event {
                appended += 1;
                committed.emitted.push(event.id);
            }
        }
        // 人推了提交（head 与最近一次成功的 push_branch 不同，且非自身事件）→ head_diverged（§7.7）。
        if let (Some(basis), Some(delivery), Some(target)) =
            (head_drift, delivery, drift_target.clone())
        {
            let drift = DriftPayload::HeadDiverged(HeadDivergedDrift {
                delivery_id: delivery,
                target,
                expected: HeadDivergedExpected {
                    head_sha: basis.pushed,
                    basis_action: basis.action_id,
                },
                actual: HeadDivergedActual {
                    head_sha: basis.observed,
                    // webhook 路径取 sender；对账回读拿不到推送者，记为 unknown（不编造身份）。
                    pushed_by: RequiredNullable::Value(actor_of(payload.get("sender"))),
                },
                resolution: DriftResolution::GithubWins,
                synthesized_events: if by.is_some() {
                    vec![DriftSynthesizedEvent::PrSynchronized]
                } else {
                    Vec::new()
                },
                detected_by: by.unwrap_or(DriftDetectedBy::Webhook),
            });
            if let gantry_store::Appended::New(event) = tx
                .append(NewEvent {
                    ty: EventType::ForgeDriftDetected,
                    subject: project_subject(&project)?,
                    project_id: Some(project),
                    actor: forge_actor(),
                    causation_id: None,
                    correlation_id: Some(delivery.to_string()),
                    data: serde_json::to_value(&drift)
                        .map_err(|error| StoreError::ModuleValue(error.to_string()))?,
                    dedup_key: Some(format!(
                        "drift:head:{}:{}",
                        binding.project.repo, basis.observed
                    )),
                })
                .await?
            {
                appended += 1;
                committed.emitted.push(event.id);
                committed.drifts.push(DriftKind::HeadDiverged);
            }
        }
        if needs_drift {
            if let (Some(actual), Some(target)) = (orphan, drift_target) {
                let drift = DriftPayload::OrphanEffect(OrphanEffectDrift {
                    delivery_id: delivery.map_or(MaybeDeliveryId::Null, MaybeDeliveryId::Delivery),
                    target,
                    expected: OrphanEffectExpected {
                        outbox_action: NullOnly,
                    },
                    actual,
                    resolution: DriftResolution::GithubWins,
                    synthesized_events: Vec::new(),
                    detected_by: by.unwrap_or(DriftDetectedBy::Webhook),
                });
                tx.append(NewEvent {
                    ty: EventType::ForgeDriftDetected,
                    subject: project_subject(&project)?,
                    project_id: Some(project),
                    actor: forge_actor(),
                    causation_id: None,
                    correlation_id: delivery.map(|id| id.to_string()),
                    data: serde_json::to_value(&drift)
                        .map_err(|error| StoreError::ModuleValue(error.to_string()))?,
                    dedup_key: Some(format!("drift:orphan:{}", row.delivery_id)),
                })
                .await?;
                appended += 1;
                committed.drifts.push(DriftKind::OrphanEffect);
            }
        }
        if let Some(mut synced) = synced {
            synced.source = source;
            let appended_event = tx
                .append(NewEvent {
                    ty: EventType::ForgeSynced,
                    subject: project_subject(&project)?,
                    project_id: Some(project),
                    actor: forge_actor(),
                    causation_id: None,
                    correlation_id: delivery.map(|id| id.to_string()),
                    data: serde_json::to_value(&synced)
                        .map_err(|error| StoreError::ModuleValue(error.to_string()))?,
                    dedup_key: None,
                })
                .await?;
            if let gantry_store::Appended::New(event) = appended_event {
                committed.emitted.push(event.id);
            }
            appended += 1;
        }
        if let Some((key, value)) = snapshot {
            tx.module_conn(MODULE).put(SNAPSHOT_TABLE, &key, &value)?;
        }
        let mut row = row;
        if by.is_none() {
            row.status = InboxStatus::Done;
            row.retry_at = None;
            tx.module_conn(MODULE)
                .put(INBOX_TABLE, &row.delivery_id, &row)?;
        }
        tx.commit().await?;
        committed.outcome = Some(DispatchOutcome::Done {
            delivery: row.delivery_id,
            events: appended,
        });
        Ok(committed)
    }

    /// 绑定：PR 绑定行 → 预期效果行。都没有 → 停放 / 丢弃。
    fn bind(
        &self,
        tx: &mut WriteTx,
        repo: &RepoSlug,
        number: Option<u64>,
        effects: &[String],
    ) -> Result<Option<EffectRow>, StoreError> {
        let conn = tx.module_conn(MODULE);
        if let Some(number) = number {
            if let Some(row) = conn.get::<EffectRow>(EFFECT_TABLE, &pr_key(repo, number))? {
                return Ok(Some(row));
            }
        }
        for key in effects {
            if let Some(row) = conn.get::<EffectRow>(EFFECT_TABLE, key)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn within_grace(&self, row: &InboxRow, now: Timestamp) -> bool {
        now < after(row.received_at, self.opts.self_event_grace)
    }

    /// FA §4.4 规则 1–5（停放已在计划阶段处理）。
    fn attribute(
        &self,
        tx: &mut WriteTx,
        candidate: &Candidate,
        payload: &Value,
        binding: &ProjectBinding,
    ) -> Result<(ForgeOrigin, Option<ActionId>), StoreError> {
        if candidate.always_external {
            return Ok((ForgeOrigin::External, None));
        }
        if let Some(key) = &candidate.effect {
            if let Some(row) = tx.module_conn(MODULE).get::<EffectRow>(EFFECT_TABLE, key)? {
                return Ok((ForgeOrigin::Own, Some(row.action_id)));
            }
        }
        if sender_id(payload) == Some(binding.project.bot_user_id) {
            return Ok((ForgeOrigin::OwnUnattributed, None));
        }
        Ok((ForgeOrigin::External, None))
    }

    /// bot 发起但索引未命中、仍在宽限期内 → 停放（规则 3）。
    fn should_park_for_attribution(
        &self,
        tx: &mut WriteTx,
        events: &[Candidate],
        payload: &Value,
        binding: &ProjectBinding,
        row: &InboxRow,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        if sender_id(payload) != Some(binding.project.bot_user_id) || !self.within_grace(row, now) {
            return Ok(false);
        }
        for candidate in events.iter().filter(|c| !c.always_external) {
            let hit = match &candidate.effect {
                Some(key) => tx
                    .module_conn(MODULE)
                    .get::<EffectRow>(EFFECT_TABLE, key)?
                    .is_some(),
                None => false,
            };
            if !hit {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[allow(clippy::too_many_lines)]
    fn plan_pr(
        &self,
        tx: &mut WriteTx,
        row: &InboxRow,
        payload: &Value,
        binding: &ProjectBinding,
        pr: &Value,
        now: Timestamp,
    ) -> Result<Plan, StoreError> {
        let repo = &binding.project.repo;
        let project = binding.project.id;
        let Some(next) = pr_snap(pr) else {
            return Ok(Plan::Drop);
        };
        let n = next.number;
        let key = snapshot::pr_key(&project, n);
        let previous: Option<PrSnap> = tx.module_conn(MODULE).get(SNAPSHOT_TABLE, &key)?;
        let opened_effect = effect_key(
            repo,
            "pull_request.opened",
            &matches([
                ("base_ref", json!(next.base_ref)),
                ("head_ref", json!(next.head_ref)),
            ]),
        );
        let Some(bound) = self.bind(tx, repo, Some(n), std::slice::from_ref(&opened_effect))?
        else {
            return Ok(if self.within_grace(row, now) {
                Plan::Park
            } else {
                Plan::Drop
            });
        };

        let target = ForgeEventTarget {
            kind: ForgeTargetType::Pr,
            number: Some(n),
            node_id: Some(next.node_id.clone()),
        };
        let sender = actor_of(payload.get("sender"));
        let author = actor_of(pr.get("user"));
        let raw = |field: &str| pr.get(field).and_then(Value::as_str).unwrap_or_default();
        let when = |field: &str| Timestamp::parse_rfc3339(raw(field)).unwrap_or(row.received_at);
        let mut events = Vec::new();
        let mut next = next;

        let candidate =
            |kind, dedup_key: String, occurred_at, data, effect: Option<String>, actor| Candidate {
                kind,
                dedup_key,
                target: target.clone(),
                actor,
                occurred_at,
                stale: false,
                data,
                effect,
                always_external: false,
            };

        match &previous {
            None => {
                next.head_epoch = 0;
                events.push(candidate(
                    ForgeEventKind::PrOpened,
                    format!("pr_opened:{repo}:{n}:{}", raw("created_at")),
                    when("created_at"),
                    ForgeEventData::PrOpened(PrOpenedData {
                        number: n,
                        node_id: next.node_id.clone(),
                        title: limited(&next.title, "github:pr.title", self.opts.max_text_bytes),
                        body: limited(&next.body, "github:pr.body", self.opts.max_text_bytes),
                        head: PrHead {
                            ref_name: next.head_ref.clone(),
                            sha: next.head_sha,
                            repo: head_repo(pr).unwrap_or_else(|| repo.clone()),
                        },
                        base: PrBase {
                            ref_name: next.base_ref.clone(),
                        },
                        draft: next.draft,
                        labels: next.labels.clone(),
                        author: author.clone(),
                        // shortcut: closing_issues 要 GraphQL closingIssuesReferences（FA OQ-9）。
                        closing_issues: Vec::new(),
                        created_by_gantry: false,
                    }),
                    Some(opened_effect.clone()),
                    author.clone(),
                ));
            }
            Some(prev) => {
                next.head_epoch = prev.head_epoch;
                if prev.head_sha != next.head_sha {
                    next.head_epoch = prev.head_epoch + 1;
                    events.push(candidate(
                        ForgeEventKind::PrSynchronized,
                        format!("pr_sync:{repo}:{n}:{}", next.head_epoch),
                        when("updated_at"),
                        ForgeEventData::PrSynchronized(PrSynchronizedData {
                            head_sha: next.head_sha,
                            previous_head_sha: prev.head_sha,
                            head_epoch: next.head_epoch,
                            forced: None,
                            base_ref: next.base_ref.clone(),
                            commits_added: None,
                        }),
                        Some(effect_key(
                            repo,
                            "pull_request.synchronize",
                            &matches([
                                ("branch", json!(next.head_ref)),
                                ("head_sha", json!(next.head_sha.to_string())),
                            ]),
                        )),
                        sender.clone(),
                    ));
                }
                if prev.state != ForgeSyncedState::Open && next.state == ForgeSyncedState::Open {
                    events.push(candidate(
                        ForgeEventKind::PrReopened,
                        format!("pr_reopened:{repo}:{n}:{}", raw("updated_at")),
                        when("updated_at"),
                        ForgeEventData::PrReopened(PrReopenedData {
                            number: n,
                            head_sha: next.head_sha,
                            base_ref: next.base_ref.clone(),
                        }),
                        None,
                        sender.clone(),
                    ));
                }
                let mut changed = Vec::new();
                if prev.title != next.title {
                    changed.push(PrChangedField::Title);
                }
                if prev.body != next.body {
                    changed.push(PrChangedField::Body);
                }
                if prev.base_ref != next.base_ref {
                    changed.push(PrChangedField::Base);
                }
                if prev.draft != next.draft {
                    changed.push(PrChangedField::Draft);
                }
                if prev.labels != next.labels {
                    changed.push(PrChangedField::Labels);
                }
                if !changed.is_empty() {
                    events.push(candidate(
                        ForgeEventKind::PrUpdated,
                        format!("pr_upd:{repo}:{n}:{}", raw("updated_at")),
                        when("updated_at"),
                        ForgeEventData::PrUpdated(PrUpdatedData { changed }),
                        Some(effect_key(
                            repo,
                            "pull_request.edited",
                            &matches([
                                ("pr", json!(n)),
                                ("sender", json!(binding.project.bot_user_id)),
                            ]),
                        )),
                        sender.clone(),
                    ));
                }
            }
        }
        // 关闭 / 合并：首次看到即已关闭时，紧跟在 pr_opened 之后（§7.5 差异表第 1 行）。
        let was_open = previous
            .as_ref()
            .is_none_or(|prev| prev.state == ForgeSyncedState::Open);
        if was_open && next.state == ForgeSyncedState::Closed {
            events.push(candidate(
                ForgeEventKind::PrClosed,
                format!("pr_closed:{repo}:{n}:{}", raw("closed_at")),
                when("closed_at"),
                ForgeEventData::PrClosed(PrClosedData {
                    head_sha: next.head_sha,
                    closed_by: sender.clone(),
                }),
                None,
                sender.clone(),
            ));
        }
        if previous
            .as_ref()
            .is_none_or(|prev| prev.state != ForgeSyncedState::Merged)
            && next.state == ForgeSyncedState::Merged
        {
            let merge_commit = pr
                .get("merge_commit_sha")
                .and_then(Value::as_str)
                .and_then(|sha| GitOid::parse(sha).ok());
            if let Some(merge_commit) = merge_commit {
                let merged_by = pr
                    .get("merged_by")
                    .filter(|value| !value.is_null())
                    .map_or_else(|| sender.clone(), |actor| actor_of(Some(actor)));
                events.push(candidate(
                    ForgeEventKind::PrMerged,
                    format!("pr_merged:{repo}:{n}:{merge_commit}"),
                    when("merged_at"),
                    ForgeEventData::PrMerged(PrMergedData {
                        merge_commit_sha: merge_commit,
                        head_sha: next.head_sha,
                        base_ref: next.base_ref.clone(),
                        merged_by: merged_by.clone(),
                        via_merge_queue: false,
                        merge_method: None,
                    }),
                    Some(effect_key(
                        repo,
                        "pull_request.closed",
                        &matches([
                            ("head_sha", json!(next.head_sha.to_string())),
                            ("pr", json!(n)),
                        ]),
                    )),
                    merged_by,
                ));
            }
        }
        if row.event == "check_suite" {
            if let Some(event) = check_suite(repo, payload, &target, &next, row) {
                events.push(event);
            }
        }
        if self.should_park_for_attribution(tx, &events, payload, binding, row, now)? {
            return Ok(Plan::Park);
        }
        // created_by_gantry：pr_opened 的预期效果命中即是。
        let own_open = tx
            .module_conn(MODULE)
            .get::<EffectRow>(EFFECT_TABLE, &opened_effect)?
            .is_some();
        for event in &mut events {
            if let ForgeEventData::PrOpened(data) = &mut event.data {
                data.created_by_gantry = own_open;
            }
        }

        let head_basis = match &previous {
            Some(prev) if prev.head_sha != next.head_sha => tx
                .module_conn(MODULE)
                .get::<LastPush>(EFFECT_TABLE, &push_key(repo, &next.head_ref))?
                .filter(|push| push.head_sha != next.head_sha)
                .map(|push| HeadBasis {
                    action_id: push.action_id,
                    pushed: push.head_sha,
                    observed: next.head_sha,
                }),
            _ => None,
        };
        let synced = synced_for(
            ForgeSyncedTargetKind::Pr,
            n,
            &next.node_id,
            now,
            match &previous {
                None => SyncedDelta::Initial(next.initial()),
                Some(prev) => SyncedDelta::Changes(prev.changes(&next)),
            },
        );
        let orphan = Some(OrphanEffectActual {
            object: OrphanObject::PullRequest,
            github_id: pr.get("id").and_then(Value::as_u64).unwrap_or(n),
            node_id: next.node_id.clone(),
            marker: ForgeText::new(marker_of(&next.body), "github:pr.body", false),
        });
        let snapshot = serde_json::to_value(&next).ok().map(|value| (key, value));
        Ok(Plan::Commit {
            events,
            synced,
            delivery: Some(bound.delivery_id),
            snapshot,
            orphan,
            drift_target: Some(DriftTarget::Pr {
                repo: repo.clone(),
                number: n,
            }),
            head_basis,
        })
    }

    fn plan_issue(
        &self,
        tx: &mut WriteTx,
        row: &InboxRow,
        payload: &Value,
        binding: &ProjectBinding,
        issue: &Value,
        now: Timestamp,
    ) -> Result<Plan, StoreError> {
        let repo = &binding.project.repo;
        let project = binding.project.id;
        let Some(next) = issue_snap(issue) else {
            return Ok(Plan::Drop);
        };
        let n = next.number;
        let key = issue_key(&project, n);
        let previous: Option<IssueSnap> = tx.module_conn(MODULE).get(SNAPSHOT_TABLE, &key)?;
        let closed_effect = effect_key(
            repo,
            "issues.closed",
            &matches([
                ("issue", json!(n)),
                ("sender", json!(binding.project.bot_user_id)),
            ]),
        );
        let Some(bound) = self.bind(tx, repo, None, std::slice::from_ref(&closed_effect))? else {
            return Ok(if self.within_grace(row, now) {
                Plan::Park
            } else {
                Plan::Drop
            });
        };
        let raw = |field: &str| issue.get(field).and_then(Value::as_str).unwrap_or_default();
        let when = |field: &str| Timestamp::parse_rfc3339(raw(field)).unwrap_or(row.received_at);
        let target = ForgeEventTarget {
            kind: ForgeTargetType::Issue,
            number: Some(n),
            node_id: Some(next.node_id.clone()),
        };
        let sender = actor_of(payload.get("sender"));
        let was = previous
            .as_ref()
            .map_or(ForgeSyncedState::Open, |prev| prev.state);
        let mut events = Vec::new();
        if was == ForgeSyncedState::Open && next.state == ForgeSyncedState::Closed {
            events.push(Candidate {
                kind: ForgeEventKind::IssueClosed,
                dedup_key: format!("issue_closed:{repo}:{n}:{}", raw("closed_at")),
                target: target.clone(),
                actor: sender.clone(),
                occurred_at: when("closed_at"),
                stale: false,
                data: ForgeEventData::IssueClosed(IssueClosedData {
                    number: n,
                    state_reason: issue
                        .get("state_reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    closed_by: sender.clone(),
                    closed_via_pr: None,
                }),
                effect: Some(closed_effect.clone()),
                always_external: false,
            });
        }
        if previous.is_some()
            && was == ForgeSyncedState::Closed
            && next.state == ForgeSyncedState::Open
        {
            events.push(Candidate {
                kind: ForgeEventKind::IssueReopened,
                dedup_key: format!("issue_reopened:{repo}:{n}:{}", raw("updated_at")),
                target: target.clone(),
                actor: sender.clone(),
                occurred_at: when("updated_at"),
                stale: false,
                data: ForgeEventData::IssueReopened(IssueReopenedData { number: n }),
                effect: None,
                always_external: false,
            });
        }
        if self.should_park_for_attribution(tx, &events, payload, binding, row, now)? {
            return Ok(Plan::Park);
        }
        let synced = synced_for(
            ForgeSyncedTargetKind::Issue,
            n,
            &next.node_id,
            now,
            match &previous {
                None => SyncedDelta::Initial(next.initial()),
                Some(prev) => SyncedDelta::Changes(prev.changes(&next)),
            },
        );
        let snapshot = serde_json::to_value(&next).ok().map(|value| (key, value));
        Ok(Plan::Commit {
            events,
            synced,
            delivery: Some(bound.delivery_id),
            snapshot,
            orphan: Some(OrphanEffectActual {
                object: OrphanObject::Issue,
                github_id: issue.get("id").and_then(Value::as_u64).unwrap_or(n),
                node_id: next.node_id.clone(),
                marker: ForgeText::new(String::new(), "github:issue.body", false),
            }),
            drift_target: Some(DriftTarget::Issue {
                repo: repo.clone(),
                number: n,
            }),
            head_basis: None,
        })
    }

    /// 默认分支前移 → `base_advanced`（项目级，不绑定交付单元）。
    fn plan_push(
        &self,
        tx: &mut WriteTx,
        payload: &Value,
        binding: &ProjectBinding,
        branch: &str,
        sha: GitOid,
    ) -> Result<Plan, StoreError> {
        let repo = &binding.project.repo;
        let project = binding.project.id;
        let key = branch_key(&project, branch);
        let previous: Option<BranchSnap> = tx.module_conn(MODULE).get(SNAPSHOT_TABLE, &key)?;
        let before = previous.as_ref().map(|prev| prev.sha).or_else(|| {
            payload
                .get("before")
                .and_then(Value::as_str)
                .and_then(|sha| GitOid::parse(sha).ok())
        });
        let snapshot = serde_json::to_value(BranchSnap { sha })
            .ok()
            .map(|value| (key, value));
        let mut events = Vec::new();
        if let Some(before) = before.filter(|before| *before != sha) {
            let mut affected = Vec::new();
            let conn = tx.module_conn(MODULE);
            let mut after_key: Option<String> = None;
            loop {
                let page = conn.scan::<PrSnap>(
                    SNAPSHOT_TABLE,
                    &snapshot::pr_prefix(&project),
                    after_key.as_deref(),
                    gantry_store::MODULE_SCAN_MAX,
                )?;
                let full = page.len() == gantry_store::MODULE_SCAN_MAX as usize;
                for (key, pr) in page {
                    after_key = Some(key);
                    if pr.state == ForgeSyncedState::Open && pr.base_ref == branch {
                        affected.push(pr.number);
                    }
                }
                if !full {
                    break;
                }
            }
            events.push(Candidate {
                kind: ForgeEventKind::BaseAdvanced,
                dedup_key: format!("base:{repo}:{branch}:{sha}"),
                target: ForgeEventTarget {
                    kind: ForgeTargetType::Branch,
                    number: None,
                    node_id: None,
                },
                actor: actor_of(payload.get("sender")),
                occurred_at: payload
                    .pointer("/head_commit/timestamp")
                    .and_then(Value::as_str)
                    .and_then(|raw| Timestamp::parse_rfc3339(raw).ok())
                    .unwrap_or_else(|| self.clock.now()),
                stale: false,
                data: ForgeEventData::BaseAdvanced(BaseAdvancedData {
                    base_ref: branch.to_owned(),
                    before,
                    after: sha,
                    affected_prs: affected,
                }),
                effect: None,
                // base 前移不论谁推，引擎都要重验；不按 sender 归因、也不停放。
                always_external: true,
            });
        }
        Ok(Plan::Commit {
            events,
            synced: None,
            delivery: None,
            snapshot,
            orphan: None,
            drift_target: None,
            head_basis: None,
        })
    }
}

fn scan_all<T: serde::de::DeserializeOwned>(
    reader: &gantry_store::ModuleReader<'_>,
    prefix: &str,
) -> Result<Vec<T>, StoreError> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = reader.scan::<T>(
            SNAPSHOT_TABLE,
            prefix,
            after.as_deref(),
            gantry_store::MODULE_SCAN_MAX,
        )?;
        let full = page.len() == gantry_store::MODULE_SCAN_MAX as usize;
        for (key, value) in page {
            after = Some(key);
            out.push(value);
        }
        if !full {
            break;
        }
    }
    Ok(out)
}

enum SyncedDelta {
    Initial(gantry_core::events::ForgeSyncedInitial),
    Changes(Vec<gantry_core::events::ForgeSyncedChange>),
}

fn synced_for(
    kind: ForgeSyncedTargetKind,
    number: u64,
    node_id: &str,
    now: Timestamp,
    delta: SyncedDelta,
) -> Option<ForgeSyncedData> {
    let (changes, initial) = match delta {
        SyncedDelta::Initial(initial) => (Vec::new(), Some(initial)),
        SyncedDelta::Changes(changes) if changes.is_empty() => return None,
        SyncedDelta::Changes(changes) => (changes, None),
    };
    Some(ForgeSyncedData {
        target: ForgeSyncedTarget {
            kind,
            number,
            node_id: node_id.to_owned(),
        },
        source: ForgeEventSource::Webhook,
        changes,
        synced_at: now,
        initial,
    })
}

fn check_suite(
    _repo: &RepoSlug,
    payload: &Value,
    target: &ForgeEventTarget,
    current: &PrSnap,
    row: &InboxRow,
) -> Option<Candidate> {
    let suite = payload.get("check_suite")?;
    let id = suite.get("id")?.as_u64()?;
    let head_sha = GitOid::parse(suite.get("head_sha")?.as_str()?).ok()?;
    let conclusion: CheckConclusion =
        serde_json::from_value(suite.get("conclusion")?.clone()).ok()?;
    let updated_at = suite
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let repo = row.repo.clone().unwrap_or_default();
    let is_current_head = head_sha == current.head_sha;
    let app = suite.get("app");
    Some(Candidate {
        kind: ForgeEventKind::CheckSuiteCompleted,
        dedup_key: format!("check_suite:{repo}:{id}:{updated_at}"),
        target: target.clone(),
        actor: actor_of(app.and_then(|app| app.get("owner"))),
        occurred_at: Timestamp::parse_rfc3339(updated_at).unwrap_or(row.received_at),
        // G4：非当前 head 的 check 事件标 stale。
        stale: !is_current_head,
        data: ForgeEventData::CheckSuiteCompleted(CheckSuiteCompletedData {
            check_suite_id: id,
            head_sha,
            conclusion,
            app: ForgeApp {
                id: app
                    .and_then(|app| app.get("id"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                slug: app
                    .and_then(|app| app.get("slug"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            },
            pr_numbers: suite
                .get("pull_requests")
                .and_then(Value::as_array)
                .map(|prs| {
                    prs.iter()
                        .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
                        .collect()
                })
                .unwrap_or_default(),
            is_current_head,
            // shortcut: workflow_runs / failed_check_runs 由 Actions: read 回读补充（M2）。
            workflow_runs: Vec::new(),
            failed_check_runs: Vec::new(),
        }),
        effect: None,
        always_external: true,
    })
}

fn pr_snap(pr: &Value) -> Option<PrSnap> {
    let str_at = |pointer: &str| pr.pointer(pointer).and_then(Value::as_str);
    let state = if pr.get("merged").and_then(Value::as_bool) == Some(true) {
        ForgeSyncedState::Merged
    } else if str_at("/state") == Some("closed") {
        ForgeSyncedState::Closed
    } else {
        ForgeSyncedState::Open
    };
    let head_sha = GitOid::parse(str_at("/head/sha")?).ok()?;
    Some(PrSnap {
        number: pr.get("number")?.as_u64()?,
        node_id: str_at("/node_id").unwrap_or_default().to_owned(),
        state,
        title: str_at("/title").unwrap_or_default().to_owned(),
        body: str_at("/body").unwrap_or_default().to_owned(),
        head_ref: str_at("/head/ref")?.to_owned(),
        head_sha,
        base_ref: str_at("/base/ref")?.to_owned(),
        // REST 的 base.sha 缺失时（替身）退到 head：只影响快照，不进事件 data。
        base_sha: str_at("/base/sha")
            .and_then(|sha| GitOid::parse(sha).ok())
            .unwrap_or(head_sha),
        draft: pr.get("draft").and_then(Value::as_bool).unwrap_or(false),
        labels: pr
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|label| label.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        mergeable_state: str_at("/mergeable_state")
            .filter(|state| !state.is_empty())
            .unwrap_or("unknown")
            .to_owned(),
        head_epoch: 0,
    })
}

fn issue_snap(issue: &Value) -> Option<IssueSnap> {
    let state = match issue.get("state").and_then(Value::as_str)? {
        "closed" => ForgeSyncedState::Closed,
        _ => ForgeSyncedState::Open,
    };
    Some(IssueSnap {
        number: issue.get("number")?.as_u64()?,
        node_id: issue
            .get("node_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        state,
        state_reason: issue
            .get("state_reason")
            .and_then(|reason| serde_json::from_value::<IssueStateReason>(reason.clone()).ok()),
        title: issue
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

/// 写入事件日志前按 `forge.events.max_text_bytes` 截断（§7.5 第 3 点）。
fn limited(text: &str, source: &str, max_bytes: usize) -> ForgeText {
    if text.len() <= max_bytes {
        return ForgeText::new(text, source, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    ForgeText::new(&text[..end], source, true)
}

fn head_repo(pr: &Value) -> Option<RepoSlug> {
    RepoSlug::parse(pr.pointer("/head/repo/full_name")?.as_str()?).ok()
}

fn actor_of(value: Option<&Value>) -> ForgeActor {
    let value = value.unwrap_or(&Value::Null);
    let kind = match value.get("type").and_then(Value::as_str) {
        Some("User") => ForgeActorKind::User,
        Some("Bot") => ForgeActorKind::Bot,
        Some("Organization") | Some("App") => ForgeActorKind::App,
        _ => ForgeActorKind::Unknown,
    };
    ForgeActor {
        login: value
            .get("login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        github_id: value.get("id").and_then(Value::as_u64).unwrap_or(0),
        kind,
        association: None,
    }
}

fn sender_id(payload: &Value) -> Option<i64> {
    payload.pointer("/sender/id").and_then(Value::as_i64)
}

fn marker_of(body: &str) -> String {
    body.lines()
        .find(|line| line.trim_start().starts_with("<!-- gantry:"))
        .unwrap_or_default()
        .to_owned()
}

fn matches<const N: usize>(pairs: [(&str, Value); N]) -> std::collections::BTreeMap<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn forge_actor() -> Actor {
    Actor::new(ActorKind::Forge, "forge")
}

fn project_subject(project: &ProjectId) -> Result<Subject, StoreError> {
    Subject::parse(&project.to_string()).map_err(|error| StoreError::ModuleValue(error.to_string()))
}

fn after(at: Timestamp, delay: Duration) -> Timestamp {
    Timestamp::from_millis(
        at.as_millis()
            .saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX)),
    )
}
