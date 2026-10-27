//! `DeliveryState` 折叠（engine.md §7.14.4）与由状态推出的前置条件（§7.14.5）。

mod common;

use std::collections::BTreeMap;

use common::*;
use gantry_core::entities::{
    ApprovalStatus, AttemptStatus, BudgetUsed, Exec, ForgeActionKind, InboxKind, LockHolder,
    NodeStatus, OutboxStatus, Role, Seq, Stage, StageStatus, Tier, WorkflowStatus,
};
use gantry_core::events::Subject;
use gantry_core::level::Level;
use gantry_core::machines::delivery::AbandonCause;
use gantry_core::ports::inbox::DedupKey;
use gantry_engine::decide::EngineConsts;
use gantry_engine::state::{
    ApprovalLite, AttemptState, Blocking, DeliveryState, Evidence, LedgerState, LockLite,
    NodeState, OpenInbox, OutboxState, RaisedBy, WorkflowState, WorktreeInfo,
};
use gantry_engine::{retry_not_before, Route, Router};
use gantry_store::{DeliveryRow, StageEntered};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

fn v<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn id<T: std::str::FromStr>(text: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    text.parse().unwrap()
}

fn seq(n: u64) -> Seq {
    Seq::new(n).unwrap()
}

fn fold(specs: &[Spec]) -> DeliveryState {
    DeliveryState::fold(&events(specs)).unwrap().unwrap()
}

/// 夹具序列走完后的期望状态，逐字段写出。
fn expected_low_path() -> DeliveryState {
    let node = NodeState {
        id: id(NODE),
        key: "single".to_owned(),
        role: Role::Single,
        tier: Tier::Worker,
        exec: Exec::Sandbox,
        depends_on: vec![],
        required: true,
        critical: false,
        status: NodeStatus::Succeeded,
        verdict: None,
        superseded_by: None,
        attempts: vec![AttemptState {
            id: id(ATTEMPT),
            n: 1,
            exec: Exec::Sandbox,
            status: AttemptStatus::Succeeded,
            base_sha: v(json!(BASE_SHA)),
            tokens_reserved: 1000,
            wall_time_s: 600,
            config_rev: seq(2),
            started_at: ts(9),
            finished_at: Some(ts(12)),
            failure_class: None,
            failure_detail: None,
            result_ref: Some(v(json!(PATCH_SHA))),
            result_tree: Some(v(json!(TREE))),
            result_commit: Some(v(json!(COMMIT))),
            accepted_open_pr: None,
            after_retry_request: 1,
        }],
        attempts_at_retry: 0,
    };
    let mut used = BudgetUsed {
        nodes: 1,
        attempts: 1,
        forge_writes: 1,
        ..BudgetUsed::default()
    };
    used.tokens_by_tier.insert(Tier::Worker, 32600);
    DeliveryState {
        delivery: DeliveryRow {
            id: id(DELIVERY),
            project_id: id(PROJECT),
            title: "fix login timeout".to_owned(),
            origin: v(json!("conversation")),
            level_cap: Level::Medium,
            level_effective: Level::Low,
            stage: Stage::Merge,
            stage_status: StageStatus::Running,
            status_before_pause: None,
            issues: vec![],
            config_rev: seq(1),
            budget: v(json!({
                "template": "low.single",
                "limits": {
                    "tokens_by_tier": {"utility": 50000, "planner": 100000, "worker": 1000000},
                    "nodes": 6, "rounds": 0, "wall_time": 3600, "forge_writes": 20, "concurrency": 1
                }
            })),
            stage_entered: StageEntered {
                seq: seq(23),
                ts: ts(23),
                reason: None,
            },
            abandon_cause: None,
        },
        pr: Some(v(
            json!({"number": 12, "node_id": "PR_x", "head_sha": BASE_SHA}),
        )),
        worktree: Some(WorktreeInfo {
            id: id(WORKTREE),
            branch: "gantry/dlv-12".to_owned(),
            base_ref: "main".to_owned(),
            base_sha: v(json!(BASE_SHA)),
        }),
        ledger: LedgerState {
            used,
            exhausted: vec![],
        },
        workflows: BTreeMap::from([(
            id(WORKFLOW),
            WorkflowState {
                id: id(WORKFLOW),
                stage: Some(Stage::Implement),
                graph_rev: 1,
                status: WorkflowStatus::Succeeded,
                template: "low.implement".to_owned(),
                nodes: BTreeMap::from([(id(NODE), node)]),
                round: 0,
                created_seq: seq(4),
            },
        )]),
        evidence: Evidence {
            judged_head: None,
            candidate_head: Some(v(json!(COMMIT))),
            candidate_tree: Some(v(json!(TREE))),
            patch_sha256: Some(v(json!(PATCH_SHA))),
            stage_verdicts: BTreeMap::new(),
        },
        // inbox.resolved 已把审批项移出。
        open_inbox: BTreeMap::new(),
        outbox: BTreeMap::from([(
            v(json!(IDEMPOTENCY_KEY)),
            OutboxState {
                action_id: id(ACTION),
                kind: ForgeActionKind::CreatePr,
                epoch: 0,
                status: OutboxStatus::Succeeded,
                failure_code: None,
                charged: true,
            },
        )]),
        approvals: BTreeMap::from([(
            id(APPROVAL),
            ApprovalLite {
                action: v(json!("merge_pr")),
                subject_digest: v(json!(BASE_SHA)),
                status: ApprovalStatus::Granted,
            },
        )]),
        // worktree.unlocked 已释放。
        locks: BTreeMap::new(),
        round_history: vec![],
        last_seq: seq(27),
    }
}

