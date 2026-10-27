//! `decide` 的 Low 正常路径（engine.md §7.14.5、§12.2，#70）。
//!
//! 纯函数层面：直接构造 `DeliveryState` 与输入序列，断言产出的决策。小模拟器把 `Append` 当作提交
//! （先过 `NewEvent::validate`，即事件形状校验），把副作用按脚本回流成 `Report` 或外部事件。

mod common;

use std::collections::BTreeSet;

use common::{ts, BASE_SHA, DELIVERY, PROJECT, T0};
use gantry_core::canon::{GitOid, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, AttemptStatus, BudgetLedger, ForgeActionKind, InboxKind,
    MergeMethod, NodeStatus, PolicyActionId, PullRequestRef, Seq, Severity, Stage, StageStatus,
    Tier, TokenUsage,
};
use gantry_core::events::{Event, EventType, NewEvent, Subject};
use gantry_core::forge::ActionPayload;
use gantry_core::ids::{ApprovalId, EventId};
use gantry_core::ports::policy::{
    ActionDecision, ApprovalSpec, DenyReason, ProposalDecision, ReportHint,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::{OpenPrParams, Proposal, ProposalKind, ProposalParams};
use gantry_engine::decide::{
    DecideEnv, DecideSeed, Decision, Effect, EffectKey, EffectReport, EngineConsts, EngineSettings,
    Finalized, Input, InputIdentity, ModelKey, ModelTable, OutputSignals, PatchResult,
    ProjectFacts, ProposalMissing, ProposalOutcome, ProposalRef, ProposalText, ProposalTexts,
    ReservationPolicy, ResolvedModel, SandboxLimits, TemplateRegistry, TickReason,
};
use gantry_engine::state::{ActionKey, DeliveryState};
use gantry_engine::{decide, expected_actions};
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
    assert!(rest.is_empty());
}

