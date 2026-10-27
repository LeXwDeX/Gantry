//! `DeliveryState` 测试共用的夹具：Low 主路径的完整事件序列。
//!
//! `data` 取自 `crates/gantry-core/fixtures/events/cases.json` 的正例，只替换 ID 以串成一条交付。
//! 除 `approval.requested`（契约把归属放在信封里）外，信封都不带 `correlation_id`，
//! 用来证明归属按路由表而不是按 `correlation_id`。

#![allow(dead_code)]

use gantry_core::entities::{Actor, ActorKind, Seq};
use gantry_core::events::{Event, NewEvent, Subject};
use gantry_core::ids::{EventId, ProjectId};
use gantry_core::time::Timestamp;
use serde_json::{json, Value};

pub const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
pub const OTHER_PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZB";
pub const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
pub const OTHER_DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC";
pub const WORKFLOW: &str = "wfl_01JB7M8T2X6V9P3Q5R1K4W7YZC";
pub const OTHER_WORKFLOW: &str = "wfl_01JB7M8T2X6V9P3Q5R1K4W7YZD";
pub const NODE: &str = "nod_01JB7M8V5X1Z3N6R9Q2K4W7YZC";
pub const OTHER_NODE: &str = "nod_01JB7M8V5X1Z3N6R9Q2K4W7YZD";
pub const ATTEMPT: &str = "att_01JB7X4Q9ZK3M8T5V2N7RHC6WY";
pub const WORKTREE: &str = "wtr_01JBA1D9Q4MT7X2C6E8H0K3PRV";
pub const ACTION: &str = "act_01JBA1D7P2KS6W8Y0A4C1F5GJN";
pub const APPROVAL: &str = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";
pub const INBOX: &str = "inb_01JBX2F5H7K9M1P3R5T7V9X0ZC";
pub const BASE_SHA: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
pub const COMMIT: &str = "d7d018f867d6c5b78e82906386420d45a4d84486";
pub const TREE: &str = "1405df66cbe219b0bf6355bc3d60361a8376b6b4";
pub const PATCH_SHA: &str =
    "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9";
pub const IDEMPOTENCY_KEY: &str = "gk1_anbfwcip7r36is6wsqcuxxfd34";
pub const T0: i64 = 1_700_000_000_000;

/// 一条事件的信封与 data；`seq` 在拼成序列时按顺序分配。
pub struct Spec {
    pub ty: &'static str,
    pub subject: &'static str,
    pub data: Value,
    pub actor: ActorKind,
    pub project: Option<&'static str>,
    pub correlation: Option<&'static str>,
}

pub fn spec(ty: &'static str, subject: &'static str, data: Value) -> Spec {
    Spec {
        ty,
        subject,
        data,
        actor: ActorKind::Engine,
        project: Some(PROJECT),
        correlation: None,
    }
}

impl Spec {
    pub fn actor(mut self, actor: ActorKind) -> Self {
        self.actor = actor;
        self
    }

    pub fn correlated(mut self, id: &'static str) -> Self {
        self.correlation = Some(id);
        self
    }

    pub fn project(mut self, project: Option<&'static str>) -> Self {
        self.project = project;
        self
    }

    pub fn to_new(&self) -> NewEvent {
        NewEvent {
            ty: self.ty.parse().unwrap(),
            subject: Subject::parse(self.subject).unwrap(),
            project_id: self.project.map(|p| p.parse::<ProjectId>().unwrap()),
            actor: Actor::new(self.actor, "test"),
            causation_id: None,
            correlation_id: self.correlation.map(str::to_owned),
            data: self.data.clone(),
            dedup_key: None,
        }
    }
}

pub fn ts(seq: u64) -> Timestamp {
    Timestamp::from_millis(T0 + seq as i64 * 1_000)
}

/// 按顺序分配 seq 1..n 与时间戳。
pub fn events(specs: &[Spec]) -> Vec<Event<Value>> {
    specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let seq = i as u64 + 1;
            let new = spec.to_new();
            Event {
                id: format!("evt_{:0>26}", seq).parse::<EventId>().unwrap(),
                seq: Seq::new(seq).unwrap(),
                ts: ts(seq),
                ty: new.ty,
                subject: new.subject,
                project_id: new.project_id,
                actor: new.actor,
                causation_id: None,
                correlation_id: new.correlation_id,
                data: new.data,
            }
        })
        .collect()
}