#[test]
fn low_path_rebuilds_every_field() {
    assert_eq!(fold(&low_path()), expected_low_path());
}

#[test]
fn intermediate_state_tracks_lock_inbox_and_running_attempt() {
    let path = low_path();
    // 到 seq 9：锁已取、尝试在跑。
    let state = fold(&path[..9]);
    assert_eq!(
        state.locks[&id(WORKTREE)],
        LockLite {
            holder: LockHolder::Engine,
            lease_until: v(json!("2026-10-24T12:45:00.000Z")),
            fence: seq(7),
        }
    );
    let token = state.lock_token(&id(WORKTREE)).unwrap();
    assert_eq!(token.fence, seq(7));
    assert_eq!(token.delivery_id, id(DELIVERY));
    let attempt = &state.node(&id(NODE)).unwrap().attempts[0];
    assert_eq!(attempt.status, AttemptStatus::Running);
    assert_eq!(attempt.finished_at, None);

    // 到 seq 25：审批上报项打开，阻塞为 waiting_human。
    let state = fold(&path[..25]);
    assert_eq!(
        state.open_inbox[&id(INBOX)],
        OpenInbox {
            id: id(INBOX),
            kind: InboxKind::ApprovalNeeded,
            reason_code: None,
            dedup_key: DedupKey::new(format!("approval:{APPROVAL}")),
            raised_by: RaisedBy::Engine,
            effect: Blocking::WaitingHuman,
            node_id: None,
            attempt_id: None,
            approval_id: Some(id(APPROVAL)),
            target_level: None,
            options: vec![],
            raised_seq: seq(25),
        }
    );
    assert_eq!(
        state.approvals[&id(APPROVAL)].status,
        ApprovalStatus::Pending
    );
}

#[test]
fn dismiss_does_not_unblock() {
    let mut path = low_path();
    path.truncate(25);
    path.push(spec("inbox.dismissed", INBOX, json!({"inbox_id": INBOX})));
    assert!(fold(&path).open_inbox.contains_key(&id(INBOX)));
}

#[test]
fn abandon_records_cause_and_stage_entry() {
    let mut path = low_path();
    path.truncate(2);
    path.push(spec(
        "delivery.abandoned",
        DELIVERY,
        json!({"reason": "pr_closed", "by": "forge"}),
    ));
    let state = fold(&path);
    assert_eq!(state.delivery.stage, Stage::Abandoned);
    assert_eq!(
        state.delivery.abandon_cause,
        Some(AbandonCause::PrClosedUnmerged)
    );
    assert_eq!(state.delivery.stage_entered.seq, seq(3));
    assert_eq!(
        state.delivery.stage_entered.reason.as_deref(),
        Some("pr_closed")
    );
}