fn open_pr(id: &str, title: &str) -> ProposalOutcome {
    ProposalOutcome {
        proposal_id: id.to_owned(),
        decision: ProposalDecision::Accepted {
            proposal_id: id.to_owned(),
            action_id: None,
        },
        open_pr: Some(Proposal {
            id: id.to_owned(),
            kind: ProposalKind::OpenPr,
            target: None,
            params: ProposalParams::OpenPr(OpenPrParams {
                title: title.to_owned(),
                body: "body".to_owned(),
                draft: false,
            }),
            rationale: "r".to_owned(),
        }),
    }
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

#[test]
fn low_path_end_to_end_with_merge_approval() {
    let mut sim = dispatched();
    // T2 的 reason。
    assert_eq!(
        sim.state.delivery.stage_entered.reason.as_deref(),
        Some("admission_skipped")
    );
    finish(&mut sim, vec![]);
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
    assert_eq!(node.status, NodeStatus::Succeeded);
    assert_eq!(sim.state.evidence.candidate_head, Some(oid(COMMIT)));
    assert_eq!(sim.state.ledger.used.wall_time, 120);

    // T6 → propose；入队 push_branch，再入队 create_pr（depends_on = push 的动作 ID）。
    let (push, create_req) = proposed(&mut sim);
    let push_id = sim.state.outbox[&push].action_id;
    assert_eq!(create_req.depends_on, vec![push_id]);
    let ActionPayload::CreatePr(pr) = &create_req.payload else {
        panic!()
    };
    assert_eq!(pr.title, "fix login timeout");
    assert_eq!(pr.base, "main");
    assert_eq!(pr.expected_head_oid, oid(COMMIT));
    sim.enqueue(&create_req, "act_01JBA1D7P2KS6W8Y0A4C1F5GJP", Some(12));

    // T8：快照绑定且 head = 已推送提交。
    sim.state.pr_snapshot = Some(snapshot(COMMIT, false, "open"));
    assert!(judge(&sim.settle()).is_empty());
    assert_eq!(sim.state.delivery.stage, Stage::Review);

    // CI 未过：留在 review。
    assert!(sim.settle().is_empty());
    assert_eq!(sim.state.delivery.stage, Stage::Review);

    // CI 通过 → T10 → merge；判定 merge_pr。
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "open"));
    let effects = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Merge);
    let merge = judge(&effects);
    assert_eq!(merge.len(), 1);
    let merge_key = merge[0].0.clone();

    // NeedsApproval → RequestApproval；审批授予前没有 merge_pr 入队。
    let spec = ApprovalSpec {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        target: serde_json::from_value(json!({"kind": "pr", "repo": "acme/web", "number": 12}))
            .unwrap(),
        subject_digest: SubjectDigest::git(oid(COMMIT)),
        delivery_id: Some(DELIVERY.parse().unwrap()),
        ttl: std::time::Duration::from_secs(3600),
    };
    let needs = ActionDecision::NeedsApproval {
        spec: spec.clone(),
        pending: None,
        config_rev: Seq::new(1).unwrap(),
    };
    let rest = sim.report(
        EffectReport::ActionJudged {
            key: merge_key.clone(),
            decision: needs,
        },
        EffectKey::Judge(merge_key.clone()),
    );
    assert_eq!(
        rest,
        vec![Decision::Effect(Effect::RequestApproval {
            key: merge_key.clone(),
            spec
        })]
    );

    // policy 写 approval.requested（归属在信封 correlation_id）→ 上报 approval_needed，且不再判定。
    sim.external(
        "approval.requested",
        APPROVAL,
        json!({"action": "merge_pr", "target": {"kind": "pr", "repo": "acme/web", "number": 12},
               "subject_digest": COMMIT, "expires_at": "2026-10-24T12:45:00.000Z", "config_rev": 1,
               "requested_by": {"kind": "engine", "id": "engine"}}),
    );
    assert!(judge(&sim.settle()).is_empty());
    let raised: Vec<_> = sim.state.open_inbox.values().collect();
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0].kind, InboxKind::ApprovalNeeded);
    assert!(!sim
        .state
        .outbox
        .values()
        .any(|r| r.kind == ForgeActionKind::MergePr));

    // 授予 → 再次判定 → Allow(approval) → 入队 merge_pr，expected_head_sha = subject_digest。
    sim.external(
        "approval.granted",
        APPROVAL,
        json!({"approval_id": APPROVAL, "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
               "approval_pubkey_fpr": PATCH_SHA, "payload_sha256": RESULT_REF}),
    );
    let effects = sim.settle();
    assert_eq!(judge(&effects).len(), 1);
    let approval: ApprovalId = APPROVAL.parse().unwrap();
    let merge_req = enqueue_of(sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: merge_key.clone(),
            decision: allow(Some(approval)),
        }),
        InputIdentity::Report(EffectKey::Judge(merge_key)),
    ));
    assert_eq!(merge_req.requires_approval, Some(approval));
    let ActionPayload::MergePr(m) = &merge_req.payload else {
        panic!()
    };
    assert_eq!(
        SubjectDigest::git(m.expected_head_sha),
        SubjectDigest::git(oid(COMMIT))
    );
    assert_eq!(m.merge_method, MergeMethod::Squash);
    sim.enqueue(&merge_req, "act_01JBA1D7P2KS6W8Y0A4C1F5GJQ", None);

    // forge.pr_merged 后快照为 merged → T12 → closed，释放工作树。
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "merged"));
    let effects = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Closed);
    assert_eq!(sim.state.delivery.stage_status, StageStatus::Done);
    assert!(matches!(
        effects.as_slice(),
        [Decision::Effect(Effect::RemoveWorktree { .. })]
    ));

    assert_eq!(
        sim.stages,
        vec![
            Stage::Spec,
            Stage::Implement,
            Stage::Propose,
            Stage::Review,
            Stage::Merge,
            Stage::Closed
        ]
    );
}

