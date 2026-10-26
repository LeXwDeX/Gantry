//! `docs/interfaces/forge-drift.md` §4.5 的正负例。
//!
//! 每个用例走 `DriftPayload` 的严格解码；未知 kind、分支错配、未知字段和缺必填
//! 都必须失败。

use std::collections::BTreeSet;

use gantry_core::forge::{
    DriftPayload, DriftSynthesizedEvent, ForgeActor, ForgeText, PrTerminalState,
};
use gantry_core::ports::forge::{DriftKind, IssueReadback, PullRequestReadback};
use serde_json::{json, Value};

const DELIVERY_ID: &str = "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM";
const ACTION_ID: &str = "act_01JBA1H5T7XA3P9C2E6J8M1QWD";
const HEAD_BEFORE: &str = "f2c03d84a858156b0121130fd1ec2f391aa90865";
const HEAD_AFTER: &str = "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b";
const DIGEST_A: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DIGEST_B: &str = "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn pr_target() -> Value {
    json!({"type": "pr", "repo": "acme/shop-api", "number": 57})
}

fn issue_target() -> Value {
    json!({"type": "issue", "repo": "acme/shop-api", "number": 42})
}

fn actor() -> Value {
    json!({
        "login": "alice",
        "github_id": 1234567,
        "type": "user",
        "association": null
    })
}

fn base(kind: &str, target: Value, expected: Value, actual: Value, delivery_id: Value) -> Value {
    json!({
        "kind": kind,
        "delivery_id": delivery_id,
        "target": target,
        "expected": expected,
        "actual": actual,
        "resolution": "github_wins",
        "synthesized_events": [],
        "detected_by": "patrol"
    })
}

fn pr_state_closed() -> Value {
    let mut value = base(
        "pr_state_diverged",
        pr_target(),
        json!({"state": "open"}),
        json!({"state": "closed"}),
        json!(DELIVERY_ID),
    );
    value["synthesized_events"] = json!(["pr_closed"]);
    value
}

fn pr_state_merged() -> Value {
    let mut value = base(
        "pr_state_diverged",
        pr_target(),
        json!({"state": "open"}),
        json!({"state": "merged", "merge_commit_sha": HEAD_AFTER}),
        json!(DELIVERY_ID),
    );
    value["synthesized_events"] = json!(["pr_merged"]);
    value
}

fn head_diverged() -> Value {
    let mut value = base(
        "head_diverged",
        pr_target(),
        json!({"head_sha": HEAD_BEFORE, "basis_action": ACTION_ID}),
        json!({"head_sha": HEAD_AFTER, "pushed_by": actor()}),
        json!(DELIVERY_ID),
    );
    value["synthesized_events"] = json!(["pr_synchronized"]);
    value
}

fn effect_missing_status_comment() -> Value {
    base(
        "effect_missing",
        pr_target(),
        json!({"effect": "status_comment", "action_id": ACTION_ID, "render_digest": DIGEST_A}),
        json!({"observation": "content_changed", "render_digest": DIGEST_B}),
        json!(DELIVERY_ID),
    )
}

fn effect_missing_branch() -> Value {
    base(
        "effect_missing",
        json!({"type": "branch", "repo": "acme/shop-api", "branch": "gantry/d-1"}),
        json!({"effect": "branch", "action_id": ACTION_ID}),
        json!({"observation": "missing"}),
        json!(DELIVERY_ID),
    )
}

fn effect_missing_pull_request() -> Value {
    base(
        "effect_missing",
        pr_target(),
        json!({"effect": "pull_request", "action_id": ACTION_ID, "state": "open"}),
        json!({"observation": "state_changed", "state": "closed"}),
        json!(DELIVERY_ID),
    )
}

fn orphan_effect() -> Value {
    let mut value = base(
        "orphan_effect",
        pr_target(),
        json!({"outbox_action": null}),
        json!({
            "object": "comment",
            "github_id": 9001,
            "node_id": "IC_kwDOAbc123",
            "marker": "<!-- gantry:act_01JBA1H5T7XA3P9C2E6J8M1QWD -->"
        }),
        Value::Null,
    );
    value["detected_by"] = json!("on_demand");
    value
}

fn issue_state_closed() -> Value {
    let mut value = base(
        "issue_state_diverged",
        issue_target(),
        json!({"state": "open"}),
        json!({"observation": "state", "state": "closed", "state_reason": "completed"}),
        json!(DELIVERY_ID),
    );
    value["synthesized_events"] = json!(["issue_closed"]);
    value
}

