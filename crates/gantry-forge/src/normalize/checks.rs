//! 当前 head 的检查汇总（forge.md §7.5 第 1 点；event-data §2.2.5.1 `checks`）。
//!
//! 数据来自 check runs（`GET /commits/{ref}/check-runs`，`filter=latest`）与传统 commit
//! statuses（`GET /commits/{ref}/statuses`）。required 信息本片不读取，按"所有观察到的检查
//! 都必需"保守计算；没有观察到任何检查时 `required_ok=false`，不把空结果当作通过
//! （F-OQ-16）。

use std::collections::BTreeSet;

use gantry_core::canon::GitOid;

use gantry_core::entities::CheckSummary;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Passed,
    Failed,
    Pending,
}

pub(crate) fn summary(
    check_runs: &[Value],
    statuses: &[Value],
    head: &GitOid,
) -> Result<CheckSummary, String> {
    for run in check_runs {
        validate_run(run, head)?;
    }
    for status in statuses {
        validate_status(status, head)?;
    }
    let mut summary = CheckSummary {
        passed: 0,
        failed: 0,
        pending: 0,
        required_ok: false,
    };
    // `filter=latest` 的服务端结果即最新集合；客户端不再按 app+name 二次合并。
    for run in check_runs {
        count(&mut summary, run_outcome(run));
    }
    // 官方按创建时间倒序返回 statuses（最新在前）：同 `context` 只取第一条。
    let mut contexts = BTreeSet::new();
    for status in statuses {
        if contexts.insert(status["context"].as_str().unwrap_or_default()) {
            count(&mut summary, status_outcome(status));
        }
    }
    // required 规则不可得：全部观察到的检查都必需；空集合显式不算通过。
    summary.required_ok = summary
        .passed
        .saturating_add(summary.failed)
        .saturating_add(summary.pending)
        > 0
        && summary.failed == 0
        && summary.pending == 0;
    Ok(summary)
}

/// 官方 schema 的必填字段缺失时整体失败，不把 malformed run 当作通过。
fn validate_run(run: &Value, head: &GitOid) -> Result<(), String> {
    if run.get("id").and_then(Value::as_u64).is_none() {
        return Err("check run lacks id".into());
    }
    if run.get("name").and_then(Value::as_str).is_none() {
        return Err("check run lacks name".into());
    }
    if run.get("status").and_then(Value::as_str).is_none() {
        return Err("check run lacks status".into());
    }
    if run.get("conclusion").is_none() {
        return Err("check run lacks conclusion".into());
    }
    let run_head = run
        .get("head_sha")
        .and_then(Value::as_str)
        .ok_or_else(|| "check run lacks head_sha".to_owned())?;
    let run_head =
        GitOid::parse(run_head).map_err(|_| "check run has invalid head_sha".to_owned())?;
    if run_head != *head {
        return Err(format!(
            "check run head_sha {run_head} does not match requested head {head}"
        ));
    }
    if run
        .pointer("/check_suite/id")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err("check run lacks check_suite.id".into());
    }
    match run.get("app") {
        Some(Value::Null) => {}
        Some(app) if app.get("id").and_then(Value::as_u64).is_some() => {}
        Some(_) => return Err("check run app lacks id".into()),
        None => return Err("check run lacks app".into()),
    }
    Ok(())
}

/// 传统 commit status 的必填字段缺失或 ref 不一致时整体失败。
fn validate_status(status: &Value, head: &GitOid) -> Result<(), String> {
    if status.get("id").and_then(Value::as_u64).is_none() {
        return Err("commit status lacks id".into());
    }
    if status.get("context").and_then(Value::as_str).is_none() {
        return Err("commit status lacks context".into());
    }
    if status.get("state").and_then(Value::as_str).is_none() {
        return Err("commit status lacks state".into());
    }
    let sha = status
        .get("sha")
        .and_then(Value::as_str)
        .ok_or_else(|| "commit status lacks sha".to_owned())?;
    let sha = GitOid::parse(sha).map_err(|_| "commit status has invalid sha".to_owned())?;
    if sha != *head {
        return Err(format!(
            "commit status sha {sha} does not match requested head {head}"
        ));
    }
    Ok(())
}

fn count(summary: &mut CheckSummary, outcome: Outcome) {
    match outcome {
        Outcome::Passed => summary.passed = summary.passed.saturating_add(1),
        Outcome::Failed => summary.failed = summary.failed.saturating_add(1),
        Outcome::Pending => summary.pending = summary.pending.saturating_add(1),
    }
}

