//! 状态机：纯函数 + 常量转移表。触发者与时钟由调用方提供；core 只回答"是否合法"。
//!
//! 暂停 / 恢复与 PR 重开需要显式上下文：`next_status` 不带上下文时对 `Resume`
//! 返回 `context_required`，`next_stage` 对 `PrReopened` 同理；调用方必须使用带上下文的
//! 函数并传入暂停前状态或放弃原因，不从状态值猜测。

use std::fmt;

use crate::entities::{
    Approval, ApprovalStatus, Attempt, AttemptStatus, FailureClass, InboxStatus, LockHolder,
    NodeStatus, OutboxStatus, SandboxStatus, Stage, StageStatus, WorkflowStatus, WorktreeLock,
};
use crate::level::Level;
use crate::time::Timestamp;

/// 状态机错误；`machine` 是低基数指标标签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionError {
    pub machine: &'static str,
    pub from: String,
    pub to: String,
    pub code: &'static str,
}

impl TransitionError {
    fn new(
        machine: &'static str,
        from: impl ToString,
        to: impl ToString,
        code: &'static str,
    ) -> Self {
        Self {
            machine,
            from: from.to_string(),
            to: to.to_string(),
            code,
        }
    }

    fn illegal(machine: &'static str, from: impl ToString, to: impl ToString) -> Self {
        Self::new(machine, from, to, "illegal_transition")
    }

    fn context_required(machine: &'static str, from: impl ToString, to: impl ToString) -> Self {
        Self::new(machine, from, to, "context_required")
    }
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} -> {} is not legal ({})",
            self.machine, self.from, self.to, self.code
        )
    }
}

impl std::error::Error for TransitionError {}

/// 交付单元阶段。
pub mod delivery {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum StageTrigger {
        AdmissionAccept,
        AdmissionRevise,
        AdmissionSkipped,
        PlanProduced,
        PatchValidated,
        PlanInfeasible,
        Replan,
        PrCreated,
        ReviewRevise,
        CiFailureNeedsCode,
        ReviewAcceptCiGreen,
        ApprovalInvalidated,
        BaseAdvanced,
        MergedAndIssuesClosed,
        UserAbandoned,
        IssueNotPlanned,
        PrClosedUnmerged,
        PrReopened,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum StatusTrigger {
        StageEntered,
        WaitHuman,
        HumanResolved,
        GateBlocked,
        Unblocked,
        Pause,
        Resume,
        StageDone,
    }

    /// 放弃原因；`PrReopened` 只允许从 `PrClosedUnmerged` 的 `abandoned` 回到 `review`。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum AbandonCause {
        UserAbandoned,
        IssueNotPlanned,
        PrClosedUnmerged,
    }