fn issue_state_reopened() -> Value {
    let mut value = base(
        "issue_state_diverged",
        issue_target(),
        json!({"state": "closed"}),
        json!({"observation": "state", "state": "open"}),
        json!(DELIVERY_ID),
    );
    value["synthesized_events"] = json!(["issue_reopened"]);
    value
}

fn issue_state_deleted() -> Value {
    base(
        "issue_state_diverged",
        issue_target(),
        json!({"state": "open"}),
        json!({"observation": "deleted"}),
        json!(DELIVERY_ID),
    )
}

fn issue_state_transferred() -> Value {
    base(
        "issue_state_diverged",
        issue_target(),
        json!({"state": "open"}),
        json!({"observation": "transferred", "to_repo": "acme/other", "new_number": 88}),
        json!(DELIVERY_ID),
    )
}

fn review_state_diverged() -> Value {
    base(
        "review_state_diverged",
        pr_target(),
        json!({"review_state": "approved"}),
        json!({"review_state": "changes_requested"}),
        json!(DELIVERY_ID),
    )
}

fn merge_state_diverged() -> Value {
    let mut value = base(
        "merge_state_diverged",
        pr_target(),
        json!({"in_merge_queue": true}),
        json!({"in_merge_queue": false, "reason": "checks failed"}),
        json!(DELIVERY_ID),
    );
    value["synthesized_events"] = json!(["pr_dequeued"]);
    value
}

fn default_branch_changed() -> Value {
    let mut value = base(
        "default_branch_changed",
        json!({"type": "repo", "repo": "acme/shop-api"}),
        json!({"default_branch": "main"}),
        json!({"default_branch": "master", "renamed_from": "main"}),
        Value::Null,
    );
    value["detected_by"] = json!("startup_sweep");
    value
}

fn installation_unreachable() -> Value {
    let mut value = base(
        "installation_unreachable",
        json!({"type": "installation", "installation_id": 12345678}),
        json!({"reachable": true}),
        json!({"reachable": false, "code": "token_exchange_failed", "http_status": 401}),
        Value::Null,
    );
    value["detected_by"] = json!("recovery");
    value
}

fn assert_roundtrip(value: Value, kind: &str) {
    let payload = DriftPayload::from_value(value.clone())
        .unwrap_or_else(|error| panic!("{kind} must decode: {error}"));
    assert_eq!(payload.kind().as_str(), kind);
    payload
        .validate()
        .unwrap_or_else(|error| panic!("{kind} must validate: {error}"));
    assert_eq!(
        payload.to_value().expect("payload must serialize"),
        value,
        "{kind} must roundtrip"
    );
}

fn assert_rejected(value: Value, case: &str) {
    assert!(
        DriftPayload::from_value(value).is_err(),
        "{case} must be rejected"
    );
}

#[test]
fn nine_drift_kinds_roundtrip() {
    for (kind, value) in [
        ("pr_state_diverged", pr_state_closed()),
        ("head_diverged", head_diverged()),
        ("effect_missing", effect_missing_status_comment()),
        ("orphan_effect", orphan_effect()),
        ("issue_state_diverged", issue_state_closed()),
        ("review_state_diverged", review_state_diverged()),
        ("merge_state_diverged", merge_state_diverged()),
        ("default_branch_changed", default_branch_changed()),
        ("installation_unreachable", installation_unreachable()),
    ] {
        assert_roundtrip(value, kind);
    }
}

#[test]
fn branch_and_optional_variants_roundtrip() {
    for (kind, value) in [
        ("pr_state_diverged", pr_state_merged()),
        ("effect_missing", effect_missing_branch()),
        ("effect_missing", effect_missing_pull_request()),
        ("issue_state_diverged", issue_state_reopened()),
        ("issue_state_diverged", issue_state_deleted()),
        ("issue_state_diverged", issue_state_transferred()),
    ] {
        assert_roundtrip(value, kind);
    }

    let mut merged_without_sha = pr_state_merged();
    merged_without_sha["actual"] = json!({"state": "merged"});
    merged_without_sha["synthesized_events"] = json!([]);
    assert_roundtrip(merged_without_sha, "pr_state_diverged");

    let mut dequeued_without_reason = merge_state_diverged();
    dequeued_without_reason["actual"] = json!({"in_merge_queue": false});
    assert_roundtrip(dequeued_without_reason, "merge_state_diverged");

    let mut null_pushed_by = head_diverged();
    null_pushed_by["actual"]["pushed_by"] = Value::Null;
    null_pushed_by["synthesized_events"] = json!([]);
    assert_roundtrip(null_pushed_by, "head_diverged");

    let mut closed_without_event = pr_state_closed();
    closed_without_event["synthesized_events"] = json!([]);
    assert_roundtrip(closed_without_event, "pr_state_diverged");
}