fn run_outcome(run: &Value) -> Outcome {
    match run.get("status").and_then(Value::as_str) {
        Some("completed") => match run.get("conclusion").and_then(Value::as_str) {
            Some("success" | "neutral" | "skipped") => Outcome::Passed,
            // failure / cancelled / timed_out / action_required / stale，以及未知结论和 null：保守失败。
            _ => Outcome::Failed,
        },
        Some("queued" | "in_progress" | "waiting" | "requested" | "pending") => Outcome::Pending,
        // 未知 status（缺字段 / 新枚举值）：保守失败，不当作 pending 或 success。
        _ => Outcome::Failed,
    }
}

fn status_outcome(status: &Value) -> Outcome {
    match status.get("state").and_then(Value::as_str) {
        Some("success") => Outcome::Passed,
        Some("pending") => Outcome::Pending,
        // failure / error，以及未知 state：保守失败。
        _ => Outcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::summary;

    fn head() -> gantry_core::canon::GitOid {
        gantry_core::canon::GitOid::parse("1111111111111111111111111111111111111111").unwrap()
    }

    fn run(app: serde_json::Value) -> serde_json::Value {
        json!({
            "id": 1,
            "name": "test",
            "status": "completed",
            "conclusion": "success",
            "head_sha": "1111111111111111111111111111111111111111",
            "check_suite": {"id": 1},
            "app": app,
        })
    }

    fn status(id: u64, context: &str, state: &str) -> serde_json::Value {
        json!({
            "id": id,
            "context": context,
            "state": state,
            "sha": "1111111111111111111111111111111111111111",
        })
    }

    #[test]
    fn null_app_is_valid_and_malformed_fields_fail_closed() {
        assert!(
            summary(&[run(serde_json::Value::Null)], &[], &head())
                .unwrap()
                .required_ok
        );
        let mut missing_app = run(serde_json::Value::Null);
        missing_app.as_object_mut().unwrap().remove("app");
        assert!(summary(&[missing_app], &[], &head()).is_err());
        let mut failure = run(serde_json::Value::Null);
        failure["id"] = json!(2);
        failure["conclusion"] = json!("failure");
        let counts = summary(&[run(serde_json::Value::Null), failure], &[], &head()).unwrap();
        assert_eq!(
            (counts.passed, counts.failed),
            (1, 1),
            "null app must not merge"
        );

        let mut old_head = run(serde_json::Value::Null);
        old_head["head_sha"] = json!("2222222222222222222222222222222222222222");
        assert!(summary(&[old_head], &[], &head()).is_err());
        let mut missing_head = run(serde_json::Value::Null);
        missing_head.as_object_mut().unwrap().remove("head_sha");
        assert!(summary(&[missing_head], &[], &head()).is_err());
    }

    #[test]
    fn statuses_merge_with_runs_and_keep_the_latest_context() {
        let counts = summary(
            &[run(serde_json::Value::Null)],
            &[
                status(2, "ci/legacy", "failure"),
                status(1, "ci/legacy", "success"),
                status(3, "ci/pending", "pending"),
            ],
            &head(),
        )
        .unwrap();
        assert_eq!(
            (
                counts.passed,
                counts.failed,
                counts.pending,
                counts.required_ok
            ),
            (1, 1, 1, false),
            "latest status per context only"
        );

        let counts = summary(&[], &[status(1, "ci/legacy", "success")], &head()).unwrap();
        assert_eq!(
            (
                counts.passed,
                counts.failed,
                counts.pending,
                counts.required_ok
            ),
            (1, 0, 0, true)
        );
    }

    #[test]
    fn malformed_and_old_head_statuses_fail_closed() {
        let mut old_head = status(1, "ci/legacy", "success");
        old_head["sha"] = json!("2222222222222222222222222222222222222222");
        assert!(summary(&[], &[old_head], &head()).is_err());
        let mut unknown = status(2, "ci/legacy", "mystery");
        unknown["state"] = json!("mystery");
        assert!(!summary(&[], &[unknown], &head()).unwrap().required_ok);
        let mut missing_context = status(3, "ci/legacy", "success");
        missing_context.as_object_mut().unwrap().remove("context");
        assert!(summary(&[], &[missing_context], &head()).is_err());
    }
}
