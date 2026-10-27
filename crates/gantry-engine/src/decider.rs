//! `decide`：Low 档正常路径的纯决策（engine.md §7.14.5，#70）。
//!
//! 不做 IO、不读时钟（时间取 `env.now`）、不取随机数（ID 取 `env.mint()`）。
//! 每个决策都先查前置条件，状态已前进就不再产出，这是幂等的来源（§7.1）。
//!
//! 输入分两类处理：
//! - `Tick` 与 `Event`：只按当前状态求值（[`derive`]），不依赖"刚才发生了什么"；
//! - `Report`：只处理该回报本身，之后由执行体的 `Tick(Settle)` 继续求值（§7.14.1 第 4 步）；
//! - `Tick(Recover)` 另外为未终结尝试重发 `RunAttempt { resume: true }`（§7.13 第 2 步）。
//!
//! 本切片不覆盖、留给后续切片的（每项在 #70 记录）：
//! - `ReleaseSandbox` / `ReleaseLock`、自动重试、尝试失败的处理：切片 4（#72）；
//! - 命令（`SetLevelCap` 等）与异常路径（放弃收尾、T11、T14）：切片 5（#73）；
//! - 预算不足时的 `budget.exhausted` 与上报、`models_not_configured` 上报：本切片只"不产出"，上报归切片 5；
//! - token 维度的 `budget.charged`：需要 `weight_audit`（权重与 `config_rev`），`DecideEnv` 还没有。

use std::collections::BTreeMap;

use gantry_core::canon::{IdempotencyKey, SubjectDigest};
use gantry_core::egress::LedgerRef;
use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, ApprovalStatus, AttemptStatus, ForgeActionKind, InboxKind,
    InboxPayload, NodeStatus, OutboxStatus, Priority, PullRequestState, Severity, Stage,
    StageStatus, WorkflowStatus,
};
use gantry_core::events::{
    AttemptFinishedData, BudgetChargeSource, BudgetChargedData, BudgetChargedDimension,
    DeliveryStageChangedData, EventType, NewEvent, NodeDescriptor, NodeEventData, Subject,
    WorkflowCreatedData, WorkflowFinishedData,
};
use gantry_core::forge::{
    derive_target_intent, ActionPayload, CreatePrPayload, MergeMode, MergePrPayload,
    PushBranchPayload, PushTransport,
};
use gantry_core::ids::{ActionId, Nod, NodeId, Wfl};
use gantry_core::level::Level;
use gantry_core::machines::delivery::StageTrigger;
use gantry_core::ports::forge::{EnqueueRequest, EnqueuedBy, EnqueuedByModule};
use gantry_core::ports::inbox::{DedupKey, RaiseRequest, SafeSummary};
use gantry_core::ports::policy::{ActionDecision, ActionFacts, ActionRequest, ReportHint};
use gantry_core::worker::ProposalParams;
use serde::Serialize;

use crate::decide::{
    AttemptPlan, Command, CommandRejection, DecideEnv, Decision, Effect, EffectKey, EffectReport,
    Finalized, IdMint, Input, ModelKey, PatchResult, ProposalRef, ProposalText, ResolvedModel,
    TickReason, TrustedInstructions,
};
use crate::stage::advance;
use crate::state::{ActionKey, AttemptState, DeliveryState, NodeState, WorkflowState};
use crate::template::{stage_graph, ExitRule};

/// 动作的 `epoch` 初值；M1 内不自增（§7.14.4）。
pub const INITIAL_EPOCH: u32 = 1;

/// `create_pr` 没有被接受的提议（或提议确定缺失）时的正文模板。
pub fn pr_body_template(state: &DeliveryState) -> String {
    format!("由 Gantry 交付单元 {} 生成。", state.delivery.id)
}

pub fn decide(state: &DeliveryState, input: &Input, env: &DecideEnv) -> Vec<Decision> {
    let mut mint = env.mint();
    match input {
        Input::Tick(TickReason::Recover) => {
            let mut out = derive(state, env, &mut mint);
            resume_unfinished(state, env, &mut out);
            out
        }
        Input::Tick(_) | Input::Event(_) => derive(state, env, &mut mint),
        Input::Report(report) => on_report(state, report, env),
        Input::Command(command) => vec![Decision::Reply(Err(on_command(command)))],
    }
}

fn on_command(command: &Command) -> CommandRejection {
    match command {
        Command::SupplySpec { .. } | Command::RequestHeal(_) | Command::Classify(_) => {
            CommandRejection::NotSupportedYet("M2")
        }
        // 控制面命令归切片 5（#73）。
        _ => CommandRejection::NotSupportedYet("decide slice 5 (#73)"),
    }
}

// ---------------------------------------------------------------------------
// 按状态求值
// ---------------------------------------------------------------------------

/// 只看当前状态推出全部可产出的决策；每一项都带自己的前置条件。
pub fn derive(state: &DeliveryState, env: &DecideEnv, mint: &mut IdMint) -> Vec<Decision> {
    let mut out = Vec::new();
    // 阶段推进至多一条：同一次求值里推进后，其余决策基于新阶段在下一轮 Settle 产出。
    if let Some(decision) = stage_transition(state) {
        out.push(decision);
        return out;
    }
    build_workflow(state, mint, &mut out);
    queue_nodes(state, &mut out);
    ensure_worktree(state, env, &mut out);
    acquire_locks(state, env, &mut out);
    dispatch(state, env, mint, &mut out);
    finish_workflows(state, &mut out);
    judge_actions(state, env, &mut out);
    raise_approval_needed(state, &mut out);
    release_worktree(state, env, &mut out);
    out
}

fn delivery_event<T: Serialize>(
    state: &DeliveryState,
    ty: EventType,
    subject: &str,
    data: &T,
) -> Decision {
    Decision::Append(NewEvent {
        ty,
        subject: Subject::parse(subject).expect("engine ids are valid subjects"),
        project_id: Some(state.delivery.project_id),
        actor: engine_actor(),
        causation_id: None,
        correlation_id: Some(state.delivery.id.to_string()),
        data: without_nulls(serde_json::to_value(data).expect("event data serializes")),
        dedup_key: None,
    })
}

