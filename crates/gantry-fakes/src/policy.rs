//! `FakePolicy`：可脚本化的 `PolicyPort` 替身。
//!
//! 审批只保存在内存里；`grant` 是测试辅助方法，不模拟签名验签。消费语义是
//! 真实工作的：同一审批只能被同一主体消费一次；第二次得到
//! `AlreadyConsumedBySameAction` / `AlreadyConsumedBySameAttempt`，由别的
//! 主体消费得到 `ConsumedByOther`。补丁审批的消费走 `FakePolicyTx`，因此
//! 调用方事务回滚时审批保持原状。

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::{RepoSlug, SubjectDigest};
use gantry_core::entities::{
    Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind, PolicyActionId, Seq,
};
use gantry_core::ids::{ActionId, ApprovalId, AttemptId, DeliveryId, ProjectId};
use gantry_core::ports::{
    ActionDecision, ActionRequest, ActorCtx, AddedLineSource, ApprovalSpec, DenyReason,
    Disposition, ExecutionCheck, ExecutionPermit, GateError, InvalidateReason, MismatchReason,
    PatchApprovalCheck, PatchPermit, PatchPolicyVerdict, PathRuleHit, PathRuleSet, PolicyError,
    PolicyPort, ProposalContext, ProposalDecision, ReportHint, RuleId, SecretFinding, StoreError,
    TxHandle,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::{PatchFacts, PatchVerdict, PathConstraints, Proposal, RepoPath};

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PolicySnapshot {
    now: Timestamp,
    generation: u64,
    counter: u128,
    config_rev: Seq,
    decisions: BTreeMap<ForgeActionKind, ActionDecision>,
    proposals: BTreeMap<String, ProposalDecision>,
    patches: BTreeMap<AttemptId, PatchPolicyVerdict>,
    approvals: BTreeMap<ApprovalId, Approval>,
    paths: BTreeMap<ProjectId, PathConstraints>,
    secret_tokens: BTreeMap<String, String>,
}

impl PolicySnapshot {
    fn new(now: Timestamp) -> Self {
        Self {
            now,
            generation: 0,
            counter: 0,
            config_rev: Seq::ZERO,
            decisions: BTreeMap::new(),
            proposals: BTreeMap::new(),
            patches: BTreeMap::new(),
            approvals: BTreeMap::new(),
            paths: BTreeMap::new(),
            secret_tokens: BTreeMap::new(),
        }
    }

    fn next_id<K: gantry_core::ids::IdKind>(&mut self) -> gantry_core::ids::Id<K> {
        self.counter += 1;
        let ms = self.now.as_millis().max(0) as u64;
        gantry_core::ids::Id::from_parts(ms, self.counter)
            .expect("fake policy counter must fit in 80 bits")
    }
}

fn expires_at(now: Timestamp, ttl: Duration) -> Timestamp {
    Timestamp::from_millis(now.as_millis().saturating_add(ttl.as_millis() as i64))
}

// ---------------------------------------------------------------------------
// FakePolicy
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct FakePolicy {
    state: Arc<Mutex<PolicySnapshot>>,
}

impl std::fmt::Debug for FakePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("FakePolicy")
            .field("approvals", &state.approvals.len())
            .field("config_rev", &state.config_rev)
            .field("now", &state.now)
            .finish()
    }
}