#[test]
fn retry_requested_restarts_attempt_ordinal() {
    let mut path = low_path();
    path.truncate(10);
    path.push(spec(
        "attempt.finished",
        ATTEMPT,
        json!({"status": "failed", "failure_class": "exec_failed", "failure_detail": "exit_nonzero"}),
    ));
    path.push(node("node.finished", NODE, "failed"));
    path.push(spec(
        "node.retry_requested",
        NODE,
        json!({"node_id": NODE, "by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"}),
    ));
    path.push(spec(
        "attempt.started",
        "att_01JB7X4Q9ZK3M8T5V2N7RHC6WZ",
        json!({
            "attempt_id": "att_01JB7X4Q9ZK3M8T5V2N7RHC6WZ", "node_id": NODE, "n": 2,
            "provider": "anthropic", "model": "claude", "reasoning_effort": "medium",
            "config_rev": 2, "base_sha": BASE_SHA, "tokens_total": 1000, "wall_time_s": 600
        }),
    ));
    let state = fold(&path);
    let node = state.node(&id(NODE)).unwrap();
    assert_eq!(node.attempts.len(), 2);
    assert_eq!(node.attempts[0].after_retry_request, 1);
    assert_eq!(
        node.attempts[0].failure_detail.as_ref().unwrap().as_str(),
        "exit_nonzero"
    );
    assert_eq!(node.attempts[1].after_retry_request, 1);
    assert_eq!(node.attempts[1].n, 2);
}

#[test]
fn fold_rejects_events_before_created() {
    let path = low_path();
    let events = events(&path[1..2]);
    assert!(DeliveryState::fold(&events).is_err());
}

#[test]
fn fold_rejects_attempt_finish_for_unknown_attempt() {
    let mut path = low_path();
    path.truncate(9);
    let mut finished = attempt_finished();
    finished.subject = "att_01JB7X4Q9ZK3M8T5V2N7RHC6WX";
    path.push(finished);
    assert!(DeliveryState::fold(&events(&path)).is_err());
}

// ---------------------------------------------------------------------------
// 路由（§7.14.2）
// ---------------------------------------------------------------------------

/// 混入同项目另一交付单元、未绑定的 forge 事件与设置广播后，路由出来的子序列折叠结果不变。
#[test]
fn router_selects_exactly_this_delivery() {
    let mut mixed = low_path();
    let noise = [
        (1, created(OTHER_DELIVERY)),
        (4, workflow(OTHER_WORKFLOW, OTHER_DELIVERY, OTHER_NODE)),
        (7, node("node.queued", OTHER_NODE, "queued")),
        (
            12,
            spec(
                "forge.synced",
                PROJECT,
                json!({"target": {"type": "project"}, "synced_at": "2026-10-24T12:41:03.120Z"}),
            )
            .actor(gantry_core::entities::ActorKind::Forge),
        ),
    ];
    for (offset, (at, spec)) in noise.into_iter().enumerate() {
        mixed.insert(at + offset, spec);
    }
    let all = events(&mixed);
    let mut router = Router::default();
    let target = id(DELIVERY);
    let mut mine = Vec::new();
    let mut other = 0;
    for event in &all {
        match router.route(event) {
            Ok(Route::Delivery { id, .. }) if id == target => mine.push(event.clone()),
            Ok(Route::Delivery { .. }) => other += 1,
            // forge.synced 夹具形状不在本测试关心范围内；只要求不投给本交付单元。
            Ok(_) | Err(_) => {}
        }
    }
    assert_eq!(other, 3);
    let mut state = DeliveryState::fold(&mine).unwrap().unwrap();
    // seq 因插入噪声而后移；其余字段与纯序列一致。
    let expected = expected_low_path();
    assert_eq!(state.last_seq, all.last().unwrap().seq);
    state.last_seq = expected.last_seq;
    assert_eq!(state.delivery.id, expected.delivery.id);
    assert_eq!(state.workflows.len(), 1);
    assert_eq!(state.outbox, expected.outbox);
    assert_eq!(state.approvals, expected.approvals);
    assert_eq!(state.ledger, expected.ledger);
    assert_eq!(state.evidence, expected.evidence);
}