    /// 阶段转移的档位守卫。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum LevelGuard {
        Any,
        HighOrAbove,
        Medium,
        Low,
        NotLow,
    }

    impl LevelGuard {
        fn allows(self, level: Level) -> bool {
            match self {
                Self::Any => true,
                Self::HighOrAbove => level >= Level::High,
                Self::Medium => level == Level::Medium,
                Self::Low => level == Level::Low,
                Self::NotLow => level != Level::Low,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Edge {
        pub from: Stage,
        pub trigger: StageTrigger,
        pub to: Stage,
        pub level_guard: LevelGuard,
        /// 仅 `PrReopened` 边需要放弃原因。
        pub require_abandon_cause: Option<AbandonCause>,
    }

    const fn edge(from: Stage, trigger: StageTrigger, to: Stage, level_guard: LevelGuard) -> Edge {
        Edge {
            from,
            trigger,
            to,
            level_guard,
            require_abandon_cause: None,
        }
    }

    /// data-model §4.1 与 §7.4 的逐边实现；未列出的情形一律拒绝。
    pub const EDGES: &[Edge] = &[
        edge(
            Stage::Spec,
            StageTrigger::AdmissionAccept,
            Stage::Plan,
            LevelGuard::HighOrAbove,
        ),
        edge(
            Stage::Spec,
            StageTrigger::AdmissionRevise,
            Stage::Spec,
            LevelGuard::HighOrAbove,
        ),
        edge(
            Stage::Spec,
            StageTrigger::AdmissionSkipped,
            Stage::Plan,
            LevelGuard::Medium,
        ),
        edge(
            Stage::Spec,
            StageTrigger::AdmissionSkipped,
            Stage::Implement,
            LevelGuard::Low,
        ),
        edge(
            Stage::Plan,
            StageTrigger::PlanProduced,
            Stage::Implement,
            LevelGuard::NotLow,
        ),
        edge(
            Stage::Implement,
            StageTrigger::PatchValidated,
            Stage::Propose,
            LevelGuard::Any,
        ),
        edge(
            Stage::Implement,
            StageTrigger::PlanInfeasible,
            Stage::Plan,
            LevelGuard::NotLow,
        ),
        edge(
            Stage::Implement,
            StageTrigger::Replan,
            Stage::Plan,
            LevelGuard::NotLow,
        ),
        edge(
            Stage::Review,
            StageTrigger::Replan,
            Stage::Plan,
            LevelGuard::NotLow,
        ),
        edge(
            Stage::Propose,
            StageTrigger::PrCreated,
            Stage::Review,
            LevelGuard::Any,
        ),
        edge(
            Stage::Review,
            StageTrigger::ReviewRevise,
            Stage::Implement,
            LevelGuard::Any,
        ),
        edge(
            Stage::Review,
            StageTrigger::CiFailureNeedsCode,
            Stage::Implement,
            LevelGuard::Any,
        ),
        edge(
            Stage::Review,
            StageTrigger::ReviewAcceptCiGreen,
            Stage::Merge,
            LevelGuard::Any,
        ),
        edge(
            Stage::Merge,
            StageTrigger::ApprovalInvalidated,
            Stage::Review,
            LevelGuard::Any,
        ),
        edge(
            Stage::Merge,
            StageTrigger::BaseAdvanced,
            Stage::Review,
            LevelGuard::Any,
        ),
        edge(
            Stage::Merge,
            StageTrigger::MergedAndIssuesClosed,
            Stage::Closed,
            LevelGuard::Any,
        ),
        // 任何非终态阶段 → abandoned（P-55）。
        edge(
            Stage::Spec,
            StageTrigger::UserAbandoned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Plan,
            StageTrigger::UserAbandoned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Implement,
            StageTrigger::UserAbandoned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Propose,
            StageTrigger::UserAbandoned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Review,
            StageTrigger::UserAbandoned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Merge,
            StageTrigger::UserAbandoned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Spec,
            StageTrigger::IssueNotPlanned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Plan,
            StageTrigger::IssueNotPlanned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Implement,
            StageTrigger::IssueNotPlanned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Propose,
            StageTrigger::IssueNotPlanned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Review,
            StageTrigger::IssueNotPlanned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Merge,
            StageTrigger::IssueNotPlanned,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Spec,
            StageTrigger::PrClosedUnmerged,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Plan,
            StageTrigger::PrClosedUnmerged,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Implement,
            StageTrigger::PrClosedUnmerged,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Propose,
            StageTrigger::PrClosedUnmerged,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Review,
            StageTrigger::PrClosedUnmerged,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        edge(
            Stage::Merge,
            StageTrigger::PrClosedUnmerged,
            Stage::Abandoned,
            LevelGuard::Any,
        ),
        Edge {
            from: Stage::Abandoned,
            trigger: StageTrigger::PrReopened,
            to: Stage::Review,
            level_guard: LevelGuard::Any,
            require_abandon_cause: Some(AbandonCause::PrClosedUnmerged),
        },
    ];

    /// 不带放弃原因：`PrReopened` 返回 `context_required`。
    pub fn next_stage(
        from: Stage,
        trigger: StageTrigger,
        level_effective: Level,
    ) -> Result<Stage, TransitionError> {
        next_stage_with_abandon(from, trigger, level_effective, None)
    }

    /// 显式传入 `abandoned` 的原因；`PrReopened` 要求 `PrClosedUnmerged`。
    pub fn next_stage_with_abandon(
        from: Stage,
        trigger: StageTrigger,
        level_effective: Level,
        abandon_cause: Option<AbandonCause>,
    ) -> Result<Stage, TransitionError> {
        let mut matched = false;
        for edge in EDGES {
            if edge.from != from || edge.trigger != trigger {
                continue;
            }
            matched = true;
            if !edge.level_guard.allows(level_effective) {
                continue;
            }
            match edge.require_abandon_cause {
                Some(required) => {
                    let cause = abandon_cause.ok_or_else(|| {
                        TransitionError::context_required("delivery", from, trigger_name(trigger))
                    })?;
                    if cause == required {
                        return Ok(edge.to);
                    }
                }
                None => return Ok(edge.to),
            }
        }
        if matched && trigger == StageTrigger::PrReopened {
            return Err(TransitionError::new(
                "delivery",
                from,
                trigger_name(trigger),
                "abandon_cause_mismatch",
            ));
        }
        Err(TransitionError::illegal(
            "delivery",
            from,
            trigger_name(trigger),
        ))
    }

    /// 供 store 校验 `delivery.stage_changed`；不检查放弃原因。
    pub fn is_legal_change(from: Stage, to: Stage, level_effective: Level) -> bool {
        EDGES.iter().any(|edge| {
            edge.from == from && edge.to == to && edge.level_guard.allows(level_effective)
        })
    }

    /// 每档位的阶段序列（P-55）：Low 无 plan；其余档位有 plan。
    pub fn stages_for(level: Level) -> &'static [Stage] {
        const LOW: &[Stage] = &[
            Stage::Spec,
            Stage::Implement,
            Stage::Propose,
            Stage::Review,
            Stage::Merge,
            Stage::Closed,
        ];
        const OTHER: &[Stage] = &[
            Stage::Spec,
            Stage::Plan,
            Stage::Implement,
            Stage::Propose,
            Stage::Review,
            Stage::Merge,
            Stage::Closed,
        ];
        if level == Level::Low {
            LOW
        } else {
            OTHER
        }
    }

    /// 恢复暂停所需的上下文。
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct StatusContext {
        pub paused_from: Option<StageStatus>,
        pub human_resolved: bool,
    }

    pub fn next_status(
        cur: StageStatus,
        trigger: StatusTrigger,
    ) -> Result<StageStatus, TransitionError> {
        next_status_with(cur, trigger, StatusContext::default())
    }

    pub fn next_status_with(
        cur: StageStatus,
        trigger: StatusTrigger,
        ctx: StatusContext,
    ) -> Result<StageStatus, TransitionError> {
        use StageStatus::*;
        use StatusTrigger::*;
        match (cur, trigger) {
            (_, StageEntered) => Ok(Running),
            (Running, WaitHuman) => Ok(WaitingHuman),
            (WaitingHuman | Blocked, HumanResolved) => Ok(Running),
            (Running, GateBlocked) => Ok(Blocked),
            (Blocked, Unblocked) => Ok(Running),
            (Running | WaitingHuman | Blocked, Pause) => Ok(Paused),
            (Paused, Resume) => {
                let previous = ctx.paused_from.ok_or_else(|| {
                    TransitionError::context_required("stage_status", cur, "resume")
                })?;
                if !matches!(previous, Running | WaitingHuman | Blocked) {
                    return Err(TransitionError::new(
                        "stage_status",
                        cur,
                        "resume",
                        "invalid_paused_from",
                    ));
                }
                if previous == WaitingHuman && ctx.human_resolved {
                    Ok(Running)
                } else {
                    Ok(previous)
                }
            }
            (_, StageDone) => Ok(Done),
            _ => Err(TransitionError::illegal(
                "stage_status",
                status_name(cur),
                status_trigger_name(trigger),
            )),
        }
    }

    fn status_name(status: StageStatus) -> &'static str {
        match status {
            StageStatus::Running => "running",
            StageStatus::WaitingHuman => "waiting_human",
            StageStatus::Blocked => "blocked",
            StageStatus::Paused => "paused",
            StageStatus::Done => "done",
        }
    }

    fn status_trigger_name(trigger: StatusTrigger) -> &'static str {
        match trigger {
            StatusTrigger::StageEntered => "stage_entered",
            StatusTrigger::WaitHuman => "wait_human",
            StatusTrigger::HumanResolved => "human_resolved",
            StatusTrigger::GateBlocked => "gate_blocked",
            StatusTrigger::Unblocked => "unblocked",
            StatusTrigger::Pause => "pause",
            StatusTrigger::Resume => "resume",
            StatusTrigger::StageDone => "stage_done",
        }
    }

    fn trigger_name(trigger: StageTrigger) -> &'static str {
        match trigger {
            StageTrigger::AdmissionAccept => "admission_accept",
            StageTrigger::AdmissionRevise => "admission_revise",
            StageTrigger::AdmissionSkipped => "admission_skipped",
            StageTrigger::PlanProduced => "plan_produced",
            StageTrigger::PatchValidated => "patch_validated",
            StageTrigger::PlanInfeasible => "plan_infeasible",
            StageTrigger::Replan => "replan",
            StageTrigger::PrCreated => "pr_created",
            StageTrigger::ReviewRevise => "review_revise",
            StageTrigger::CiFailureNeedsCode => "ci_failure_needs_code",
            StageTrigger::ReviewAcceptCiGreen => "review_accept_ci_green",
            StageTrigger::ApprovalInvalidated => "approval_invalidated",
            StageTrigger::BaseAdvanced => "base_advanced",
            StageTrigger::MergedAndIssuesClosed => "merged_and_issues_closed",
            StageTrigger::UserAbandoned => "user_abandoned",
            StageTrigger::IssueNotPlanned => "issue_not_planned",
            StageTrigger::PrClosedUnmerged => "pr_closed_unmerged",
            StageTrigger::PrReopened => "pr_reopened",
        }
    }
}