impl FakePolicy {
    pub fn new(now: Timestamp) -> Self {
        Self {
            state: Arc::new(Mutex::new(PolicySnapshot::new(now))),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PolicySnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_now(&self, now: Timestamp) {
        self.lock().now = now;
    }

    pub fn advance_ms(&self, ms: i64) {
        let mut state = self.lock();
        state.now = Timestamp::from_millis(state.now.as_millis().saturating_add(ms));
    }

    pub fn set_config_rev(&self, rev: Seq) {
        self.lock().config_rev = rev;
    }

    pub fn script_decision(&self, kind: ForgeActionKind, decision: ActionDecision) {
        self.lock().decisions.insert(kind, decision);
    }

    pub fn script_proposal(&self, proposal_id: impl Into<String>, decision: ProposalDecision) {
        self.lock().proposals.insert(proposal_id.into(), decision);
    }

    pub fn script_patch_verdict(&self, attempt_id: AttemptId, verdict: PatchPolicyVerdict) {
        self.lock().patches.insert(attempt_id, verdict);
    }

    pub fn script_secret_token(&self, token: impl Into<String>, pattern: impl Into<String>) {
        self.lock()
            .secret_tokens
            .insert(token.into(), pattern.into());
    }

    pub fn set_path_rules(&self, project: ProjectId, constraints: PathConstraints) {
        self.lock().paths.insert(project, constraints);
    }

    pub fn approval(&self, id: &ApprovalId) -> Option<Approval> {
        self.lock().approvals.get(id).cloned()
    }

    pub fn approvals_len(&self) -> usize {
        self.lock().approvals.len()
    }

    /// 测试辅助：把 pending 审批置为 granted，不模拟签名。
    pub fn grant(&self, id: &ApprovalId) -> Result<Approval, PolicyError> {
        let mut state = self.lock();
        let now = state.now;
        let approval = state.approvals.get_mut(id).ok_or(PolicyError::NotFound)?;
        if approval.status != ApprovalStatus::Pending {
            return Err(PolicyError::Conflict {
                message: format!("approval {id} is {:?}", approval.status),
            });
        }
        approval.status = ApprovalStatus::Granted;
        approval.signature = Some("fake-signature".into());
        approval.updated_at = now;
        Ok(approval.clone())
    }

    pub fn begin_write(&self) -> FakePolicyTx {
        let snapshot = self.lock().clone();
        FakePolicyTx {
            policy: self.clone(),
            snapshot,
        }
    }

    fn consume_patch_on(
        state: &mut PolicySnapshot,
        approval_id: &ApprovalId,
        check: &PatchApprovalCheck,
    ) -> Result<PatchPermit, GateError> {
        let now = state.now;
        let approval = state
            .approvals
            .get_mut(approval_id)
            .ok_or(GateError::ApprovalInvalid {
                reason: MismatchReason::NotGranted,
            })?;
        if approval.expires_at <= now {
            return Err(GateError::ApprovalInvalid {
                reason: MismatchReason::Expired,
            });
        }
        let action = approval.action;
        let target = approval.target.clone();
        let subject = approval.subject_digest;
        let status = approval.status;
        let consumed_by = approval.consumed_by.clone();
        let ApprovalTarget::Patch {
            repo,
            attempt_id,
            rules,
            paths,
        } = target
        else {
            return Err(GateError::ApprovalInvalid {
                reason: MismatchReason::TargetMismatch,
            });
        };
        if action != ApprovalAction::PatchApplyProtected {
            return Err(GateError::ApprovalInvalid {
                reason: MismatchReason::ActionMismatch,
            });
        }
        if repo != check.repo
            || attempt_id != check.attempt_id
            || !check
                .rules
                .iter()
                .all(|rule| rules.iter().any(|target| target == rule.as_str()))
            || !check
                .paths
                .iter()
                .all(|path| paths.iter().any(|target| target == path.as_str()))
        {
            return Err(GateError::ApprovalInvalid {
                reason: MismatchReason::TargetMismatch,
            });
        }
        if subject != SubjectDigest::sha256(check.patch_sha256) {
            return Err(GateError::ApprovalInvalid {
                reason: MismatchReason::DigestMismatch,
            });
        }
        match status {
            ApprovalStatus::Granted => {
                approval.status = ApprovalStatus::Consumed;
                approval.consumed_by = Some(check.attempt_id.to_string());
                approval.updated_at = now;
                Ok(PatchPermit::Consumed)
            }
            ApprovalStatus::Consumed
                if consumed_by.as_deref() == Some(check.attempt_id.to_string().as_str()) =>
            {
                Ok(PatchPermit::AlreadyConsumedBySameAttempt)
            }
            ApprovalStatus::Consumed => Err(GateError::ApprovalInvalid {
                reason: MismatchReason::Consumed,
            }),
            ApprovalStatus::Expired => Err(GateError::ApprovalInvalid {
                reason: MismatchReason::Expired,
            }),
            _ => Err(GateError::ApprovalInvalid {
                reason: MismatchReason::NotGranted,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// 事务句柄
// ---------------------------------------------------------------------------

pub struct FakePolicyTx {
    policy: FakePolicy,
    snapshot: PolicySnapshot,
}

impl FakePolicyTx {
    pub async fn commit(mut self) -> Result<(), StoreError> {
        let mut shared = self.policy.lock();
        if shared.generation != self.snapshot.generation {
            return Err(StoreError::Conflict {
                code: "fake_policy_tx_conflict",
            });
        }
        self.snapshot.generation += 1;
        *shared = self.snapshot;
        Ok(())
    }

    pub async fn rollback(self) -> Result<(), StoreError> {
        Ok(())
    }
}

impl TxHandle for FakePolicyTx {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn policy_tx_required() -> StoreError {
    StoreError::Rejected {
        message: "FakePolicy::consume_patch_approval requires a FakePolicyTx".into(),
    }
}

// ---------------------------------------------------------------------------
// PolicyPort
// ---------------------------------------------------------------------------

#[async_trait]
impl PolicyPort for FakePolicy {
    async fn decide_action(
        &self,
        req: ActionRequest,
        _ctx: &ActorCtx,
    ) -> Result<ActionDecision, PolicyError> {
        let state = self.lock();
        if let Some(decision) = state.decisions.get(&req.kind) {
            return Ok(decision.clone());
        }
        Ok(ActionDecision::Allow {
            action_id: PolicyActionId::from(req.kind),
            approval: None,
            config_rev: state.config_rev,
        })
    }

    async fn request_approval(
        &self,
        spec: ApprovalSpec,
        _ctx: ActorCtx,
    ) -> Result<Approval, PolicyError> {
        let mut state = self.lock();
        let now = state.now;
        if let Some(existing) = state.approvals.values().find(|approval| {
            approval.status == ApprovalStatus::Pending
                && approval.action == spec.action
                && approval.target == spec.target
                && approval.subject_digest == spec.subject_digest
                && approval.delivery_id == spec.delivery_id
        }) {
            return Ok(existing.clone());
        }
        let id: ApprovalId = state.next_id();
        let approval = Approval {
            id,
            delivery_id: spec.delivery_id,
            action: spec.action,
            target: spec.target,
            subject_digest: spec.subject_digest,
            device_id: None,
            signature: None,
            status: ApprovalStatus::Pending,
            expires_at: expires_at(now, spec.ttl),
            consumed_by: None,
            created_at: now,
            updated_at: now,
        };
        approval
            .validate()
            .map_err(|error| PolicyError::InvalidRequest {
                pointer: "/spec".into(),
                message: error.to_string(),
            })?;
        state.approvals.insert(id, approval.clone());
        Ok(approval)
    }

    async fn invalidate(
        &self,
        id: &ApprovalId,
        _reason: InvalidateReason,
        _ctx: &ActorCtx,
    ) -> Result<Approval, PolicyError> {
        let mut state = self.lock();
        let now = state.now;
        let approval = state.approvals.get_mut(id).ok_or(PolicyError::NotFound)?;
        match approval.status {
            ApprovalStatus::Pending | ApprovalStatus::Granted => {
                approval.status = ApprovalStatus::Invalidated;
                approval.updated_at = now;
                Ok(approval.clone())
            }
            status => Err(PolicyError::Conflict {
                message: format!("approval {id} is {status:?}"),
            }),
        }
    }

    async fn live_approvals_for_pr(
        &self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Vec<Approval>, PolicyError> {
        Ok(self
            .lock()
            .approvals
            .values()
            .filter(|approval| {
                let same_pr = match &approval.target {
                    ApprovalTarget::Pr {
                        repo: target_repo,
                        number: target_number,
                    } => target_repo == repo && *target_number == number,
                    _ => false,
                };
                same_pr
                    && matches!(
                        approval.status,
                        ApprovalStatus::Pending | ApprovalStatus::Granted
                    )
            })
            .cloned()
            .collect())
    }

    async fn decide_proposals(
        &self,
        _proposal_ctx: ProposalContext,
        items: &[Proposal],
        _ctx: &ActorCtx,
    ) -> Result<Vec<ProposalDecision>, PolicyError> {
        let state = self.lock();
        Ok(items
            .iter()
            .map(|item| {
                state.proposals.get(&item.id).cloned().unwrap_or_else(|| {
                    ProposalDecision::Accepted {
                        proposal_id: item.id.clone(),
                        action_id: None,
                    }
                })
            })
            .collect())
    }

    async fn evaluate_patch(
        &self,
        facts: &PatchFacts,
        _added: &dyn AddedLineSource,
        _ctx: &ActorCtx,
    ) -> Result<PatchPolicyVerdict, PolicyError> {
        let state = self.lock();
        if let Some(verdict) = state.patches.get(&facts.attempt_id) {
            return Ok(verdict.clone());
        }
        Ok(PatchPolicyVerdict {
            verdict: PatchVerdict::Allow,
            reasons: Vec::new(),
            matched_approval_rules: Vec::new(),
            report: None,
            config_rev: state.config_rev,
        })
    }

    async fn consume_patch_approval(
        &self,
        tx: &mut dyn TxHandle,
        approval: &ApprovalId,
        check: &PatchApprovalCheck,
        _ctx: &ActorCtx,
    ) -> Result<PatchPermit, GateError> {
        let tx = tx
            .as_any_mut()
            .downcast_mut::<FakePolicyTx>()
            .ok_or_else(|| GateError::Store(policy_tx_required()))?;
        if !Arc::ptr_eq(&tx.policy.state, &self.state) {
            return Err(GateError::Store(StoreError::Rejected {
                message: "FakePolicyTx belongs to another FakePolicy".into(),
            }));
        }
        FakePolicy::consume_patch_on(&mut tx.snapshot, approval, check)
    }

    fn path_rules(&self, project: &ProjectId) -> Arc<dyn PathRuleSet> {
        let constraints = self
            .lock()
            .paths
            .get(project)
            .cloned()
            .unwrap_or_else(empty_paths);
        Arc::new(FakePathRules { constraints })
    }

    fn path_rules_snapshot(&self, project: &ProjectId) -> PathConstraints {
        self.lock()
            .paths
            .get(project)
            .cloned()
            .unwrap_or_else(empty_paths)
    }

    fn scan_text(&self, text: &str) -> Vec<SecretFinding> {
        self.lock()
            .secret_tokens
            .iter()
            .filter(|(token, _)| text.contains(token.as_str()))
            .map(|(_, pattern)| SecretFinding {
                path: None,
                line: None,
                pattern: pattern.clone(),
            })
            .collect()
    }

    async fn consume_for_execution(
        &self,
        approval: &ApprovalId,
        check: &ExecutionCheck,
        act: &ActionId,
        _ctx: &ActorCtx,
    ) -> Result<ExecutionPermit, GateError> {
        let mut state = self.lock();
        let now = state.now;
        let approval = state
            .approvals
            .get_mut(approval)
            .ok_or(GateError::ApprovalInvalid {
                reason: MismatchReason::NotGranted,
            })?;
        if approval.expires_at <= now {
            return Err(GateError::ApprovalInvalid {
                reason: MismatchReason::Expired,
            });
        }
        let status = approval.status;
        let action = approval.action;
        let target = approval.target.clone();
        let subject = approval.subject_digest;
        let consumed_by = approval.consumed_by.clone();
        match status {
            ApprovalStatus::Granted => {}
            ApprovalStatus::Consumed => {
                if consumed_by.as_deref() == Some(act.to_string().as_str()) {
                    return Ok(ExecutionPermit::AlreadyConsumedBySameAction);
                }
                return match consumed_by
                    .as_deref()
                    .and_then(|by| by.parse::<ActionId>().ok())
                {
                    Some(by) => Err(GateError::ConsumedByOther { by }),
                    None => Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::Consumed,
                    }),
                };
            }
            ApprovalStatus::Pending | ApprovalStatus::Denied | ApprovalStatus::Invalidated => {
                return Err(GateError::ApprovalInvalid {
                    reason: MismatchReason::NotGranted,
                })
            }
            ApprovalStatus::Expired => {
                return Err(GateError::ApprovalInvalid {
                    reason: MismatchReason::Expired,
                })
            }
        }
        match check {
            ExecutionCheck::MergePr {
                repo,
                number,
                expected_head_sha,
                live_head_sha,
            } => {
                if action != ApprovalAction::Forge(ForgeActionKind::MergePr) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::ActionMismatch,
                    });
                }
                if !matches!(&target, ApprovalTarget::Pr { repo: target_repo, number: target_number }
                    if target_repo == repo && target_number == number)
                {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    });
                }
                if subject != SubjectDigest::git(*expected_head_sha) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::DigestMismatch,
                    });
                }
                if expected_head_sha != live_head_sha {
                    return Err(GateError::HeadChanged {
                        live_head_sha: *live_head_sha,
                    });
                }
            }
            ExecutionCheck::PushBranch {
                repo,
                branch,
                expected_remote_oid,
                ..
            } => {
                if !matches!(
                    action,
                    ApprovalAction::Forge(ForgeActionKind::PushBranch)
                        | ApprovalAction::ForcePush
                        | ApprovalAction::PushOutsidePrefix
                ) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::ActionMismatch,
                    });
                }
                let ApprovalTarget::Branch {
                    repo: target_repo,
                    branch: target_branch,
                    expected_remote_oid: target_remote,
                } = &target
                else {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    });
                };
                if target_repo != repo || target_branch != branch {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    });
                }
                if target_remote.is_some() && target_remote != expected_remote_oid {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::DigestMismatch,
                    });
                }
                if !matches!(subject, SubjectDigest::Git(_)) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::DigestMismatch,
                    });
                }
            }
            ExecutionCheck::Other { kind, target } => {
                if action != ApprovalAction::Forge(*kind) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::ActionMismatch,
                    });
                }
                let expected =
                    serde_json::to_value(target).map_err(|_| GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    })?;
                let actual = serde_json::from_str::<serde_json::Value>(target).map_err(|_| {
                    GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    }
                })?;
                if expected != actual {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    });
                }
            }
        }
        approval.status = ApprovalStatus::Consumed;
        approval.consumed_by = Some(act.to_string());
        approval.updated_at = now;
        Ok(ExecutionPermit::Consumed)
    }
}

