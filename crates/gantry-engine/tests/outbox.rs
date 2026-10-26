//! Outbox 意图构造（engine.md §7.12）。

use gantry_core::entities::Priority;
use gantry_core::forge::{ActionPayload, CreatePrPayload, MergePrPayload, PushBranchPayload};
use gantry_core::ids::{ActionId, DeliveryId, ProjectId};
use gantry_core::time::Timestamp;
use gantry_engine::outbox::{build, OutboxIntent};
use serde_json::json;

fn project() -> ProjectId {
    "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap()
}
fn delivery() -> DeliveryId {
    "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap()
}
fn action(id: &str) -> ActionId {
    id.parse().unwrap()
}
fn now() -> Timestamp {
    Timestamp::from_millis(1_700_000_000_000)
}

fn merge_payload() -> MergePrPayload {
    serde_json::from_value(json!({
        "repo": "acme/shop-api",
        "pull_number": 57,
        "expected_head_sha": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
        "merge_method": "squash",
        "mode": "auto",
        "commit_title": null,
        "commit_message": null
    }))
    .unwrap()
}

fn intent(payload: ActionPayload, target: &str, epoch: u32) -> gantry_core::entities::OutboxAction {
    build(OutboxIntent {
        id: action("act_01JBA1F3Q9TX2N7B5D8H0M4RVC"),
        project_id: project(),
        delivery_id: delivery(),
        payload,
        target,
        intent: json!({"pull_number": 57, "method": "squash"}),
        epoch,
        depends_on: vec![],
        priority: Priority::Normal,
        now: now(),
    })
    .unwrap()
}

#[test]
fn merge_pr_action_is_queued_and_keyed() {
    let action = intent(ActionPayload::MergePr(merge_payload()), "57", 0);
    assert_eq!(action.kind, gantry_core::entities::ForgeActionKind::MergePr);
    assert_eq!(action.status, gantry_core::entities::OutboxStatus::Queued);
    assert_eq!(action.attempts, 0);
    assert!(action.depends_on.is_empty());
    assert!(action.expected_effects.is_empty());
    assert!(action.github_ref.is_none());
    assert_eq!(action.created_at, now());
    assert!(action.idempotency_key.as_str().starts_with("gk1_"));
}

#[test]
fn same_intent_is_idempotent() {
    let first = intent(ActionPayload::MergePr(merge_payload()), "57", 0);
    let second = intent(ActionPayload::MergePr(merge_payload()), "57", 0);
    assert_eq!(first.idempotency_key, second.idempotency_key);
}

#[test]
fn key_changes_with_target_intent_or_epoch() {
    let base = intent(ActionPayload::MergePr(merge_payload()), "57", 0);
    let target = intent(ActionPayload::MergePr(merge_payload()), "58", 0);
    let epoch = intent(ActionPayload::MergePr(merge_payload()), "57", 1);
    assert_ne!(base.idempotency_key, target.idempotency_key);
    assert_ne!(base.idempotency_key, epoch.idempotency_key);
}

#[test]
fn create_pr_and_push_branch_kinds_are_derived_from_payload() {
    let create_pr: CreatePrPayload = serde_json::from_value(json!({
        "repo": "acme/shop-api",
        "head": "gantry/42-fix",
        "base": "main",
        "title": "fix",
        "body": "",
        "draft": false,
        "closes": [42],
        "expected_head_oid": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
        "maintainer_can_modify": true
    }))
    .unwrap();
    let push: PushBranchPayload = serde_json::from_value(json!({
        "repo": "acme/shop-api",
        "branch": "gantry/42-fix",
        "base_ref": "main",
        "worktree_id": "wtr_01JBA1D7P2KS6W8Y0A4C1F5GJM",
        "local_head_oid": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
        "expected_tree_oid": "f2c03d84a858156b0121130fd1ec2f391aa90865",
        "expected_remote_oid": null,
        "validation_ref": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "transport": "git",
        "force": false
    }))
    .unwrap();

    let created = intent(ActionPayload::CreatePr(create_pr), "gantry/42-fix", 0);
    assert_eq!(
        created.kind,
        gantry_core::entities::ForgeActionKind::CreatePr
    );
    let pushed = intent(ActionPayload::PushBranch(push), "gantry/42-fix", 0);
    assert_eq!(
        pushed.kind,
        gantry_core::entities::ForgeActionKind::PushBranch
    );
    assert_ne!(created.idempotency_key, pushed.idempotency_key);
}