/// 节点状态。
pub mod node {
    use super::*;

    pub fn check(from: NodeStatus, to: NodeStatus) -> Result<(), TransitionError> {
        use NodeStatus::*;
        let allowed = matches!(
            (&from, &to),
            (Pending, Queued)
                | (Pending, Cancelled)
                | (Pending, Superseded)
                | (Queued, Running)
                | (Queued, Cancelled)
                | (Queued, Superseded)
                | (Running, Succeeded)
                | (Running, Failed)
                | (Running, Skipped)
                | (Running, Cancelled)
                | (Running, Superseded)
                | (Failed, Queued)
        );
        if allowed {
            Ok(())
        } else {
            Err(TransitionError::illegal(
                "node",
                format!("{from:?}"),
                format!("{to:?}"),
            ))
        }
    }
}

/// 尝试状态。
pub mod attempt {
    use super::*;

    pub fn check(from: AttemptStatus, to: AttemptStatus) -> Result<(), TransitionError> {
        use AttemptStatus::*;
        let allowed = matches!(
            (&from, &to),
            (Preparing, Running)
                | (Preparing, Failed)
                | (Preparing, Cancelled)
                | (Running, Succeeded)
                | (Running, Failed)
                | (Running, Cancelled)
        );
        if allowed {
            Ok(())
        } else {
            Err(TransitionError::illegal(
                "attempt",
                format!("{from:?}"),
                format!("{to:?}"),
            ))
        }
    }