#[test]
fn common_negative_cases_are_rejected() {
    let valid = pr_state_closed();

    let mut unknown_kind = valid.clone();
    unknown_kind["kind"] = json!("repo_changed");
    assert_rejected(unknown_kind, "unknown kind");

    let mut target_mismatch = valid.clone();
    target_mismatch["target"] = issue_target();
    assert_rejected(target_mismatch, "kind/target mismatch");

    let mut expected_null = valid.clone();
    expected_null["expected"] = Value::Null;
    assert_rejected(expected_null, "null expected");

    let mut actual_null = valid.clone();
    actual_null["actual"] = Value::Null;
    assert_rejected(actual_null, "null actual");

    let mut bad_resolution = valid.clone();
    bad_resolution["resolution"] = json!("manual");
    assert_rejected(bad_resolution, "unknown resolution");

    let mut bad_detected_by = valid.clone();
    bad_detected_by["detected_by"] = json!("scheduled");
    assert_rejected(bad_detected_by, "unknown detected_by");

    for events in [
        json!(["pr_updated"]),
        json!(["forge.pr_closed"]),
        json!(["unknown_event"]),
        json!(["pr_closed", "pr_closed"]),
    ] {
        let mut value = valid.clone();
        value["synthesized_events"] = events;
        assert_rejected(value, "bad synthesized_events");
    }

    let mut top_level_extra = valid.clone();
    top_level_extra["extra"] = json!(1);
    assert_rejected(top_level_extra, "unknown top-level field");

    let mut expected_extra = valid.clone();
    expected_extra["expected"]["extra"] = json!(1);
    assert_rejected(expected_extra, "unknown expected field");

    let mut actual_extra = valid.clone();
    actual_extra["actual"]["extra"] = json!(1);
    assert_rejected(actual_extra, "unknown actual field");

    let mut target_extra = valid.clone();
    target_extra["target"]["extra"] = json!(1);
    assert_rejected(target_extra, "unknown target field");

    for field in [
        "kind",
        "delivery_id",
        "target",
        "expected",
        "actual",
        "resolution",
        "synthesized_events",
        "detected_by",
    ] {
        let mut value = valid.clone();
        value.as_object_mut().expect("object").remove(field);
        assert_rejected(value, "missing required field");
    }

    let mut missing_repo = valid.clone();
    missing_repo["target"] = json!({"type": "pr", "number": 57});
    assert_rejected(missing_repo, "pr target without repo");

    let mut missing_number = valid.clone();
    missing_number["target"] = json!({"type": "pr", "repo": "acme/shop-api"});
    assert_rejected(missing_number, "pr target without number");

    let mut null_delivery = valid.clone();
    null_delivery["delivery_id"] = Value::Null;
    assert_rejected(null_delivery, "pr_state_diverged requires delivery_id");
}

#[test]
fn pr_state_diverged_negative_cases_are_rejected() {
    let mut expected_closed = pr_state_closed();
    expected_closed["expected"]["state"] = json!("closed");
    assert_rejected(expected_closed, "expected.state must be open");

    let mut missing_actual_state = pr_state_closed();
    missing_actual_state["actual"] = json!({});
    assert_rejected(missing_actual_state, "actual.state is required");

    let mut merged_as_closed = pr_state_merged();
    merged_as_closed["synthesized_events"] = json!(["pr_closed"]);
    assert_rejected(merged_as_closed, "merged cannot synthesize pr_closed");

    let mut merged_without_sha = pr_state_merged();
    merged_without_sha["actual"] = json!({"state": "merged"});
    merged_without_sha["synthesized_events"] = json!(["pr_merged"]);
    assert_rejected(merged_without_sha, "pr_merged requires merge_commit_sha");

    let mut closed_with_merge_sha = pr_state_closed();
    closed_with_merge_sha["actual"]["merge_commit_sha"] = json!(HEAD_AFTER);
    assert_rejected(
        closed_with_merge_sha,
        "closed must not carry merge_commit_sha",
    );
}

