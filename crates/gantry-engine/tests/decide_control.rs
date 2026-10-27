//! 控制命令与异常路径的决策（engine.md §7.6 T11 / T13 / T14 / T15、§7.11、§7.14.5、§12.2，#73）。
//!
//! 纯函数层面：沿用 `decide_low.rs` 的小模拟器（`Append` 先过 `NewEvent::validate` 再折叠，
//! 外部模块的事件以 `external` 注入）。M1 不由引擎产生的上报项（`level_cap_exceeded`、`gate_reject`）
//! 以注入的 `inbox.raised` / `inbox.resolved` 覆盖其选项的执行路径（§7.14.8）。

mod common;

use std::collections::BTreeSet;

use common::{ts, BASE_SHA, DELIVERY, PROJECT, T0};
use gantry_core::canon::GitOid;
use gantry_core::entities::{
    Actor, ActorKind, ApprovalStatus, AttemptStatus, BudgetDimension, BudgetLedger, DecisionKind,
    FailureClass, ForgeActionKind, InboxKind, InboxOption, InboxPayload, MergeMethod, NodeStatus,
    PolicyActionId, PullRequestRef, ReasonCode, Seq, Severity, Stage, StageStatus, Tier,
    TokenUsage,
};
use gantry_core::events::{Event, EventType, InboxResolution, NewEvent, Subject};
use gantry_core::forge::{ActionPayload, BaseAdvancedData, ForgeEventData, PrState};
use gantry_core::ids::{ApprovalId, EventId, NodeId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::AbandonCause;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::forge::{CancelReason, ReconcileTarget};
use gantry_core::ports::inbox::{
    DedupKey, OptionId, RaiseRequest, Resolution, ResolveTarget, SafeSummary,
};
use gantry_core::ports::policy::{ActionDecision, InvalidateReason};
use gantry_core::time::Timestamp;
use gantry_engine::decide;
use gantry_engine::decide::{
    Command, CommandRejection, DecideEnv, DecideSeed, Decision, Effect, EffectKey, EffectReport,
    EngineConsts, EngineSettings, Finalized, Input, InputIdentity, ModelKey, ModelTable, Observed,
    ObservedData, OutputSignals, PatchResult, ProjectFacts, ProposalOutcome, ProposalTexts,
    ReservationPolicy, ResolvedModel, SandboxLimits, TemplateRegistry, TickReason,
};
use gantry_engine::state::{ActionKey, DeliveryState, OpenInbox};
use serde_json::{json, Value};

const COMMIT: &str = "d7d018f867d6c5b78e82906386420d45a4d84486";
const TREE: &str = "1405df66cbe219b0bf6355bc3d60361a8376b6b4";
const PATCH_SHA: &str = "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9";
const RESULT_REF: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WORKTREE: &str = "wtr_01JBA1D9Q4MT7X2C6E8H0K3PRV";
const APPROVAL: &str = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK";

struct FixedReserve;
impl ReservationPolicy for FixedReserve {
    fn reserve(&self, _: &BudgetLedger, _: Tier, _: usize) -> u64 {
        1000
    }
}

fn oid(text: &str) -> GitOid {
    serde_json::from_value(json!(text)).unwrap()
}

/// 决策环境的全部可变部分；`env()` 借用它们拼出 `DecideEnv`。
struct Fixture {
    settings: EngineSettings,
    models: ModelTable,
    templates: TemplateRegistry,
    inflight: BTreeSet<EffectKey>,
    denied: BTreeSet<ActionKey>,
    proposals: ProposalTexts,
}

impl Fixture {
    fn new() -> Self {
        Self {
            settings: EngineSettings {
                config_rev: Seq::new(1).unwrap(),
                project: ProjectFacts {
                    repo: serde_json::from_value(json!("acme/web")).unwrap(),
                    default_branch: "main".to_owned(),
                    branch_prefix: "gantry/".to_owned(),
                    allowed_merge_methods: vec![MergeMethod::Squash, MergeMethod::Merge],
                },
                sandbox: SandboxLimits {
                    wall_time_s: 1800,
                    kill_grace_s: 10,
                },
            },
            models: ModelTable::from([(
                ModelKey {
                    sandbox: true,
                    level: gantry_core::level::Level::Low,
                    tier: Tier::Worker,
                    critical: false,
                },
                ResolvedModel {
                    provider_id: "anthropic".to_owned(),
                    model: "claude".to_owned(),
                    reasoning_effort: gantry_core::level::Effort::Medium,
                    attempt_wall_s: 900,
                },
            )]),
            templates: TemplateRegistry,
            inflight: BTreeSet::new(),
            denied: BTreeSet::new(),
            proposals: ProposalTexts::default(),
        }
    }

    fn env<'a>(&'a self, state: &DeliveryState, input: &InputIdentity) -> DecideEnv<'a> {
        DecideEnv {
            now: Timestamp::from_millis(T0 + 1_000_000),
            seed: DecideSeed::derive(&state.delivery.id, input, state.last_seq),
            settings: &self.settings,
            models: &self.models,
            templates: &self.templates,
            reservation: &FixedReserve,
            sandbox_capacity: 4,
            inflight: &self.inflight,
            denied: &self.denied,
            proposals: &self.proposals,
            consts: EngineConsts::default(),
        }
    }
}

/// 状态 + 已提交事件；`commit` 校验事件形状后折叠。
struct Sim {
    fx: Fixture,
    state: DeliveryState,
    seq: u64,
    stages: Vec<Stage>,
}

impl Sim {
    fn new() -> Self {
        let created = common::events(&[common::created(DELIVERY)]);
        let state = DeliveryState::fold(&created).unwrap().unwrap();
        Self {
            fx: Fixture::new(),
            state,
            seq: 1,
            stages: vec![Stage::Spec],
        }
    }

    /// 低档建单（`created` 夹具是 cap Medium / effective Low）。
    fn low() -> Self {
        Self::new()
    }

    fn commit(&mut self, event: NewEvent) {
        event
            .validate()
            .unwrap_or_else(|e| panic!("{} invalid: {e}", event.ty.as_str()));
        self.seq += 1;
        let event = Event {
            id: format!("evt_{:0>26}", self.seq).parse::<EventId>().unwrap(),
            seq: Seq::new(self.seq).unwrap(),
            ts: ts(self.seq),
            ty: event.ty,
            subject: event.subject,
            project_id: event.project_id,
            actor: event.actor,
            causation_id: event.causation_id,
            correlation_id: event.correlation_id,
            data: event.data,
        };
        self.state.apply(&event).unwrap();
        if self.stages.last() != Some(&self.state.delivery.stage) {
            self.stages.push(self.state.delivery.stage);
        }
    }

    /// 外部模块（repo、forge、policy、notify）写入的事件。
    fn external(&mut self, ty: &str, subject: &str, data: Value) {
        self.commit(NewEvent {
            ty: ty.parse().unwrap(),
            subject: Subject::parse(subject).unwrap(),
            project_id: Some(PROJECT.parse().unwrap()),
            actor: Actor::new(ActorKind::System, "test"),
            causation_id: None,
            correlation_id: Some(DELIVERY.to_owned()),
            data,
            dedup_key: None,
        });
    }

    fn decide(&self, input: Input, identity: InputIdentity) -> Vec<Decision> {
        let env = self.fx.env(&self.state, &identity);
        decide(&self.state, &input, &env)
    }

    fn tick(&self) -> Vec<Decision> {
        self.decide(
            Input::Tick(TickReason::Settle),
            InputIdentity::Tick(TickReason::Settle),
        )
    }

    /// 执行事务内决策（`Append`），返回其余决策；`Raise` 以 `inbox.raised` 落盘。
    fn apply(&mut self, decisions: Vec<Decision>) -> Vec<Decision> {
        let mut rest = Vec::new();
        for decision in decisions {
            match decision {
                Decision::Append(event) => self.commit(event),
                Decision::Raise(request) => self.raise(&request),
                other => rest.push(other),
            }
        }
        rest
    }

    fn raise(&mut self, request: &gantry_core::ports::inbox::RaiseRequest) {
        let id = gantry_core::ids::InboxId::from_parts(T0 as u64, self.seq as u128)
            .unwrap()
            .to_string();
        self.commit(NewEvent {
            ty: EventType::InboxRaised,
            subject: Subject::parse(&id).unwrap(),
            project_id: request.project_id,
            actor: request.actor.clone(),
            causation_id: None,
            correlation_id: Some(DELIVERY.to_owned()),
            data: json!({"item": {
                "id": id, "project_id": PROJECT, "delivery_id": DELIVERY,
                "kind": request.kind, "severity": request.severity,
                "summary": request.summary.as_str(), "payload": request.payload,
                "status": "open", "dedup_key": request.dedup_key.as_str(),
                "created_at": "2026-10-24T12:41:03.120Z", "updated_at": "2026-10-24T12:41:03.120Z"
            }}),
            dedup_key: None,
        });
    }