    /// 终态尝试不可变；`failure_class` 与状态一致。
    pub fn check_terminal(attempt: &Attempt) -> Result<(), InvariantError> {
        let terminal = matches!(
            attempt.status,
            AttemptStatus::Succeeded | AttemptStatus::Failed | AttemptStatus::Cancelled
        );
        if !terminal {
            if attempt.failure_class.is_some() {
                return Err(InvariantError::new(
                    "attempt",
                    "failure_class_present_before_terminal",
                ));
            }
            return Ok(());
        }
        match attempt.status {
            AttemptStatus::Succeeded => {
                if attempt.failure_class.is_some() {
                    return Err(InvariantError::new(
                        "attempt",
                        "succeeded_with_failure_class",
                    ));
                }
            }
            AttemptStatus::Failed => {
                if attempt.failure_class.is_none() {
                    return Err(InvariantError::new(
                        "attempt",
                        "failed_without_failure_class",
                    ));
                }
            }
            AttemptStatus::Cancelled => {
                if attempt.failure_class != Some(FailureClass::Cancelled) {
                    return Err(InvariantError::new(
                        "attempt",
                        "cancelled_failure_class_mismatch",
                    ));
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

/// 工作流状态。
pub mod workflow {
    use super::*;

    pub fn check(from: WorkflowStatus, to: WorkflowStatus) -> Result<(), TransitionError> {
        use WorkflowStatus::*;
        let allowed = matches!(
            (&from, &to),
            (Running, Paused)
                | (Paused, Running)
                | (Running, Succeeded)
                | (Paused, Succeeded)
                | (Running, Failed)
                | (Paused, Failed)
                | (Running, Cancelled)
                | (Paused, Cancelled)
        );
        if allowed {
            Ok(())
        } else {
            Err(TransitionError::illegal(
                "workflow",
                format!("{from:?}"),
                format!("{to:?}"),
            ))
        }
    }
}

/// 沙盒状态。
pub mod sandbox {
    use super::*;

    pub fn check(from: SandboxStatus, to: SandboxStatus) -> Result<(), TransitionError> {
        use SandboxStatus::*;
        let allowed = matches!(
            (&from, &to),
            (Created, Running)
                | (Running, Stopped)
                | (Created, Destroyed)
                | (Running, Destroyed)
                | (Stopped, Destroyed)
                | (Created, Lost)
                | (Running, Lost)
                | (Stopped, Lost)
        );
        if allowed {
            Ok(())
        } else {
            Err(TransitionError::illegal(
                "sandbox",
                format!("{from:?}"),
                format!("{to:?}"),
            ))
        }
    }
}

/// 收件箱状态。
pub mod inbox {
    use super::*;

    pub fn check(from: InboxStatus, to: InboxStatus) -> Result<(), TransitionError> {
        use InboxStatus::*;
        let allowed = matches!((&from, &to), (Open, Resolved) | (Open, Dismissed));
        if allowed {
            Ok(())
        } else {
            Err(TransitionError::illegal(
                "inbox",
                format!("{from:?}"),
                format!("{to:?}"),
            ))
        }
    }

    /// `dismiss` 对 `dismissed` 幂等（api.md §4.8）；对 `resolved` 冲突。
    pub fn check_dismiss(from: InboxStatus) -> Result<(), TransitionError> {
        match from {
            InboxStatus::Open | InboxStatus::Dismissed => Ok(()),
            InboxStatus::Resolved => Err(TransitionError::new(
                "inbox",
                "resolved",
                "dismissed",
                "conflict",
            )),
        }
    }
}

/// 审批状态。
pub mod approval {
    use super::*;
    use crate::canon::SubjectDigest;

    pub fn check(from: ApprovalStatus, to: ApprovalStatus) -> Result<(), TransitionError> {
        use ApprovalStatus::*;
        let allowed = matches!(
            (&from, &to),
            (Pending, Granted)
                | (Pending, Denied)
                | (Pending, Invalidated)
                | (Pending, Expired)
                | (Granted, Consumed)
                | (Granted, Invalidated)
                | (Granted, Expired)
        );
        if allowed {
            Ok(())
        } else {
            Err(TransitionError::illegal(
                "approval",
                format!("{from:?}"),
                format!("{to:?}"),
            ))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
    pub enum ConsumeError {
        #[error("approval has not been granted")]
        NotGranted,
        #[error("approval has expired")]
        Expired,
        #[error("approval subject has changed")]
        SubjectChanged,
        #[error("approval was already consumed")]
        AlreadyConsumed,
    }

    /// `granted` 且未过期且 subject 仍一致才可消费一次；消费发生在执行开始前（P-54）。
    pub fn can_consume(
        approval: &Approval,
        current_subject: &SubjectDigest,
        now: Timestamp,
    ) -> Result<(), ConsumeError> {
        if approval.status == ApprovalStatus::Consumed {
            return Err(ConsumeError::AlreadyConsumed);
        }
        if approval.status != ApprovalStatus::Granted {
            return Err(ConsumeError::NotGranted);
        }
        if now >= approval.expires_at {
            return Err(ConsumeError::Expired);
        }
        if &approval.subject_digest != current_subject {
            return Err(ConsumeError::SubjectChanged);
        }
        Ok(())
    }

    /// 主体变化时 pending / granted 应失效；其余返回 `None`。
    pub fn on_subject_changed(
        approval: &Approval,
        new_subject: &SubjectDigest,
    ) -> Option<ApprovalStatus> {
        if matches!(
            approval.status,
            ApprovalStatus::Pending | ApprovalStatus::Granted
        ) && &approval.subject_digest != new_subject
        {
            Some(ApprovalStatus::Invalidated)
        } else {
            None
        }
    }
}

/// Outbox 动作状态。
pub mod outbox {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum OutboxCause {
        Retry,
        RateLimited,
        LeaseExpired,
        Exhausted,
        TerminalError,
        DependencyFailed,
        Superseded,
        Manual,
    }

    pub fn check(
        from: OutboxStatus,
        to: OutboxStatus,
        cause: OutboxCause,
    ) -> Result<(), TransitionError> {
        use OutboxStatus::*;
        match (&from, &to) {
            (Queued, Executing) | (Queued, Cancelled) => Ok(()),
            (Executing, Succeeded) => Ok(()),
            (Executing, Queued) => {
                if matches!(
                    cause,
                    OutboxCause::Retry | OutboxCause::RateLimited | OutboxCause::LeaseExpired
                ) {
                    Ok(())
                } else {
                    Err(TransitionError::new(
                        "outbox",
                        "executing",
                        "queued",
                        "cause_not_retryable",
                    ))
                }
            }
            (Executing, Failed) => {
                if matches!(
                    cause,
                    OutboxCause::Exhausted
                        | OutboxCause::TerminalError
                        | OutboxCause::DependencyFailed
                        | OutboxCause::Manual
                ) {
                    Ok(())
                } else {
                    Err(TransitionError::new(
                        "outbox",
                        "executing",
                        "failed",
                        "cause_not_terminal",
                    ))
                }
            }
            (Executing, Cancelled) => Ok(()),
            _ => Err(TransitionError::illegal(
                "outbox",
                format!("{from:?}"),
                format!("{to:?}"),
            )),
        }
    }
}

/// 工作树锁。
pub mod worktree_lock {
    use super::*;
    use crate::ids::DeliveryId;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct LockRequest {
        pub holder: LockHolder,
        /// engine / guardian 必填，human 必须为空（`LockHolder::check_delivery`）。
        pub delivery_id: Option<DeliveryId>,
        pub lease_until: Timestamp,
        /// 调用方（api.md §3.7）给出的上限；`human` 锁超过它时拒绝。
        pub max_lease: Option<Timestamp>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
    pub enum LockError {
        #[error("worktree is locked by another holder")]
        LockConflict,
        #[error("lease must end after now")]
        InvalidLease,
        #[error("lease exceeds the caller-provided maximum")]
        LeaseTooLong,
        #[error("only the current holder may renew or release")]
        NotHolder,
        #[error("engine/guardian locks require delivery_id, human locks must not carry one")]
        DeliveryBinding,
    }

    pub fn acquire(
        cur: Option<&WorktreeLock>,
        req: &LockRequest,
        now: Timestamp,
    ) -> Result<WorktreeLock, LockError> {
        req.holder
            .check_delivery(req.delivery_id.as_ref())
            .map_err(|_| LockError::DeliveryBinding)?;
        if req.lease_until <= now {
            return Err(LockError::InvalidLease);
        }
        if let Some(max) = req.max_lease {
            if req.lease_until > max {
                return Err(LockError::LeaseTooLong);
            }
        }
        if let Some(lock) = cur {
            if !is_expired(lock, now) && lock.holder != req.holder {
                return Err(LockError::LockConflict);
            }
        }
        Ok(WorktreeLock {
            holder: req.holder,
            delivery_id: req.delivery_id,
            acquired_at: now,
            lease_until: req.lease_until,
        })
    }

    pub fn renew(
        cur: &WorktreeLock,
        holder: LockHolder,
        until: Timestamp,
        now: Timestamp,
    ) -> Result<WorktreeLock, LockError> {
        if cur.holder != holder {
            return Err(LockError::NotHolder);
        }
        if until <= now {
            return Err(LockError::InvalidLease);
        }
        let mut next = cur.clone();
        next.lease_until = until;
        Ok(next)
    }

    pub fn release(cur: &WorktreeLock, holder: LockHolder) -> Result<(), LockError> {
        if cur.holder != holder {
            return Err(LockError::NotHolder);
        }
        Ok(())
    }

    pub fn is_expired(cur: &WorktreeLock, now: Timestamp) -> bool {
        now >= cur.lease_until
    }
}

/// 通用不变式错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvariantError {
    pub machine: &'static str,
    pub code: &'static str,
}

impl InvariantError {
    fn new(machine: &'static str, code: &'static str) -> Self {
        Self { machine, code }
    }
}

impl fmt::Display for InvariantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: invariant violated ({})", self.machine, self.code)
    }
}

impl std::error::Error for InvariantError {}

#[cfg(test)]
mod tests {
    use super::delivery::{AbandonCause, StageTrigger, StatusContext, StatusTrigger};
    use super::*;
    use crate::ids::DeliveryId;

    fn ts(ms: i64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    #[test]
    fn delivery_guards_and_reopen_cause() {
        assert_eq!(
            delivery::next_stage(Stage::Spec, StageTrigger::AdmissionAccept, Level::High).unwrap(),
            Stage::Plan
        );
        assert!(
            delivery::next_stage(Stage::Spec, StageTrigger::AdmissionAccept, Level::Medium)
                .is_err()
        );
        assert_eq!(
            delivery::next_stage(Stage::Spec, StageTrigger::AdmissionSkipped, Level::Low).unwrap(),
            Stage::Implement
        );
        let reopened = delivery::next_stage(Stage::Abandoned, StageTrigger::PrReopened, Level::Low);
        assert_eq!(reopened.unwrap_err().code, "context_required");
        assert_eq!(
            delivery::next_stage_with_abandon(
                Stage::Abandoned,
                StageTrigger::PrReopened,
                Level::Low,
                Some(AbandonCause::UserAbandoned)
            )
            .unwrap_err()
            .code,
            "abandon_cause_mismatch"
        );
        assert_eq!(
            delivery::next_stage_with_abandon(
                Stage::Abandoned,
                StageTrigger::PrReopened,
                Level::Low,
                Some(AbandonCause::PrClosedUnmerged)
            )
            .unwrap(),
            Stage::Review
        );
    }

    #[test]
    fn stage_status_pause_resume_requires_previous() {
        let paused = delivery::next_status(StageStatus::Running, StatusTrigger::Pause).unwrap();
        assert_eq!(paused, StageStatus::Paused);
        assert_eq!(
            delivery::next_status(paused, StatusTrigger::Resume)
                .unwrap_err()
                .code,
            "context_required"
        );
        assert_eq!(
            delivery::next_status_with(
                paused,
                StatusTrigger::Resume,
                StatusContext {
                    paused_from: Some(StageStatus::Running),
                    human_resolved: false
                }
            )
            .unwrap(),
            StageStatus::Running
        );
        assert_eq!(
            delivery::next_status_with(
                StageStatus::Paused,
                StatusTrigger::Resume,
                StatusContext {
                    paused_from: Some(StageStatus::WaitingHuman),
                    human_resolved: true
                }
            )
            .unwrap(),
            StageStatus::Running
        );
    }

    #[test]
    fn every_node_pair_is_checked() {
        use NodeStatus::*;
        let all = [
            Pending, Queued, Running, Succeeded, Failed, Skipped, Cancelled, Superseded,
        ];
        for from in all {
            for to in all {
                let expected = matches!(
                    (from, to),
                    (Pending, Queued)
                        | (Pending, Cancelled)
                        | (Pending, Superseded)
                        | (Queued, Running)
                        | (Queued, Cancelled)
                        | (Queued, Superseded)
                        | (Running, Succeeded)
                        | (Running, Failed)
                        | (Running, Skipped)
                        | (Running, Cancelled)
                        | (Running, Superseded)
                        | (Failed, Queued)
                );
                assert_eq!(node::check(from, to).is_ok(), expected, "{from:?}->{to:?}");
            }
        }
    }

    #[test]
    fn outbox_retry_causes() {
        use outbox::OutboxCause;
        use OutboxStatus::*;
        assert!(outbox::check(Executing, Queued, OutboxCause::Retry).is_ok());
        assert!(outbox::check(Executing, Queued, OutboxCause::Exhausted).is_err());
        assert!(outbox::check(Failed, Queued, OutboxCause::Retry).is_err());
        assert!(outbox::check(Executing, Failed, OutboxCause::Exhausted).is_ok());
    }

    #[test]
    fn worktree_lock_rules() {
        use worktree_lock::*;
        let delivery: DeliveryId = "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM".parse().unwrap();
        let request = LockRequest {
            holder: LockHolder::Human,
            delivery_id: None,
            lease_until: ts(2_000),
            max_lease: Some(ts(3_000)),
        };
        let lock = acquire(None, &request, ts(1_000)).unwrap();
        assert!(lock.validate().is_ok());
        assert!(acquire(Some(&lock), &request, ts(1_500)).is_ok());
        assert_eq!(
            acquire(
                Some(&lock),
                &LockRequest {
                    holder: LockHolder::Engine,
                    delivery_id: Some(delivery),
                    ..request.clone()
                },
                ts(1_500)
            )
            .unwrap_err(),
            LockError::LockConflict
        );
        let reacquire = LockRequest {
            lease_until: ts(2_500),
            ..request.clone()
        };
        assert!(acquire(Some(&lock), &reacquire, ts(2_000)).is_ok());
        assert_eq!(
            release(&lock, LockHolder::Engine).unwrap_err(),
            LockError::NotHolder
        );
        assert!(renew(&lock, LockHolder::Human, ts(2_500), ts(1_500)).is_ok());
        assert_eq!(
            acquire(
                None,
                &LockRequest {
                    lease_until: ts(9_000),
                    ..request.clone()
                },
                ts(1_000)
            )
            .unwrap_err(),
            LockError::LeaseTooLong
        );
    }

    /// #122：engine / guardian 锁必须绑定交付单元，人工锁必须不绑定。
    #[test]
    fn worktree_lock_delivery_binding() {
        use worktree_lock::*;
        let delivery: DeliveryId = "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM".parse().unwrap();
        let request = |holder, delivery_id| LockRequest {
            holder,
            delivery_id,
            lease_until: ts(2_000),
            max_lease: None,
        };
        for holder in [LockHolder::Engine, LockHolder::Guardian] {
            let lock = acquire(None, &request(holder, Some(delivery)), ts(1_000)).unwrap();
            assert!(lock.validate().is_ok());
            assert_eq!(
                acquire(None, &request(holder, None), ts(1_000)).unwrap_err(),
                LockError::DeliveryBinding
            );
        }
        assert_eq!(
            acquire(None, &request(LockHolder::Human, Some(delivery)), ts(1_000)).unwrap_err(),
            LockError::DeliveryBinding
        );
        let mut lock = acquire(None, &request(LockHolder::Human, None), ts(1_000)).unwrap();
        lock.delivery_id = Some(delivery);
        assert_eq!(
            lock.validate(),
            Err(crate::entities::EntityError::LockDeliveryBinding)
        );
    }
}