pub fn created(subject: &'static str) -> Spec {
    spec(
        "delivery.created",
        subject,
        json!({
            "title": "fix login timeout",
            "level_cap": "Medium",
            "level_effective": "Low",
            "origin": "conversation",
            "source_message_id": null, "user_text": "修复登录超时",
            "issues": [],
            "budget": {
                "template": "low.single",
                "limits": {
                    "tokens_by_tier": {"utility": 50000, "planner": 100000, "worker": 1000000},
                    "nodes": 6,
                    "rounds": 0,
                    "wall_time": 3600,
                    "forge_writes": 20,
                    "concurrency": 1
                }
            },
            "config_rev": 1
        }),
    )
}

pub fn stage(from: &str, to: &str, reason: Option<&str>) -> Spec {
    spec(
        "delivery.stage_changed",
        DELIVERY,
        json!({"from": from, "to": to, "reason": reason, "stage_status": "running"}),
    )
}

pub fn workflow(subject: &'static str, delivery: &str, node: &str) -> Spec {
    spec(
        "workflow.created",
        subject,
        json!({
            "stage": "implement",
            "template": "low.implement",
            "graph_rev": 1,
            "nodes": [{
                "node_id": node,
                "key": "single",
                "role": "single",
                "depends_on": [],
                "required": true,
                "critical": false
            }],
            "delivery_id": delivery,
            "conversation_id": null
        }),
    )
}

pub fn node(ty: &'static str, node: &'static str, status: &str) -> Spec {
    spec(
        ty,
        node,
        json!({"node_id": node, "status": status, "verdict": null, "superseded_by": null}),
    )
}

pub fn attempt_started() -> Spec {
    spec(
        "attempt.started",
        ATTEMPT,
        json!({
            "attempt_id": ATTEMPT,
            "node_id": NODE,
            "n": 1,
            "provider": "anthropic",
            "model": "claude",
            "reasoning_effort": "medium",
            "config_rev": 2,
            "base_sha": BASE_SHA,
            "tokens_total": 1000,
            "wall_time_s": 600
        }),
    )
}

pub fn attempt_finished() -> Spec {
    spec(
        "attempt.finished",
        ATTEMPT,
        json!({
            "status": "succeeded",
            "result_ref": PATCH_SHA,
            "failure_class": null,
            "failure_detail": null,
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_tokens": 0, "cache_write_tokens": 0, "requests": 1},
            "result_tree": TREE,
            "result_commit": COMMIT,
            "served_by": {"provider": "anthropic", "model": "claude", "effort": "medium"}
        }),
    )
}

pub fn charged(dimension: &str, used_after: u64, source: Value) -> Spec {
    spec(
        "budget.charged",
        DELIVERY,
        json!({
            "ledger": {"subject": "delivery", "id": DELIVERY},
            "dimension": dimension,
            "delta": 1,
            "used_after": used_after,
            "source": source
        }),
    )
}

pub fn tokens_charged() -> Spec {
    spec(
        "budget.charged",
        DELIVERY,
        json!({
            "ledger": {"subject": "delivery", "id": DELIVERY},
            "dimension": "tokens_by_tier",
            "tier": "worker",
            "delta": 32600,
            "used_after": 32600,
            "source": {"kind": "attempt", "attempt_id": ATTEMPT},
            "usage": {"input_tokens": 18200, "output_tokens": 2400, "cache_read_tokens": 9000, "cache_write_tokens": 1200, "requests": 1},
            "weighted_tokens": 32600,
            "weight_audit": {"basis": "job_token", "config_rev": 18240, "weights": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25}}
        }),
    )
}