    /// `Tick(Settle)` 直到不再产出事务内决策（§7.14.1 第 4 步）；返回最后一轮的副作用。
    fn settle(&mut self) -> Vec<Decision> {
        // 执行体每一轮都会发起本轮的副作用；这里累积全部轮次并去重（在途集合由执行体维护，模拟器不做）。
        let mut effects: Vec<Decision> = Vec::new();
        for _ in 0..EngineConsts::default().max_settle {
            let decisions = self.tick();
            let transactional = decisions.iter().any(|d| {
                matches!(
                    d,
                    Decision::Append(_) | Decision::Raise(_) | Decision::Enqueue(_)
                )
            });
            for effect in self.apply(decisions) {
                if !effects.contains(&effect) {
                    effects.push(effect);
                }
            }
            if !transactional {
                return effects;
            }
        }
        panic!("settle did not converge within MAX_SETTLE");
    }

    fn report(&mut self, report: EffectReport, key: EffectKey) -> Vec<Decision> {
        let decisions = self.decide(Input::Report(report), InputIdentity::Report(key));
        self.apply(decisions)
    }

    /// 模拟 OutboxPort::enqueue_in + forge 执行成功。
    fn enqueue(
        &mut self,
        request: &gantry_core::ports::forge::EnqueueRequest,
        action: &str,
        pr: Option<u64>,
    ) {
        let derived = gantry_core::forge::derive_target_intent(&request.payload).unwrap();
        let key = gantry_core::canon::IdempotencyKey::derive(
            &request.project_id,
            &request.delivery_id,
            request.payload.kind(),
            &derived.target,
            &derived.intent,
            request.epoch,
        )
        .unwrap();
        let payload = match &request.payload {
            ActionPayload::PushBranch(p) => serde_json::to_value(p),
            ActionPayload::CreatePr(p) => serde_json::to_value(p),
            ActionPayload::MergePr(p) => serde_json::to_value(p),
            ActionPayload::UpdatePr(p) => serde_json::to_value(p),
            other => panic!("unexpected {:?}", other.kind()),
        }
        .unwrap();
        self.external(
            "outbox.enqueued",
            action,
            json!({"action": {
                "delivery_id": DELIVERY,
                "kind": request.payload.kind(),
                "payload": payload,
                "target": derived.target,
                "intent": derived.intent,
                "idempotency_key": key.as_str(),
                "requires_approval": request.requires_approval,
                "depends_on": request.depends_on,
                "priority": request.priority,
                "not_before": null,
                "epoch": request.epoch,
                "coalesce_key": null,
                "enqueued_by": {"module": "engine", "causation_id": null}
            }}),
        );
        self.external(
            "outbox.executing",
            action,
            json!({"attempts": 1, "replay": false}),
        );
        let github_ref = match pr {
            Some(number) => {
                json!({"pr": {"number": number, "node_id": "PR_x", "head_sha": COMMIT}})
            }
            None => json!({}),
        };
        self.external(
            "outbox.succeeded",
            action,
            json!({"github_ref": github_ref}),
        );
    }
}

fn judge(decisions: &[Decision]) -> Vec<(ActionKey, ForgeActionKind)> {
    decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Effect(Effect::JudgeAction { key, request }) => {
                Some((key.clone(), request.kind))
            }
            _ => None,
        })
        .collect()
}

fn allow(approval: Option<ApprovalId>) -> ActionDecision {
    ActionDecision::Allow {
        action_id: PolicyActionId::PushBranch,
        approval,
        config_rev: Seq::new(1).unwrap(),
    }
}

fn snapshot(head: &str, required_ok: bool, state: &str) -> PullRequestRef {
    serde_json::from_value(json!({
        "repo": "acme/web", "number": 12, "node_id": "PR_x", "title": {"text": "t", "source": "github", "truncated": false},
        "state": state, "head_sha": head, "base_ref": "main", "base_sha": BASE_SHA,
        "draft": false, "mergeable_state": "clean", "in_merge_queue": false,
        "checks": {"passed": 1, "failed": 0, "pending": 0, "required_ok": required_ok},
        "review_state": "none", "synced_at": "2026-10-24T12:41:03.120Z"
    }))
    .unwrap()
}

/// 推进到 `implement` 并走完派发：`EnsureWorktree` → repo 写 `worktree.created`；
/// `AcquireLock` → repo 写 `worktree.locked`，执行体记下 `lock_owner`；然后派发出 `RunAttempt`。
fn dispatched() -> Sim {
    let mut sim = Sim::low();
    let effects = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Implement);
    let [Decision::Effect(Effect::EnsureWorktree { branch, base_ref })] = effects.as_slice() else {
        panic!("expected EnsureWorktree, got {effects:?}");
    };
    assert_eq!(branch.as_str(), "gantry/dlv-5t7w1yzb-fix-login-timeout");
    assert_eq!(base_ref, "main");
    sim.external(
        "worktree.created",
        WORKTREE,
        json!({"worktree_id": WORKTREE, "project_id": PROJECT, "delivery_id": DELIVERY,
               "branch": branch.as_str(), "base_ref": "main", "base_sha": BASE_SHA}),
    );
    let effects = sim.settle();
    let [Decision::Effect(Effect::AcquireLock {
        worktree_id,
        node_id,
        lease_s,
        ..
    })] = effects.as_slice()
    else {
        panic!("expected AcquireLock, got {effects:?}");
    };
    // 有效 wall_time = min(900, 1800, 3600)；租约 = 900 + 10 + 60。
    assert_eq!(*lease_s, 970);
    let node_id = *node_id;
    sim.external(
        "worktree.locked",
        &worktree_id.to_string(),
        json!({"worktree_id": worktree_id, "holder": "engine", "delivery_id": DELIVERY,
               "lease_until": "2026-10-24T12:45:00.000Z"}),
    );
    sim.state.lock_owner.insert(*worktree_id, node_id);
    let effects = sim.settle();
    let [Decision::Effect(Effect::RunAttempt(plan))] = effects.as_slice() else {
        panic!("expected RunAttempt, got {effects:?}");
    };
    assert_eq!(plan.node_id, node_id);
    assert_eq!(plan.wall_time_s, 900);
    assert_eq!(plan.tokens_total, 1000);
    assert!(plan.lock.is_some());
    assert!(!plan.resume);
    assert!(plan.instructions.as_str().ends_with("修复登录超时"));
    let node = sim.state.node(&node_id).unwrap();
    assert_eq!(node.status, NodeStatus::Running);
    assert_eq!(node.attempts.len(), 1);
    assert_eq!(sim.state.ledger.used.attempts, 1);
    sim
}

fn finalized(sim: &Sim, proposals: Vec<ProposalOutcome>) -> Finalized {
    let node = sim
        .state
        .workflows
        .values()
        .next()
        .unwrap()
        .nodes
        .values()
        .next()
        .unwrap();
    Finalized {
        attempt_id: node.attempts[0].id,
        node_id: node.id,
        status: AttemptStatus::Succeeded,
        failure_class: None,
        failure_detail: None,
        result_ref: Some(serde_json::from_value(json!(RESULT_REF)).unwrap()),
        usage_metered: TokenUsage::default(),
        weighted_tokens: 0,
        weight_audit: serde_json::from_value(json!({"basis": "job_token", "config_rev": 1, "weights": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25}})).unwrap(),
        wall_time_s: 120,
        verdict: None,
        patch: PatchResult::Committed {
            commit: oid(COMMIT),
            tree: oid(TREE),
            sha256: serde_json::from_value(json!(PATCH_SHA)).unwrap(),
        },
        proposals,
        signals: OutputSignals::default(),
    }
}

/// 终结：repo 先写 `patch.validated(allow)`，再由引擎写 `attempt.finished` 等。
fn finish(sim: &mut Sim, proposals: Vec<ProposalOutcome>) {
    let f = finalized(sim, proposals);
    let attempt = f.attempt_id;
    sim.external(
        "patch.validated",
        &attempt.to_string(),
        json!({"job_id": attempt, "sha256": PATCH_SHA, "base_sha": BASE_SHA, "result_tree": TREE,
               "verdict": "allow", "files": [], "reasons": [], "config_rev": 1}),
    );
    let rest = sim.report(EffectReport::AttemptFinalized(f), EffectKey::Run(attempt));
    // attempt.finished 落库后恰好释放一次沙盒与写权锁（§7.8 第 5 步）。
    assert!(
        matches!(
            rest.as_slice(),
            [
                Decision::Effect(Effect::ReleaseSandbox { attempt_id }),
                Decision::Effect(Effect::ReleaseLock { .. }),
            ] if *attempt_id == attempt
        ),
        "unexpected decisions after finalize: {rest:?}"
    );
}

