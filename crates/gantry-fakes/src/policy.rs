//! `FakePolicy`：可脚本化的 `PolicyPort` 替身。
//!
//! 审批与策略状态保存在 `gantry_store::Store` 的 typed 模块快照里；补丁审批与
//! 执行审批的消费走调用方的 `Store::WriteTx`，并在同一事务里追加
//! `approval.consumed`。`new(now)` 自建 Store；`new_in(store)` 与其它替身共享。
//!
//! 消费语义是真实工作的：同一审批只能被同一主体消费一次；第二次得到
//! `AlreadyConsumedBySameAction` / `AlreadyConsumedBySameAttempt`，由别的
//! 主体消费得到 `ConsumedByOther`。校验永远先于状态判断：已消费的审批在主体
//! 或 head 变化后仍必须报 `HeadChanged` / `*Mismatch`，且已消费的重放不受
//! `expires_at` 影响。读与状态转移入口对 pending / granted 做惰性过期，但不
//! 伪造 `approval.expired` 事件。只读入口只在真的发生过期时写回，未过期的
//! `approval` / `live_approvals_for_pr` 不推进 Store revision。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::{RepoSlug, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind,
    PolicyActionId, Seq,
};
use gantry_core::events::{
    ApprovalConsumedData, ApprovalRequestedData, EventType, NewEvent, Subject,
};
use gantry_core::ids::{ActionId, ApprovalId, AttemptId, DeliveryId, ProjectId};
use gantry_core::ports::{
    ActionDecision, ActionRequest, ActorCtx, AddedLineSource, ApprovalSpec, DenyReason,
    Disposition, ExecutionCheck, ExecutionPermit, GateError, InvalidateReason, MismatchReason,
    PatchApprovalCheck, PatchPermit, PatchPolicyVerdict, PathRuleHit, PathRuleSet, PolicyError,
    PolicyPort, ProposalContext, ProposalDecision, ReportHint, RuleId, SecretFinding, StoreError,
    TxHandle,
};
use gantry_core::testing::FakeClock;
use gantry_core::time::Timestamp;
use gantry_core::worker::{PatchFacts, PatchVerdict, PathConstraints, Proposal, RepoPath};
use gantry_store::WriteTx;

use crate::{port_store_error, read_state, try_update_state};

const STATE_KEY: &str = "fakes.policy";
/// `forge.push.branch_prefix` 的 M0 默认值；fake 不新增配置。
const DEFAULT_BRANCH_PREFIX: &str = "gantry/";

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PolicyState {
    now: Timestamp,
    counter: u128,
    config_rev: Seq,
    decisions: BTreeMap<ForgeActionKind, ActionDecision>,
    proposals: BTreeMap<String, ProposalDecision>,
    patches: BTreeMap<AttemptId, PatchPolicyVerdict>,
    approvals: BTreeMap<ApprovalId, Approval>,
    paths: BTreeMap<ProjectId, PathConstraints>,
    secret_tokens: BTreeMap<String, String>,
}

impl PolicyState {
    fn new(now: Timestamp) -> Self {
        Self {
            now,
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

    /// 惰性过期：只清 pending / granted，不产生事件（fake 不模拟清扫任务）。
    /// 返回是否有状态变化，供只读入口决定是否写回。
    fn expire_due(&mut self) -> bool {
        let now = self.now;
        let mut changed = false;
        for approval in self.approvals.values_mut() {
            if approval.expires_at <= now
                && matches!(
                    approval.status,
                    ApprovalStatus::Pending | ApprovalStatus::Granted
                )
            {
                approval.status = ApprovalStatus::Expired;
                approval.updated_at = now;
                changed = true;
            }
        }
        changed
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
    store: gantry_store::Store,
    clock: Option<Arc<FakeClock>>,
}

impl std::fmt::Debug for FakePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("FakePolicy")
            .field("approvals", &state.approvals.len())
            .field("config_rev", &state.config_rev)
            .field("now", &state.now)
            .finish()
    }
}

impl FakePolicy {
    pub fn new(now: Timestamp) -> Self {
        let (store, clock) = crate::open_store(now);
        Self::from_parts(store, Some(clock), now)
    }

    /// 共享调用方的 Store；审批状态与事件/Outbox 事务落在同一个 Store。
    pub fn new_in(store: gantry_store::Store) -> Self {
        let now = store.clock_now();
        Self::from_parts(store, None, now)
    }