#[test]
fn input_flag_follows_the_route_table() {
    let all = events(&low_path());
    let mut router = Router::default();
    let delivered: Vec<&str> = all
        .iter()
        .filter_map(|event| match router.route(event).unwrap() {
            Route::Delivery { input: true, .. } => Some(event.ty.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered,
        vec![
            "worktree.created",
            "worktree.locked",
            "patch.validated",
            "worktree.unlocked",
            "outbox.succeeded",
            "approval.requested",
            "approval.granted",
            "inbox.resolved",
        ]
    );
    let settings = gantry_core::events::Event {
        subject: Subject::parse("settings").unwrap(),
        ty: "settings.updated".parse().unwrap(),
        ..all[0].clone()
    };
    assert_eq!(router.route(&settings).unwrap(), Route::Broadcast);
}

// ---------------------------------------------------------------------------
// §7.14.5 前置条件表中由状态推出的部分：正例与反例
// ---------------------------------------------------------------------------

/// 派发总闸 `may_dispatch`：阶段非终态 ∧ running ∧ 无未关闭的 budget_exhausted。
#[test]
fn may_dispatch_gate() {
    let path = low_path();
    // 正例：implement 阶段、running。
    assert!(fold(&path[..9]).may_dispatch());

    // 反例 1：暂停。
    let mut paused: Vec<Spec> = low_path().into_iter().take(9).collect();
    paused.push(spec(
        "delivery.paused",
        DELIVERY,
        json!({"by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM", "status_before": "running"}),
    ));
    let state = fold(&paused);
    assert_eq!(
        state.delivery.status_before_pause,
        Some(StageStatus::Running)
    );
    assert!(!state.may_dispatch());
    // 恢复后重新放行。
    paused.push(spec(
        "delivery.resumed",
        DELIVERY,
        json!({"by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM", "status_after": "running"}),
    ));
    assert!(fold(&paused).may_dispatch());

    // 反例 2：有未关闭的 budget_exhausted 上报（即使 stage_status 仍为 running）。
    let mut exhausted: Vec<Spec> = low_path().into_iter().take(9).collect();
    exhausted.push(spec(
        "inbox.raised",
        INBOX,
        json!({"item": {"id": INBOX, "project_id": PROJECT, "delivery_id": DELIVERY, "kind": "budget_exhausted", "severity": "blocker", "summary": "预算耗尽", "payload": {"dimension": "nodes", "limit": 6, "used": 6, "options": [{"id": "raise_cap", "label": "追加", "primary": true}, {"id": "abandon", "label": "放弃"}]}, "status": "open", "dedup_key": "budget:nodes", "created_at": "2026-10-24T12:41:03.120Z", "updated_at": "2026-10-24T12:41:03.120Z"}}),
    ));
    let state = fold(&exhausted);
    assert_eq!(state.delivery.stage_status, StageStatus::Running);
    assert!(!state.may_dispatch());

    // 反例 3：终态阶段。
    let mut abandoned: Vec<Spec> = low_path().into_iter().take(2).collect();
    abandoned.push(spec(
        "delivery.abandoned",
        DELIVERY,
        json!({"reason": null, "by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"}),
    ));
    assert!(!fold(&abandoned).may_dispatch());
}

/// `Effect(RunAttempt { resume: true })`：尝试有 `attempt.started` 无 `attempt.finished`。
#[test]
fn resume_targets_only_unfinished_attempts() {
    let path = low_path();
    let running = fold(&path[..9]);
    let unfinished: Vec<_> = running.unfinished_attempts().map(|(_, a)| a.id).collect();
    assert_eq!(unfinished, vec![id(ATTEMPT)]);
    assert_eq!(fold(&path[..12]).unfinished_attempts().count(), 0);
}

/// `Append(workflow.created)`：自 `stage_entered` 以来尚未建过该阶段的 Workflow。
#[test]
fn workflow_is_built_once_per_stage_entry() {
    let path = low_path();
    // 正例：刚进入 implement，未建图 → 可建。
    assert!(!fold(&path[..2]).workflow_built_in_stage());
    // 反例：已建图。
    assert!(fold(&path[..4]).workflow_built_in_stage());
    // 离开 implement 后，旧图不算新阶段的图。
    assert!(!fold(&path[..17]).workflow_built_in_stage());
}

/// 自动重试的退避：未终结无值；终结后确定性，且落在 [finished_at, finished_at + 退避]。
#[test]
fn retry_not_before_is_deterministic_and_bounded() {
    let consts = EngineConsts::default();
    let path = low_path();
    let running = fold(&path[..9]);
    let attempt = &running.node(&id(NODE)).unwrap().attempts[0];
    assert_eq!(retry_not_before(attempt, &consts), None);

    let done = fold(&path[..12]);
    let attempt = &done.node(&id(NODE)).unwrap().attempts[0];
    let at = retry_not_before(attempt, &consts).unwrap();
    assert_eq!(Some(at), retry_not_before(attempt, &consts));
    let finished = attempt.finished_at.unwrap().as_millis();
    assert!(at.as_millis() >= finished);
    assert!(at.as_millis() <= finished + consts.retry_backoff_base_ms);
}

/// `Append(budget.charged(forge_writes))`：outbox.succeeded 的动作尚未记账。
#[test]
fn forge_write_charge_is_tracked_per_action() {
    let path = low_path();
    assert!(
        !fold(&path[..20]).outbox[&v::<gantry_core::canon::IdempotencyKey>(json!(IDEMPOTENCY_KEY))]
            .charged
    );
    assert!(
        fold(&path[..21]).outbox[&v::<gantry_core::canon::IdempotencyKey>(json!(IDEMPOTENCY_KEY))]
            .charged
    );
}

/// E5：被接受的 `open_pr` 提议 ID 随 `attempt.finished` 读入尝试状态。
#[test]
fn accepted_open_pr_is_folded_into_attempt() {
    let mut path = low_path();
    path.truncate(11);
    let mut finished = attempt_finished();
    finished.data["accepted_open_pr"] = json!("proposal-1");
    path.push(finished);
    let state = fold(&path);
    let attempt = &state.node(&id(NODE)).unwrap().attempts[0];
    assert_eq!(attempt.accepted_open_pr.as_deref(), Some("proposal-1"));
    assert_eq!(state.evidence.candidate_head, Some(v(json!(COMMIT))));
}

/// 被取代的旧项只出现在新项 `inbox.raised.data.supersedes` 里，不会另发 `inbox.resolved`；
/// 折叠时要一并移出阻塞集合，否则它会一直阻塞（§7.11）。
#[test]
fn supersedes_unblocks() {
    const NEW_INBOX: &str = "inb_01JBX2F5H7K9M1P3R5T7V9X0ZD";
    let mut path = low_path();
    path.truncate(25);
    assert!(fold(&path).open_inbox.contains_key(&id(INBOX)));
    path.push(spec(
        "inbox.raised",
        NEW_INBOX,
        json!({
            "item": {"id": NEW_INBOX, "project_id": PROJECT, "delivery_id": DELIVERY, "kind": "approval_needed", "severity": "blocker", "summary": "需要审批", "payload": {"approval_id": APPROVAL}, "status": "open", "dedup_key": "approval:new", "created_at": "2026-10-24T12:41:03.120Z", "updated_at": "2026-10-24T12:41:03.120Z"},
            "supersedes": [INBOX]
        }),
    ));
    let state = fold(&path);
    assert!(!state.open_inbox.contains_key(&id(INBOX)));
    assert!(state.open_inbox.contains_key(&id(NEW_INBOX)));
    assert_eq!(state.open_inbox.len(), 1);
}