/// 推进到 propose 并按脚本放行 push_branch / create_pr；返回 (push 键, create_pr 请求)。
fn proposed(sim: &mut Sim) -> (ActionKey, gantry_core::ports::forge::EnqueueRequest) {
    let effects = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Propose);
    let judged = judge(&effects);
    let push = judged
        .iter()
        .find(|(_, k)| *k == ForgeActionKind::PushBranch)
        .unwrap()
        .0
        .clone();
    let create = judged
        .iter()
        .find(|(_, k)| *k == ForgeActionKind::CreatePr)
        .unwrap()
        .0
        .clone();

    // create_pr 先回流：push_branch 还不在 outbox，依赖未满足 → 不入队。
    let early = sim.report(
        EffectReport::ActionJudged {
            key: create.clone(),
            decision: allow(None),
        },
        EffectKey::Judge(create.clone()),
    );
    assert!(early.is_empty());
    assert!(!sim.state.outbox.contains_key(&create));

    let enqueue = enqueue_of(sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: push.clone(),
            decision: allow(None),
        }),
        InputIdentity::Report(EffectKey::Judge(push.clone())),
    ));
    sim.enqueue(&enqueue, "act_01JBA1D7P2KS6W8Y0A4C1F5GJN", None);
    let create_req = enqueue_of(sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: create.clone(),
            decision: allow(None),
        }),
        InputIdentity::Report(EffectKey::Judge(create.clone())),
    ));
    (push, create_req)
}

fn enqueue_of(decisions: Vec<Decision>) -> gantry_core::ports::forge::EnqueueRequest {
    match decisions.as_slice() {
        [Decision::Enqueue(request)] => request.clone(),
        other => panic!("expected one Enqueue, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// §12.2 端到端（替身）与"合并需要审批"
// ---------------------------------------------------------------------------
fn review_stage() -> Sim {
    let mut sim = dispatched();
    finish(&mut sim, vec![]);
    let (_, create) = proposed(&mut sim);
    sim.enqueue(&create, "act_01JBA1D7P2KS6W8Y0A4C1F5GJP", Some(12));
    sim.state.pr_snapshot = Some(snapshot(COMMIT, false, "open"));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Review);
    sim
}

fn merge_stage() -> Sim {
    let mut sim = review_stage();
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "open"));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Merge);
    sim
}

/// 当前在跑的尝试以 `detail` 失败（`patch` 为补丁结论）。
fn fail_running(
    sim: &mut Sim,
    class: gantry_core::entities::FailureClass,
    detail: &str,
    patch: PatchResult,
) -> Vec<Decision> {
    let node = sim
        .state
        .workflows
        .values()
        .next()
        .unwrap()
        .nodes
        .values()
        .next()
        .unwrap()
        .clone();
    let attempt = node
        .attempts
        .iter()
        .find(|a| a.finished_at.is_none())
        .unwrap()
        .id;
    let mut f = finalized(sim, vec![]);
    f.attempt_id = attempt;
    f.status = if class == gantry_core::entities::FailureClass::Cancelled {
        AttemptStatus::Cancelled
    } else {
        AttemptStatus::Failed
    };
    f.failure_class = Some(class);
    f.failure_detail = Some(gantry_core::entities::FailureDetail::parse(detail).unwrap());
    f.result_ref = None;
    f.patch = patch;
    sim.report(EffectReport::AttemptFinalized(f), EffectKey::Run(attempt))
}

fn node_status(sim: &Sim) -> NodeStatus {
    sim.state
        .workflows
        .values()
        .next()
        .unwrap()
        .nodes
        .values()
        .next()
        .unwrap()
        .status
}

fn attempts(sim: &Sim) -> usize {
    sim.state
        .workflows
        .values()
        .next()
        .unwrap()
        .nodes
        .values()
        .next()
        .unwrap()
        .attempts
        .len()
}

const ACTION_PUSH: &str = "act_01JBA1D7P2KS6W8Y0A4C1F5GJN";
const ACTION_UPDATE: &str = "act_01JBA1D7P2KS6W8Y0A4C1F5GJR";

/// 只模拟 `OutboxPort::enqueue_in`：动作停在 queued。
fn enqueue_only(sim: &mut Sim, request: &gantry_core::ports::forge::EnqueueRequest, action: &str) {
    let derived = gantry_core::forge::derive_target_intent(&request.payload).unwrap();
    let key = gantry_core::canon::IdempotencyKey::derive(
        &request.project_id,
        &request.delivery_id,
        request.payload.kind(),
        &derived.target,
        &derived.intent,
        request.epoch,
    )
    .unwrap();
    let payload = match &request.payload {
        ActionPayload::PushBranch(p) => serde_json::to_value(p),
        ActionPayload::UpdatePr(p) => serde_json::to_value(p),
        other => panic!("unexpected {:?}", other.kind()),
    }
    .unwrap();
    sim.external(
        "outbox.enqueued",
        action,
        json!({"action": {
            "delivery_id": DELIVERY,
            "kind": request.payload.kind(),
            "payload": payload,
            "target": derived.target,
            "intent": derived.intent,
            "idempotency_key": key.as_str(),
            "requires_approval": request.requires_approval,
            "depends_on": request.depends_on,
            "priority": request.priority,
            "not_before": null,
            "epoch": request.epoch,
            "coalesce_key": null,
            "enqueued_by": {"module": "engine", "causation_id": null}
        }}),
    );
}

// ---------------------------------------------------------------------------
// 本文件的辅助
// ---------------------------------------------------------------------------

const NEW_HEAD: &str = "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b";

fn ctx(dedup: Option<&str>) -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        device: None,
        role: None,
        client_profile: None,
        request_id: gantry_core::ids::Ulid::from_parts(1, 7).unwrap(),
        idempotency_key: None,
        dedup_key: dedup.map(str::to_owned),
        causation_id: None,
    }
}

fn command(sim: &Sim, command: Command) -> Vec<Decision> {
    let id = gantry_core::ids::Ulid::from_parts(1, 7).unwrap();
    sim.decide(Input::Command(command), InputIdentity::Command(id))
}

fn evt(seq: u64) -> EventId {
    format!("evt_{seq:0>26}").parse().unwrap()
}

/// 已提交的最后一条事件作为观察输入（`approval.*`、`inbox.resolved` 先 `external` 再观察）。
fn observed_last(sim: &Sim, data: ObservedData) -> Observed {
    let seq = sim.state.last_seq.as_u64();
    Observed {
        seq: sim.state.last_seq,
        id: evt(seq),
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        data,
    }
}

/// 尚未折叠进状态的外部事件（`forge.*` 只投递、不折叠）：`seq` 在日志头之后。
fn observed_next(sim: &Sim, data: ObservedData) -> Observed {
    let seq = sim.seq + 1;
    Observed {
        seq: Seq::new(seq).unwrap(),
        id: evt(seq),
        actor: Actor::new(ActorKind::System, "forge"),
        data,
    }
}

fn on_event(sim: &Sim, observed: Observed) -> Vec<Decision> {
    let id = observed.id;
    sim.decide(Input::Event(observed), InputIdentity::Event(id))
}

fn appended(decisions: &[Decision]) -> Vec<&NewEvent> {
    decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Append(event) => Some(event),
            _ => None,
        })
        .collect()
}

fn types(decisions: &[Decision]) -> Vec<&'static str> {
    appended(decisions).iter().map(|e| e.ty.as_str()).collect()
}

fn effects(decisions: &[Decision]) -> Vec<&Effect> {
    decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Effect(effect) => Some(effect),
            _ => None,
        })
        .collect()
}

fn replies(decisions: &[Decision]) -> Vec<&Result<(), CommandRejection>> {
    decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Reply(r) => Some(r),
            _ => None,
        })
        .collect()
}

fn snapshot_at(head: &str, required_ok: bool, state: &str, synced_at: &str) -> PullRequestRef {
    serde_json::from_value(json!({
        "repo": "acme/web", "number": 12, "node_id": "PR_x", "title": {"text": "t", "source": "github", "truncated": false},
        "state": state, "head_sha": head, "base_ref": "main", "base_sha": BASE_SHA,
        "draft": false, "mergeable_state": "clean", "in_merge_queue": false,
        "checks": {"passed": 1, "failed": 0, "pending": 0, "required_ok": required_ok},
        "review_state": "none", "synced_at": synced_at
    }))
    .unwrap()
}

fn node_id(sim: &Sim) -> NodeId {
    *sim.state
        .workflows
        .values()
        .next()
        .unwrap()
        .nodes
        .keys()
        .next()
        .unwrap()
}

/// merge 阶段且有一份 pending 的合并审批与对应的 approval_needed。
fn merge_with_pending_approval() -> Sim {
    let mut sim = merge_stage();
    request_approval(&mut sim, APPROVAL, COMMIT);
    let _ = sim.settle();
    assert!(sim
        .state
        .open_inbox
        .values()
        .any(|i| i.approval_id == Some(APPROVAL.parse().unwrap())));
    sim
}