    fn from_parts(
        store: gantry_store::Store,
        clock: Option<Arc<FakeClock>>,
        now: Timestamp,
    ) -> Self {
        store
            .register_module_state(STATE_KEY, PolicyState::new(now))
            .expect("policy module state must register");
        Self { store, clock }
    }

    fn state(&self) -> PolicyState {
        read_state(&self.store, STATE_KEY).expect("policy module state must exist")
    }

    /// 读入口的惰性过期：没有状态变化时不写回，避免只读查询推进 Store revision。
    fn expiring_state(&self) -> PolicyState {
        let mut state = self.state();
        if !state.expire_due() {
            return state;
        }
        // 写回前在最新快照上重新判断；别的写者已经清过时不制造空写。
        let _ = try_update_state(&self.store, STATE_KEY, |latest: &mut PolicyState| {
            if latest.expire_due() {
                Ok::<(), ()>(())
            } else {
                Err(())
            }
        });
        self.state()
    }

    fn write_state<R>(&self, f: impl FnOnce(&mut PolicyState) -> R) -> R {
        crate::update_state(&self.store, STATE_KEY, f).expect("policy module state must exist")
    }

    pub fn set_now(&self, now: Timestamp) {
        if let Some(clock) = &self.clock {
            clock.set(now);
        }
        self.write_state(|state| state.now = now);
    }

    pub fn advance_ms(&self, ms: i64) {
        if let Some(clock) = &self.clock {
            clock.advance_ms(ms);
        }
        self.write_state(|state| {
            state.now = Timestamp::from_millis(state.now.as_millis().saturating_add(ms));
        });
    }

    pub fn set_config_rev(&self, rev: Seq) {
        self.write_state(|state| state.config_rev = rev);
    }

    pub fn script_decision(&self, kind: ForgeActionKind, decision: ActionDecision) {
        self.write_state(|state| {
            state.decisions.insert(kind, decision);
        });
    }

    pub fn script_proposal(&self, proposal_id: impl Into<String>, decision: ProposalDecision) {
        let proposal_id = proposal_id.into();
        self.write_state(|state| {
            state.proposals.insert(proposal_id, decision);
        });
    }

    pub fn script_patch_verdict(&self, attempt_id: AttemptId, verdict: PatchPolicyVerdict) {
        self.write_state(|state| {
            state.patches.insert(attempt_id, verdict);
        });
    }

    pub fn script_secret_token(&self, token: impl Into<String>, pattern: impl Into<String>) {
        let token = token.into();
        let pattern = pattern.into();
        self.write_state(|state| {
            state.secret_tokens.insert(token, pattern);
        });
    }

    pub fn set_path_rules(&self, project: ProjectId, constraints: PathConstraints) {
        self.write_state(|state| {
            state.paths.insert(project, constraints);
        });
    }

    pub fn approval(&self, id: &ApprovalId) -> Option<Approval> {
        self.expiring_state().approvals.get(id).cloned()
    }

    pub fn approvals_len(&self) -> usize {
        self.state().approvals.len()
    }

    /// 测试辅助：把 pending 审批置为 granted，不模拟签名。
    pub fn grant(&self, id: &ApprovalId) -> Result<Approval, PolicyError> {
        let result = try_update_state(&self.store, STATE_KEY, |state: &mut PolicyState| {
            state.expire_due();
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
        })
        .map_err(PolicyError::Store)?;
        result
    }

    /// 同步入口只给同步测试用；async 端口必须 `self.store.begin_write().await`，
    /// 否则单线程运行时里竞争写令牌会阻塞 runtime 线程而死锁。
    pub fn begin_write(&self) -> WriteTx {
        self.store
            .begin_write_sync()
            .expect("in-memory store write must start")
    }