// ---------------------------------------------------------------------------
// 决策纯度
// ---------------------------------------------------------------------------

#[test]
fn decide_is_pure_for_same_state_input_env() {
    let mut sim = Sim::low();
    // 覆盖带 mint 的决策（建图）与带判定的决策（propose）。
    let first = sim.tick();
    assert_eq!(first, sim.tick());
    sim.apply(first);
    let build = sim.tick();
    assert!(build
        .iter()
        .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::WorkflowCreated)));
    assert_eq!(build, sim.tick());

    let mut sim = dispatched();
    finish(&mut sim, vec![]);
    let _ = sim.settle();
    assert_eq!(sim.tick(), sim.tick());
    // 同一状态上反复求值不产生重复事件：收敛后再 Tick 不再有事务内决策。
    assert!(!sim.tick().iter().any(|d| matches!(d, Decision::Append(_))));
}

// ---------------------------------------------------------------------------
// §7.14.5 前置条件：每行一条正例（端到端已覆盖的在此重申）与一条反例
// ---------------------------------------------------------------------------

#[test]
fn t2_only_for_low_and_only_once() {
    let mut sim = Sim::low();
    // 正例。
    let d = sim.tick();
    assert!(
        matches!(d.as_slice(), [Decision::Append(e)] if e.ty == EventType::DeliveryStageChanged)
    );
    sim.apply(d);
    // 反例：已在 implement，不再产出 T2。
    assert!(!sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::DeliveryStageChanged)));
    // 反例：effective = Medium 不走 T2（M1 无 Medium 模板，不产出任何阶段事件）。
    let mut medium = Sim::low();
    medium.state.delivery.level_effective = gantry_core::level::Level::Medium;
    assert!(medium.tick().is_empty());
}

#[test]
fn workflow_created_once_per_stage_and_within_node_budget() {
    let mut sim = Sim::low();
    let d = sim.tick();
    sim.apply(d);
    let d = sim.tick();
    let created = d
        .iter()
        .filter(|d| matches!(d, Decision::Append(e) if e.ty == EventType::WorkflowCreated))
        .count();
    let charged = d
        .iter()
        .filter(|d| matches!(d, Decision::Append(e) if e.ty == EventType::BudgetCharged))
        .count();
    assert_eq!((created, charged), (1, 1));
    sim.apply(d);
    assert_eq!(sim.state.ledger.used.nodes, 1);
    // 反例：本阶段已建过。
    assert!(!sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::WorkflowCreated)));

    // 反例：节点预算不足 → 不建（不出现"半张图"）。
    let mut poor = Sim::low();
    let d = poor.tick();
    poor.apply(d);
    poor.state.ledger.used.nodes = poor.state.delivery.budget.limits.nodes;
    assert!(poor.tick().is_empty());
}

#[test]
fn node_queued_requires_pending_and_dispatch_gate() {
    let mut sim = Sim::low();
    let d = sim.tick();
    sim.apply(d);
    let built = sim.tick();
    // 建图与入队不在同一轮：节点在下一轮 Settle 才 queued。
    sim.apply(built);
    let d = sim.tick();
    assert!(d
        .iter()
        .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::NodeQueued)));

    // 反例：暂停 → 不派发。
    sim.state.delivery.stage_status = StageStatus::Paused;
    assert!(!sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::NodeQueued)));
    sim.state.delivery.stage_status = StageStatus::Running;
    sim.apply(d);
    // 反例：已 queued，不重复。
    assert!(!sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::NodeQueued)));
}