/// 事件契约要求可选字段省略、不写 `null`（event-data.md）；`serde` 默认把 `None` 写成 `null`。
fn without_nulls(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, without_nulls(v)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(without_nulls).collect())
        }
        other => other,
    }
}

fn engine_actor() -> Actor {
    Actor::new(ActorKind::Engine, "engine")
}

fn stage_changed(
    state: &DeliveryState,
    trigger: StageTrigger,
    reason: Option<&str>,
) -> Option<Decision> {
    let row = &state.delivery;
    let transition = advance(row.stage, row.level_effective, trigger, row.abandon_cause).ok()?;
    Some(delivery_event(
        state,
        EventType::DeliveryStageChanged,
        &row.id.to_string(),
        &DeliveryStageChangedData {
            from: transition.from,
            to: transition.to,
            reason: reason.map(str::to_owned),
            stage_status: transition.status,
        },
    ))
}

/// §7.6 中本切片覆盖的迁移：T2、T6、T8、T10、T12（Low）。
fn stage_transition(state: &DeliveryState) -> Option<Decision> {
    let row = &state.delivery;
    if row.stage.is_terminal() || row.stage_status != StageStatus::Running {
        return None;
    }
    match row.stage {
        // T2：Low 免准入、无计划阶段。
        Stage::Spec if row.level_effective == Level::Low => stage_changed(
            state,
            StageTrigger::AdmissionSkipped,
            Some("admission_skipped"),
        ),
        // T6：`single` 成功，补丁已校验（allow）且已生成本地提交。
        Stage::Implement if patch_committed(state) => {
            stage_changed(state, StageTrigger::PatchValidated, None)
        }
        // T8（首次）：create_pr 已成功、PR 已绑定，且快照 head = 已推送的本地提交。
        Stage::Propose if pr_created(state) => stage_changed(state, StageTrigger::PrCreated, None),
        // T10（Low 无评审图，只等 CI）。
        Stage::Review if ci_green(state) => {
            stage_changed(state, StageTrigger::ReviewAcceptCiGreen, None)
        }
        // T12。
        Stage::Merge if merged(state) => {
            stage_changed(state, StageTrigger::MergedAndIssuesClosed, None)
        }
        _ => None,
    }
}

/// 当前阶段图（`low.implement`）的退出条件 `PatchCommitted { node }`（§7.14.7）。
fn patch_committed(state: &DeliveryState) -> bool {
    let Some(graph) = stage_graph(state.delivery.level_effective, Stage::Implement) else {
        return false;
    };
    let ExitRule::PatchCommitted { node: key } = &graph.exit else {
        return false;
    };
    let Some(head) = state.evidence.candidate_head else {
        return false;
    };
    if state.evidence.patch_sha256.is_none() {
        return false;
    }
    current_workflow(state).is_some_and(|wf| {
        wf.nodes.values().any(|node| {
            &node.key == key
                && node.status == NodeStatus::Succeeded
                && producing_attempt(node).is_some_and(|a| a.result_commit == Some(head))
        })
    })
}

fn pr_created(state: &DeliveryState) -> bool {
    let (Some(pr), Some(snapshot), Some(head)) =
        (&state.pr, &state.pr_snapshot, state.evidence.candidate_head)
    else {
        return false;
    };
    let created = state
        .outbox
        .values()
        .any(|row| row.kind == ForgeActionKind::CreatePr && row.status == OutboxStatus::Succeeded);
    created && snapshot.number == pr.number && snapshot.head_sha == head
}

/// 无未决的、引擎创建的 `escalation` / `decision_needed`（T10 守卫）。
fn no_open_decisions(state: &DeliveryState) -> bool {
    !state.open_inbox.values().any(|item| {
        item.raised_by == crate::state::RaisedBy::Engine
            && matches!(item.kind, InboxKind::Escalation | InboxKind::DecisionNeeded)
    })
}

fn ci_green(state: &DeliveryState) -> bool {
    state
        .pr_snapshot
        .as_ref()
        .is_some_and(|pr| pr.state == PullRequestState::Open && pr.checks.required_ok)
        && no_open_decisions(state)
}

/// T12：PR 已合并；合并前的 head 等于合并审批的主体；绑定的 Issue 已关闭。
/// M1 的 PR 一律合入默认分支，绑定 Issue 由 GitHub 随合并关闭（§7.6），不另入队 `close_issue`。
fn merged(state: &DeliveryState) -> bool {
    let Some(pr) = &state.pr_snapshot else {
        return false;
    };
    if pr.state != PullRequestState::Merged {
        return false;
    }
    let subject = SubjectDigest::git(pr.head_sha);
    state.approvals.values().any(|approval| {
        approval.action == ApprovalAction::Forge(ForgeActionKind::MergePr)
            && approval.subject_digest == subject
            && matches!(
                approval.status,
                ApprovalStatus::Granted | ApprovalStatus::Consumed
            )
    })
}

/// 本阶段的 Workflow（自 `stage_entered` 起建的那张图）。
fn current_workflow(state: &DeliveryState) -> Option<&WorkflowState> {
    let entered = state.delivery.stage_entered.seq;
    state
        .workflows
        .values()
        .filter(|wf| wf.stage == Some(state.delivery.stage) && wf.created_seq > entered)
        .max_by_key(|wf| wf.created_seq)
}

/// 产出补丁的那次尝试：节点最近一次成功的尝试。
fn producing_attempt(node: &NodeState) -> Option<&AttemptState> {
    node.attempts
        .iter()
        .rev()
        .find(|a| a.status == AttemptStatus::Succeeded)
}