fn request_approval(sim: &mut Sim, id: &str, head: &str) {
    sim.external(
        "approval.requested",
        id,
        json!({"action": "merge_pr", "target": {"kind": "pr", "repo": "acme/web", "number": 12},
               "subject_digest": head, "expires_at": "2026-10-24T12:45:00.000Z", "config_rev": 1,
               "requested_by": {"kind": "engine", "id": "engine"}}),
    );
}

/// 注入一项上报（`inbox.raised`）并返回它在 `open_inbox` 里的快照。
fn inject(sim: &mut Sim, kind: InboxKind, payload: InboxPayload, dedup: &str) -> OpenInbox {
    sim.raise(&RaiseRequest {
        project_id: Some(PROJECT.parse().unwrap()),
        delivery_id: Some(DELIVERY.parse().unwrap()),
        kind,
        severity: Severity::Blocker,
        summary: SafeSummary::new("injected"),
        payload,
        dedup_key: DedupKey::new(dedup),
        supersedes: Vec::new(),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
    });
    sim.state
        .open_inbox
        .values()
        .find(|i| i.dedup_key.as_str() == dedup)
        .unwrap()
        .clone()
}

/// notify 写 `inbox.resolved(chosen, option)`，返回对应的观察输入（`item` 由调用方决定）。
fn choose(sim: &mut Sim, item: &OpenInbox, option: &str, with_item: Option<OpenInbox>) -> Observed {
    sim.external(
        "inbox.resolved",
        &item.id.to_string(),
        json!({"inbox_id": item.id, "resolution": "chosen", "option_id": option}),
    );
    observed_last(
        sim,
        ObservedData::InboxResolved {
            inbox_id: item.id,
            resolution: InboxResolution::Chosen,
            option_id: Some(OptionId::new(option)),
            item: with_item,
        },
    )
}

fn level_cap_exceeded(sim: &mut Sim, target: Option<Level>) -> OpenInbox {
    inject(
        sim,
        InboxKind::Escalation,
        InboxPayload::Escalation {
            reason_code: ReasonCode::parse("level_cap_exceeded").unwrap(),
            node_id: None,
            attempt_id: None,
            target_level: target,
            action_id: None,
            evidence: Vec::new(),
            options: Some(vec![
                InboxOption::new("upgrade", "升档", true),
                InboxOption::new("narrow", "缩小范围", false),
                InboxOption::new("abandon", "放弃", false),
            ]),
        },
        &format!("esc:{DELIVERY}:level_cap_exceeded:{target:?}"),
    )
}

fn decision_needed(sim: &mut Sim, kind: DecisionKind, reason: &str, options: &[&str]) -> OpenInbox {
    inject(
        sim,
        InboxKind::DecisionNeeded,
        InboxPayload::DecisionNeeded {
            kind,
            reason_code: ReasonCode::parse(reason).unwrap(),
            options: options
                .iter()
                .enumerate()
                .map(|(i, id)| InboxOption::new(*id, *id, i == 0))
                .collect(),
            node_id: None,
            patch: None,
        },
        &format!("decision:{DELIVERY}:{reason}"),
    )
}

// ---------------------------------------------------------------------------
// 命令：SetLevelCap（§7.14.5 `level_changed(kind = cap)` 行）与 dedup_key 约定（#99）
// ---------------------------------------------------------------------------

#[test]
fn set_level_cap_preconditions_and_dedup_key() {
    let mut sim = Sim::low();
    // 反例：None → Validation；同值 → Ok 且不写事件。
    assert_eq!(
        command(
            &sim,
            Command::SetLevelCap {
                cap: Level::None,
                ctx: ctx(None)
            }
        ),
        vec![Decision::Reply(Err(CommandRejection::Validation(
            "delivery level_cap must not be None"
        )))]
    );
    assert_eq!(
        command(
            &sim,
            Command::SetLevelCap {
                cap: Level::Medium,
                ctx: ctx(None)
            }
        ),
        vec![Decision::Reply(Ok(()))]
    );
    // 正例：Medium → High；第 1 条业务事件的 dedup_key = ctx.dedup_key，actor 取调用方。
    let out = command(
        &sim,
        Command::SetLevelCap {
            cap: Level::High,
            ctx: ctx(Some("req-1")),
        },
    );
    assert_eq!(types(&out), vec!["delivery.level_changed"]);
    let event = appended(&out)[0];
    assert_eq!(event.dedup_key.as_deref(), Some("req-1"));
    assert_eq!(event.actor.kind, ActorKind::Device);
    assert_eq!(
        event.data,
        json!({"from": "Medium", "to": "High", "kind": "cap", "reason": "user"})
    );
    assert_eq!(replies(&out), vec![&Ok(())]);
    // ctx.dedup_key 为空时不带。
    let out = command(
        &sim,
        Command::SetLevelCap {
            cap: Level::High,
            ctx: ctx(None),
        },
    );
    assert_eq!(appended(&out)[0].dedup_key, None);
    sim.apply(out);
    assert_eq!(sim.state.delivery.level_cap, Level::High);
    // 反例：终态 → InvalidState。
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    assert_eq!(
        command(
            &sim,
            Command::SetLevelCap {
                cap: Level::Max,
                ctx: ctx(None)
            }
        ),
        vec![Decision::Reply(Err(CommandRejection::InvalidState(
            "delivery is terminal"
        )))]
    );
}

// ---------------------------------------------------------------------------
// §12.2"暂停 / 继续 / 重试"（P-56）；§7.14.5 paused / resumed、retry_requested 行
// ---------------------------------------------------------------------------

#[test]
fn pause_stops_dispatch_and_resume_restores_status() {
    let mut sim = Sim::low();
    let out = command(&sim, Command::Pause { ctx: ctx(None) });
    assert_eq!(types(&out), vec!["delivery.paused"]);
    assert_eq!(
        appended(&out)[0].data,
        json!({"by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM", "status_before": "running"})
    );
    sim.apply(out);
    assert_eq!(sim.state.delivery.stage_status, StageStatus::Paused);
    // 暂停后不推进、不派发。
    assert!(sim.settle().is_empty());
    assert_eq!(sim.state.delivery.stage, Stage::Spec);
    // 已暂停再 Pause → Ok，不写事件。
    assert_eq!(
        command(&sim, Command::Pause { ctx: ctx(None) }),
        vec![Decision::Reply(Ok(()))]
    );
    // Resume → 恢复暂停前的值；之后照常推进。
    let out = command(&sim, Command::Resume { ctx: ctx(None) });
    assert_eq!(types(&out), vec!["delivery.resumed"]);
    assert_eq!(appended(&out)[0].data["status_after"], json!("running"));
    sim.apply(out);
    assert_eq!(sim.state.delivery.stage_status, StageStatus::Running);
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Implement);
    // 反例：未暂停 Resume → InvalidState；终态 Pause → InvalidState。
    assert_eq!(
        command(&sim, Command::Resume { ctx: ctx(None) }),
        vec![Decision::Reply(Err(CommandRejection::InvalidState(
            "delivery is not paused"
        )))]
    );
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    assert!(matches!(
        command(&sim, Command::Pause { ctx: ctx(None) }).as_slice(),
        [Decision::Reply(Err(CommandRejection::InvalidState(_)))]
    ));
}

/// 暂停时运行中的尝试照常跑完并落库；不进入下一阶段，恢复后才推进。
#[test]
fn running_attempt_finishes_while_paused() {
    let mut sim = dispatched();
    sim.apply(command(&sim, Command::Pause { ctx: ctx(None) }));
    finish(&mut sim, vec![]);
    let node = sim.state.node(&node_id(&sim)).unwrap();
    assert_eq!(node.status, NodeStatus::Succeeded);
    assert!(types(&sim.settle()).is_empty());
    assert_eq!(sim.state.delivery.stage, Stage::Implement);
    sim.apply(command(&sim, Command::Resume { ctx: ctx(None) }));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Propose);
}