#[test]
fn attempt_finished_only_once_and_workflow_finishes_after_required_nodes() {
    let mut sim = dispatched();
    // 反例：required 节点仍在跑 → 不 finish。
    assert!(!sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::WorkflowFinished)));
    let f = finalized(&sim, vec![]);
    let again = finalized(&sim, vec![]);
    finish(&mut sim, vec![]);
    // 反例：该尝试已有 attempt.finished → 重放回报不产出。
    assert!(sim
        .decide(
            Input::Report(EffectReport::AttemptFinalized(again)),
            InputIdentity::Report(EffectKey::Run(f.attempt_id))
        )
        .is_empty());
    // 正例：节点成功后 Workflow finish（与 T6 在相邻两轮 Settle 里产出）。
    let _ = sim.settle();
    let wf = sim.state.workflows.values().next().unwrap();
    assert_eq!(wf.status, gantry_core::entities::WorkflowStatus::Succeeded);
}

#[test]
fn t6_requires_committed_patch() {
    let mut sim = dispatched();
    let mut f = finalized(&sim, vec![]);
    f.patch = PatchResult::NoChange;
    let attempt = f.attempt_id;
    sim.report(EffectReport::AttemptFinalized(f), EffectKey::Run(attempt));
    // 反例：节点成功但没有本地提交 → 留在 implement。
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Implement);
}

#[test]
fn judge_action_preconditions() {
    let mut sim = dispatched();
    finish(&mut sim, vec![]);
    let effects = sim.settle();
    let judged = judge(&effects);
    assert_eq!(judged.len(), 2);
    let push = judged
        .iter()
        .find(|(_, k)| *k == ForgeActionKind::PushBranch)
        .unwrap()
        .0
        .clone();

    // 反例：在途。
    sim.fx.inflight.insert(EffectKey::Judge(push.clone()));
    assert!(!judge(&sim.tick()).iter().any(|(k, _)| *k == push));
    sim.fx.inflight.clear();
    // 反例：本进程已判 Deny。
    sim.fx.denied.insert(push.clone());
    assert!(!judge(&sim.tick()).iter().any(|(k, _)| *k == push));
    sim.fx.denied.clear();
    // 反例：outbox 已有该键。
    let enqueue = enqueue_of(sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: push.clone(),
            decision: allow(None),
        }),
        InputIdentity::Report(EffectKey::Judge(push.clone())),
    ));
    sim.enqueue(&enqueue, "act_01JBA1D7P2KS6W8Y0A4C1F5GJN", None);
    assert!(!judge(&sim.tick()).iter().any(|(k, _)| *k == push));
}

#[test]
fn enqueue_preconditions() {
    let mut sim = dispatched();
    finish(&mut sim, vec![]);
    let effects = sim.settle();
    let push = judge(&effects)
        .into_iter()
        .find(|(_, k)| *k == ForgeActionKind::PushBranch)
        .unwrap()
        .0;

    // 反例：意图已不在期望集合（例如阶段已离开 propose）。
    let mut moved = DeliveryState::clone(&sim.state);
    moved.delivery.stage = Stage::Review;
    let env = sim.fx.env(
        &moved,
        &InputIdentity::Report(EffectKey::Judge(push.clone())),
    );
    let out = decide(
        &moved,
        &Input::Report(EffectReport::ActionJudged {
            key: push.clone(),
            decision: allow(None),
        }),
        &env,
    );
    assert!(out.is_empty());

    // 反例：Deny 不入队，按 ReportHint 上报 escalation。
    let hint = ReportHint {
        inbox_kind: InboxKind::Escalation,
        payload_kind: None,
        reason_code: gantry_core::entities::ReasonCode::parse("policy.action_denied").unwrap(),
        severity: Severity::Blocker,
        attach_patch: false,
    };
    let deny = ActionDecision::Deny {
        action_id: PolicyActionId::PushBranch,
        reason: DenyReason::ActionDenied,
        report: Some(hint),
        config_rev: Seq::new(1).unwrap(),
    };
    let out = sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: push.clone(),
            decision: deny,
        }),
        InputIdentity::Report(EffectKey::Judge(push.clone())),
    );
    assert!(matches!(out.as_slice(), [Decision::Raise(r)] if r.kind == InboxKind::Escalation));

    // 反例：不在期望集合中的键（伪造）不入队。
    let bogus: ActionKey = serde_json::from_value(json!("gk1_anbfwcip7r36is6wsqcuxxfd34")).unwrap();
    assert!(sim
        .decide(
            Input::Report(EffectReport::ActionJudged {
                key: bogus.clone(),
                decision: allow(None)
            }),
            InputIdentity::Report(EffectKey::Judge(bogus)),
        )
        .is_empty());
}