    fn consume_patch_on(
        state: &mut PolicyState,
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
        // 入口先惰性过期未消费的审批；已消费的审批保留状态，重放仍按校验结果处理。
        if matches!(
            approval.status,
            ApprovalStatus::Pending | ApprovalStatus::Granted
        ) && approval.expires_at <= now
        {
            approval.status = ApprovalStatus::Expired;
            approval.updated_at = now;
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
        let consumed_by_same_attempt =
            consumed_by.as_deref() == Some(check.attempt_id.to_string().as_str());
        // 先看已消费的重放：即使 expires_at 已过，同一尝试仍可继续。
        match status {
            ApprovalStatus::Consumed if consumed_by_same_attempt => {
                Ok(PatchPermit::AlreadyConsumedBySameAttempt)
            }
            ApprovalStatus::Consumed => Err(GateError::ApprovalInvalid {
                reason: MismatchReason::Consumed,
            }),
            ApprovalStatus::Granted => {
                approval.status = ApprovalStatus::Consumed;
                approval.consumed_by = Some(check.attempt_id.to_string());
                approval.updated_at = now;
                Ok(PatchPermit::Consumed)
            }
            ApprovalStatus::Expired => Err(GateError::ApprovalInvalid {
                reason: MismatchReason::Expired,
            }),
            _ => Err(GateError::ApprovalInvalid {
                reason: MismatchReason::NotGranted,
            }),
        }
    }

    /// 校验与状态分支分离：先核对 action/target/subject/head，再决定是否消费。
    fn check_execution(approval: &Approval, check: &ExecutionCheck) -> Result<(), GateError> {
        let action = approval.action;
        let target = &approval.target;
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
                if !matches!(target, ApprovalTarget::Pr { repo: target_repo, number: target_number }
                    if target_repo == repo && target_number == number)
                {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    });
                }
                if approval.subject_digest != SubjectDigest::git(*expected_head_sha) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::DigestMismatch,
                    });
                }
                // 每次检查 live head：消费后的重放也不能绕过。
                if expected_head_sha != live_head_sha {
                    return Err(GateError::HeadChanged {
                        live_head_sha: *live_head_sha,
                    });
                }
            }
            ExecutionCheck::PushBranch {
                repo,
                branch,
                force,
                local_head_oid,
                expected_remote_oid,
                validation_ref: _,
            } => {
                let outside_prefix = !branch.starts_with(DEFAULT_BRANCH_PREFIX);
                let required = if *force {
                    ApprovalAction::ForcePush
                } else if outside_prefix {
                    ApprovalAction::PushOutsidePrefix
                } else {
                    ApprovalAction::Forge(ForgeActionKind::PushBranch)
                };
                if action != required {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::ActionMismatch,
                    });
                }
                let ApprovalTarget::Branch {
                    repo: target_repo,
                    branch: target_branch,
                    expected_remote_oid: target_remote,
                } = target
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
                if approval.subject_digest != SubjectDigest::git(*local_head_oid) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::DigestMismatch,
                    });
                }
                // 审批目标与执行检查必须带同一个远端前置；缺失不等于任意远端。
                if target_remote != expected_remote_oid {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::DigestMismatch,
                    });
                }
                // `validation_ref` 在 ApprovalTarget 里没有承载字段；不虚构绑定。
            }
            ExecutionCheck::Other {
                kind,
                target: target_str,
            } => {
                if action.shape().is_none() {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::ActionMismatch,
                    });
                }
                if action != ApprovalAction::Forge(*kind) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::ActionMismatch,
                    });
                }
                if action.shape().map(|shape| shape.target_kind) != Some(target.kind()) {
                    return Err(GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    });
                }
                let expected =
                    serde_json::to_value(target).map_err(|_| GateError::ApprovalInvalid {
                        reason: MismatchReason::TargetMismatch,
                    })?;
                let actual =
                    serde_json::from_str::<serde_json::Value>(target_str).map_err(|_| {
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
        Ok(())
    }

    async fn consume_execution_on(
        tx: &mut WriteTx,
        approval_id: &ApprovalId,
        check: &ExecutionCheck,
        act: &ActionId,
        ctx: &ActorCtx,
    ) -> Result<ExecutionPermit, GateError> {
        let mut state = tx
            .module_state::<PolicyState>(STATE_KEY)
            .map_err(|error| GateError::Store(port_store_error(error)))?
            .ok_or(GateError::Store(StoreError::NotFound))?;
        let now = state.now;
        let approval =
            state
                .approvals
                .get(approval_id)
                .cloned()
                .ok_or(GateError::ApprovalInvalid {
                    reason: MismatchReason::NotGranted,
                })?;
        // 入口先惰性过期未消费的审批（含仍 pending 的）；已消费的审批保留状态，
        // 让重放先完成 action/target/head 校验。
        if matches!(
            approval.status,
            ApprovalStatus::Pending | ApprovalStatus::Granted
        ) && approval.expires_at <= now
        {
            if let Some(entry) = state.approvals.get_mut(approval_id) {
                entry.status = ApprovalStatus::Expired;
                entry.updated_at = now;
            }
            tx.update_module_state(STATE_KEY, |current: &mut PolicyState| *current = state)
                .map_err(|error| GateError::Store(port_store_error(error)))?;
            return Err(GateError::ApprovalInvalid {
                reason: MismatchReason::Expired,
            });
        }
        Self::check_execution(&approval, check)?;
        match approval.status {
            ApprovalStatus::Granted => {}
            ApprovalStatus::Consumed => {
                if approval.consumed_by.as_deref() == Some(act.to_string().as_str()) {
                    return Ok(ExecutionPermit::AlreadyConsumedBySameAction);
                }
                return match approval
                    .consumed_by
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
        let subject_digest = approval.subject_digest;
        let consumed_by = act.to_string();
        let data = ApprovalConsumedData {
            approval_id: *approval_id,
            consumed_by: consumed_by.clone(),
            subject_digest,
        };
        let event = NewEvent {
            ty: EventType::ApprovalConsumed,
            subject: Subject::parse(&approval_id.to_string()).map_err(|_| {
                GateError::Store(StoreError::Rejected {
                    message: "approval id is not a valid subject".into(),
                })
            })?,
            project_id: None,
            actor: Actor::new(ActorKind::System, "policy"),
            causation_id: ctx.causation_id,
            correlation_id: None,
            data: serde_json::to_value(&data).map_err(|_| {
                GateError::Store(StoreError::Rejected {
                    message: "approval.consumed data must serialize".into(),
                })
            })?,
            dedup_key: None,
        };
        tx.append(event)
            .await
            .map_err(|error| GateError::Store(port_store_error(error)))?;
        let entry = state
            .approvals
            .get_mut(approval_id)
            .ok_or(GateError::Store(StoreError::NotFound))?;
        entry.status = ApprovalStatus::Consumed;
        entry.consumed_by = Some(consumed_by);
        entry.updated_at = now;
        tx.update_module_state(STATE_KEY, |current: &mut PolicyState| *current = state)
            .map_err(|error| GateError::Store(port_store_error(error)))?;
        Ok(ExecutionPermit::Consumed)
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
        let state = self.state();
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
        ctx: ActorCtx,
    ) -> Result<Approval, PolicyError> {
        let mut tx = self
            .store
            .begin_write()
            .await
            .map_err(|error| PolicyError::Store(port_store_error(error)))?;
        let mut state = tx
            .module_state::<PolicyState>(STATE_KEY)
            .map_err(|error| PolicyError::Store(port_store_error(error)))?
            .ok_or(PolicyError::Store(StoreError::NotFound))?;
        let expired = state.expire_due();
        if let Some(existing) = state
            .approvals
            .values()
            .find(|approval| {
                approval.status == ApprovalStatus::Pending
                    && approval.action == spec.action
                    && approval.target == spec.target
                    && approval.subject_digest == spec.subject_digest
                    && approval.delivery_id == spec.delivery_id
            })
            .cloned()
        {
            if expired {
                tx.update_module_state(STATE_KEY, |current: &mut PolicyState| *current = state)
                    .map_err(|error| PolicyError::Store(port_store_error(error)))?;
                tx.commit()
                    .await
                    .map_err(|error| PolicyError::Store(port_store_error(error)))?;
            }
            return Ok(existing);
        }

        let now = state.now;
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
        let data = ApprovalRequestedData {
            action: approval.action,
            target: approval.target.clone(),
            subject_digest: approval.subject_digest,
            expires_at: approval.expires_at,
            config_rev: state.config_rev,
            requested_by: ctx.actor.clone(),
        };
        let event = NewEvent {
            ty: EventType::ApprovalRequested,
            subject: Subject::parse(&id.to_string()).map_err(|_| {
                PolicyError::Store(StoreError::Rejected {
                    message: "approval id is not a valid subject".into(),
                })
            })?,
            project_id: None,
            actor: ctx.actor,
            causation_id: ctx.causation_id,
            correlation_id: approval.delivery_id.map(|id| id.to_string()),
            data: serde_json::to_value(&data).map_err(|_| {
                PolicyError::Store(StoreError::Rejected {
                    message: "approval.requested data must serialize".into(),
                })
            })?,
            dedup_key: None,
        };
        tx.append(event)
            .await
            .map_err(|error| PolicyError::Store(port_store_error(error)))?;
        state.approvals.insert(id, approval.clone());
        tx.update_module_state(STATE_KEY, |current: &mut PolicyState| *current = state)
            .map_err(|error| PolicyError::Store(port_store_error(error)))?;
        tx.commit()
            .await
            .map_err(|error| PolicyError::Store(port_store_error(error)))?;
        Ok(approval)
    }

    async fn invalidate(
        &self,
        id: &ApprovalId,
        _reason: InvalidateReason,
        _ctx: &ActorCtx,
    ) -> Result<Approval, PolicyError> {
        let result = try_update_state(&self.store, STATE_KEY, |state: &mut PolicyState| {
            state.expire_due();
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
        })
        .map_err(PolicyError::Store)?;
        result
    }

    async fn live_approvals_for_pr(
        &self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Vec<Approval>, PolicyError> {
        let state = self.expiring_state();
        Ok(state
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
            .collect::<Vec<_>>())
    }

    async fn decide_proposals(
        &self,
        _proposal_ctx: ProposalContext,
        items: &[Proposal],
        _ctx: &ActorCtx,
    ) -> Result<Vec<ProposalDecision>, PolicyError> {
        let state = self.state();
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
        let state = self.state();
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
        ctx: &ActorCtx,
    ) -> Result<PatchPermit, GateError> {
        let tx = tx
            .as_any_mut()
            .downcast_mut::<WriteTx>()
            .ok_or_else(|| GateError::Store(policy_tx_required()))?;
        if !tx.same_store(&self.store) {
            return Err(GateError::Store(StoreError::Rejected {
                message: "WriteTx belongs to another Store".into(),
            }));
        }
        let mut state = tx
            .module_state::<PolicyState>(STATE_KEY)
            .map_err(|error| GateError::Store(port_store_error(error)))?
            .ok_or(GateError::Store(StoreError::NotFound))?;
        let permit = match Self::consume_patch_on(&mut state, approval, check) {
            Ok(permit) => permit,
            Err(error) => {
                // 惰性过期可能已经改了快照；错误返回也必须把它写回调用方事务。
                tx.update_module_state(STATE_KEY, |current: &mut PolicyState| *current = state)
                    .map_err(|error| GateError::Store(port_store_error(error)))?;
                return Err(error);
            }
        };
        if permit != PatchPermit::Consumed {
            return Ok(permit);
        }
        let consumed_by = check.attempt_id.to_string();
        let subject_digest = SubjectDigest::sha256(check.patch_sha256);
        let data = ApprovalConsumedData {
            approval_id: *approval,
            consumed_by,
            subject_digest,
        };
        let event = NewEvent {
            ty: EventType::ApprovalConsumed,
            subject: Subject::parse(&approval.to_string()).map_err(|_| {
                GateError::Store(StoreError::Rejected {
                    message: "approval id is not a valid subject".into(),
                })
            })?,
            project_id: None,
            actor: Actor::new(ActorKind::System, "policy"),
            causation_id: ctx.causation_id,
            correlation_id: None,
            data: serde_json::to_value(&data).map_err(|_| {
                GateError::Store(StoreError::Rejected {
                    message: "approval.consumed data must serialize".into(),
                })
            })?,
            dedup_key: None,
        };
        tx.append(event)
            .await
            .map_err(|error| GateError::Store(port_store_error(error)))?;
        tx.update_module_state(STATE_KEY, |current: &mut PolicyState| *current = state)
            .map_err(|error| GateError::Store(port_store_error(error)))?;
        Ok(PatchPermit::Consumed)
    }

    fn path_rules(&self, project: &ProjectId) -> Arc<dyn PathRuleSet> {
        let constraints = self
            .state()
            .paths
            .get(project)
            .cloned()
            .unwrap_or_else(empty_paths);
        Arc::new(FakePathRules { constraints })
    }

    fn path_rules_snapshot(&self, project: &ProjectId) -> PathConstraints {
        self.state()
            .paths
            .get(project)
            .cloned()
            .unwrap_or_else(empty_paths)
    }

    fn scan_text(&self, text: &str) -> Vec<SecretFinding> {
        self.state()
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
        ctx: &ActorCtx,
    ) -> Result<ExecutionPermit, GateError> {
        let mut tx = self
            .store
            .begin_write()
            .await
            .map_err(|error| GateError::Store(port_store_error(error)))?;
        let permit = Self::consume_execution_on(&mut tx, approval, check, act, ctx).await?;
        tx.commit()
            .await
            .map_err(|error| GateError::Store(port_store_error(error)))?;
        Ok(permit)
    }
}

fn policy_tx_required() -> StoreError {
    StoreError::Rejected {
        message: "FakePolicy::consume_patch_approval requires a Store WriteTx".into(),
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

/// 编译期检查：状态快照必须可克隆（Store 的 testing 模块状态要求）。
#[allow(dead_code)]
fn _state_anchor(state: PolicyState) -> PolicyState {
    state.clone()
}