#[test]
fn retry_node_requeues_failed_node_and_resolves_its_escalation() {
    let mut sim = dispatched();
    let node = node_id(&sim);
    // 反例：节点不是 failed（running）→ InvalidState。
    assert_eq!(
        command(
            &sim,
            Command::RetryNode {
                node_id: node,
                ctx: ctx(None)
            }
        ),
        vec![Decision::Reply(Err(CommandRejection::InvalidState(
            "node is not failed"
        )))]
    );
    fail_running(
        &mut sim,
        FailureClass::ExecFailed,
        "exec.bad_job",
        PatchResult::None,
    );
    assert_eq!(node_status(&sim), NodeStatus::Failed);
    assert_eq!(sim.state.stage_status(), StageStatus::WaitingHuman);
    let dedup = gantry_engine::decider::attempt_failed_dedup(DELIVERY.parse().unwrap(), node);

    // 正例：node.retry_requested + 同一决策关闭 escalation(attempt_failed) + Ok。
    let out = command(
        &sim,
        Command::RetryNode {
            node_id: node,
            ctx: ctx(Some("retry-1")),
        },
    );
    assert_eq!(types(&out), vec!["node.retry_requested"]);
    assert_eq!(appended(&out)[0].dedup_key.as_deref(), Some("retry-1"));
    assert!(out.contains(&Decision::Resolve {
        target: ResolveTarget::DedupKey(DedupKey::new(dedup.clone())),
        how: Resolution::System {
            reason: "node_retry_requested".to_owned()
        },
    }));
    assert_eq!(replies(&out), vec![&Ok(())]);
    sim.apply(out);
    // notify 落盘关闭（InboxPort::resolve）。
    let item = sim
        .state
        .open_inbox
        .values()
        .find(|i| i.dedup_key.as_str() == dedup)
        .unwrap()
        .id;
    sim.external(
        "inbox.resolved",
        &item.to_string(),
        json!({"inbox_id": item, "resolution": "system", "reason": "node_retry_requested"}),
    );
    assert_eq!(node_status(&sim), NodeStatus::Queued);
    assert_eq!(sim.state.stage_status(), StageStatus::Running);
    // failed → queued 后新建 Attempt。
    let _ = sim.settle();
    assert_eq!(attempts(&sim), 2);
    assert_eq!(node_status(&sim), NodeStatus::Running);
    // 反例：重复调用（节点已不是 failed）→ InvalidState；未知节点 → InvalidState。
    assert!(matches!(
        command(
            &sim,
            Command::RetryNode {
                node_id: node,
                ctx: ctx(None)
            }
        )
        .as_slice(),
        [Decision::Reply(Err(CommandRejection::InvalidState(_)))]
    ));
    assert!(matches!(
        command(
            &sim,
            Command::RetryNode {
                node_id: common::OTHER_NODE.parse().unwrap(),
                ctx: ctx(None)
            }
        )
        .as_slice(),
        [Decision::Reply(Err(CommandRejection::InvalidState(_)))]
    ));
}

/// 没有未决的 attempt_failed 上报时只写 retry_requested，不产出 Resolve。
#[test]
fn retry_node_without_open_escalation_does_not_resolve() {
    let mut sim = dispatched();
    let node = node_id(&sim);
    fail_running(
        &mut sim,
        FailureClass::ExecFailed,
        "exec.bad_job",
        PatchResult::None,
    );
    let item = sim.state.open_inbox.keys().next().copied().unwrap();
    sim.external(
        "inbox.resolved",
        &item.to_string(),
        json!({"inbox_id": item, "resolution": "system", "reason": "x"}),
    );
    let out = command(
        &sim,
        Command::RetryNode {
            node_id: node,
            ctx: ctx(None),
        },
    );
    assert_eq!(types(&out), vec!["node.retry_requested"]);
    assert!(!out.iter().any(|d| matches!(d, Decision::Resolve { .. })));
}

// ---------------------------------------------------------------------------
// §12.2"放弃"（T13）与 §7.14.5 abandoned / 放弃收尾行
// ---------------------------------------------------------------------------

#[test]
fn abandon_command_preconditions() {
    let mut sim = Sim::low();
    // 反例：保留原因 / 超长原因 → Validation。
    for reason in ["pr_closed", "issue_not_planned"] {
        assert!(matches!(
            command(
                &sim,
                Command::Abandon {
                    reason: Some(reason.to_owned()),
                    ctx: ctx(None)
                }
            )
            .as_slice(),
            [Decision::Reply(Err(CommandRejection::Validation(_)))]
        ));
    }
    assert!(matches!(
        command(
            &sim,
            Command::Abandon {
                reason: Some("x".repeat(501)),
                ctx: ctx(None)
            }
        )
        .as_slice(),
        [Decision::Reply(Err(CommandRejection::Validation(_)))]
    ));
    // spec 阶段放弃：没有可清理的资源，收尾不产出。
    let out = command(
        &sim,
        Command::Abandon {
            reason: Some("不需要了".to_owned()),
            ctx: ctx(Some("ab-1")),
        },
    );
    assert_eq!(types(&out), vec!["delivery.abandoned"]);
    assert_eq!(appended(&out)[0].dedup_key.as_deref(), Some("ab-1"));
    sim.apply(out);
    assert_eq!(sim.state.delivery.stage, Stage::Abandoned);
    assert_eq!(
        sim.state.delivery.abandon_cause,
        Some(AbandonCause::UserAbandoned)
    );
    assert!(sim.settle().is_empty());
    // 重复放弃：Ok，不写事件。
    assert_eq!(
        command(
            &sim,
            Command::Abandon {
                reason: None,
                ctx: ctx(None)
            }
        ),
        vec![Decision::Reply(Ok(()))]
    );
}

#[test]
fn abandon_in_implement_cancels_attempt_then_releases_worktree() {
    let mut sim = dispatched();
    let node = node_id(&sim);
    // 反例：未放弃时没有任何收尾决策。
    assert!(!sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Effect(Effect::CancelAttempt { .. }))));
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    let out = sim.tick();
    assert_eq!(types(&out), vec!["workflow.finished"]);
    assert_eq!(appended(&out)[0].data, json!({"status": "cancelled"}));
    let attempt = sim.state.node(&node).unwrap().attempts[0].id;
    assert_eq!(
        effects(&out),
        vec![&Effect::CancelAttempt {
            attempt_id: attempt,
            why: gantry_core::ports::engine::CancelWhy::DeliveryClosed
        }]
    );
    sim.apply(out);
    // 尝试未终结前不删工作树；终结（cancelled）后节点 cancelled，再删工作树。
    assert!(!effects(&sim.tick())
        .iter()
        .any(|e| matches!(e, Effect::RemoveWorktree { .. })));
    fail_running(
        &mut sim,
        FailureClass::Cancelled,
        "cancel.requested",
        PatchResult::None,
    );
    assert_eq!(node_status(&sim), NodeStatus::Cancelled);
    let out = sim.settle();
    assert!(matches!(
        out.as_slice(),
        [Decision::Effect(Effect::RemoveWorktree { .. })]
    ));
    // 收尾可重放：资源已清理后再求值，只剩幂等的 RemoveWorktree（工作树事件回流前）。
    let worktree = sim.state.worktree.as_ref().unwrap().id;
    sim.external(
        "worktree.removed",
        &worktree.to_string(),
        json!({"worktree_id": worktree, "branch": "gantry/x"}),
    );
    assert!(sim.settle().is_empty());
}

/// pending / queued 节点 → node.finished(cancelled)。
#[test]
fn abandon_cancels_pending_and_queued_nodes() {
    let mut sim = Sim::low();
    // 推进到 implement 并建图（节点 pending → queued），还没有工作树。
    let _ = sim.settle();
    assert_eq!(node_status(&sim), NodeStatus::Queued);
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    let out = sim.settle();
    assert!(
        out.is_empty(),
        "no effects without attempts or worktree: {out:?}"
    );
    assert_eq!(node_status(&sim), NodeStatus::Cancelled);
    assert!(sim
        .state
        .workflows
        .values()
        .all(|wf| wf.status == gantry_core::entities::WorkflowStatus::Cancelled));
}

/// propose：排队中的 Outbox 动作收到 CancelOutbox。
#[test]
fn abandon_in_propose_cancels_queued_outbox_actions() {
    let mut sim = dispatched();
    finish(&mut sim, vec![]);
    let _ = sim.settle();
    let push = judge(&sim.tick())
        .into_iter()
        .find(|(_, k)| *k == ForgeActionKind::PushBranch)
        .unwrap()
        .0;
    let request = enqueue_of(sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: push.clone(),
            decision: allow(None),
        }),
        InputIdentity::Report(EffectKey::Judge(push.clone())),
    ));
    enqueue_only(&mut sim, &request, ACTION_PUSH);
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    let out = sim.settle();
    assert!(out.contains(&Decision::Effect(Effect::CancelOutbox {
        action_id: ACTION_PUSH.parse().unwrap(),
        reason: CancelReason::DeliveryAbandoned,
    })));
    // 没有 PR：不期望 update_pr。
    assert!(judge(&out).is_empty());
    assert!(out
        .iter()
        .any(|d| matches!(d, Decision::Effect(Effect::RemoveWorktree { .. }))));
    // 反例：已取消（outbox.cancelled 回流）后不再产出。
    sim.external("outbox.cancelled", ACTION_PUSH, json!({"reason": "manual"}));
    assert!(!sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Effect(Effect::CancelOutbox { .. }))));
}