#[test]
fn request_approval_only_without_pending() {
    let mut sim = merge_stage();
    let key = judge(&sim.tick())[0].0.clone();
    let spec = ApprovalSpec {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        target: serde_json::from_value(json!({"kind": "pr", "repo": "acme/web", "number": 12}))
            .unwrap(),
        subject_digest: SubjectDigest::git(oid(COMMIT)),
        delivery_id: None,
        ttl: std::time::Duration::from_secs(60),
    };
    let pending = ActionDecision::NeedsApproval {
        spec,
        pending: Some(APPROVAL.parse().unwrap()),
        config_rev: Seq::new(1).unwrap(),
    };
    // 反例：已有 pending 审批 → 不再请求。
    assert!(sim
        .report(
            EffectReport::ActionJudged {
                key: key.clone(),
                decision: pending
            },
            EffectKey::Judge(key)
        )
        .is_empty());
}

#[test]
fn merge_enqueue_requires_mergeable_snapshot() {
    let mut sim = merge_stage();
    let key = judge(&sim.tick())[0].0.clone();
    let mut snap = sim.state.pr_snapshot.clone().unwrap();
    snap.mergeable_state = "blocked".to_owned();
    sim.state.pr_snapshot = Some(snap);
    // 反例：不可合并 → 不入队。
    assert!(sim
        .decide(
            Input::Report(EffectReport::ActionJudged {
                key: key.clone(),
                decision: allow(Some(APPROVAL.parse().unwrap()))
            }),
            InputIdentity::Report(EffectKey::Judge(key)),
        )
        .is_empty());
}

#[test]
fn approval_needed_raised_once() {
    let mut sim = merge_stage();
    sim.external(
        "approval.requested",
        APPROVAL,
        json!({"action": "merge_pr", "target": {"kind": "pr", "repo": "acme/web", "number": 12},
               "subject_digest": COMMIT, "expires_at": "2026-10-24T12:45:00.000Z", "config_rev": 1,
               "requested_by": {"kind": "engine", "id": "engine"}}),
    );
    // 正例。
    assert!(sim
        .tick()
        .iter()
        .any(|d| matches!(d, Decision::Raise(r) if r.kind == InboxKind::ApprovalNeeded)));
    let _ = sim.settle();
    // 反例：open_inbox 已有带该 approval_id 的项。
    assert!(!sim.tick().iter().any(|d| matches!(d, Decision::Raise(_))));
}

#[test]
fn t8_t10_t12_guards() {
    // T8 反例：快照 head 与已推送提交不同。
    let mut sim = dispatched();
    finish(&mut sim, vec![]);
    let (_, create) = proposed(&mut sim);
    sim.enqueue(&create, "act_01JBA1D7P2KS6W8Y0A4C1F5GJP", Some(12));
    sim.state.pr_snapshot = Some(snapshot(BASE_SHA, false, "open"));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Propose);

    // T10 反例：CI 通过但有未决的引擎 escalation。
    let mut sim = review_stage();
    sim.raise(&gantry_core::ports::inbox::RaiseRequest {
        project_id: Some(PROJECT.parse().unwrap()),
        delivery_id: Some(DELIVERY.parse().unwrap()),
        kind: InboxKind::Escalation,
        severity: Severity::Blocker,
        summary: gantry_core::ports::inbox::SafeSummary::new("x"),
        payload: serde_json::from_value(json!({"reason_code": "attempt_failed", "evidence": []}))
            .unwrap(),
        dedup_key: gantry_core::ports::inbox::DedupKey::new("esc:x"),
        supersedes: vec![],
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
    });
    sim.state.delivery.stage_status = StageStatus::Running;
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "open"));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Review);

    // T12 反例：已合并，但合并审批的主体不是合并前的 head。
    let mut sim = merge_stage();
    sim.external(
        "approval.requested",
        APPROVAL,
        json!({"action": "merge_pr", "target": {"kind": "pr", "repo": "acme/web", "number": 12},
               "subject_digest": BASE_SHA, "expires_at": "2026-10-24T12:45:00.000Z", "config_rev": 1,
               "requested_by": {"kind": "engine", "id": "engine"}}),
    );
    sim.state.pr_snapshot = Some(snapshot(COMMIT, true, "merged"));
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Merge);
}

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

