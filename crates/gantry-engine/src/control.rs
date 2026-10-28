//! 控制命令与异常路径的决策（engine.md §7.6 T11 / T13 / T14 / T15、§7.11、§7.14.5，#73）。
//!
//! 与 [`crate::decider`] 一样是纯函数：不做 IO、不读时钟、ID 取自传入的 `IdMint`。
//!
//! - 命令（§7.14.2 `Command`）：`SetLevelCap` / `Pause` / `Resume` / `Abandon` / `RetryNode`；
//!   M2 的 `SupplySpec` / `RequestHeal` / `Classify` 回 `NotSupportedYet`。
//!   命令追加的第 1 条业务事件 `dedup_key = ctx.dedup_key`，第 n 条（n ≥ 2）为 `"<dedup_key>:<n>"`
//!   （与 api 泳道约定，#99）；`ctx.dedup_key` 为空时不带。
//! - 事件驱动的迁移：T11（`approval.invalidated` / `expired`、`forge.pr_synchronized`、
//!   `forge.base_advanced`）。触发事件只出现一次，所以按事件求值；重放靠
//!   "事件 `seq` 晚于 `stage_entered`"这一前置条件消化。
//! - 按状态求值（由 `derive` 调用）：T14、T15、放弃收尾、base 前移后的对账、
//!   `models_not_configured` 的上报与关闭。
//! - 上报项选项的执行（§7.11"关闭与执行"）：`inbox.resolved(chosen, option_id)`。

use std::collections::BTreeSet;

