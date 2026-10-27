//! worker.md §12.H（M1：`open_pr`、`escalate`；线程类的 target 规则一并断言）与 §12.C 的大小上限。

mod common;

use std::fs;

use common::{assert_valid, TempDir, JOB_SPEC, PROPOSALS};
use gantry_core::worker::{JobSpec, JobTarget, ProposalsMode, TargetKind};
use gantry_worker::artifacts::{write_proposals, write_report, MAX_REPORT_BYTES};
use gantry_worker::proposals::{filter_proposals, DropReason, MAX_PROPOSALS};
use serde_json::{json, Value};

fn job() -> JobSpec {
    serde_json::from_str(JOB_SPEC).unwrap()
}

fn open_pr(id: &str) -> Value {
    json!({"id": id, "kind": "open_pr", "target": null,
           "params": {"title": "t", "body": "b", "draft": false}, "rationale": "r"})
}

fn escalate(id: &str) -> Value {
    json!({"id": id, "kind": "escalate", "target": null,
           "params": {"inbox_kind": "decision_needed", "message": "m"}, "rationale": "r"})
}

fn reply(id: &str, target: &str) -> Value {
    json!({"id": id, "kind": "reply_review_thread", "target": target,
           "params": {"body": "b"}, "rationale": "r"})
}

#[test]
fn fixture_proposals_pass_unchanged() {
    let fixture: Value = serde_json::from_str(PROPOSALS).unwrap();
    let raw = fixture["proposals"].as_array().unwrap();
    let filtered = filter_proposals(&job(), raw);
    assert!(filtered.dropped.is_empty());
    let kept = filtered.proposals.unwrap();
    assert_eq!(serde_json::to_value(&kept).unwrap(), fixture);
    assert_eq!(kept.job_id, job().job_id);
}

#[test]
fn disallowed_kinds_bad_targets_and_excess_are_dropped_others_kept() {
    let mut job = job();
    job.targets = vec![JobTarget {
        handle: "t1".into(),
        kind: TargetKind::ReviewThread,
        reference: "PRRT_x".into(),
    }];
    // fixture：allowed_kinds = [open_pr, escalate]，max = 3。
    let raw = vec![
        open_pr("p1"),
        // 线程类不在 allowed_kinds（即使 target 合法）。
        reply("p2", "t1"),
        // open_pr 带 target。
        json!({"id": "p3", "kind": "open_pr", "target": "t1",
               "params": {"title": "t", "body": "b", "draft": false}, "rationale": "r"}),
        // 危险动作与未知 kind：类型层面不存在，反序列化即拒。
        json!({"id": "p4", "kind": "merge_pr", "target": null, "params": {}, "rationale": "r"}),
        json!({"id": "p5", "kind": "force_push", "target": null, "params": {}, "rationale": "r"}),
        json!({"id": "p6", "kind": "create_comment", "target": null, "params": {"body": "x"}, "rationale": "r"}),
        // params 多余字段。
        json!({"id": "p7", "kind": "open_pr", "target": null,
               "params": {"title": "t", "body": "b", "draft": false, "base": "main"}, "rationale": "r"}),
        escalate("p1"),
        escalate("p8"),
        escalate("p9"),
        escalate("p10"),
    ];
    let filtered = filter_proposals(&job, &raw);
    let kept: Vec<String> = filtered
        .proposals
        .unwrap()
        .proposals
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(kept, ["p1", "p8", "p9"]);
    assert_eq!(
        filtered.dropped,
        [
            (1, DropReason::KindNotAllowed),
            (2, DropReason::BadTarget),
            (3, DropReason::Malformed),
            (4, DropReason::Malformed),
            (5, DropReason::Malformed),
            (6, DropReason::Malformed),
            (7, DropReason::DuplicateId),
            (10, DropReason::OverCount),
        ]
    );
}