/// merge：作废 pending 审批；PR 仍 open → 期望 update_pr(closed)，入队后不被自己的收尾取消。
#[test]
fn abandon_in_merge_invalidates_approval_and_closes_pr() {
    let mut sim = merge_with_pending_approval();
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    let out = sim.settle();
    let approval: ApprovalId = APPROVAL.parse().unwrap();
    assert!(out.contains(&Decision::Effect(Effect::InvalidateApproval {
        approval_id: approval,
        reason: InvalidateReason::DeliveryAbandoned,
    })));
    let judged = judge(&out);
    assert_eq!(judged.len(), 1);
    assert_eq!(judged[0].1, ForgeActionKind::UpdatePr);
    let key = judged[0].0.clone();
    let request = enqueue_of(sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: key.clone(),
            decision: allow(None),
        }),
        InputIdentity::Report(EffectKey::Judge(key.clone())),
    ));
    let ActionPayload::UpdatePr(payload) = &request.payload else {
        panic!("expected update_pr, got {:?}", request.payload.kind())
    };
    assert_eq!(payload.state, Some(PrState::Closed));
    assert_eq!(payload.pull_number, 12);
    enqueue_only(&mut sim, &request, ACTION_UPDATE);
    // 反例：期望集合里的 update_pr 不被取消；审批已作废后不再作废。
    sim.external(
        "approval.invalidated",
        APPROVAL,
        json!({"reason": "delivery_abandoned"}),
    );
    let out = sim.settle();
    assert!(!out.iter().any(|d| matches!(
        d,
        Decision::Effect(Effect::CancelOutbox { .. } | Effect::InvalidateApproval { .. })
    )));
}

/// review：PR 仍 open → update_pr(closed)。
#[test]
fn abandon_in_review_closes_open_pr() {
    let mut sim = review_stage();
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    let judged = judge(&sim.settle());
    assert_eq!(judged.len(), 1);
    assert_eq!(judged[0].1, ForgeActionKind::UpdatePr);
    // 反例：PR 已关闭（快照）→ 不期望 update_pr。
    sim.state.pr_snapshot = Some(snapshot(COMMIT, false, "closed"));
    assert!(judge(&sim.tick()).is_empty());
}

// ---------------------------------------------------------------------------
// §12.2"PR 关闭与重开"：T14 / T15
// ---------------------------------------------------------------------------

#[test]
fn pr_closed_abandons_and_reopen_returns_to_review() {
    let mut sim = merge_with_pending_approval();
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "closed"));
    let out = sim.tick();
    assert_eq!(types(&out), vec!["delivery.abandoned"]);
    assert_eq!(appended(&out)[0].data["reason"], json!("pr_closed"));
    sim.apply(out);
    assert_eq!(sim.state.delivery.stage, Stage::Abandoned);
    assert_eq!(
        sim.state.delivery.abandon_cause,
        Some(AbandonCause::PrClosedUnmerged)
    );
    // 收尾：作废审批（PrClosed），不需要 update_pr（已关）。
    let out = sim.settle();
    assert!(out.contains(&Decision::Effect(Effect::InvalidateApproval {
        approval_id: APPROVAL.parse().unwrap(),
        reason: InvalidateReason::PrClosed,
    })));
    assert!(judge(&out).is_empty());
    // PR 重开 → T15。
    sim.state.pr_snapshot = Some(snapshot(COMMIT, false, "open"));
    let out = sim.tick();
    assert_eq!(types(&out), vec!["delivery.stage_changed"]);
    assert_eq!(
        appended(&out)[0].data,
        json!({"from": "abandoned", "to": "review", "reason": "pr_reopened", "stage_status": "running"})
    );
    sim.apply(out);
    assert_eq!(sim.state.delivery.stage, Stage::Review);
}

#[test]
fn user_abandoned_is_not_reopened() {
    let mut sim = review_stage();
    sim.apply(command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(None),
        },
    ));
    sim.state.pr_snapshot = Some(snapshot(COMMIT, false, "closed"));
    let _ = sim.settle();
    sim.state.pr_snapshot = Some(snapshot(COMMIT, false, "open"));
    let out = sim.tick();
    assert!(!types(&out).contains(&"delivery.stage_changed"));
    assert_eq!(sim.state.delivery.stage, Stage::Abandoned);
}

/// T14 反例：PR 已合并（不是关闭）→ 不放弃；没有 PR 绑定 → 不放弃。
#[test]
fn pr_closed_requires_unmerged_closed_snapshot() {
    let mut sim = merge_stage();
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "merged"));
    assert!(!types(&sim.tick()).contains(&"delivery.abandoned"));
    let mut sim = Sim::low();
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "closed"));
    assert!(!types(&sim.tick()).contains(&"delivery.abandoned"));
}

// ---------------------------------------------------------------------------
// §12.2"新推送使审批失效"与"base 前移"：T11 与 T10 的 base_advanced 附加条件
// ---------------------------------------------------------------------------

#[test]
fn approval_invalidated_returns_merge_to_review() {
    let mut sim = merge_with_pending_approval();
    let old_key = judge(&sim.tick()).first().map(|(k, _)| k.clone());
    sim.external(
        "approval.invalidated",
        APPROVAL,
        json!({"reason": "head_changed", "previous_head_sha": COMMIT, "current_head_sha": NEW_HEAD}),
    );
    let approval: ApprovalId = APPROVAL.parse().unwrap();
    let observed = observed_last(
        &sim,
        ObservedData::Approval {
            approval_id: approval,
            status: ApprovalStatus::Invalidated,
        },
    );
    let out = on_event(&sim, observed);
    assert_eq!(types(&out), vec!["delivery.stage_changed"]);
    assert_eq!(
        appended(&out)[0].data,
        json!({"from": "merge", "to": "review", "reason": "head_changed", "stage_status": "running"})
    );
    // 原 approval_needed 由引擎关闭。
    assert!(out.contains(&Decision::Resolve {
        target: ResolveTarget::Approval(approval),
        how: Resolution::ApprovalSettled {
            approval_id: approval,
            status: ApprovalStatus::Invalidated
        },
    }));
    sim.apply(out);
    assert_eq!(sim.state.delivery.stage, Stage::Review);
    // 新 head 的 CI 未过：留在 review；通过后回到 merge，按新 head 判定（键不同）。
    sim.state.pr_snapshot = Some(snapshot(NEW_HEAD, false, "open"));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Review);
    sim.state.pr_snapshot = Some(snapshot(NEW_HEAD, true, "open"));
    let effects = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Merge);
    let new_key = judge(&effects).first().map(|(k, _)| k.clone());
    assert!(new_key.is_some());
    assert_ne!(new_key, old_key);
}

#[test]
fn approval_lapse_preconditions() {
    // 反例 1：另有 pending 的合并审批 → 不迁移。
    let mut sim = merge_with_pending_approval();
    const SECOND: &str = "apr_01JB7P4M8T2X6N9R3Q5K1V7WZM";
    request_approval(&mut sim, SECOND, COMMIT);
    sim.external("approval.expired", APPROVAL, json!({}));
    let observed = observed_last(
        &sim,
        ObservedData::Approval {
            approval_id: APPROVAL.parse().unwrap(),
            status: ApprovalStatus::Expired,
        },
    );
    assert!(!types(&on_event(&sim, observed)).contains(&"delivery.stage_changed"));

    // 正例：expired 且没有其他活跃审批 → 迁移。
    let mut sim = merge_with_pending_approval();
    sim.external("approval.expired", APPROVAL, json!({}));
    let observed = observed_last(
        &sim,
        ObservedData::Approval {
            approval_id: APPROVAL.parse().unwrap(),
            status: ApprovalStatus::Expired,
        },
    );
    let stale = Observed {
        seq: sim.state.delivery.stage_entered.seq,
        id: observed.id,
        actor: observed.actor.clone(),
        data: ObservedData::Approval {
            approval_id: APPROVAL.parse().unwrap(),
            status: ApprovalStatus::Expired,
        },
    };
    assert_eq!(
        types(&on_event(&sim, observed)),
        vec!["delivery.stage_changed"]
    );
    // 反例 2：事件不晚于进入本阶段（重放）→ 不迁移。
    assert!(!types(&on_event(&sim, stale)).contains(&"delivery.stage_changed"));

    // 反例 3：不在 merge 阶段。
    let mut sim = review_stage();
    request_approval(&mut sim, APPROVAL, COMMIT);
    sim.external("approval.expired", APPROVAL, json!({}));
    let observed = observed_last(
        &sim,
        ObservedData::Approval {
            approval_id: APPROVAL.parse().unwrap(),
            status: ApprovalStatus::Expired,
        },
    );
    assert!(!types(&on_event(&sim, observed)).contains(&"delivery.stage_changed"));
}

#[test]
fn pr_synchronized_returns_merge_to_review() {
    let sim = merge_stage();
    let data = ObservedData::Forge(ForgeEventData::PrSynchronized(
        serde_json::from_value(json!({
            "head_sha": NEW_HEAD, "previous_head_sha": COMMIT, "head_epoch": 2, "base_ref": "main"
        }))
        .unwrap(),
    ));
    let out = on_event(&sim, observed_next(&sim, data));
    assert_eq!(types(&out), vec!["delivery.stage_changed"]);
    assert_eq!(appended(&out)[0].data["reason"], json!("head_changed"));
    // 反例：review 阶段收到 pr_synchronized 不迁移。
    let sim = review_stage();
    let data = ObservedData::Forge(ForgeEventData::PrSynchronized(
        serde_json::from_value(json!({
            "head_sha": NEW_HEAD, "previous_head_sha": COMMIT, "head_epoch": 2, "base_ref": "main"
        }))
        .unwrap(),
    ));
    assert!(!types(&on_event(&sim, observed_next(&sim, data))).contains(&"delivery.stage_changed"));
}

