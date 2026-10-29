//! 失败分类与重试规则（worker-protocol §9；engine.md §7.8，#72）。
//!
//! [`classify`] 按 §9 的判定优先级自上而下、命中即止，只用服务端观察到的事实，不用 worker 自报。
//! [`retry_rule`] 给出 §7.8"失败处理"表对每个子码的处置。两者都是纯函数。

use gantry_core::entities::{AttemptStatus, FailureClass, Verdict};
use gantry_core::ports::sandbox::{TerminalCause, TerminalFacts, TimerKind};

/// V5 的结论（补丁步骤只在 V1–V4 通过且 `status = succeeded` 时执行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchCheck {
    /// 没有补丁、空补丁，或补丁已通过（`allow`）。
    Clean,
    /// 补丁不可应用或结构无效（P2 等）→ `exec.output_invalid`。
    Invalid,
    /// 机械拒绝或策略拒绝，`detail` 为 `policy.*` 子码（如 `policy.path_protected:workflows`）。
    Denied { detail: String },
    /// `repo` 判定 base 冲突（P10）：工作树分支头已前移，不自动 rebase。worker-protocol §9 的
    /// 词表没有对应子码；暂记 `infra.base_conflict`，按 `infra.*` 重新派发（engine.md §7.8 V5 行
    /// "用新的 `base_sha` 重新派发，计一次重试"）。
    BaseConflict,
}

/// 分类所需的全部事实：运行时事实来自 `JobOutcome`，其余是 V2–V5 的结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFacts {
    pub terminal: TerminalFacts,
    /// Egress 报告封顶切断。
    pub egress_capped: bool,
    /// Egress 记录了持续的上游错误（5xx / 429）。
    pub upstream_errors: bool,
    /// V2 intake 与 V3 `result.json` 都通过。
    pub protocol_ok: bool,
    /// `result.status = failed`。
    pub result_failed: bool,
    /// V4 `structured_output` 通过。
    pub output_ok: bool,
    pub patch: PatchCheck,
    /// 判定节点的 verdict；非判定节点为 `None`。
    pub verdict: Option<Verdict>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classification {
    pub status: AttemptStatus,
    pub class: Option<FailureClass>,
    /// `failure_detail` 子码（worker-protocol §9）。
    pub detail: Option<String>,
}

impl Classification {
    fn ok() -> Self {
        Self {
            status: AttemptStatus::Succeeded,
            class: None,
            detail: None,
        }
    }

    fn fail(class: FailureClass, detail: impl Into<String>) -> Self {
        let status = if class == FailureClass::Cancelled {
            AttemptStatus::Cancelled
        } else {
            AttemptStatus::Failed
        };
        Self {
            status,
            class: Some(class),
            detail: Some(detail.into()),
        }
    }
}