fn empty_paths() -> PathConstraints {
    PathConstraints {
        allow: Vec::new(),
        deny: Vec::new(),
        approval_required: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 路径规则
// ---------------------------------------------------------------------------

pub struct FakePathRules {
    constraints: PathConstraints,
}

impl PathRuleSet for FakePathRules {
    fn evaluate(&self, paths: &[RepoPath]) -> Vec<PathRuleHit> {
        paths
            .iter()
            .map(|path| {
                let deny = self
                    .constraints
                    .deny
                    .iter()
                    .find(|pattern| pattern_matches(pattern, path));
                let approve = self
                    .constraints
                    .approval_required
                    .iter()
                    .find(|pattern| pattern_matches(pattern, path));
                let allow = self
                    .constraints
                    .allow
                    .iter()
                    .find(|pattern| pattern_matches(pattern, path));
                let (rule_id, disposition) = if let Some(pattern) = deny {
                    (Some(RuleId::new(pattern.clone())), Disposition::Deny)
                } else if let Some(pattern) = approve {
                    (Some(RuleId::new(pattern.clone())), Disposition::Approve)
                } else if let Some(pattern) = allow {
                    (Some(RuleId::new(pattern.clone())), Disposition::Auto)
                } else {
                    (None, Disposition::Auto)
                };
                PathRuleHit {
                    path: path.clone(),
                    rule_id,
                    disposition,
                    outside_allow: !self.constraints.allow.is_empty() && allow.is_none(),
                }
            })
            .collect()
    }
}

// shortcut: 只支持精确匹配与末尾 `*` 前缀；策略规则语法冻结后再扩展。
fn pattern_matches(pattern: &str, path: &RepoPath) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => path.as_str().starts_with(prefix),
        None => path.as_str() == pattern,
    }
}