fn base_advanced(prs: Vec<u64>) -> ObservedData {
    ObservedData::Forge(ForgeEventData::BaseAdvanced(BaseAdvancedData {
        base_ref: "main".to_owned(),
        before: oid(BASE_SHA),
        after: oid(NEW_HEAD),
        affected_prs: prs,
    }))
}

#[test]
fn base_advanced_returns_merge_to_review_until_reconciled() {
    let mut sim = merge_stage();
    let target = ReconcileTarget::Pr {
        project: PROJECT.parse().unwrap(),
        number: 12,
    };
    let out = on_event(&sim, observed_next(&sim, base_advanced(vec![12])));
    assert_eq!(types(&out), vec!["delivery.stage_changed"]);
    assert_eq!(
        appended(&out)[0].data,
        json!({"from": "merge", "to": "review", "reason": "base_advanced", "stage_status": "running"})
    );
    assert!(out.contains(&Decision::Effect(Effect::ReconcileNow { target })));
    sim.apply(out);

    // T10 另需：快照 synced_at 不晚于进入 review 的时间 → 只产出 ReconcileNow，不回 merge。
    let entered = sim.state.delivery.stage_entered.ts;
    sim.state.pr_snapshot = Some(snapshot_at(
        COMMIT,
        true,
        "open",
        "2023-01-01T00:00:00.000Z",
    ));
    assert!(entered > sim.state.pr_snapshot.as_ref().unwrap().synced_at);
    let out = sim.settle();
    assert_eq!(out, vec![Decision::Effect(Effect::ReconcileNow { target })]);
    assert_eq!(sim.state.delivery.stage, Stage::Review);
    // 在途时不重复对账。
    sim.fx
        .inflight
        .insert(EffectKey::Reconcile(gantry_engine::decide::ReconcileKey(
            target,
        )));
    assert!(sim.tick().is_empty());
    sim.fx.inflight.clear();

    // 对账已回（synced_at 晚于进入时间）且条件仍满足 → 回到 merge。
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "open"));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Merge);
}

#[test]
fn base_advanced_only_affects_own_pr_in_merge() {
    // 反例 1：affected_prs 不含本 PR → 不迁移。
    let sim = merge_stage();
    let out = on_event(&sim, observed_next(&sim, base_advanced(vec![99])));
    assert!(!types(&out).contains(&"delivery.stage_changed"));
    assert!(!effects(&out)
        .iter()
        .any(|e| matches!(e, Effect::ReconcileNow { .. })));
    // 反例 2：merge 之外的阶段不受影响。
    let sim = review_stage();
    let out = on_event(&sim, observed_next(&sim, base_advanced(vec![12])));
    assert!(!types(&out).contains(&"delivery.stage_changed"));
    // 反例 3：进入 review 的原因不是 base_advanced 时，T10 不要求对账。
    let mut sim = review_stage();
    sim.state.pr_snapshot = Some(snapshot_at(
        COMMIT,
        true,
        "open",
        "2023-01-01T00:00:00.000Z",
    ));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Merge);
}

// ---------------------------------------------------------------------------
// §12.2"上报幂等"：choose 产生的 inbox.resolved(option_id) 驱动选项（§7.11"关闭与执行"）
// ---------------------------------------------------------------------------

#[test]
fn upgrade_option_sets_level_cap_to_target() {
    let mut sim = Sim::low();
    let item = level_cap_exceeded(&mut sim, Some(Level::High));
    let observed = choose(&mut sim, &item, "upgrade", Some(item.clone()));
    let out = on_event(&sim, observed);
    assert_eq!(types(&out), vec!["delivery.level_changed"]);
    let event = appended(&out)[0];
    assert_eq!(
        event.data,
        json!({"from": "Medium", "to": "High", "kind": "cap", "reason": "user"})
    );
    assert_eq!(event.causation_id, Some(evt(sim.state.last_seq.as_u64())));
    assert!(event.dedup_key.is_some());
    sim.apply(out);
    assert_eq!(sim.state.delivery.level_cap, Level::High);
}

#[test]
fn upgrade_option_preconditions() {
    // 反例 1：item 缺失（无法校验选项）→ 不动作。
    let mut sim = Sim::low();
    let item = level_cap_exceeded(&mut sim, Some(Level::High));
    let observed = choose(&mut sim, &item, "upgrade", None);
    assert!(!types(&on_event(&sim, observed)).contains(&"delivery.level_changed"));
    // 反例 2：item 没有 target_level → 记错误、不动作。
    let mut sim = Sim::low();
    let item = level_cap_exceeded(&mut sim, None);
    let observed = choose(&mut sim, &item, "upgrade", Some(item.clone()));
    assert!(!types(&on_event(&sim, observed)).contains(&"delivery.level_changed"));
    // 反例 3：option_id 不在该项的选项中 → 忽略。
    let mut sim = Sim::low();
    let item = level_cap_exceeded(&mut sim, Some(Level::High));
    let observed = choose(&mut sim, &item, "raise_cap", Some(item.clone()));
    assert!(on_event(&sim, observed)
        .iter()
        .all(|d| !matches!(d, Decision::Append(e) if e.ty == EventType::BudgetLimitRaised)));
    // 反例 4：target_level 等于当前上限 → 不写事件。
    let mut sim = Sim::low();
    let item = level_cap_exceeded(&mut sim, Some(Level::Medium));
    let observed = choose(&mut sim, &item, "upgrade", Some(item.clone()));
    assert!(!types(&on_event(&sim, observed)).contains(&"delivery.level_changed"));
}

#[test]
fn narrow_option_raises_effective_to_cap() {
    let mut sim = Sim::low();
    let item = level_cap_exceeded(&mut sim, Some(Level::High));
    let observed = choose(&mut sim, &item, "narrow", Some(item.clone()));
    let out = on_event(&sim, observed);
    assert_eq!(types(&out), vec!["delivery.level_changed"]);
    assert_eq!(
        appended(&out)[0].data,
        json!({"from": "Low", "to": "Medium", "kind": "effective", "reason": "user"})
    );
    sim.apply(out);
    assert_eq!(sim.state.delivery.level_effective, Level::Medium);
    assert_eq!(sim.state.delivery.level_cap, Level::Medium);
    // 反例：effective 已等于上限 → 不写事件。
    let item = level_cap_exceeded(&mut sim, Some(Level::Max));
    let observed = choose(&mut sim, &item, "narrow", Some(item.clone()));
    assert!(!types(&on_event(&sim, observed)).contains(&"delivery.level_changed"));
}