use gantry_core::entities::{
    ApprovalAction, ApprovalStatus, BudgetDimension, ForgeActionKind, InboxKind, InboxPayload,
    NodeStatus, OutboxStatus, PullRequestState, ReasonCode, Severity, Stage, StageStatus,
    WorkflowStatus,
};
use gantry_core::events::{
    BudgetLimitRaisedData, DeliveryAbandonedData, DeliveryLevelChangedData, DeliveryPausedData,
    DeliveryResumedData, EventType, InboxResolution, LevelChangeKind, NodeRetryRequestedData,
    WorkflowFinishedData,
};
use gantry_core::forge::{BaseAdvancedData, ForgeEventData};
use gantry_core::ids::{ApprovalId, NodeId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::{AbandonCause, StageTrigger};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::engine::CancelWhy;
use gantry_core::ports::forge::{CancelReason, ReconcileTarget};
use gantry_core::ports::inbox::{
    DedupKey, OptionId, RaiseRequest, Resolution, ResolveTarget, SafeSummary,
};
use gantry_core::ports::policy::InvalidateReason;

use crate::decide::{
    Command, CommandRejection, DecideEnv, Decision, Effect, EffectKey, IdMint, Observed,
    ObservedData, ReconcileKey,
};
use crate::decider::{
    attempt_failed_dedup, current_workflow, delivery_event, dispatchable, engine_actor, event_as,
    expected_actions, model_for, new_workflow, node_event, stage_changed, EventMeta,
};
use crate::state::{limit_of, DeliveryState, OpenInbox, RaisedBy};

/// `delivery.abandoned.reason` 中由引擎保留的值：映射到 `PrClosedUnmerged` / `IssueNotPlanned`
/// （store `abandon_cause`），用户命令不得使用，否则用户放弃会被当成 PR 关闭而可被 T15 恢复。
const RESERVED_ABANDON_REASONS: [&str; 2] = ["pr_closed", "issue_not_planned"];

/// `delivery.level_changed.reason`（engine.md §11 词表）。
const REASON_USER: &str = "user";

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

pub(crate) fn on_command(state: &DeliveryState, command: &Command) -> Vec<Decision> {
    match command {
        Command::SetLevelCap { cap, ctx } => set_level_cap(state, *cap, ctx),
        Command::Pause { ctx } => pause(state, ctx),
        Command::Resume { ctx } => resume(state, ctx),
        Command::Abandon { reason, ctx } => abandon(state, reason.as_deref(), ctx),
        Command::RetryNode { node_id, ctx } => retry_node(state, *node_id, ctx),
        Command::SupplySpec { .. } | Command::RequestHeal(_) | Command::Classify(_) => {
            reply(Err(CommandRejection::NotSupportedYet("M2")))
        }
    }
}

fn reply(result: Result<(), CommandRejection>) -> Vec<Decision> {
    vec![Decision::Reply(result)]
}

/// 命令追加的第 `n` 条（1 起）业务事件的信封：actor 取调用方，`dedup_key` 按 #99 约定派生。
fn command_meta(ctx: &ActorCtx, n: usize) -> EventMeta {
    EventMeta {
        actor: ctx.actor.clone(),
        causation_id: ctx.causation_id,
        dedup_key: ctx.dedup_key.as_ref().map(|key| {
            if n <= 1 {
                key.clone()
            } else {
                format!("{key}:{n}")
            }
        }),
    }
}

fn ensure_active(state: &DeliveryState) -> Result<(), CommandRejection> {
    if state.delivery.stage.is_terminal() {
        return Err(CommandRejection::InvalidState("delivery is terminal"));
    }
    Ok(())
}

/// `SetLevelCap`：`None` → `Validation`；终态 → `InvalidState`；同值 → `Ok`；否则
/// `delivery.level_changed(kind = cap, reason = user)`（§7.14.5）。
fn set_level_cap(state: &DeliveryState, cap: Level, ctx: &ActorCtx) -> Vec<Decision> {
    if cap == Level::None {
        return reply(Err(CommandRejection::Validation(
            "delivery level_cap must not be None",
        )));
    }
    if let Err(rejection) = ensure_active(state) {
        return reply(Err(rejection));
    }
    let mut out: Vec<Decision> = level_cap_event(state, cap, command_meta(ctx, 1))
        .into_iter()
        .collect();
    out.push(Decision::Reply(Ok(())));
    out
}

/// `level_changed(kind = cap)`：`cap ≠ None`、≠ 当前上限、阶段非终态。
fn level_cap_event(state: &DeliveryState, cap: Level, meta: EventMeta) -> Option<Decision> {
    let row = &state.delivery;
    if cap == Level::None || cap == row.level_cap || row.stage.is_terminal() {
        return None;
    }
    Some(event_as(
        state,
        EventType::DeliveryLevelChanged,
        &row.id.to_string(),
        &DeliveryLevelChangedData {
            from: row.level_cap,
            to: cap,
            kind: LevelChangeKind::Cap,
            reason: REASON_USER.to_owned(),
        },
        meta,
    ))
}

/// `Pause`：终态 → `InvalidState`；已暂停 → `Ok`（不写事件）；否则 `delivery.paused`（P-56）。
fn pause(state: &DeliveryState, ctx: &ActorCtx) -> Vec<Decision> {
    if let Err(rejection) = ensure_active(state) {
        return reply(Err(rejection));
    }
    let row = &state.delivery;
    if row.stage_status == StageStatus::Paused {
        return reply(Ok(()));
    }
    vec![
        event_as(
            state,
            EventType::DeliveryPaused,
            &row.id.to_string(),
            &DeliveryPausedData {
                by: ctx.actor.id.clone(),
                status_before: row.stage_status,
            },
            command_meta(ctx, 1),
        ),
        Decision::Reply(Ok(())),
    ]
}

/// `Resume`：终态或未暂停 → `InvalidState`；否则 `delivery.resumed`，恢复暂停前的值（P-56）。
fn resume(state: &DeliveryState, ctx: &ActorCtx) -> Vec<Decision> {
    if let Err(rejection) = ensure_active(state) {
        return reply(Err(rejection));
    }
    let row = &state.delivery;
    if row.stage_status != StageStatus::Paused {
        return reply(Err(CommandRejection::InvalidState(
            "delivery is not paused",
        )));
    }
    vec![
        event_as(
            state,
            EventType::DeliveryResumed,
            &row.id.to_string(),
            &DeliveryResumedData {
                by: ctx.actor.id.clone(),
                status_after: row.status_before_pause.unwrap_or(StageStatus::Running),
            },
            command_meta(ctx, 1),
        ),
        Decision::Reply(Ok(())),
    ]
}

/// `Abandon`（T13）：已放弃 → `Ok`（幂等，不写事件）；`closed` → `InvalidState`；
/// 否则 `delivery.abandoned`。收尾在随后的 `Tick(Settle)` 里由 [`abandon_cleanup`] 派生。
fn abandon(state: &DeliveryState, reason: Option<&str>, ctx: &ActorCtx) -> Vec<Decision> {
    if let Some(reason) = reason {
        if reason.chars().count() > 500 {
            return reply(Err(CommandRejection::Validation(
                "abandon reason must be at most 500 characters",
            )));
        }
        if RESERVED_ABANDON_REASONS.contains(&reason) {
            return reply(Err(CommandRejection::Validation(
                "abandon reason is reserved for the engine",
            )));
        }
    }
    match state.delivery.stage {
        Stage::Abandoned => return reply(Ok(())),
        Stage::Closed => return reply(Err(CommandRejection::InvalidState("delivery is closed"))),
        _ => {}
    }
    vec![
        abandoned_event(
            state,
            reason.map(str::to_owned),
            ctx.actor.id.clone(),
            command_meta(ctx, 1),
        ),
        Decision::Reply(Ok(())),
    ]
}

fn abandoned_event(
    state: &DeliveryState,
    reason: Option<String>,
    by: String,
    meta: EventMeta,
) -> Decision {
    event_as(
        state,
        EventType::DeliveryAbandoned,
        &state.delivery.id.to_string(),
        &DeliveryAbandonedData { reason, by },
        meta,
    )
}

/// `RetryNode`（P-56）：节点 `failed` 且交付单元非终态 → `node.retry_requested`，同一决策关闭该节点的
/// `escalation(attempt_failed)`（`Resolution::System`）；否则 `InvalidState`。
fn retry_node(state: &DeliveryState, node_id: NodeId, ctx: &ActorCtx) -> Vec<Decision> {
    if let Err(rejection) = ensure_active(state) {
        return reply(Err(rejection));
    }
    let Some(node) = state.node(&node_id) else {
        return reply(Err(CommandRejection::InvalidState(
            "node does not belong to this delivery",
        )));
    };
    if node.status != NodeStatus::Failed {
        return reply(Err(CommandRejection::InvalidState("node is not failed")));
    }
    // 阻塞未解除时重试只会再失败一次（#73）：受保护路径要先有已授予的补丁审批，
    // 预算耗尽要先追加额度。
    if let Some(detail) = last_failure(node) {
        if detail.starts_with("policy.path_protected:") && !patch_approval_granted(state) {
            return reply(Err(CommandRejection::InvalidState(
                "protected path change has no granted approval",
            )));
        }
        if detail.starts_with("budget.")
            && !budget_raised_since_exhausted(state, BudgetDimension::TokensByTier(node.tier))
        {
            return reply(Err(CommandRejection::InvalidState(
                "budget quota has not been raised",
            )));
        }
    }
    let mut out = vec![event_as(
        state,
        EventType::NodeRetryRequested,
        &node_id.to_string(),
        &NodeRetryRequestedData {
            node_id,
            by: ctx.actor.id.clone(),
        },
        command_meta(ctx, 1),
    )];
    let dedup = attempt_failed_dedup(state.delivery.id, node_id);
    if state
        .open_inbox
        .values()
        .any(|item| item.dedup_key.as_str() == dedup)
    {
        out.push(Decision::Resolve {
            target: ResolveTarget::DedupKey(DedupKey::new(dedup)),
            how: Resolution::System {
                reason: "node_retry_requested".to_owned(),
            },
        });
    }
    out.push(Decision::Reply(Ok(())));
    out
}

fn last_failure(node: &crate::state::NodeState) -> Option<&str> {
    node.attempts
        .last()
        .and_then(|a| a.failure_detail.as_ref())
        .map(|d| d.as_str())
}

fn patch_approval_granted(state: &DeliveryState) -> bool {
    state.approvals.values().any(|a| {
        a.action == ApprovalAction::PatchApplyProtected && a.status == ApprovalStatus::Granted
    })
}

/// 该维度最近一次 `budget.exhausted` 之后写过 `budget.limit_raised`。
fn budget_raised_since_exhausted(state: &DeliveryState, dimension: BudgetDimension) -> bool {
    let Some(exhausted) = state
        .ledger
        .exhausted
        .iter()
        .filter(|(d, _)| *d == dimension)
        .map(|(_, seq)| *seq)
        .max()
    else {
        return false;
    };
    state
        .ledger
        .raised
        .iter()
        .any(|(d, _, seq)| *d == dimension && *seq > exhausted)
}

/// `raise_cap` 之后继续（#73）：当前图里因预算封顶失败的节点，在该维度追加额度后由引擎重新排队
/// （`node.retry_requested`，`by = engine`），再走正常派发。追加前或 `budget_exhausted` 未关闭时不产出；
/// 重新排队后节点不再是 failed，重放不重复。
pub(crate) fn requeue_after_raise(state: &DeliveryState, out: &mut Vec<Decision>) {
    if !state.may_dispatch() {
        return;
    }
    let Some(workflow) = current_workflow(state) else {
        return;
    };
    for node in workflow.nodes.values() {
        let budget_failed = node.status == NodeStatus::Failed
            && last_failure(node).is_some_and(|d| d.starts_with("budget."))
            && (node.exec == gantry_core::entities::Exec::Direct
                || node.attempts.last().is_some_and(|a| a.result_ref.is_some()));
        if budget_failed
            && budget_raised_since_exhausted(state, BudgetDimension::TokensByTier(node.tier))
        {
            out.push(engine_node_retry(state, node.id));
        }
    }
}

fn engine_node_retry(state: &DeliveryState, node_id: NodeId) -> Decision {
    event_as(
        state,
        EventType::NodeRetryRequested,
        &node_id.to_string(),
        &NodeRetryRequestedData {
            node_id,
            by: engine_actor().id,
        },
        EventMeta::engine(),
    )
}

// ---------------------------------------------------------------------------
// 事件驱动：T11 与选项执行
// ---------------------------------------------------------------------------

pub(crate) fn on_event(
    state: &DeliveryState,
    observed: &Observed,
    env: &DecideEnv,
    mint: &mut IdMint,
) -> Vec<Decision> {
    match &observed.data {
        ObservedData::Approval {
            approval_id,
            status: ApprovalStatus::Invalidated | ApprovalStatus::Expired,
        } => approval_lapsed(state, observed, *approval_id),
        ObservedData::Forge(ForgeEventData::PrSynchronized(_)) => back_to_review(
            state,
            observed,
            StageTrigger::ApprovalInvalidated,
            "head_changed",
        ),
        ObservedData::Forge(ForgeEventData::BaseAdvanced(data)) => {
            base_advanced(state, observed, data, env)
        }
        ObservedData::InboxResolved {
            resolution: InboxResolution::Chosen,
            option_id: Some(option),
            item,
            ..
        } => execute_option(state, observed, option, item.as_ref(), env, mint),
        _ => Vec::new(),
    }
}

fn is_merge_approval(state: &DeliveryState, id: &ApprovalId) -> bool {
    state
        .approvals
        .get(id)
        .is_some_and(|a| a.action == ApprovalAction::Forge(ForgeActionKind::MergePr))
}

/// T11（`ApprovalInvalidated`）：合并审批变为 invalidated / expired，且没有 pending / granted 的合并审批。
fn approval_lapsed(
    state: &DeliveryState,
    observed: &Observed,
    approval_id: ApprovalId,
) -> Vec<Decision> {
    if !is_merge_approval(state, &approval_id) {
        return Vec::new();
    }
    let active = state.approvals.values().any(|a| {
        a.action == ApprovalAction::Forge(ForgeActionKind::MergePr)
            && matches!(a.status, ApprovalStatus::Pending | ApprovalStatus::Granted)
    });
    if active {
        return Vec::new();
    }
    back_to_review(
        state,
        observed,
        StageTrigger::ApprovalInvalidated,
        "head_changed",
    )
}

/// T11 的共同部分：阶段为 `merge`，触发事件晚于进入本阶段（重放不重复迁移）；
/// `stage_changed(merge → review)`，并关闭合并审批对应的未决上报项（§7.6 T11 动作）。
fn back_to_review(
    state: &DeliveryState,
    observed: &Observed,
    trigger: StageTrigger,
    reason: &str,
) -> Vec<Decision> {
    let row = &state.delivery;
    if row.stage != Stage::Merge || observed.seq <= row.stage_entered.seq {
        return Vec::new();
    }
    let Some(change) = stage_changed(state, trigger, Some(reason)) else {
        return Vec::new();
    };
    let mut out = vec![change];
    let mut closed = BTreeSet::new();
    for item in state.open_inbox.values() {
        let Some(approval_id) = item.approval_id else {
            continue;
        };
        if !is_merge_approval(state, &approval_id) || !closed.insert(approval_id) {
            continue;
        }
        let status = state.approvals[&approval_id].status;
        let how = match status {
            ApprovalStatus::Pending | ApprovalStatus::Granted => Resolution::System {
                reason: reason.to_owned(),
            },
            settled => Resolution::ApprovalSettled {
                approval_id,
                status: settled,
            },
        };
        out.push(Decision::Resolve {
            target: ResolveTarget::Approval(approval_id),
            how,
        });
    }
    out
}

fn pr_target(state: &DeliveryState) -> Option<ReconcileTarget> {
    state.pr.as_ref().map(|pr| ReconcileTarget::Pr {
        project: state.delivery.project_id,
        number: pr.number,
    })
}

fn reconcile(env: &DecideEnv, target: ReconcileTarget) -> Option<Decision> {
    if env
        .inflight
        .contains(&EffectKey::Reconcile(ReconcileKey(target)))
    {
        return None;
    }
    Some(Decision::Effect(Effect::ReconcileNow { target }))
}

/// T11（`BaseAdvanced`）：`affected_prs` 含本交付单元的 PR、阶段为 `merge` →
/// `stage_changed(merge → review, base_advanced)` + `ReconcileNow`。
fn base_advanced(
    state: &DeliveryState,
    observed: &Observed,
    data: &BaseAdvancedData,
    env: &DecideEnv,
) -> Vec<Decision> {
    let Some(pr) = &state.pr else {
        return Vec::new();
    };
    if !data.affected_prs.contains(&pr.number) {
        return Vec::new();
    }
    let mut out = back_to_review(state, observed, StageTrigger::BaseAdvanced, "base_advanced");
    if out.is_empty() {
        return out;
    }
    if let Some(target) = pr_target(state) {
        out.extend(reconcile(env, target));
    }
    out
}

/// T10 另需（§7.14.5）：本阶段因 `base_advanced` 进入时，快照 `synced_at > stage_entered.ts`。
pub(crate) fn base_synced(state: &DeliveryState) -> bool {
    let entered = &state.delivery.stage_entered;
    if entered.reason.as_deref() != Some("base_advanced") {
        return true;
    }
    state
        .pr_snapshot
        .as_ref()
        .is_some_and(|pr| pr.synced_at > entered.ts)
}

/// 因 `base_advanced` 回到 `review` 而对账未回：只产出 `ReconcileNow`（不在途时）。
pub(crate) fn reconcile_after_base_advance(
    state: &DeliveryState,
    env: &DecideEnv,
    out: &mut Vec<Decision>,
) {
    if state.delivery.stage != Stage::Review || base_synced(state) {
        return;
    }
    if let Some(target) = pr_target(state) {
        out.extend(reconcile(env, target));
    }
}

// ---------------------------------------------------------------------------
// T14 / T15
// ---------------------------------------------------------------------------

/// T14：非终态且快照 PR 已关闭（未合并）→ `delivery.abandoned(reason = pr_closed)`。
pub(crate) fn pr_closed(state: &DeliveryState) -> Option<Decision> {
    if state.delivery.stage.is_terminal() || state.pr.is_none() {
        return None;
    }
    let snapshot = state.pr_snapshot.as_ref()?;
    if snapshot.state != PullRequestState::Closed {
        return None;
    }
    Some(abandoned_event(
        state,
        Some("pr_closed".to_owned()),
        engine_actor().id,
        EventMeta::engine(),
    ))
}

/// T15：阶段 `abandoned`、放弃原因为 `PrClosedUnmerged`、PR 仍绑定且快照为 open →
/// `stage_changed(abandoned → review, pr_reopened)`。用户放弃造成的 `abandoned` 不迁移（core 的边）。
pub(crate) fn pr_reopened(state: &DeliveryState) -> Option<Decision> {
    let row = &state.delivery;
    if row.stage != Stage::Abandoned || row.abandon_cause != Some(AbandonCause::PrClosedUnmerged) {
        return None;
    }
    let (pr, snapshot) = (state.pr.as_ref()?, state.pr_snapshot.as_ref()?);
    if snapshot.state != PullRequestState::Open || snapshot.number != pr.number {
        return None;
    }
    stage_changed(state, StageTrigger::PrReopened, Some("pr_reopened"))
}

// ---------------------------------------------------------------------------
// 放弃收尾（§7.14.5"放弃收尾"行）
// ---------------------------------------------------------------------------

/// 阶段为 `abandoned` 时，每项资源独立派生、可重放，不依赖"刚刚放弃"这个输入。
/// `update_pr(closed)` 进入期望动作集合（[`expected_actions`]），`RemoveWorktree` 见 `release_worktree`。
pub(crate) fn abandon_cleanup(state: &DeliveryState, env: &DecideEnv, out: &mut Vec<Decision>) {
    let row = &state.delivery;
    if row.stage != Stage::Abandoned {
        return;
    }
    for workflow in state.workflows.values() {
        for node in workflow.nodes.values() {
            // pending / queued → cancelled（core 节点状态机允许）；running 且没有未终结尝试的节点
            // （等待自动重试退避）也取消，否则它永远停在 running。有未终结尝试的节点在尝试
            // 以 cancelled 终结时由终结管线写 node.finished。
            let idle_running = node.status == NodeStatus::Running
                && node.attempts.iter().all(|a| a.finished_at.is_some());
            if matches!(node.status, NodeStatus::Pending | NodeStatus::Queued) || idle_running {
                out.push(node_event(
                    state,
                    EventType::NodeFinished,
                    node,
                    NodeStatus::Cancelled,
                ));
            }
        }
    }
    for workflow in state.workflows.values() {
        if workflow.status == WorkflowStatus::Running {
            out.push(delivery_event(
                state,
                EventType::WorkflowFinished,
                &workflow.id.to_string(),
                &WorkflowFinishedData {
                    status: WorkflowStatus::Cancelled,
                },
            ));
        }
    }
    for (_, attempt) in state.unfinished_attempts() {
        if !env.inflight.contains(&EffectKey::Cancel(attempt.id)) {
            out.push(Decision::Effect(Effect::CancelAttempt {
                attempt_id: attempt.id,
                why: CancelWhy::DeliveryClosed,
            }));
        }
    }
    // 仍排队的动作：forge 不会因交付单元已放弃而跳过（forge.md §7.8），CancelOutbox 是唯一防线。
    // 放弃收尾自己期望的 update_pr(closed) 不取消。
    let expected = expected_actions(state, env);
    for (key, action) in &state.outbox {
        if action.status != OutboxStatus::Queued
            || expected.contains_key(key)
            || env
                .inflight
                .contains(&EffectKey::CancelOutbox(action.action_id))
        {
            continue;
        }
        out.push(Decision::Effect(Effect::CancelOutbox {
            action_id: action.action_id,
            reason: CancelReason::DeliveryAbandoned,
        }));
    }
    let reason = if row.abandon_cause == Some(AbandonCause::PrClosedUnmerged) {
        InvalidateReason::PrClosed
    } else {
        InvalidateReason::DeliveryAbandoned
    };
    for (approval_id, approval) in &state.approvals {
        if !matches!(
            approval.status,
            ApprovalStatus::Pending | ApprovalStatus::Granted
        ) || env.inflight.contains(&EffectKey::Invalidate(*approval_id))
        {
            continue;
        }
        out.push(Decision::Effect(Effect::InvalidateApproval {
            approval_id: *approval_id,
            reason: reason.clone(),
        }));
    }
}

// ---------------------------------------------------------------------------
// models_not_configured（§9、§7.14.5）
// ---------------------------------------------------------------------------

const MODELS_NOT_CONFIGURED: &str = "models_not_configured";

/// `escalation(models_not_configured)` 的 `dedup_key`。
pub fn models_not_configured_dedup(state: &DeliveryState, node: NodeId) -> String {
    format!("esc:{}:{MODELS_NOT_CONFIGURED}:{node}", state.delivery.id)
}

/// 派发时 `env.models` 缺所需表项 → `Raise(escalation(models_not_configured))`：
/// 节点可派发、`may_dispatch`、`open_inbox` 中无同键项。
pub(crate) fn raise_models_not_configured(
    state: &DeliveryState,
    env: &DecideEnv,
    out: &mut Vec<Decision>,
) {
    if !state.may_dispatch() {
        return;
    }
    let Some(workflow) = current_workflow(state) else {
        return;
    };
    for node in workflow.nodes.values() {
        if !dispatchable(node, env) || model_for(env, state, node).is_some() {
            continue;
        }
        let dedup = models_not_configured_dedup(state, node.id);
        if state
            .open_inbox
            .values()
            .any(|item| item.dedup_key.as_str() == dedup)
        {
            continue;
        }
        out.push(Decision::Raise(RaiseRequest {
            project_id: Some(state.delivery.project_id),
            delivery_id: Some(state.delivery.id),
            kind: InboxKind::Escalation,
            severity: Severity::Blocker,
            summary: SafeSummary::new(format!("节点 {} 所需的模型未配置", node.key)),
            payload: InboxPayload::Escalation {
                reason_code: ReasonCode::parse(MODELS_NOT_CONFIGURED)
                    .expect("models_not_configured is in the vocabulary"),
                node_id: Some(node.id),
                attempt_id: None,
                target_level: None,
                action_id: None,
                evidence: Vec::new(),
                options: None,
            },
            dedup_key: DedupKey::new(dedup),
            supersedes: Vec::new(),
            actor: engine_actor(),
            causation_id: None,
        }));
    }
}

/// `env.models` 已有所需表项 → `Resolve` 该节点的 `escalation(models_not_configured)`。
/// 设置切换只发生在两次决策之间并伴随 `settings.updated`，此处按状态求值，
/// 因而也覆盖 `Tick(Recover)`。
pub(crate) fn resolve_models_configured(
    state: &DeliveryState,
    env: &DecideEnv,
    out: &mut Vec<Decision>,
) {
    for item in state.open_inbox.values() {
        if item.raised_by != RaisedBy::Engine
            || item.reason_code.as_ref().map(ReasonCode::as_str) != Some(MODELS_NOT_CONFIGURED)
        {
            continue;
        }
        let Some(node) = item.node_id.and_then(|id| state.node(&id)) else {
            continue;
        };
        if model_for(env, state, node).is_none() {
            continue;
        }
        out.push(Decision::Resolve {
            target: ResolveTarget::DedupKey(item.dedup_key.clone()),
            how: Resolution::System {
                reason: "models_configured".to_owned(),
            },
        });
    }
}

// ---------------------------------------------------------------------------
// 上报项选项的执行（§7.11"关闭与执行"）
// ---------------------------------------------------------------------------

/// 选项触发的事件：`causation_id` 指向那条 `inbox.resolved`；`dedup_key` 由它派生，
/// 同一 `inbox.resolved` 重放时 store 返回已有行。
fn option_meta(observed: &Observed, n: usize) -> EventMeta {
    let base = format!("inbox-option:{}", observed.id);
    EventMeta {
        actor: engine_actor(),
        causation_id: Some(observed.id),
        dedup_key: Some(if n <= 1 { base } else { format!("{base}:{n}") }),
    }
}

/// `item` 缺失时无法校验选项属于该项，失败闭合（不动作）；`option_id` 不在 `item.options` 中
/// 同样不动作（§7.14.4 `OpenInbox.options`）。
fn execute_option(
    state: &DeliveryState,
    observed: &Observed,
    option: &OptionId,
    item: Option<&OpenInbox>,
    env: &DecideEnv,
    mint: &mut IdMint,
) -> Vec<Decision> {
    let Some(item) = item else {
        return Vec::new();
    };
    if !item.options.contains(option) {
        return Vec::new();
    }
    match option.as_str() {
        "abandon" => {
            if state.delivery.stage.is_terminal() {
                return Vec::new();
            }
            vec![abandoned_event(
                state,
                None,
                observed.actor.id.clone(),
                option_meta(observed, 1),
            )]
        }
        // set_level_cap(target_level)；缺 target_level 不动作。
        "upgrade" => match item.target_level {
            Some(level) => level_cap_event(state, level, option_meta(observed, 1))
                .into_iter()
                .collect(),
            None => Vec::new(),
        },
        "narrow" => narrow(state, observed).into_iter().collect(),
        "replan" => replan(state, observed, env, mint),
        "raise_cap" => raise_cap(state, observed, item).into_iter().collect(),
        // done / continue：解除阻塞（该项已被 inbox.resolved 移出 open_inbox）后重新求值，
        // 由随后的按状态求值完成；keep：不动作；fix（M2，T9）与 retry（#67 的 epoch + 1）不在本切片。
        _ => Vec::new(),
    }
}

/// `narrow`：保持上限，`level_effective = level_cap`（只升，P-57）。
fn narrow(state: &DeliveryState, observed: &Observed) -> Option<Decision> {
    let row = &state.delivery;
    if row.stage.is_terminal() || row.level_effective >= row.level_cap {
        return None;
    }
    Some(event_as(
        state,
        EventType::DeliveryLevelChanged,
        &row.id.to_string(),
        &DeliveryLevelChangedData {
            from: row.level_effective,
            to: row.level_cap,
            kind: LevelChangeKind::Effective,
            reason: REASON_USER.to_owned(),
        },
        option_meta(observed, 1),
    ))
}

/// Low 档 `replan`（§7.14.5 末行）：阶段为 `implement`，当前 `low.implement` 建于该
/// `inbox.resolved` 之前（重放不重复重建）；同一决策取消当前图与其未终结节点并新建 `low.implement`。
/// 节点预算不足时不产出（耗尽上报不在本切片）。非 Low 的 T7 在 M1 不实现。
fn replan(
    state: &DeliveryState,
    observed: &Observed,
    env: &DecideEnv,
    mint: &mut IdMint,
) -> Vec<Decision> {
    let row = &state.delivery;
    if row.level_effective != Level::Low || row.stage != Stage::Implement {
        return Vec::new();
    }
    let Some(workflow) = current_workflow(state) else {
        return Vec::new();
    };
    if workflow.created_seq > observed.seq {
        return Vec::new();
    }
    let Some(created) = new_workflow(state, mint) else {
        return Vec::new();
    };
    let mut appends = Vec::new();
    let mut effects = Vec::new();
    if workflow.status == WorkflowStatus::Running {
        appends.push(delivery_event(
            state,
            EventType::WorkflowFinished,
            &workflow.id.to_string(),
            &WorkflowFinishedData {
                status: WorkflowStatus::Cancelled,
            },
        ));
    }
    for node in workflow.nodes.values() {
        if matches!(
            node.status,
            NodeStatus::Pending | NodeStatus::Queued | NodeStatus::Running
        ) {
            appends.push(node_event(
                state,
                EventType::NodeFinished,
                node,
                NodeStatus::Cancelled,
            ));
        }
        for attempt in node.attempts.iter().filter(|a| a.finished_at.is_none()) {
            if !env.inflight.contains(&EffectKey::Cancel(attempt.id)) {
                effects.push(Decision::Effect(Effect::CancelAttempt {
                    attempt_id: attempt.id,
                    why: CancelWhy::Superseded,
                }));
            }
        }
    }
    appends.extend(created);
    // 每条事件都带由 inbox.resolved 派生的 dedup_key 与 causation_id。
    let mut out: Vec<Decision> = appends
        .into_iter()
        .enumerate()
        .map(|(i, decision)| match decision {
            Decision::Append(mut event) => {
                let meta = option_meta(observed, i + 1);
                event.causation_id = meta.causation_id;
                event.dedup_key = meta.dedup_key;
                Decision::Append(event)
            }
            other => other,
        })
        .collect();
    out.extend(effects);
    out
}

/// `raise_cap`（D-19）：给耗尽的维度追加一份该档模板原值，写 `budget.limit_raised`。
/// 维度取该上报项之前最近一次 `budget.exhausted`（`OpenInbox` 不带维度，二者同事务写入）；
/// 该 `inbox.resolved` 之后已为同维度追加过则不再追加（重放）。
fn raise_cap(state: &DeliveryState, observed: &Observed, item: &OpenInbox) -> Option<Decision> {
    let row = &state.delivery;
    if row.stage.is_terminal() || item.kind != InboxKind::BudgetExhausted {
        return None;
    }
    let dimension: BudgetDimension = state
        .ledger
        .exhausted
        .iter()
        .rev()
        .find(|(_, seq)| *seq <= item.raised_seq)
        .map(|(dimension, _)| *dimension)?;
    if state
        .ledger
        .raised
        .iter()
        .any(|(d, _, seq)| *d == dimension && *seq > observed.seq)
    {
        return None;
    }
    let from = limit_of(&row.budget.limits, dimension);
    let raised: u64 = state
        .ledger
        .raised
        .iter()
        .filter(|(d, _, _)| *d == dimension)
        .map(|(_, by, _)| *by)
        .sum();
    let by = from.checked_sub(raised)?;
    if by == 0 {
        return None;
    }
    let to = from.checked_add(by)?;
    Some(event_as(
        state,
        EventType::BudgetLimitRaised,
        &row.id.to_string(),
        &BudgetLimitRaisedData {
            dimension,
            from,
            to,
            by,
        },
        option_meta(observed, 1),
    ))
}