#[test]
fn thread_targets_must_be_review_thread_handles_from_job() {
    let mut job = job();
    job.output.proposals = ProposalsMode::Allowed {
        allowed_kinds: vec![gantry_core::worker::ProposalKind::ReplyReviewThread],
        max: 10,
    };
    job.targets = vec![
        JobTarget {
            handle: "t1".into(),
            kind: TargetKind::ReviewThread,
            reference: "PRRT_x".into(),
        },
        JobTarget {
            handle: "pr".into(),
            kind: TargetKind::Pr,
            reference: "acme/widgets#7".into(),
        },
    ];
    // worker 臆造的编号 / node_id、非线程句柄、缺 target 都丢弃。
    let raw = vec![
        reply("a", "t1"),
        reply("b", "PRRT_kwDOAbc"),
        reply("c", "#42"),
        reply("d", "pr"),
        json!({"id": "e", "kind": "reply_review_thread", "target": null,
               "params": {"body": "b"}, "rationale": "r"}),
    ];
    let filtered = filter_proposals(&job, &raw);
    assert_eq!(filtered.proposals.unwrap().proposals.len(), 1);
    assert_eq!(
        filtered.dropped,
        [
            (1, DropReason::BadTarget),
            (2, DropReason::BadTarget),
            (3, DropReason::BadTarget),
            (4, DropReason::Malformed),
        ]
    );
}

#[test]
fn forbidden_mode_and_empty_input_produce_no_file() {
    let mut job = job();
    job.output.proposals = ProposalsMode::Forbidden;
    let filtered = filter_proposals(&job, &[open_pr("p1")]);
    assert!(filtered.proposals.is_none());
    assert_eq!(filtered.dropped, [(0, DropReason::KindNotAllowed)]);
    assert!(filter_proposals(&self::job(), &[]).proposals.is_none());
}

#[test]
fn count_is_capped_at_twenty_even_if_max_is_larger() {
    let mut job = job();
    job.output.proposals = ProposalsMode::Allowed {
        allowed_kinds: vec![gantry_core::worker::ProposalKind::Escalate],
        max: 1000,
    };
    let raw: Vec<Value> = (0..30).map(|i| escalate(&format!("p{i}"))).collect();
    let filtered = filter_proposals(&job, &raw);
    assert_eq!(filtered.proposals.unwrap().proposals.len(), MAX_PROPOSALS);
    assert_eq!(filtered.dropped.len(), 30 - MAX_PROPOSALS);
}

#[test]
fn oversize_proposals_are_dropped_from_the_tail_and_file_stays_under_limit() {
    let mut job = job();
    job.output.proposals = ProposalsMode::Allowed {
        allowed_kinds: vec![gantry_core::worker::ProposalKind::Escalate],
        max: 20,
    };
    let big = "x".repeat(100 * 1024);
    let raw: Vec<Value> = (0..4)
        .map(|i| {
            json!({"id": format!("p{i}"), "kind": "escalate", "target": null,
                   "params": {"inbox_kind": "escalation", "message": big}, "rationale": "r"})
        })
        .collect();
    let filtered = filter_proposals(&job, &raw);
    let kept = filtered.proposals.unwrap();
    assert_eq!(kept.proposals.len(), 2);
    assert_eq!(
        filtered.dropped,
        [(2, DropReason::OverSize), (3, DropReason::OverSize)]
    );
    let out = TempDir::new("proposals");
    let reference = write_proposals(out.path(), &kept).unwrap();
    assert!(reference.bytes as usize <= 256 * 1024);
    let value: Value =
        serde_json::from_slice(&fs::read(out.path().join("proposals.json")).unwrap()).unwrap();
    assert_valid("proposals.schema.json", &value);
}

#[test]
fn report_is_truncated_at_limit_on_char_boundary() {
    let out = TempDir::new("report");
    let (small, truncated) = write_report(out.path(), "# ok\n").unwrap();
    assert!(!truncated);
    assert_eq!(small.bytes, 5);

    let huge = "界".repeat(MAX_REPORT_BYTES);
    let (reference, truncated) = write_report(out.path(), &huge).unwrap();
    assert!(truncated);
    let bytes = fs::read(out.path().join("report.md")).unwrap();
    assert!(bytes.len() <= MAX_REPORT_BYTES);
    assert_eq!(reference.bytes as usize, bytes.len());
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.ends_with("[report truncated by gantry-worker]\n"));
}