#[test]
fn head_diverged_negative_cases_are_rejected() {
    let mut same_head = head_diverged();
    same_head["actual"]["head_sha"] = json!(HEAD_BEFORE);
    assert_rejected(same_head, "same head_sha");

    let mut null_pushed_by = head_diverged();
    null_pushed_by["actual"]["pushed_by"] = Value::Null;
    assert_rejected(null_pushed_by, "null pushed_by cannot synthesize");

    let mut missing_pushed_by = head_diverged();
    missing_pushed_by["actual"]
        .as_object_mut()
        .expect("object")
        .remove("pushed_by");
    assert_rejected(missing_pushed_by, "pushed_by field is required");

    let mut bad_action = head_diverged();
    bad_action["expected"]["basis_action"] = json!("act_short");
    assert_rejected(bad_action, "basis_action must be act_ id");

    let mut short_sha = head_diverged();
    short_sha["expected"]["head_sha"] = json!("abc123");
    assert_rejected(short_sha, "short head_sha");

    let mut null_delivery = head_diverged();
    null_delivery["delivery_id"] = Value::Null;
    assert_rejected(null_delivery, "head_diverged requires delivery_id");
}

#[test]
fn effect_missing_negative_cases_are_rejected() {
    let mut state_changed_on_status_comment = effect_missing_status_comment();
    state_changed_on_status_comment["actual"] =
        json!({"observation": "state_changed", "state": "closed"});
    assert_rejected(
        state_changed_on_status_comment,
        "status_comment cannot use state_changed",
    );

    let mut content_changed_on_branch = effect_missing_branch();
    content_changed_on_branch["actual"] =
        json!({"observation": "content_changed", "render_digest": DIGEST_B});
    assert_rejected(
        content_changed_on_branch,
        "branch cannot use content_changed",
    );

    let mut same_digest = effect_missing_status_comment();
    same_digest["actual"] = json!({"observation": "content_changed", "render_digest": DIGEST_A});
    assert_rejected(same_digest, "content_changed digest must differ");

    let mut bad_target = effect_missing_status_comment();
    bad_target["target"] =
        json!({"type": "branch", "repo": "acme/shop-api", "branch": "gantry/d-1"});
    assert_rejected(bad_target, "status_comment target must be pr or issue");

    let mut branch_on_pr = effect_missing_branch();
    branch_on_pr["target"] = pr_target();
    assert_rejected(branch_on_pr, "branch effect target must be branch");

    let mut pull_request_on_issue = effect_missing_pull_request();
    pull_request_on_issue["target"] = issue_target();
    assert_rejected(
        pull_request_on_issue,
        "pull_request effect target must be pr",
    );

    let mut synthesized = effect_missing_status_comment();
    synthesized["synthesized_events"] = json!(["pr_closed"]);
    assert_rejected(synthesized, "effect_missing cannot synthesize events");

    let mut missing_extra = effect_missing_branch();
    missing_extra["actual"] = json!({"observation": "missing", "extra": 1});
    assert_rejected(missing_extra, "missing must reject unknown fields");
}

#[test]
fn orphan_effect_negative_cases_are_rejected() {
    let mut outbox_action_set = orphan_effect();
    outbox_action_set["expected"]["outbox_action"] = json!(ACTION_ID);
    assert_rejected(outbox_action_set, "outbox_action must be null");

    let mut marker_object = orphan_effect();
    marker_object["actual"]["marker"] =
        json!({"text": "x", "source": "forge:event", "truncated": false});
    assert_rejected(marker_object, "marker must be a wire string");

    let mut branch_target = orphan_effect();
    branch_target["target"] =
        json!({"type": "branch", "repo": "acme/shop-api", "branch": "gantry/d-1"});
    assert_rejected(branch_target, "orphan_effect target must be pr or issue");

    let mut synthesized = orphan_effect();
    synthesized["synthesized_events"] = json!(["pr_synchronized"]);
    assert_rejected(synthesized, "orphan_effect cannot synthesize events");

    let mut missing_delivery = orphan_effect();
    missing_delivery
        .as_object_mut()
        .expect("object")
        .remove("delivery_id");
    assert_rejected(missing_delivery, "orphan delivery_id field must exist");
}