// ---------------------------------------------------------------------------
// 提议原文（#125，§12.2"提议原文"在纯函数层面的部分）
// ---------------------------------------------------------------------------

fn create_pr_title(sim: &Sim) -> Option<String> {
    let env = sim
        .fx
        .env(&sim.state, &InputIdentity::Tick(TickReason::Settle));
    expected_actions(&sim.state, &env)
        .values()
        .find_map(|intent| match &intent.payload {
            ActionPayload::CreatePr(p) => Some(p.title.clone()),
            _ => None,
        })
}

#[test]
fn create_pr_follows_proposal_texts() {
    let mut sim = dispatched();
    finish(&mut sim, vec![open_pr("p1", "proposed title")]);
    let _ = sim.settle();
    assert_eq!(sim.state.delivery.stage, Stage::Propose);
    let attempt = sim
        .state
        .workflows
        .values()
        .next()
        .unwrap()
        .nodes
        .values()
        .next()
        .unwrap()
        .attempts[0]
        .clone();
    assert_eq!(attempt.accepted_open_pr.as_deref(), Some("p1"));
    let key = ProposalRef::of(&attempt).unwrap();

    // 键不在表中 → 不期望 create_pr（只判 push_branch）。
    assert_eq!(create_pr_title(&sim), None);
    let judged = judge(&sim.tick());
    assert_eq!(judged.len(), 1);
    assert_eq!(judged[0].1, ForgeActionKind::PushBranch);

    // Found → 取原文。
    sim.fx.proposals.insert(
        key.clone(),
        ProposalText::Found(OpenPrParams {
            title: "proposed title".to_owned(),
            body: "b".to_owned(),
            draft: true,
        }),
    );
    assert_eq!(create_pr_title(&sim).as_deref(), Some("proposed title"));

    // Missing → 退回模板，且键与"没有提议"时的模板意图相同。
    let mut fx = Fixture::new();
    fx.proposals
        .insert(key, ProposalText::Missing(ProposalMissing::NotInResult));
    sim.fx = fx;
    assert_eq!(create_pr_title(&sim).as_deref(), Some("fix login timeout"));
}

