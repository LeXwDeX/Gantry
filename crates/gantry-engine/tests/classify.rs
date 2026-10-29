//! worker-protocol §9 失败分类优先级（engine.md §12.2"失败分类优先级"，#72）：表驱动覆盖 10 条。

use gantry_core::entities::{AttemptStatus, FailureClass, Verdict};
use gantry_core::ports::sandbox::{TerminalCause, TerminalFacts, TimerKind};
use gantry_engine::classify::{classify, retry_rule, PatchCheck, RetryRule, RunFacts};

fn exited(code: i32) -> TerminalFacts {
    TerminalFacts {
        cause: TerminalCause::Exited,
        exit_code: Some(code),
        signal: None,
        oom: false,
        timer: None,
        cancel_requested: false,
    }
}

fn ok() -> RunFacts {
    RunFacts {
        terminal: exited(0),
        egress_capped: false,
        upstream_errors: false,
        protocol_ok: true,
        result_failed: false,
        output_ok: true,
        patch: PatchCheck::Clean,
        verdict: None,
    }
}

fn detail(facts: &RunFacts) -> Option<String> {
    classify(facts).detail
}

#[test]
fn priority_table() {
    // 每行：(说明, 构造事实, 期望子码)。后一行的事实在前一行的基础上"多了一个更低优先级的问题"，
    // 用来证明更高优先级的规则先命中。
    let mut cases: Vec<(&str, RunFacts, Option<&str>)> = Vec::new();

    // 10. 否则成功。
    cases.push(("10 success", ok(), None));
    // 9. verdict ≠ ACCEPT。
    let mut f = ok();
    f.verdict = Some(Verdict::Revise);
    cases.push(("9 verdict", f.clone(), Some("verdict.revise")));
    // 8b. V5 策略拒绝优先于 verdict。
    f.patch = PatchCheck::Denied {
        detail: "policy.path_protected:workflows".into(),
    };
    cases.push((
        "8 V5 deny",
        f.clone(),
        Some("policy.path_protected:workflows"),
    ));
    // 8a. V4 失败优先于 V5。
    f.output_ok = false;
    cases.push(("8 V4", f.clone(), Some("exec.output_invalid")));
    // 7. result.status = failed 优先于 V4。
    f.result_failed = true;
    cases.push(("7 agent_failed", f.clone(), Some("exec.agent_failed")));
    // 6. 退出 0 但 V2 / V3 失败优先于 result.status。
    f.protocol_ok = false;
    cases.push(("6 protocol", f.clone(), Some("exec.protocol")));
    // 5. 退出码 ≠ 0 优先于 V2 / V3。
    f.terminal = exited(1);
    cases.push(("5 worker_crash", f.clone(), Some("exec.worker_crash")));
    let mut g = f.clone();
    g.terminal = exited(10);
    cases.push(("5 bad_job", g.clone(), Some("exec.bad_job")));
    g.terminal = exited(12);
    cases.push(("5 exit 12 → killed", g, Some("exec.killed")));
    // 4. Egress 封顶优先于退出码。
    f.egress_capped = true;
    cases.push(("4 budget", f.clone(), Some("budget.tokens")));
    // 3. OOM / 异常信号优先于封顶。
    f.terminal.oom = true;
    cases.push(("3 oom", f.clone(), Some("exec.oom")));
    let mut g = f.clone();
    g.terminal.oom = false;
    g.terminal.cause = TerminalCause::KilledRuntime;
    g.terminal.signal = Some(9);
    cases.push(("3 signal", g, Some("exec.killed")));
    // 2. 计时器优先于 OOM。
    f.terminal.cause = TerminalCause::KilledTimer;
    f.terminal.timer = Some(TimerKind::Wall);
    cases.push(("2 wall", f.clone(), Some("timeout.wall")));
    let mut g = f.clone();
    g.terminal.timer = Some(TimerKind::Idle);
    cases.push(("2 idle", g, Some("timeout.idle")));
    // 1. 取消优先于一切（取消先被记录时）。
    f.terminal.cause = TerminalCause::KilledCancel;
    f.terminal.cancel_requested = true;
    cases.push(("1 cancel", f, Some("cancel.requested")));

    for (name, facts, expected) in cases {
        assert_eq!(detail(&facts).as_deref(), expected, "{name}");
    }
}

/// 取消与超时同时发生：先记录者胜（§12.2）。
#[test]
fn cancel_and_timeout_first_recorded_wins() {
    let mut timer_first = ok();
    timer_first.terminal = TerminalFacts {
        cause: TerminalCause::KilledTimer,
        exit_code: None,
        signal: None,
        oom: false,
        timer: Some(TimerKind::Wall),
        cancel_requested: true,
    };
    assert_eq!(detail(&timer_first).as_deref(), Some("timeout.wall"));

    let mut cancel_first = timer_first.clone();
    cancel_first.terminal.cause = TerminalCause::KilledCancel;
    let c = classify(&cancel_first);
    assert_eq!(c.detail.as_deref(), Some("cancel.requested"));
    assert_eq!(c.class, Some(FailureClass::Cancelled));
    assert_eq!(c.status, AttemptStatus::Cancelled);
}

/// 上游持续出错时细分为 exec.model_api；丢失的沙盒为 infra.lost。
#[test]
fn model_api_and_lost() {
    let mut f = ok();
    f.terminal = exited(1);
    f.upstream_errors = true;
    assert_eq!(detail(&f).as_deref(), Some("exec.model_api"));
    let mut lost = ok();
    lost.terminal.cause = TerminalCause::Lost;
    lost.terminal.exit_code = None;
    assert_eq!(detail(&lost).as_deref(), Some("infra.lost"));
}

/// §7.8 表：每个子码的重试处置。
#[test]
fn retry_rules_follow_section_7_8() {
    let table = [
        ("cancel.requested", RetryRule::NotAFailure),
        ("timeout.wall", RetryRule::Once),
        ("timeout.idle", RetryRule::UpToMax),
        ("exec.oom", RetryRule::Never),
        ("exec.killed", RetryRule::UpToMax),
        ("exec.worker_crash", RetryRule::UpToMax),
        ("exec.protocol", RetryRule::UpToMax),
        ("exec.model_api", RetryRule::UpToMax),
        ("exec.bad_job", RetryRule::Never),
        ("exec.agent_failed", RetryRule::Once),
        ("exec.output_invalid", RetryRule::UpToMax),
        ("infra.lost", RetryRule::UpToMax),
        ("infra.job_identity_unavailable", RetryRule::Never),
        ("infra.sandbox_create", RetryRule::UpToMax),
        ("budget.tokens", RetryRule::Never),
        ("policy.path_protected:workflows", RetryRule::Never),
        ("policy.path_protected:lockfiles", RetryRule::Never),
        ("policy.size", RetryRule::Once),
        ("verdict.revise", RetryRule::NotAFailure),
    ];
    for (detail, rule) in table {
        assert_eq!(retry_rule(detail), rule, "{detail}");
    }
}