/// `Append(workflow.created)` + `budget.charged(nodes)`：当前阶段有 agent 图，且自 `stage_entered`
/// 以来尚未建过；追加后 `used.nodes ≤ limits.nodes`（否则不建，耗尽上报归切片 5）。
fn build_workflow(state: &DeliveryState, mint: &mut IdMint, out: &mut Vec<Decision>) {
    let row = &state.delivery;
    if row.stage.is_terminal() || state.workflow_built_in_stage() || !state.may_dispatch() {
        return;
    }
    let Some(graph) = stage_graph(row.level_effective, row.stage) else {
        return;
    };
    let k = graph.nodes.len() as u64;
    let used = state.ledger.used.nodes;
    if used + k > row.budget.limits.nodes {
        return;
    }
    let workflow_id = mint.next_id::<Wfl>();
    let ids: Vec<NodeId> = graph.nodes.iter().map(|_| mint.next_id::<Nod>()).collect();
    let nodes = graph
        .nodes
        .iter()
        .zip(&ids)
        .map(|(spec, id)| NodeDescriptor {
            node_id: *id,
            key: spec.key.clone(),
            role: spec.role.clone(),
            depends_on: spec.depends_on.clone(),
            required: spec.required,
            critical: spec.critical,
        })
        .collect();
    out.push(delivery_event(
        state,
        EventType::WorkflowCreated,
        &workflow_id.to_string(),
        &WorkflowCreatedData {
            stage: Some(row.stage),
            template: graph.template.to_owned(),
            graph_rev: 1,
            nodes,
            delivery_id: Some(row.id),
            conversation_id: None,
        },
    ));
    for (i, id) in ids.iter().enumerate() {
        out.push(delivery_event(
            state,
            EventType::BudgetCharged,
            &row.id.to_string(),
            &BudgetChargedData {
                ledger: LedgerRef::Delivery { id: row.id },
                dimension: BudgetChargedDimension::Nodes,
                delta: 1,
                used_after: used + i as u64 + 1,
                source: BudgetChargeSource::Node { node_id: *id },
                tier: None,
                usage: None,
                weighted_tokens: None,
                weight_audit: None,
            },
        ));
    }
}

/// 依赖满足（§7.4 通则）：每个 required 依赖已 succeeded，每个非 required 依赖已终态。
fn dependencies_met(workflow: &WorkflowState, node: &NodeState) -> bool {
    node.depends_on.iter().all(|dep| {
        workflow.nodes.get(dep).is_some_and(|d| {
            if d.required {
                d.status == NodeStatus::Succeeded
            } else {
                matches!(
                    d.status,
                    NodeStatus::Succeeded
                        | NodeStatus::Failed
                        | NodeStatus::Skipped
                        | NodeStatus::Cancelled
                        | NodeStatus::Superseded
                )
            }
        })
    })
}

/// `Append(node.queued)`：节点 pending、依赖满足、`may_dispatch`。
fn queue_nodes(state: &DeliveryState, out: &mut Vec<Decision>) {
    if !state.may_dispatch() {
        return;
    }
    for workflow in state.workflows.values() {
        if workflow.status != WorkflowStatus::Running {
            continue;
        }
        for node in workflow.nodes.values() {
            if node.status == NodeStatus::Pending && dependencies_met(workflow, node) {
                out.push(node_event(
                    state,
                    EventType::NodeQueued,
                    node,
                    NodeStatus::Queued,
                ));
            }
        }
    }
}

fn node_event(
    state: &DeliveryState,
    ty: EventType,
    node: &NodeState,
    status: NodeStatus,
) -> Decision {
    delivery_event(
        state,
        ty,
        &node.id.to_string(),
        &NodeEventData {
            node_id: node.id,
            status,
            verdict: None,
            superseded_by: None,
        },
    )
}