#[test]
fn commands_are_deferred_to_slice_5() {
    let sim = Sim::low();
    let ctx = gantry_core::ports::context::ActorCtx {
        actor: Actor::new(ActorKind::Device, "dev"),
        device: None,
        role: None,
        client_profile: None,
        request_id: gantry_core::ids::Ulid::from_parts(1, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    };
    let request_id = ctx.request_id;
    let out = sim.decide(
        Input::Command(gantry_engine::decide::Command::Pause { ctx }),
        InputIdentity::Command(request_id),
    );
    assert!(matches!(out.as_slice(), [Decision::Reply(Err(_))]));
}

// ---------------------------------------------------------------------------
// 工作树、写权锁、派发与恢复重发（#70 A / B / C）
// ---------------------------------------------------------------------------

/// 只到 `worktree.created`，还没取锁。
fn worktree_ready() -> Sim {
    let mut sim = Sim::low();
    let _ = sim.settle();
    let branch = gantry_engine::decider::branch_name(&sim.state, "gantry/");
    sim.external(
        "worktree.created",
        WORKTREE,
        json!({"worktree_id": WORKTREE, "project_id": PROJECT, "delivery_id": DELIVERY,
               "branch": branch, "base_ref": "main", "base_sha": BASE_SHA}),
    );
    sim
}

fn has(decisions: &[Decision], pred: impl Fn(&Effect) -> bool) -> bool {
    decisions
        .iter()
        .any(|d| matches!(d, Decision::Effect(e) if pred(e)))
}

#[test]
fn ensure_worktree_preconditions() {
    let mut sim = Sim::low();
    let d = sim.tick();
    sim.apply(d);
    let d = sim.tick();
    sim.apply(d);
    // 正例：implement 阶段已建图、工作树为空。
    assert!(has(&sim.tick(), |e| matches!(
        e,
        Effect::EnsureWorktree { .. }
    )));
    // 反例：在途。
    sim.fx
        .inflight
        .insert(EffectKey::Worktree(DELIVERY.parse().unwrap()));
    assert!(!has(&sim.tick(), |e| matches!(
        e,
        Effect::EnsureWorktree { .. }
    )));
    // 反例：工作树已建。
    let sim = worktree_ready();
    assert!(!has(&sim.tick(), |e| matches!(
        e,
        Effect::EnsureWorktree { .. }
    )));
}

#[test]
fn acquire_lock_preconditions() {
    let mut sim = worktree_ready();
    let lock = |d: &[Decision]| has(d, |e| matches!(e, Effect::AcquireLock { .. }));
    // 正例。
    assert!(lock(&sim.tick()));
    // 反例：暂停（派发总闸）。
    sim.state.delivery.stage_status = StageStatus::Paused;
    assert!(!lock(&sim.tick()));
    sim.state.delivery.stage_status = StageStatus::Running;
    // 反例：在途。
    sim.fx
        .inflight
        .insert(EffectKey::Lock(WORKTREE.parse().unwrap()));
    assert!(!lock(&sim.tick()));
    sim.fx.inflight.clear();
    // 反例：他人（human）持有未过期租约。
    sim.external(
        "worktree.locked",
        WORKTREE,
        json!({"worktree_id": WORKTREE, "holder": "human",
               "lease_until": "2026-10-24T12:45:00.000Z"}),
    );
    assert!(!lock(&sim.tick()));
}

#[test]
fn dispatch_preconditions() {
    let attempt_started = |d: &[Decision]| {
        d.iter()
            .any(|d| matches!(d, Decision::Append(e) if e.ty == EventType::AttemptStarted))
    };
    let locked = || {
        let mut sim = worktree_ready();
        sim.external(
            "worktree.locked",
            WORKTREE,
            json!({"worktree_id": WORKTREE, "holder": "engine", "delivery_id": DELIVERY,
                   "lease_until": "2026-10-24T12:45:00.000Z"}),
        );
        let node = sim
            .state
            .workflows
            .values()
            .next()
            .unwrap()
            .nodes
            .keys()
            .next()
            .copied()
            .unwrap();
        sim.state.lock_owner.insert(WORKTREE.parse().unwrap(), node);
        sim
    };
    // 正例。
    let sim = locked();
    assert!(attempt_started(&sim.tick()));

    // 反例：lock_owner 不指向本节点（例如重启后内存为空）。
    let mut sim = locked();
    sim.state.lock_owner.clear();
    assert!(!attempt_started(&sim.tick()));

    // 反例：租约已过期。
    let mut sim = locked();
    sim.external(
        "worktree.locked",
        WORKTREE,
        json!({"worktree_id": WORKTREE, "holder": "engine", "delivery_id": DELIVERY,
               "lease_until": "2023-11-14T22:13:20.000Z"}),
    );
    assert!(!attempt_started(&sim.tick()));

    // 反例：models 缺表项。
    let mut sim = locked();
    sim.fx.models.clear();
    assert!(!attempt_started(&sim.tick()));

    // 反例：并发闸 1（concurrency = 1，已有在途节点）。
    let sim = dispatched();
    assert!(!attempt_started(&sim.tick()));

    // 反例：并发闸 2（沙盒在途 submit 已满）。
    let sim = locked();
    let identity = InputIdentity::Tick(TickReason::Settle);
    let mut env = sim.fx.env(&sim.state, &identity);
    env.sandbox_capacity = 0;
    assert!(!attempt_started(&decide(
        &sim.state,
        &Input::Tick(TickReason::Settle),
        &env
    )));
}

#[test]
fn recover_resends_unfinished_attempts() {
    let mut sim = dispatched();
    let recover = |sim: &Sim| {
        sim.decide(
            Input::Tick(TickReason::Recover),
            InputIdentity::Tick(TickReason::Recover),
        )
    };
    // 正例：有 attempt.started 无 attempt.finished → RunAttempt { resume: true }。
    let out = recover(&sim);
    assert!(has(
        &out,
        |e| matches!(e, Effect::RunAttempt(plan) if plan.resume)
    ));
    // Settle 不重发。
    assert!(!has(&sim.tick(), |e| matches!(e, Effect::RunAttempt(_))));
    // 反例：在途。
    let attempt = sim.state.unfinished_attempts().next().unwrap().1.id;
    sim.fx.inflight.insert(EffectKey::Run(attempt));
    assert!(!has(&recover(&sim), |e| matches!(e, Effect::RunAttempt(_))));
    sim.fx.inflight.clear();
    // 反例：已终结。
    finish(&mut sim, vec![]);
    assert!(!has(&recover(&sim), |e| matches!(e, Effect::RunAttempt(_))));
}

#[test]
fn branch_name_follows_section_7_12() {
    let mut sim = Sim::low();
    assert_eq!(
        gantry_engine::decider::branch_name(&sim.state, "gantry/"),
        "gantry/dlv-5t7w1yzb-fix-login-timeout"
    );
    sim.state.delivery.title = "修复：登录超时!!".to_owned();
    assert_eq!(
        gantry_engine::decider::branch_name(&sim.state, "gantry/"),
        "gantry/dlv-5t7w1yzb"
    );
    sim.state.delivery.title = "a".repeat(80);
    let name = gantry_engine::decider::branch_name(&sim.state, "gantry/");
    assert_eq!(name.len(), "gantry/dlv-5t7w1yzb-".len() + 40);
}

/// `Deny(HeadMismatch)`：不上报、不入队，先 `ReconcileNow` 刷新快照（§7.12，#206）。
#[test]
fn head_mismatch_reconciles_instead_of_raising() {
    let sim = merge_stage();
    let key = judge(&sim.tick())[0].0.clone();
    let deny = ActionDecision::Deny {
        action_id: PolicyActionId::MergePr,
        reason: DenyReason::HeadMismatch,
        report: None,
        config_rev: Seq::new(1).unwrap(),
    };
    let out = sim.decide(
        Input::Report(EffectReport::ActionJudged {
            key: key.clone(),
            decision: deny.clone(),
        }),
        InputIdentity::Report(EffectKey::Judge(key.clone())),
    );
    assert!(matches!(
        out.as_slice(),
        [Decision::Effect(Effect::ReconcileNow {
            target: gantry_core::ports::forge::ReconcileTarget::Pr { number: 12, .. }
        })]
    ));
    // 反例：同一对账在途 → 不重复发起。
    let mut sim = sim;
    let Decision::Effect(Effect::ReconcileNow { target }) = &out[0] else {
        unreachable!()
    };
    sim.fx
        .inflight
        .insert(EffectKey::Reconcile(gantry_engine::decide::ReconcileKey(
            *target,
        )));
    assert!(sim
        .decide(
            Input::Report(EffectReport::ActionJudged {
                key: key.clone(),
                decision: deny
            }),
            InputIdentity::Report(EffectKey::Judge(key)),
        )
        .is_empty());
}