#[test]
fn issue_state_diverged_negative_cases_are_rejected() {
    let mut deleted_with_event = issue_state_deleted();
    deleted_with_event["synthesized_events"] = json!(["issue_closed"]);
    assert_rejected(deleted_with_event, "deleted cannot synthesize issue_closed");

    let mut transferred_with_event = issue_state_transferred();
    transferred_with_event["synthesized_events"] = json!(["issue_closed"]);
    assert_rejected(
        transferred_with_event,
        "transferred cannot synthesize events",
    );

    let mut closed_with_reopened = issue_state_closed();
    closed_with_reopened["synthesized_events"] = json!(["issue_reopened"]);
    assert_rejected(
        closed_with_reopened,
        "closed cannot synthesize issue_reopened",
    );

    let mut missing_to_repo = issue_state_transferred();
    missing_to_repo["actual"]
        .as_object_mut()
        .expect("object")
        .remove("to_repo");
    assert_rejected(missing_to_repo, "transferred requires to_repo");

    let mut missing_new_number = issue_state_transferred();
    missing_new_number["actual"]
        .as_object_mut()
        .expect("object")
        .remove("new_number");
    assert_rejected(missing_new_number, "transferred requires new_number");

    let mut deleted_extra = issue_state_deleted();
    deleted_extra["actual"] = json!({"observation": "deleted", "extra": 1});
    assert_rejected(deleted_extra, "deleted must reject unknown fields");

    let mut mismatched_target = issue_state_closed();
    mismatched_target["target"] = pr_target();
    assert_rejected(
        mismatched_target,
        "issue_state_diverged target must be issue",
    );

    let mut null_delivery = issue_state_closed();
    null_delivery["delivery_id"] = Value::Null;
    assert_rejected(null_delivery, "issue_state_diverged requires delivery_id");
}

#[test]
fn review_and_merge_negative_cases_are_rejected() {
    let mut commented = review_state_diverged();
    commented["actual"]["review_state"] = json!("commented");
    assert_rejected(commented, "commented is not a ReviewSnapshotState");

    let mut review_on_issue = review_state_diverged();
    review_on_issue["target"] = issue_target();
    assert_rejected(review_on_issue, "review target must be pr");

    let mut review_synthesizes = review_state_diverged();
    review_synthesizes["synthesized_events"] = json!(["pr_synchronized"]);
    assert_rejected(review_synthesizes, "review cannot synthesize events");

    let mut still_queued = merge_state_diverged();
    still_queued["actual"] = json!({"in_merge_queue": true});
    assert_rejected(still_queued, "actual must be out of the merge queue");

    let mut expected_out = merge_state_diverged();
    expected_out["expected"] = json!({"in_merge_queue": false});
    assert_rejected(expected_out, "expected must be in the merge queue");

    let mut merge_on_issue = merge_state_diverged();
    merge_on_issue["target"] = issue_target();
    assert_rejected(merge_on_issue, "merge target must be pr");
}

#[test]
fn default_branch_and_installation_negative_cases_are_rejected() {
    let mut repo_changed = default_branch_changed();
    repo_changed["synthesized_events"] = json!(["repo_changed"]);
    assert_rejected(repo_changed, "repo_changed is not synthesizable in M0");

    let mut branch_target = default_branch_changed();
    branch_target["target"] = json!({"type": "branch", "repo": "acme/shop-api", "branch": "main"});
    assert_rejected(branch_target, "default branch target must be repo");

    let mut non_null_delivery = default_branch_changed();
    non_null_delivery["delivery_id"] = json!(DELIVERY_ID);
    assert_rejected(non_null_delivery, "default branch delivery_id must be null");

    let mut renamed_mismatch = default_branch_changed();
    renamed_mismatch["actual"]["renamed_from"] = json!("develop");
    assert_rejected(renamed_mismatch, "renamed_from must equal expected");

    let mut missing_default_delivery = default_branch_changed();
    missing_default_delivery
        .as_object_mut()
        .expect("object")
        .remove("delivery_id");
    assert_rejected(
        missing_default_delivery,
        "default branch delivery_id must exist",
    );

    let mut same_branch = default_branch_changed();
    same_branch["actual"]["default_branch"] = json!("main");
    same_branch["actual"]
        .as_object_mut()
        .expect("object")
        .remove("renamed_from");
    assert_rejected(same_branch, "default branch must differ");

    let mut unknown_code = installation_unreachable();
    unknown_code["actual"]["code"] = json!("unknown_code");
    assert_rejected(unknown_code, "unknown installation code");

    let mut repo_target = installation_unreachable();
    repo_target["target"] = json!({"type": "repo", "repo": "acme/shop-api"});
    assert_rejected(repo_target, "installation target must be installation");

    let mut non_null_installation_delivery = installation_unreachable();
    non_null_installation_delivery["delivery_id"] = json!(DELIVERY_ID);
    assert_rejected(
        non_null_installation_delivery,
        "installation delivery_id must be null",
    );

    let mut missing_installation_delivery = installation_unreachable();
    missing_installation_delivery
        .as_object_mut()
        .expect("object")
        .remove("delivery_id");
    assert_rejected(
        missing_installation_delivery,
        "installation delivery_id must exist",
    );

    let mut string_status = installation_unreachable();
    string_status["actual"]["http_status"] = json!("500");
    assert_rejected(string_status, "http_status must be an integer");

    let mut negative_status = installation_unreachable();
    negative_status["actual"]["http_status"] = json!(-1);
    assert_rejected(negative_status, "http_status must fit u16");

    let mut reachable = installation_unreachable();
    reachable["actual"]["reachable"] = json!(true);
    assert_rejected(reachable, "actual.reachable must be false");

    let mut missing_installation_id = installation_unreachable();
    missing_installation_id["target"] = json!({"type": "installation"});
    assert_rejected(
        missing_installation_id,
        "installation target requires installation_id",
    );
}