// ---------------------------------------------------------------------------
// 一致性用例
// ---------------------------------------------------------------------------

/// 动作判定必须可重复调用且结果稳定；`scan_text` 对探针文本不应有命中。
///
/// 前置条件：实现的 `scan_text` 不把 `"conformance-probe"` 当作密钥。
pub async fn action_conformance(
    port: &dyn PolicyPort,
    req: ActionRequest,
    ctx: &ActorCtx,
) -> Result<(), String> {
    let first = port
        .decide_action(req.clone(), ctx)
        .await
        .map_err(|error| format!("decide_action failed: {error}"))?;
    let second = port
        .decide_action(req, ctx)
        .await
        .map_err(|error| format!("second decide_action failed: {error}"))?;
    if first != second {
        return Err("decide_action must be stable for the same request".into());
    }
    if !port.scan_text("conformance-probe").is_empty() {
        return Err("scan_text must not flag the conformance probe".into());
    }
    Ok(())
}

/// 执行审批只能被同一动作消费一次。
///
/// 前置条件：`approval` 已 granted 且未过期，`check` 与它匹配。
pub async fn execution_conformance(
    port: &dyn PolicyPort,
    approval: &ApprovalId,
    check: &ExecutionCheck,
    act: &ActionId,
    ctx: &ActorCtx,
) -> Result<(), String> {
    let first = port
        .consume_for_execution(approval, check, act, ctx)
        .await
        .map_err(|error| format!("first consume failed: {error}"))?;
    if first != ExecutionPermit::Consumed {
        return Err("first execution must consume the approval".into());
    }
    let second = port
        .consume_for_execution(approval, check, act, ctx)
        .await
        .map_err(|error| format!("second consume failed: {error}"))?;
    if second != ExecutionPermit::AlreadyConsumedBySameAction {
        return Err("the same action must see AlreadyConsumedBySameAction".into());
    }
    Ok(())
}

/// 未使用的字段类型在编译期被引用，提醒保持与端口一致。
#[allow(dead_code)]
fn _type_anchor(_: Option<DenyReason>, _: Option<ReportHint>, _: Option<DeliveryId>) {}