/// Low 主路径：创建 → 建图 → 派发 → 终结 → 入队 → outbox 成功 → 审批。
pub fn low_path() -> Vec<Spec> {
    vec![
        // 1
        created(DELIVERY),
        // 2
        stage("spec", "implement", Some("admission_skipped")),
        // 3
        spec(
            "worktree.created",
            WORKTREE,
            json!({
                "worktree_id": WORKTREE,
                "project_id": PROJECT,
                "delivery_id": DELIVERY,
                "branch": "gantry/dlv-12",
                "base_ref": "main",
                "base_sha": BASE_SHA
            }),
        ),
        // 4
        workflow(WORKFLOW, DELIVERY, NODE),
        // 5
        charged("nodes", 1, json!({"kind": "node", "node_id": NODE})),
        // 6
        node("node.queued", NODE, "queued"),
        // 7
        spec(
            "worktree.locked",
            WORKTREE,
            json!({
                "worktree_id": WORKTREE,
                "holder": "engine",
                "delivery_id": DELIVERY,
                "lease_until": "2026-10-24T12:45:00.000Z"
            }),
        ),
        // 8
        node("node.started", NODE, "running"),
        // 9
        attempt_started(),
        // 10
        charged("attempts", 1, json!({"kind": "attempt", "attempt_id": ATTEMPT})),
        // 11
        spec(
            "patch.validated",
            ATTEMPT,
            json!({
                "job_id": ATTEMPT,
                "sha256": PATCH_SHA,
                "base_sha": BASE_SHA,
                "result_tree": TREE,
                "verdict": "allow",
                "files": [{"path": "src/lib.rs", "status": "modified", "is_binary": false, "is_symlink": false, "is_submodule": false, "added": 2, "deleted": 1}],
                "reasons": [],
                "config_rev": 10
            }),
        ),
        // 12
        attempt_finished(),
        // 13
        tokens_charged(),
        // 14
        node("node.finished", NODE, "succeeded"),
        // 15
        spec("workflow.finished", WORKFLOW, json!({"status": "succeeded"})),
        // 16
        spec(
            "worktree.unlocked",
            WORKTREE,
            json!({"worktree_id": WORKTREE, "reason": "released"}),
        ),
        // 17
        stage("implement", "propose", None),
        // 18
        spec(
            "outbox.enqueued",
            ACTION,
            json!({"action": {
                "delivery_id": DELIVERY,
                "kind": "create_pr",
                "payload": {"repo": "acme/shop-api", "head": "gantry/42-fix-pagination", "base": "main", "title": "Fix pagination off-by-one in /orders", "body": "Fixes the off-by-one when page size divides total count.", "draft": false, "closes": [42], "expected_head_oid": "d7d018f867d6c5b78e82906386420d45a4d84486", "maintainer_can_modify": false},
                "target": "pr:gantry/42-fix-pagination->main",
                "intent": {"base": "main", "head": "gantry/42-fix-pagination", "title": "Fix pagination off-by-one in /orders", "body": "Fixes the off-by-one when page size divides total count.", "draft": false, "closes": [42]},
                "idempotency_key": IDEMPOTENCY_KEY,
                "requires_approval": null,
                "depends_on": [],
                "priority": "normal",
                "not_before": null,
                "epoch": 0,
                "coalesce_key": null,
                "enqueued_by": {"module": "engine", "causation_id": null}
            }}),
        ),
        // 19
        spec("outbox.executing", ACTION, json!({"attempts": 1, "replay": false}))
            .actor(ActorKind::Forge),
        // 20
        spec(
            "outbox.succeeded",
            ACTION,
            json!({"github_ref": {"bot": {"github_id": 2, "login": "gantry[bot]"}, "pr": {"number": 12, "node_id": "PR_x", "head_sha": BASE_SHA}, "marker": "gantry:v1", "recorded_at": "2026-10-24T12:41:03.120Z"}}),
        )
        .actor(ActorKind::Forge),
        // 21
        charged(
            "forge_writes",
            1,
            json!({"kind": "outbox_action", "action_id": ACTION}),
        ),
        // 22
        stage("propose", "review", None),
        // 23
        stage("review", "merge", None),
        // 24：归属在信封 correlation_id（event-data.md approval 信封）
        spec(
            "approval.requested",
            APPROVAL,
            json!({"action": "merge_pr", "target": {"kind": "pr", "repo": "acme/web", "number": 12}, "subject_digest": BASE_SHA, "expires_at": "2026-10-24T12:45:00.000Z", "config_rev": 10, "requested_by": {"kind": "engine", "id": "engine"}}),
        )
        .correlated(DELIVERY),
        // 25
        spec(
            "inbox.raised",
            INBOX,
            json!({"item": {"id": INBOX, "project_id": PROJECT, "delivery_id": DELIVERY, "kind": "approval_needed", "severity": "blocker", "summary": "需要审批", "payload": {"approval_id": APPROVAL}, "status": "open", "dedup_key": format!("approval:{APPROVAL}"), "created_at": "2026-10-24T12:41:03.120Z", "updated_at": "2026-10-24T12:41:03.120Z"}}),
        ),
        // 26
        spec(
            "approval.granted",
            APPROVAL,
            json!({"approval_id": APPROVAL, "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM", "approval_pubkey_fpr": PATCH_SHA, "payload_sha256": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}),
        )
        .actor(ActorKind::Device),
        // 27
        spec(
            "inbox.resolved",
            INBOX,
            json!({"inbox_id": INBOX, "resolution": "approval_settled"}),
        )
        .actor(ActorKind::System),
    ]
}