#[test]
fn drift_kind_strings_match_the_contract() {
    assert_eq!(DriftKind::ALL.len(), 9);
    let unique: BTreeSet<&str> = DriftKind::ALL.iter().map(|kind| kind.as_str()).collect();
    assert_eq!(unique.len(), 9);
    for kind in DriftKind::ALL {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
        assert_eq!(
            serde_json::from_value::<DriftKind>(json!(kind.as_str())).unwrap(),
            kind
        );
    }
}

#[test]
fn drift_payload_schema_has_nine_strict_branches() {
    let schema = serde_json::to_value(schemars::schema_for!(DriftPayload)).unwrap();
    let branches = schema["oneOf"]
        .as_array()
        .expect("DriftPayload must be a oneOf");
    assert_eq!(branches.len(), 9);

    let kinds: BTreeSet<&str> = branches
        .iter()
        .map(|branch| {
            branch["properties"]["kind"]["const"]
                .as_str()
                .expect("every branch must have a kind const")
        })
        .collect();
    assert_eq!(kinds.len(), 9);

    for branch in branches {
        assert_eq!(
            branch["additionalProperties"],
            json!(false),
            "each drift branch must reject unknown fields"
        );
        let required = branch["required"]
            .as_array()
            .expect("every branch must have required fields");
        for field in [
            "kind",
            "delivery_id",
            "target",
            "expected",
            "actual",
            "resolution",
            "synthesized_events",
            "detected_by",
        ] {
            assert!(
                required.iter().any(|value| value == field),
                "branch must require {field}"
            );
            assert!(
                branch["properties"].get(field).is_some(),
                "branch must define {field}"
            );
        }
    }
}

#[test]
fn readback_dtos_keep_optional_actor_facts() {
    let actor: ForgeActor = serde_json::from_value(actor()).unwrap();
    let pr = PullRequestReadback {
        head_sha: HEAD_AFTER.parse().unwrap(),
        base_ref: "main".into(),
        closed_by: Some(actor.clone()),
        merged_by: None,
        via_merge_queue: Some(true),
    };
    let value = serde_json::to_value(&pr).unwrap();
    assert_eq!(value["head_sha"], json!(HEAD_AFTER));
    assert_eq!(value["base_ref"], json!("main"));
    assert_eq!(value["via_merge_queue"], json!(true));
    assert_eq!(
        serde_json::from_value::<PullRequestReadback>(value).unwrap(),
        pr
    );

    let issue = IssueReadback {
        closed_by: Some(actor),
    };
    let value = serde_json::to_value(&issue).unwrap();
    assert_eq!(
        serde_json::from_value::<IssueReadback>(value).unwrap(),
        issue
    );

    let absent: IssueReadback = serde_json::from_value(json!({})).unwrap();
    assert_eq!(absent.closed_by, None);
}

#[test]
fn drift_text_and_terminal_state_use_wire_shapes() {
    assert_eq!(
        serde_json::to_value(ForgeText::new("marker", "test", false)).unwrap(),
        json!("marker")
    );
    assert_eq!(
        serde_json::to_value(PrTerminalState::Closed).unwrap(),
        json!("closed")
    );
    assert_eq!(
        serde_json::to_value(PrTerminalState::Merged).unwrap(),
        json!("merged")
    );
    assert_eq!(
        serde_json::to_value(DriftSynthesizedEvent::PrDequeued).unwrap(),
        json!("pr_dequeued")
    );
}