#[test]
fn abandon_option_abandons() {
    let mut sim = dispatched();
    let item = level_cap_exceeded(&mut sim, Some(Level::High));
    let observed = choose(&mut sim, &item, "abandon", Some(item.clone()));
    let out = on_event(&sim, observed);
    assert_eq!(types(&out), vec!["delivery.abandoned"]);
    assert_eq!(
        appended(&out)[0].data,
        json!({"by": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"})
    );
    sim.apply(out);
    assert_eq!(
        sim.state.delivery.abandon_cause,
        Some(AbandonCause::UserAbandoned)
    );
    // 反例：已终结时重放 → 不再写。
    let item = decision_needed(
        &mut sim,
        DecisionKind::ManualAction,
        "policy.path_protected:workflows",
        &["done", "abandon"],
    );
    let observed = choose(&mut sim, &item, "abandon", Some(item.clone()));
    assert!(!types(&on_event(&sim, observed)).contains(&"delivery.abandoned"));
}

/// done / continue / keep：不写选项事件，解除阻塞后按状态重新求值；dismiss 不解除阻塞由
/// `state_fold.rs::dismiss_does_not_unblock` 锁定。
#[test]
fn done_continue_keep_only_reevaluate() {
    let mut sim = Sim::low();
    let _ = sim.settle();
    let item = decision_needed(
        &mut sim,
        DecisionKind::ManualAction,
        "policy.path_protected:workflows",
        &["done", "abandon"],
    );
    let branch = gantry_engine::decider::branch_name(&sim.state, "gantry/");
    sim.external(
        "worktree.created",
        WORKTREE,
        json!({"worktree_id": WORKTREE, "project_id": PROJECT, "delivery_id": DELIVERY,
               "branch": branch, "base_ref": "main", "base_sha": BASE_SHA}),
    );
    assert_eq!(sim.state.stage_status(), StageStatus::WaitingHuman);
    // 阻塞中：不取锁、不派发。
    assert!(sim.tick().is_empty());
    let observed = choose(&mut sim, &item, "done", Some(item.clone()));
    // 解除阻塞后的求值：照常取锁（即"重新求值当前阶段"），不写选项事件。
    let out = on_event(&sim, observed);
    assert!(types(&out).is_empty());
    assert!(matches!(
        out.as_slice(),
        [Decision::Effect(Effect::AcquireLock { .. })]
    ));
    for (kind, option) in [
        (DecisionKind::Other, "continue"),
        (DecisionKind::ReviewDispute, "keep"),
    ] {
        let mut sim = Sim::low();
        let reason = if option == "continue" {
            "github_state_changed"
        } else {
            "guardian.human_feedback"
        };
        let opts: &[&str] = if option == "continue" {
            &["continue", "abandon"]
        } else {
            &["fix", "keep"]
        };
        let item = decision_needed(&mut sim, kind, reason, opts);
        let observed = choose(&mut sim, &item, option, Some(item.clone()));
        let out = on_event(&sim, observed);
        assert!(!types(&out)
            .iter()
            .any(|t| *t == "delivery.level_changed" || *t == "delivery.abandoned"));
    }
}

#[test]
fn replan_option_rebuilds_low_implement() {
    let mut sim = dispatched();
    let node = node_id(&sim);
    let old = *sim.state.workflows.keys().next().unwrap();
    let attempt = sim.state.node(&node).unwrap().attempts[0].id;
    let item = decision_needed(
        &mut sim,
        DecisionKind::GateReject,
        "gate_reject",
        &["replan", "abandon"],
    );
    let observed = choose(&mut sim, &item, "replan", Some(item.clone()));
    let replay = Observed {
        seq: observed.seq,
        id: observed.id,
        actor: observed.actor.clone(),
        data: ObservedData::InboxResolved {
            inbox_id: item.id,
            resolution: InboxResolution::Chosen,
            option_id: Some(OptionId::new("replan")),
            item: Some(item.clone()),
        },
    };
    let out = on_event(&sim, observed);
    assert_eq!(
        types(&out),
        vec![
            "workflow.finished",
            "node.finished",
            "workflow.created",
            "budget.charged"
        ]
    );
    assert_eq!(appended(&out)[0].data, json!({"status": "cancelled"}));
    assert_eq!(appended(&out)[1].data["status"], json!("cancelled"));
    // 同一决策内的事件都带由 inbox.resolved 派生、互不相同的 dedup_key。
    let keys: BTreeSet<_> = appended(&out).iter().map(|e| e.dedup_key.clone()).collect();
    assert_eq!(keys.len(), 4);
    assert!(effects(&out).contains(&&Effect::CancelAttempt {
        attempt_id: attempt,
        why: gantry_core::ports::engine::CancelWhy::Superseded
    }));
    sim.apply(out);
    // 阶段不变；当前图换成新的 low.implement。
    assert_eq!(sim.state.delivery.stage, Stage::Implement);
    assert_eq!(sim.state.workflows.len(), 2);
    assert_eq!(
        sim.state.workflows[&old].status,
        gantry_core::entities::WorkflowStatus::Cancelled
    );
    // 反例：重放同一 inbox.resolved → 不再重建。
    assert!(!types(&on_event(&sim, replay)).contains(&"workflow.created"));
}

#[test]
fn replan_requires_low_implement() {
    // 反例：propose 阶段收到 replan → 不动作。
    let mut sim = dispatched();
    finish(&mut sim, vec![]);
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Propose);
    let item = decision_needed(
        &mut sim,
        DecisionKind::GateReject,
        "gate_reject",
        &["replan", "abandon"],
    );
    let observed = choose(&mut sim, &item, "replan", Some(item.clone()));
    let out = on_event(&sim, observed);
    assert!(!types(&out).contains(&"workflow.created"));
    assert!(!types(&out).contains(&"workflow.finished"));
}

#[test]
fn raise_cap_option_extends_exhausted_dimension() {
    let mut sim = dispatched();
    fail_running(
        &mut sim,
        FailureClass::BudgetExhausted,
        "budget.tokens",
        PatchResult::None,
    );
    let item = sim
        .state
        .open_inbox
        .values()
        .find(|i| i.kind == InboxKind::BudgetExhausted)
        .unwrap()
        .clone();
    let observed = choose(&mut sim, &item, "raise_cap", Some(item.clone()));
    let replay_id = observed.id;
    let replay_seq = observed.seq;
    let out = on_event(&sim, observed);
    assert_eq!(types(&out), vec!["budget.limit_raised"]);
    assert_eq!(
        appended(&out)[0].data,
        json!({"dimension": "tokens_by_tier.worker", "from": 1000000, "to": 2000000, "by": 1000000})
    );
    sim.apply(out);
    let dimension = BudgetDimension::TokensByTier(Tier::Worker);
    assert_eq!(
        gantry_engine::state::limit_of(&sim.state.delivery.budget.limits, dimension),
        2_000_000
    );
    // 反例：重放同一 inbox.resolved → 不再追加。
    let replay = Observed {
        seq: replay_seq,
        id: replay_id,
        actor: Actor::new(ActorKind::Device, "dev"),
        data: ObservedData::InboxResolved {
            inbox_id: item.id,
            resolution: InboxResolution::Chosen,
            option_id: Some(OptionId::new("raise_cap")),
            item: Some(item.clone()),
        },
    };
    assert!(!types(&on_event(&sim, replay)).contains(&"budget.limit_raised"));
}

// ---------------------------------------------------------------------------
// models_not_configured（§9；§7.14.5 Raise(*) 与 Resolve(models_not_configured) 行）
// ---------------------------------------------------------------------------

#[test]
fn models_not_configured_is_raised_then_resolved_after_settings_update() {
    let mut sim = Sim::low();
    let models = std::mem::take(&mut sim.fx.models);
    let _ = sim.settle();
    let branch = gantry_engine::decider::branch_name(&sim.state, "gantry/");
    sim.external(
        "worktree.created",
        WORKTREE,
        json!({"worktree_id": WORKTREE, "project_id": PROJECT, "delivery_id": DELIVERY,
               "branch": branch, "base_ref": "main", "base_sha": BASE_SHA}),
    );
    let out = sim.settle();
    // 不取锁、不派发；上报 escalation(models_not_configured)，阻塞派发。
    assert!(!out.iter().any(|d| matches!(
        d,
        Decision::Effect(Effect::AcquireLock { .. } | Effect::RunAttempt(_))
    )));
    let node = node_id(&sim);
    let dedup = gantry_engine::control::models_not_configured_dedup(&sim.state, node);
    let item = sim
        .state
        .open_inbox
        .values()
        .find(|i| i.dedup_key.as_str() == dedup)
        .expect("models_not_configured raised")
        .clone();
    assert_eq!(
        item.reason_code.as_ref().unwrap().as_str(),
        "models_not_configured"
    );
    assert_eq!(item.node_id, Some(node));
    assert_eq!(sim.state.stage_status(), StageStatus::WaitingHuman);
    // 反例：已有同键项 → 不重复上报；设置更新但模型仍缺 → 不关闭。
    assert!(sim.settle().is_empty());
    let settings = ObservedData::SettingsUpdated {
        rev: Seq::new(sim.seq + 1).unwrap(),
    };
    assert!(!on_event(&sim, observed_next(&sim, settings))
        .iter()
        .any(|d| matches!(d, Decision::Resolve { .. })));
    // 正例：设置切换后表项存在 → Resolve 该项。
    sim.fx.models = models;
    let settings = ObservedData::SettingsUpdated {
        rev: Seq::new(sim.seq + 1).unwrap(),
    };
    let out = on_event(&sim, observed_next(&sim, settings));
    assert!(out.contains(&Decision::Resolve {
        target: ResolveTarget::DedupKey(DedupKey::new(dedup)),
        how: Resolution::System {
            reason: "models_configured".to_owned()
        },
    }));
}

// ---------------------------------------------------------------------------
// 决策纯度：本切片的命令与事件输入也满足同一 (state, input, env) → 同一结果
// ---------------------------------------------------------------------------

#[test]
fn control_decisions_are_pure() {
    let mut sim = dispatched();
    let a = command(
        &sim,
        Command::Abandon {
            reason: None,
            ctx: ctx(Some("k")),
        },
    );
    assert_eq!(
        a,
        command(
            &sim,
            Command::Abandon {
                reason: None,
                ctx: ctx(Some("k"))
            }
        )
    );
    let item = decision_needed(
        &mut sim,
        DecisionKind::GateReject,
        "gate_reject",
        &["replan", "abandon"],
    );
    let observed = choose(&mut sim, &item, "replan", Some(item.clone()));
    let again = Observed {
        seq: observed.seq,
        id: observed.id,
        actor: observed.actor.clone(),
        data: ObservedData::InboxResolved {
            inbox_id: item.id,
            resolution: InboxResolution::Chosen,
            option_id: Some(OptionId::new("replan")),
            item: Some(item.clone()),
        },
    };
    assert_eq!(on_event(&sim, observed), on_event(&sim, again));
}