/// worker-protocol §9 的判定优先级（自上而下，命中即止）。
pub fn classify(facts: &RunFacts) -> Classification {
    let t = &facts.terminal;
    // 1. 已记录取消请求 → cancelled。取消与计时器同时发生时先记录者胜：计时器先杀则算 timeout。
    if t.cause == TerminalCause::KilledCancel
        || (t.cancel_requested && t.cause != TerminalCause::KilledTimer)
    {
        return Classification::fail(FailureClass::Cancelled, "cancel.requested");
    }
    // 2. 墙钟或空闲计时器触发 → timeout。
    if t.cause == TerminalCause::KilledTimer || t.timer.is_some() {
        let detail = match t.timer {
            Some(TimerKind::Idle) => "timeout.idle",
            _ => "timeout.wall",
        };
        return Classification::fail(FailureClass::Timeout, detail);
    }
    // 沙盒丢失（恢复时已不存在）→ 新沙盒重试（§7.13）。
    if t.cause == TerminalCause::Lost {
        return Classification::fail(FailureClass::InfraError, "infra.lost");
    }
    // 3. 运行时报告 OOM / 异常信号。
    if t.oom {
        return Classification::fail(FailureClass::ExecFailed, "exec.oom");
    }
    if t.cause == TerminalCause::KilledRuntime || t.signal.is_some() {
        return Classification::fail(FailureClass::ExecFailed, "exec.killed");
    }
    // 4. Egress 封顶切断 → budget.tokens。
    if facts.egress_capped {
        return Classification::fail(FailureClass::BudgetExhausted, "budget.tokens");
    }
    // 5. 退出码 ≠ 0。上游持续出错时细分为 exec.model_api（result.error 只作线索）。
    match t.exit_code {
        Some(0) => {}
        Some(10) => return Classification::fail(FailureClass::ExecFailed, "exec.bad_job"),
        Some(12) => return Classification::fail(FailureClass::ExecFailed, "exec.killed"),
        _ if facts.upstream_errors => {
            return Classification::fail(FailureClass::ExecFailed, "exec.model_api")
        }
        _ => return Classification::fail(FailureClass::ExecFailed, "exec.worker_crash"),
    }
    // 6. 退出码 0 但 V2 / V3 失败。
    if !facts.protocol_ok {
        return Classification::fail(FailureClass::ExecFailed, "exec.protocol");
    }
    // 7. result.status = failed。
    if facts.result_failed {
        let detail = if facts.upstream_errors {
            "exec.model_api"
        } else {
            "exec.agent_failed"
        };
        return Classification::fail(FailureClass::ExecFailed, detail);
    }
    // 8. V4 失败 → exec.output_invalid；V5 失败 → policy.* / exec.output_invalid。
    if !facts.output_ok {
        return Classification::fail(FailureClass::ExecFailed, "exec.output_invalid");
    }
    match &facts.patch {
        PatchCheck::Clean => {}
        PatchCheck::Invalid => {
            return Classification::fail(FailureClass::ExecFailed, "exec.output_invalid")
        }
        PatchCheck::Denied { detail } => {
            return Classification::fail(FailureClass::PolicyDenied, detail.clone())
        }
        PatchCheck::BaseConflict => {
            return Classification::fail(FailureClass::InfraError, "infra.base_conflict")
        }
    }
    // 9. gate 的 verdict ≠ ACCEPT → verdict.*（不是重试，按 §7.5 进入图的迭代）。
    match facts.verdict {
        Some(Verdict::Revise) => {
            return Classification::fail(FailureClass::VerdictFail, "verdict.revise")
        }
        Some(Verdict::Reject) => {
            return Classification::fail(FailureClass::VerdictFail, "verdict.reject")
        }
        Some(Verdict::Blocked) => {
            return Classification::fail(FailureClass::VerdictFail, "verdict.blocked")
        }
        Some(Verdict::Accept) | None => {}
    }
    // 10. 否则成功。
    Classification::ok()
}

/// §7.8"失败处理"表：失败后是否自动重试。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryRule {
    /// 受 `MAX_ATTEMPTS_PER_NODE` 约束地重试。
    UpToMax,
    /// 同一子码只重试 1 次，仍失败则上报。
    Once,
    /// 不重试：节点失败并上报。
    Never,
    /// 不是失败（取消、verdict）：不重试、不上报。
    NotAFailure,
}

pub fn retry_rule(detail: &str) -> RetryRule {
    match detail {
        "cancel.requested" => RetryRule::NotAFailure,
        d if d.starts_with("verdict.") => RetryRule::NotAFailure,
        "timeout.wall" => RetryRule::Once,
        "timeout.idle" => RetryRule::UpToMax,
        "exec.oom" | "exec.bad_job" | "budget.tokens" | "infra.job_identity_unavailable" => {
            RetryRule::Never
        }
        "exec.killed" | "exec.worker_crash" | "exec.protocol" | "exec.model_api" => {
            RetryRule::UpToMax
        }
        "exec.agent_failed" => RetryRule::Once,
        "exec.output_invalid" => RetryRule::UpToMax,
        d if d.starts_with("infra.") => RetryRule::UpToMax,
        // 受保护路径（任何里程碑的 workflows，与 M1 的其他受保护规则）：不重试，上报。
        d if d.starts_with("policy.path_protected:") => RetryRule::Never,
        // 其他 policy.*：带"缩小改动"反馈重试 1 次，仍被拒则上报（OQ-15）。
        d if d.starts_with("policy.") => RetryRule::Once,
        _ => RetryRule::Never,
    }
}