/// `Append(workflow.finished(succeeded))`：全部 required 节点 succeeded。
/// required 节点终态失败时不 finish（§7.14.5）。
fn finish_workflows(state: &DeliveryState, out: &mut Vec<Decision>) {
    for workflow in state.workflows.values() {
        if workflow.status != WorkflowStatus::Running {
            continue;
        }
        let done = workflow
            .nodes
            .values()
            .filter(|n| n.required)
            .all(|n| n.status == NodeStatus::Succeeded);
        if done && !workflow.nodes.is_empty() {
            out.push(delivery_event(
                state,
                EventType::WorkflowFinished,
                &workflow.id.to_string(),
                &WorkflowFinishedData {
                    status: WorkflowStatus::Succeeded,
                },
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// 期望动作集合（§7.14.1、§7.12）
// ---------------------------------------------------------------------------

/// 一条期望中的 Outbox 动作。
#[derive(Clone, Debug, PartialEq)]
pub struct ActionIntent {
    pub key: ActionKey,
    pub payload: ActionPayload,
    pub facts: ActionFacts,
    pub priority: Priority,
    /// 必须先在 `outbox` 里的动作（`create_pr` 依赖本次 `push_branch`）。
    pub depends_on: Option<ActionKey>,
}

impl ActionIntent {
    pub fn kind(&self) -> ForgeActionKind {
        self.payload.kind()
    }
}

/// 从状态推出期望中的 Outbox 动作（纯函数）。`proposals` 只影响 `create_pr`（§7.14.3）。
pub fn expected_actions(
    state: &DeliveryState,
    env: &DecideEnv,
) -> BTreeMap<ActionKey, ActionIntent> {
    let mut out = BTreeMap::new();
    let row = &state.delivery;
    let project = &env.settings.project;
    match row.stage {
        Stage::Propose => {
            let Some(push) = push_branch(state, env) else {
                return out;
            };
            if state.pr.is_none() {
                if let Some(create) = create_pr(state, env, &push.key) {
                    out.insert(create.key.clone(), create);
                }
            }
            out.insert(push.key.clone(), push);
        }
        Stage::Merge => {
            let (Some(pr), Some(snapshot)) = (&state.pr, &state.pr_snapshot) else {
                return out;
            };
            if snapshot.state != PullRequestState::Open {
                return out;
            }
            let Some(merge_method) = project.allowed_merge_methods.first().cloned() else {
                return out;
            };
            let closes: Vec<String> = row
                .issues
                .iter()
                .map(|issue| format!("Closes #{}", issue.number))
                .collect();
            let payload = ActionPayload::MergePr(MergePrPayload {
                repo: project.repo.clone(),
                pull_number: pr.number,
                expected_head_sha: snapshot.head_sha,
                merge_method,
                mode: MergeMode::Auto,
                commit_title: None,
                commit_message: (!closes.is_empty()).then(|| closes.join("\n")),
            });
            let facts = ActionFacts::MergePr {
                repo: project.repo.clone(),
                number: pr.number,
                expected_head_sha: snapshot.head_sha,
            };
            if let Some(intent) = intent(state, payload, facts, Priority::High, None) {
                out.insert(intent.key.clone(), intent);
            }
        }
        _ => {}
    }
    out
}

fn intent(
    state: &DeliveryState,
    payload: ActionPayload,
    facts: ActionFacts,
    priority: Priority,
    depends_on: Option<ActionKey>,
) -> Option<ActionIntent> {
    let derived = derive_target_intent(&payload).ok()?;
    let key = IdempotencyKey::derive(
        &state.delivery.project_id,
        &state.delivery.id,
        payload.kind(),
        &derived.target,
        &derived.intent,
        INITIAL_EPOCH,
    )
    .ok()?;
    Some(ActionIntent {
        key,
        payload,
        facts,
        priority,
        depends_on,
    })
}

fn push_branch(state: &DeliveryState, env: &DecideEnv) -> Option<ActionIntent> {
    let project = &env.settings.project;
    let worktree = state.worktree.as_ref()?;
    let head = state.evidence.candidate_head?;
    let tree = state.evidence.candidate_tree?;
    let validation = state.evidence.patch_sha256?;
    let payload = ActionPayload::PushBranch(PushBranchPayload {
        repo: project.repo.clone(),
        branch: worktree.branch.clone(),
        base_ref: project.default_branch.clone(),
        worktree_id: worktree.id,
        local_head_oid: head,
        expected_tree_oid: tree,
        // M1 只有首次推送（修正轮在 M2），上次推送的 head 为空。
        expected_remote_oid: None,
        validation_ref: validation,
        transport: PushTransport::Git,
        force: false,
    });
    let facts = ActionFacts::PushBranch {
        repo: project.repo.clone(),
        branch: worktree.branch.clone(),
        branch_prefix: project.branch_prefix.clone(),
        force: false,
        local_head_oid: head,
        expected_remote_oid: None,
        expected_tree_oid: Some(tree),
        validation_ref: Some(validation),
    };
    intent(state, payload, facts, Priority::High, None)
}

/// `create_pr` 的标题 / 正文 / 草稿：按 §7.14.3 末的表取提议原文或模板；
/// 提议引用尚未预取（`None`）时本轮不期望 `create_pr`。
fn create_pr(state: &DeliveryState, env: &DecideEnv, push: &ActionKey) -> Option<ActionIntent> {
    let project = &env.settings.project;
    let worktree = state.worktree.as_ref()?;
    let head = state.evidence.candidate_head?;
    let producing = current_or_last_patch_attempt(state, head)?;
    let template = (state.delivery.title.clone(), pr_body_template(state), false);
    let (title, body, draft) = match ProposalRef::of(producing) {
        None => template,
        Some(key) => match env.proposals.get(&key)? {
            ProposalText::Found(params) => {
                (params.title.clone(), params.body.clone(), params.draft)
            }
            ProposalText::Missing(_) => template,
        },
    };
    let payload = ActionPayload::CreatePr(CreatePrPayload {
        repo: project.repo.clone(),
        head: worktree.branch.clone(),
        base: project.default_branch.clone(),
        title,
        body,
        draft,
        // base 为默认分支时才填绑定的 Issue（§7.12）；M1 的 base 一律是默认分支。
        closes: state
            .delivery
            .issues
            .iter()
            .map(|issue| issue.number)
            .collect(),
        expected_head_oid: head,
        maintainer_can_modify: false,
    });
    // create_pr 的目标身份是 pr_creation（event-data §2.2.6 动作目标映射，#182）。
    let facts = ActionFacts::Other {
        target: gantry_core::events::PolicyTarget::PrCreation {
            repo: project.repo.clone(),
            head: worktree.branch.clone(),
            base: project.default_branch.clone(),
        },
    };
    intent(state, payload, facts, Priority::High, Some(push.clone()))
}

/// 产出 `candidate_head` 的那次尝试（`result_commit = candidate_head`）。
fn current_or_last_patch_attempt(
    state: &DeliveryState,
    head: gantry_core::canon::GitOid,
) -> Option<&AttemptState> {
    state
        .workflows
        .values()
        .flat_map(|wf| wf.nodes.values())
        .flat_map(|node| node.attempts.iter())
        .find(|a| a.result_commit == Some(head))
}

/// 主体为 `subject` 的审批是否有 pending 的（需审批动作的判定前置条件）。
fn approval_pending(state: &DeliveryState, intent: &ActionIntent) -> bool {
    let ActionPayload::MergePr(merge) = &intent.payload else {
        return false;
    };
    let subject = SubjectDigest::git(merge.expected_head_sha);
    state.approvals.values().any(|approval| {
        approval.action == ApprovalAction::Forge(ForgeActionKind::MergePr)
            && approval.subject_digest == subject
            && approval.status == ApprovalStatus::Pending
    })
}

/// `Effect(JudgeAction)`：意图 ∈ 期望集合；`outbox` 无该键；不在 `env.denied`；未在途；
/// 需审批的动作没有同主体 pending 的审批。
fn judge_actions(state: &DeliveryState, env: &DecideEnv, out: &mut Vec<Decision>) {
    for (key, intent) in expected_actions(state, env) {
        if state.outbox.contains_key(&key)
            || env.denied.contains(&key)
            || env.inflight.contains(&EffectKey::Judge(key.clone()))
            || approval_pending(state, &intent)
        {
            continue;
        }
        out.push(Decision::Effect(Effect::JudgeAction {
            request: ActionRequest {
                project_id: state.delivery.project_id,
                delivery_id: Some(state.delivery.id),
                kind: intent.kind(),
                facts: intent.facts.clone(),
                requested_by: engine_actor(),
                epoch: INITIAL_EPOCH,
            },
            key,
        }));
    }
}

/// `Raise(approval_needed)`：阶段为 merge；有本交付单元 pending 的审批，
/// 且 `open_inbox` 中没有带该 `approval_id` 的项。
fn raise_approval_needed(state: &DeliveryState, out: &mut Vec<Decision>) {
    if state.delivery.stage != Stage::Merge {
        return;
    }
    for (approval_id, approval) in &state.approvals {
        if approval.status != ApprovalStatus::Pending {
            continue;
        }
        if state
            .open_inbox
            .values()
            .any(|item| item.approval_id == Some(*approval_id))
        {
            continue;
        }
        out.push(Decision::Raise(RaiseRequest {
            project_id: Some(state.delivery.project_id),
            delivery_id: Some(state.delivery.id),
            kind: InboxKind::ApprovalNeeded,
            severity: Severity::Blocker,
            summary: SafeSummary::new("合并需要审批"),
            payload: InboxPayload::ApprovalNeeded {
                approval_id: *approval_id,
            },
            dedup_key: DedupKey::new(format!("approval:{approval_id}")),
            supersedes: Vec::new(),
            actor: engine_actor(),
            causation_id: None,
        }));
    }
}

/// T12 的"释放工作树"：阶段 `closed`、尝试全部终结、工作树仍在 → `RemoveWorktree`。
fn release_worktree(state: &DeliveryState, env: &DecideEnv, out: &mut Vec<Decision>) {
    if state.delivery.stage != Stage::Closed || state.unfinished_attempts().next().is_some() {
        return;
    }
    let Some(worktree) = &state.worktree else {
        return;
    };
    if env
        .inflight
        .contains(&EffectKey::RemoveWorktree(worktree.id))
    {
        return;
    }
    let lock = state.lock_token(&worktree.id).filter(|token| {
        token.holder == gantry_core::entities::LockHolder::Engine && token.lease_until > env.now
    });
    out.push(Decision::Effect(Effect::RemoveWorktree {
        worktree_id: worktree.id,
        lock,
    }));
}

// ---------------------------------------------------------------------------
// 回报
// ---------------------------------------------------------------------------

fn on_report(state: &DeliveryState, report: &EffectReport, env: &DecideEnv) -> Vec<Decision> {
    match report {
        EffectReport::ActionJudged { key, decision } => on_judged(state, key, decision, env),
        EffectReport::AttemptFinalized(finalized) => on_finalized(state, finalized),
        // 其余回报只把键移出在途集合（执行体负责），由随后的 Tick(Settle) 求值。
        _ => Vec::new(),
    }
}

/// 判定回流：`Allow` → `Enqueue`（意图仍在期望集合、`outbox` 仍无该键、依赖已在 `outbox`）；
/// `NeedsApproval` 且无 pending → `RequestApproval`；`Deny` → 按 `ReportHint` 上报（E3）。
fn on_judged(
    state: &DeliveryState,
    key: &ActionKey,
    decision: &ActionDecision,
    env: &DecideEnv,
) -> Vec<Decision> {
    let expected = expected_actions(state, env);
    let Some(intent) = expected.get(key) else {
        return Vec::new();
    };
    match decision {
        ActionDecision::Allow { approval, .. } => {
            if state.outbox.contains_key(key) {
                return Vec::new();
            }
            if intent.kind() == ForgeActionKind::MergePr && !mergeable(state) {
                return Vec::new();
            }
            let depends_on: Vec<ActionId> = match &intent.depends_on {
                None => Vec::new(),
                Some(dep) => match state.outbox.get(dep) {
                    Some(row) => vec![row.action_id],
                    None => return Vec::new(),
                },
            };
            vec![Decision::Enqueue(EnqueueRequest {
                project_id: state.delivery.project_id,
                delivery_id: state.delivery.id,
                payload: intent.payload.clone(),
                requires_approval: *approval,
                depends_on,
                priority: intent.priority,
                not_before: None,
                epoch: INITIAL_EPOCH,
                enqueued_by: EnqueuedBy {
                    module: EnqueuedByModule::Engine,
                    causation_id: None,
                },
            })]
        }
        ActionDecision::NeedsApproval { spec, pending, .. } => {
            if pending.is_some() || env.inflight.contains(&EffectKey::Approval(key.clone())) {
                return Vec::new();
            }
            vec![Decision::Effect(Effect::RequestApproval {
                key: key.clone(),
                spec: spec.clone(),
            })]
        }
        // 快照 head 与请求不符：不上报，先刷新快照，对账回来后由 Tick(Settle) 重新判定（§7.12，#206）。
        ActionDecision::Deny {
            reason: gantry_core::ports::policy::DenyReason::HeadMismatch,
            ..
        } => {
            let target = match &state.pr {
                Some(pr) => gantry_core::ports::forge::ReconcileTarget::Pr {
                    project: state.delivery.project_id,
                    number: pr.number,
                },
                None => gantry_core::ports::forge::ReconcileTarget::Delivery(state.delivery.id),
            };
            if env
                .inflight
                .contains(&EffectKey::Reconcile(crate::decide::ReconcileKey(target)))
            {
                return Vec::new();
            }
            vec![Decision::Effect(Effect::ReconcileNow { target })]
        }
        ActionDecision::Deny { report, .. } => match report {
            Some(hint) => raise_from_hint(state, key, hint).into_iter().collect(),
            // E3：其余 Deny 必带 ReportHint（码族 policy.action_denied:*，#206）；缺失是 policy 缺陷，不自造上报码。
            None => Vec::new(),
        },
    }
}

/// 合并入队前置：快照 `mergeable_state` 为可合并（`clean`）。merge queue 的项目由 forge 决定
/// 直接合并还是入队（§7.12）；`ProjectFacts` 尚无 `merge_queue_enabled`，M1 只认 `clean`。
fn mergeable(state: &DeliveryState) -> bool {
    state
        .pr_snapshot
        .as_ref()
        .is_some_and(|pr| pr.mergeable_state == "clean")
}

fn raise_from_hint(state: &DeliveryState, key: &ActionKey, hint: &ReportHint) -> Option<Decision> {
    // M1 引擎发起的动作被拒只产生 escalation（policy.md / #66）；其他形状等 #133。
    if hint.inbox_kind != InboxKind::Escalation {
        return None;
    }
    let reason = hint.reason_code.clone();
    Some(Decision::Raise(RaiseRequest {
        project_id: Some(state.delivery.project_id),
        delivery_id: Some(state.delivery.id),
        kind: InboxKind::Escalation,
        severity: hint.severity.clone(),
        summary: SafeSummary::new(format!("动作被策略拒绝：{reason}")),
        payload: InboxPayload::Escalation {
            reason_code: reason.clone(),
            node_id: None,
            attempt_id: None,
            target_level: None,
            action_id: None,
            evidence: Vec::new(),
            options: None,
        },
        dedup_key: DedupKey::new(format!("esc:{}:{reason}:{key}", state.delivery.id)),
        supersedes: Vec::new(),
        actor: engine_actor(),
        causation_id: None,
    }))
}

/// `Append(attempt.finished)` + `budget.charged(wall_time)` + `node.finished`（节点因此进入终态时）：
/// 该尝试无 `attempt.finished`，且 `Finalized` 的尝试与节点匹配。
fn on_finalized(state: &DeliveryState, finalized: &Finalized) -> Vec<Decision> {
    let Some(node) = state.node(&finalized.node_id) else {
        return Vec::new();
    };
    let Some(attempt) = node.attempts.iter().find(|a| a.id == finalized.attempt_id) else {
        return Vec::new();
    };
    if attempt.finished_at.is_some() {
        return Vec::new();
    }
    let (result_tree, result_commit) = match &finalized.patch {
        PatchResult::Committed { commit, tree, .. } => (Some(*tree), Some(*commit)),
        _ => (None, None),
    };
    let accepted_open_pr = finalized.proposals.iter().find_map(|outcome| {
        let accepted = matches!(
            outcome.decision,
            gantry_core::ports::policy::ProposalDecision::Accepted { .. }
        );
        let open_pr = matches!(
            outcome.open_pr.as_ref().map(|p| &p.params),
            Some(ProposalParams::OpenPr(_))
        );
        (accepted && open_pr).then(|| outcome.proposal_id.clone())
    });
    let mut out = vec![delivery_event(
        state,
        EventType::AttemptFinished,
        &attempt.id.to_string(),
        &AttemptFinishedData {
            status: finalized.status,
            result_ref: finalized.result_ref,
            failure_class: finalized.failure_class,
            failure_detail: finalized
                .failure_detail
                .as_ref()
                .map(|d| d.as_str().to_owned()),
            usage: Some(finalized.usage_metered),
            result_tree,
            result_commit,
            served_by: None,
            accepted_open_pr: accepted_open_pr.filter(|_| finalized.result_ref.is_some()),
        },
    )];
    if finalized.wall_time_s > 0 {
        out.push(delivery_event(
            state,
            EventType::BudgetCharged,
            &state.delivery.id.to_string(),
            &BudgetChargedData {
                ledger: LedgerRef::Delivery {
                    id: state.delivery.id,
                },
                dimension: BudgetChargedDimension::WallTime,
                delta: finalized.wall_time_s,
                used_after: state.ledger.used.wall_time + finalized.wall_time_s,
                source: BudgetChargeSource::Attempt {
                    attempt_id: attempt.id,
                },
                tier: None,
                usage: None,
                weighted_tokens: None,
                weight_audit: None,
            },
        ));
    }
    // 成功或取消即终态；失败的重试与耗尽判断归切片 4（#72）。
    let node_status = match finalized.status {
        AttemptStatus::Succeeded => Some(NodeStatus::Succeeded),
        AttemptStatus::Cancelled => Some(NodeStatus::Cancelled),
        _ => None,
    };
    if let Some(status) = node_status {
        if node.status == NodeStatus::Running {
            out.push(node_event(state, EventType::NodeFinished, node, status));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 工作树、写权锁与派发（§7.7、§7.8，#70 A / B / C）
// ---------------------------------------------------------------------------

/// `single` 角色的模板（引擎内置，可信）；与 `user_text` 一起组成 `task.instructions`（§7.8）。
pub const ROLE_TEMPLATE_SINGLE: &str =
    "你是 Gantry 的实现 agent。按下面用户的要求修改仓库，只做与要求相关的改动，并保证改动可以通过现有测试。";

/// 分支名 `<prefix>dlv-<交付单元 ID 末 8 位>-<slug>`（§7.12）：slug 取自标题，只含 `[a-z0-9-]`，≤ 40 字符。
pub fn branch_name(state: &DeliveryState, prefix: &str) -> String {
    let id = state.delivery.id.to_string();
    let tail = id[id.len() - 8..].to_ascii_lowercase();
    let mut slug = String::new();
    for ch in state.delivery.title.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("{prefix}dlv-{tail}")
    } else {
        format!("{prefix}dlv-{tail}-{slug}")
    }
}

/// 当前阶段的图是否需要写工作树：图里有 `exec = sandbox` 的节点（Low 起的 implement）。
fn stage_needs_worktree(state: &DeliveryState) -> bool {
    current_workflow(state).is_some_and(|wf| {
        wf.nodes
            .values()
            .any(|n| n.exec == gantry_core::entities::Exec::Sandbox)
    })
}

/// `Effect(EnsureWorktree)`：当前阶段需要写工作树且 `worktree` 为空（§7.14.5）。
fn ensure_worktree(state: &DeliveryState, env: &DecideEnv, out: &mut Vec<Decision>) {
    if state.worktree.is_some()
        || !stage_needs_worktree(state)
        || env
            .inflight
            .contains(&EffectKey::Worktree(state.delivery.id))
    {
        return;
    }
    let project = &env.settings.project;
    out.push(Decision::Effect(Effect::EnsureWorktree {
        branch: gantry_core::ports::repo::BranchName::new(branch_name(
            state,
            &project.branch_prefix,
        )),
        base_ref: project.default_branch.clone(),
    }));
}

fn model_for(env: &DecideEnv, state: &DeliveryState, node: &NodeState) -> Option<ResolvedModel> {
    env.models
        .get(&ModelKey {
            sandbox: node.exec != gantry_core::entities::Exec::Direct,
            level: state.delivery.level_effective,
            tier: node.tier,
            critical: node.critical,
        })
        .cloned()
}

/// 有效 `wall_time_s` = min(`attempt_wall_s`，`sandbox.limits.wall_time_s`，预算剩余 `wall_time`)（§7.8）。
fn effective_wall_time(state: &DeliveryState, env: &DecideEnv, model: &ResolvedModel) -> u64 {
    let remaining = state
        .delivery
        .budget
        .limits
        .wall_time
        .saturating_sub(state.ledger.used.wall_time);
    model
        .attempt_wall_s
        .min(env.settings.sandbox.wall_time_s)
        .min(remaining)
}

/// 锁租约 = 有效 `wall_time_s` + `kill_grace_s` + 租约余量（§7.7）。
fn lease_seconds(env: &DecideEnv, wall_time_s: u64) -> u64 {
    wall_time_s + env.settings.sandbox.kill_grace_s + env.consts.lock_lease_margin_s
}

/// 节点处于可派发状态：`queued`（本切片不含自动重试，失败重试归切片 4）。
fn dispatchable(node: &NodeState, env: &DecideEnv) -> bool {
    node.status == NodeStatus::Queued
        && node.attempts.iter().all(|a| a.finished_at.is_some())
        && (node.attempts.len() as u32 - node.attempts_at_retry) < env.consts.max_attempts_per_node
}

/// 本节点持有有效锁：`lock_owner` 指向本节点，`locks` 中该工作树的持有者为 engine 且未过期。
fn holds_valid_lock(state: &DeliveryState, env: &DecideEnv, node: &NodeState) -> bool {
    let Some(worktree) = &state.worktree else {
        return false;
    };
    state.lock_owner.get(&worktree.id) == Some(&node.id)
        && state.locks.get(&worktree.id).is_some_and(|lock| {
            lock.holder == gantry_core::entities::LockHolder::Engine && lock.lease_until > env.now
        })
}

/// `Effect(AcquireLock)`：`exec = sandbox`、节点可派发、本节点没有有效锁、工作树已建、
/// `locks` 中无他人未过期租约、`lock_owner` 未被本交付单元其他节点占用、`may_dispatch`。
fn acquire_locks(state: &DeliveryState, env: &DecideEnv, out: &mut Vec<Decision>) {
    let Some(worktree) = &state.worktree else {
        return;
    };
    if !state.may_dispatch() || env.inflight.contains(&EffectKey::Lock(worktree.id)) {
        return;
    }
    if let Some(lock) = state.locks.get(&worktree.id) {
        let ours = lock.holder == gantry_core::entities::LockHolder::Engine;
        if !ours && lock.lease_until > env.now {
            return;
        }
    }
    let Some(workflow) = current_workflow(state) else {
        return;
    };
    for node in workflow.nodes.values() {
        if node.exec != gantry_core::entities::Exec::Sandbox
            || !dispatchable(node, env)
            || holds_valid_lock(state, env, node)
        {
            continue;
        }
        if let Some(owner) = state.lock_owner.get(&worktree.id) {
            if *owner != node.id {
                continue;
            }
        }
        let Some(model) = model_for(env, state, node) else {
            continue;
        };
        let wall = effective_wall_time(state, env, &model);
        out.push(Decision::Effect(Effect::AcquireLock {
            worktree_id: worktree.id,
            node_id: node.id,
            holder: gantry_core::entities::LockHolder::Engine,
            lease_s: lease_seconds(env, wall),
        }));
        // 同一工作树一次只为一个节点取锁。
        return;
    }
}

/// 交付单元台账的只读视图，供 `ReservationPolicy` 使用。台账没有独立的创建事件（OQ-12），
/// `BudgetId` 与 `created_at` 都由交付单元 ID 确定性推出（同一 128 位值、其时间部分），只供预留策略读限额与已用量。
fn ledger_view(state: &DeliveryState) -> gantry_core::entities::BudgetLedger {
    let id = state.delivery.id;
    let created = gantry_core::time::Timestamp::from_millis(id.timestamp_ms() as i64);
    gantry_core::entities::BudgetLedger {
        id: gantry_core::ids::BudgetId::from_parts(id.timestamp_ms(), id.random80())
            .expect("same bit layout as DeliveryId"),
        subject: gantry_core::entities::BudgetSubject::Delivery,
        delivery_id: Some(id),
        conversation_id: None,
        project_id: state.delivery.project_id,
        day: None,
        template: Some(state.delivery.budget.template.clone()),
        limits: state.delivery.budget.limits.clone(),
        used: state.ledger.used.clone(),
        config_rev: Some(state.delivery.config_rev),
        created_at: created,
        updated_at: created,
    }
}

/// 交付单元内在途节点数（有未终结尝试的节点）。
fn running_nodes(state: &DeliveryState) -> u64 {
    state
        .workflows
        .values()
        .flat_map(|wf| wf.nodes.values())
        .filter(|n| n.attempts.iter().any(|a| a.finished_at.is_none()))
        .count() as u64
}

fn attempt_plan(
    state: &DeliveryState,
    workflow: &WorkflowState,
    node: &NodeState,
    attempt: &AttemptState,
    model: ResolvedModel,
    lock: Option<gantry_core::ports::repo::LockToken>,
    resume: bool,
) -> AttemptPlan {
    AttemptPlan {
        attempt_id: attempt.id,
        node_id: node.id,
        workflow_id: workflow.id,
        role: node.role.clone(),
        exec: node.exec,
        critical: node.critical,
        n: attempt.n,
        base_sha: attempt.base_sha,
        pr_base_sha: None,
        tokens_total: attempt.tokens_reserved,
        wall_time_s: attempt.wall_time_s,
        model,
        upstream: node.depends_on.clone(),
        previous: node
            .attempts
            .iter()
            .filter(|a| a.id != attempt.id)
            .filter_map(|a| Some((a.id, a.failure_class?, a.failure_detail.clone())))
            .collect(),
        instructions: TrustedInstructions::compose(ROLE_TEMPLATE_SINGLE, &state.delivery.user_text),
        refs: Vec::new(),
        lock,
        resume,
    }
}

/// `Append(node.started)` + `Append(attempt.started)` + `budget.charged(attempts)`；提交后 `Effect(RunAttempt)`。
/// 前置：`may_dispatch`；节点可派发；并发闸 1（在途节点 < `limits.concurrency`）与 2（`sandbox_capacity > 0`）；
/// `exec = sandbox` 时持有有效锁；预留 > 0；`models` 有表项（§7.14.5）。
fn dispatch(state: &DeliveryState, env: &DecideEnv, mint: &mut IdMint, out: &mut Vec<Decision>) {
    if !state.may_dispatch() {
        return;
    }
    let Some(workflow) = current_workflow(state) else {
        return;
    };
    let Some(worktree) = &state.worktree else {
        return;
    };
    let running = running_nodes(state);
    for node in workflow.nodes.values() {
        if !dispatchable(node, env) {
            continue;
        }
        if running >= state.delivery.budget.limits.concurrency {
            return;
        }
        let sandbox = node.exec != gantry_core::entities::Exec::Direct;
        if sandbox && env.sandbox_capacity == 0 {
            return;
        }
        let lock = if node.exec == gantry_core::entities::Exec::Sandbox {
            if !holds_valid_lock(state, env, node) {
                continue;
            }
            state.lock_token(&worktree.id)
        } else {
            None
        };
        let Some(model) = model_for(env, state, node) else {
            continue;
        };
        let tokens = env.reservation.reserve(&ledger_view(state), node.tier, 1);
        if tokens == 0 {
            continue;
        }
        let wall = effective_wall_time(state, env, &model);
        let attempt_id = mint.next_id::<gantry_core::ids::Att>();
        let n = node.attempts.len() as u32 + 1;
        if node.status == NodeStatus::Queued {
            out.push(node_event(
                state,
                EventType::NodeStarted,
                node,
                NodeStatus::Running,
            ));
        }
        out.push(delivery_event(
            state,
            EventType::AttemptStarted,
            &attempt_id.to_string(),
            &gantry_core::events::AttemptStartedData {
                attempt_id,
                node_id: node.id,
                n,
                provider: model.provider_id.clone(),
                model: model.model.clone(),
                reasoning_effort: model.reasoning_effort,
                config_rev: env.settings.config_rev,
                base_sha: worktree.base_sha,
                tokens_total: tokens,
                wall_time_s: wall,
            },
        ));
        out.push(delivery_event(
            state,
            EventType::BudgetCharged,
            &state.delivery.id.to_string(),
            &BudgetChargedData {
                ledger: LedgerRef::Delivery {
                    id: state.delivery.id,
                },
                dimension: BudgetChargedDimension::Attempts,
                delta: 1,
                used_after: state.ledger.used.attempts + 1,
                source: BudgetChargeSource::Attempt { attempt_id },
                tier: None,
                usage: None,
                weighted_tokens: None,
                weight_audit: None,
            },
        ));
        let attempt = AttemptState {
            id: attempt_id,
            n,
            exec: node.exec,
            status: AttemptStatus::Running,
            base_sha: worktree.base_sha,
            tokens_reserved: tokens,
            wall_time_s: wall,
            config_rev: env.settings.config_rev,
            started_at: env.now,
            finished_at: None,
            failure_class: None,
            failure_detail: None,
            result_ref: None,
            result_tree: None,
            result_commit: None,
            accepted_open_pr: None,
            after_retry_request: n - node.attempts_at_retry,
        };
        out.push(Decision::Effect(Effect::RunAttempt(Box::new(
            attempt_plan(state, workflow, node, &attempt, model, lock, false),
        ))));
        // 每次求值只派发一个尝试：budget.charged 的 used_after 基于本轮的状态。
        return;
    }
}

/// `Effect(RunAttempt { resume: true })`：尝试有 `attempt.started` 无 `attempt.finished`（`Tick(Recover)`）。
fn resume_unfinished(state: &DeliveryState, env: &DecideEnv, out: &mut Vec<Decision>) {
    for workflow in state.workflows.values() {
        for node in workflow.nodes.values() {
            for attempt in node.attempts.iter().filter(|a| a.finished_at.is_none()) {
                if env.inflight.contains(&EffectKey::Run(attempt.id)) {
                    continue;
                }
                let Some(model) = model_for(env, state, node) else {
                    continue;
                };
                let lock = state
                    .worktree
                    .as_ref()
                    .and_then(|w| state.lock_token(&w.id))
                    .filter(|_| node.exec == gantry_core::entities::Exec::Sandbox);
                out.push(Decision::Effect(Effect::RunAttempt(Box::new(
                    attempt_plan(state, workflow, node, attempt, model, lock, true),
                ))));
            }
        }
    }
}
