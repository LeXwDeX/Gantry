//! `PolicyEngine`：`core::ports::PolicyPort` 的实现（policy.md §6、§7.3–§7.6）。
//!
//! 无进程内可变状态，除了可原子替换的规则快照；审批全部经 `store` 的事件读写。
//! 一次判定从头到尾只用同一份规则快照（config.md §2.6）。

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use gantry_core::canon::{RepoSlug, SubjectDigest};
use gantry_core::entities::{
    Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind, InboxKind,
    PolicyActionId, ReasonCode, Severity,
};
use gantry_core::events::{
    ApprovalRequestedData, EventType, NewEvent, OutboxEnqueuedData, PatchValidatedData,
    PolicySubject, PolicyTarget, Subject,
};
use gantry_core::ids::{ActionId, ApprovalId, Apr, Clock, IdGen, ProjectId};
use gantry_core::ports::{
    ActionDecision, ActionFacts, ActionRequest, ActorCtx, AddedLineSource, ApprovalSpec,
    DenyReason, Disposition, ExecutionCheck, ExecutionPermit, ForgeReader, GateError,
    InvalidateReason, MismatchReason, PatchApprovalCheck, PatchPermit, PatchPolicyVerdict,
    PathRuleSet, PathRulesSnapshot, PolicyError, PolicyPort, ProposalContext, ProposalDecision,
    ReportHint, SecretFinding, StoreError, TxHandle,
};
use gantry_core::worker::{PatchFacts, PatchVerdict, Proposal, ProposalKind};
use gantry_store::{AppendOpts, EventFilter, Store, TypePattern, WriteTx};

use crate::approvals::{self, port_error, Record};
use crate::audit::{self, ActionDenial, ProposalDenial};
use crate::paths::{self, RulePaths};
use crate::rules::{PolicyBuild, PolicyRules};
use crate::secrets;

/// `forge.push.branch_prefix` 的出厂默认（config.md §5.1）；`ActionFacts` 自带前缀时以它为准。
const DEFAULT_BRANCH_PREFIX: &str = "gantry/";
/// `merge_pr` 被处置为 deny 时的上报码（data-model 上报词表、engine.md §7.14）。
const REPORT_MERGE_DENIED: &str = "merge_denied_by_policy";
const REPORT_SHAPE_UNDEFINED: &str = "approval_shape_undefined";
const REPORT_ACTION_DENIED_PREFIX: &str = "policy.action_denied:";

/// 装配依赖。`forge` 只用来读 PR 快照（当前 head）与项目的仓库，不写 GitHub。
pub struct PolicyDeps {
    pub store: Store,
    pub forge: Arc<dyn ForgeReader>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<IdGen>,
}

pub struct PolicyEngine {
    pub(crate) store: Store,
    pub(crate) forge: Arc<dyn ForgeReader>,
    pub(crate) clock: Arc<dyn Clock>,
    ids: Arc<IdGen>,
    build: PolicyBuild,
    rules: RwLock<Arc<PolicyRules>>,
}

impl PolicyEngine {
    /// M1：内置规则（`PolicyBuild::M1`），不读 `policies` 设置。
    pub fn new(deps: PolicyDeps) -> Self {
        Self::with_rules(deps, PolicyBuild::M1, PolicyRules::builtin())
    }

    pub fn with_rules(deps: PolicyDeps, build: PolicyBuild, rules: PolicyRules) -> Self {
        Self {
            store: deps.store,
            forge: deps.forge,
            clock: deps.clock,
            ids: deps.ids,
            build,
            rules: RwLock::new(Arc::new(rules)),
        }
    }

    pub fn build(&self) -> PolicyBuild {
        self.build
    }

    /// 当前规则快照。
    pub fn rules(&self) -> Arc<PolicyRules> {
        self.rules
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 整体原子替换规则（热加载入口，policy.md §7.2）。进行中的判定继续用旧快照。
    pub fn replace_rules(&self, rules: PolicyRules) {
        *self
            .rules
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(rules);
    }

    /// 读一个审批（先惰性过期）。
    pub async fn approval(&self, id: &ApprovalId) -> Result<Option<Approval>, PolicyError> {
        let log = self.store.events();
        let Some(record) = approvals::load(&log, id).await? else {
            return Ok(None);
        };
        let record = approvals::expire_if_due(&self.store, record, self.clock.now()).await?;
        Ok(Some(record.approval))
    }

    // -----------------------------------------------------------------------
    // decide_action（§7.3）
    // -----------------------------------------------------------------------

    async fn judge_action(
        &self,
        rules: &PolicyRules,
        id: PolicyActionId,
        req: &ActionRequest,
    ) -> Result<Judgement, PolicyError> {
        let disposition = rules.disposition(id);
        if disposition == Disposition::Deny {
            return Ok(Judgement::Deny(DenyReason::ActionDenied));
        }

        // 第 3 步：push_branch 族的 I8 前置——必须有匹配的 patch.validated。
        if let ActionFacts::PushBranch {
            expected_tree_oid,
            validation_ref,
            ..
        } = &req.facts
        {
            let (Some(tree), Some(sha)) = (expected_tree_oid, validation_ref) else {
                return Ok(Judgement::Deny(DenyReason::NoValidatedPatch));
            };
            match self.validated_patch(sha, tree).await? {
                None => return Ok(Judgement::Deny(DenyReason::NoValidatedPatch)),
                // needs_approval 的补丁要求已消费的 patch_apply_protected 审批（M2）；
                // M1 不存在这类审批，所以按补丁未放行拒绝。
                Some(PatchVerdict::NeedsApproval) | Some(PatchVerdict::Deny) => {
                    return Ok(Judgement::Deny(DenyReason::PatchDenied))
                }
                Some(PatchVerdict::Allow) => {}
            }
        }

        if disposition == Disposition::Auto {
            return Ok(Judgement::Allow(None));
        }

        // 处置为 approve：之后的任何读失败都按 ApprovalChainUnavailable 失败即关闭。
        match self.judge_approval(rules, id, req).await {
            Ok(judgement) => Ok(judgement),
            Err(_) => Ok(Judgement::Deny(DenyReason::ApprovalChainUnavailable)),
        }
    }

    async fn judge_approval(
        &self,
        rules: &PolicyRules,
        id: PolicyActionId,
        req: &ActionRequest,
    ) -> Result<Judgement, PolicyError> {
        // 第 4 步：merge_pr 的 head 必须与 PR 快照一致。
        if let ActionFacts::MergePr {
            repo,
            number,
            expected_head_sha,
        } = &req.facts
        {
            let pr = self
                .forge
                .pull_request(&req.project_id, *number)
                .await
                .map_err(|error| PolicyError::ApprovalUnavailable {
                    message: error.to_string(),
                })?;
            // 没有快照就无法核对 head：按不一致处理，调用方先对账再重试。
            let matches =
                pr.is_some_and(|pr| &pr.repo == repo && pr.head_sha == *expected_head_sha);
            if !matches {
                return Ok(Judgement::Deny(DenyReason::HeadMismatch));
            }
        }

        let Some(spec) = approval_spec(id, req, rules) else {
            return Ok(Judgement::Deny(DenyReason::ApprovalShapeUndefined));
        };

        // 第 5 步：查同主体的审批，取最近一条。
        let now = self.clock.now();
        let log = self.store.events();
        let latest = approvals::load_all(&log)
            .await?
            .into_iter()
            .rfind(|record| same_subject(&record.approval, &spec));
        let latest = match latest {
            Some(record) => Some(approvals::expire_if_due(&self.store, record, now).await?),
            None => None,
        };
        Ok(match latest.map(|record| record.approval) {
            Some(a) if a.status == ApprovalStatus::Granted => Judgement::Allow(Some(a.id)),
            Some(a) if a.status == ApprovalStatus::Pending => {
                Judgement::NeedsApproval(spec, Some(a.id))
            }
            Some(a) if a.status == ApprovalStatus::Denied => {
                Judgement::Deny(DenyReason::ApprovalDenied)
            }
            Some(a) if a.status == ApprovalStatus::Consumed => {
                // #89 Q3：只有更大 epoch 的新动作（retry）才需要新审批；同一或更早
                // epoch、或查不到消费方的 epoch 时按已消费拒绝，防止重复执行。
                let consumer_epoch = match &a.consumed_by {
                    Some(action) => self.enqueued_epoch(action).await?,
                    None => None,
                };
                match consumer_epoch {
                    Some(epoch) if req.epoch > epoch => Judgement::NeedsApproval(spec, None),
                    _ => Judgement::Deny(DenyReason::ApprovalConsumed),
                }
            }
            _ => Judgement::NeedsApproval(spec, None),
        })
    }

    /// 消费审批的 Outbox 动作入队时的 `epoch`（`outbox.enqueued.action.epoch`）。
    async fn enqueued_epoch(&self, action: &str) -> Result<Option<u32>, PolicyError> {
        let Ok(subject) = Subject::parse(action) else {
            return Ok(None);
        };
        let events = self
            .store
            .events()
            .by_subject(&subject)
            .await
            .map_err(port_error)?;
        Ok(events
            .into_iter()
            .filter(|event| event.ty == EventType::OutboxEnqueued)
            .find_map(|event| serde_json::from_value::<OutboxEnqueuedData>(event.data).ok())
            .map(|data| data.action.epoch))
    }

    /// 找 `sha256 == validation_ref` 且 `result_tree == expected_tree_oid` 的 patch.validated。
    async fn validated_patch(
        &self,
        sha: &gantry_core::canon::Sha256Digest,
        tree: &gantry_core::canon::GitOid,
    ) -> Result<Option<PatchVerdict>, PolicyError> {
        let log = self.store.events();
        let filter = EventFilter {
            project_ids: None,
            include: vec![TypePattern::Exact(EventType::PatchValidated)],
            exclude: Vec::new(),
        };
        let mut after = gantry_core::entities::Seq::ZERO;
        loop {
            let page = log.scan(after, &filter, 200).await.map_err(port_error)?;
            for event in &page.events {
                let Ok(data) = serde_json::from_value::<PatchValidatedData>(event.data.clone())
                else {
                    continue;
                };
                if data.sha256 == *sha && data.result_tree == *tree {
                    return Ok(Some(data.verdict));
                }
            }
            if page.scanned_to >= page.head || page.events.is_empty() {
                return Ok(None);
            }
            after = page.scanned_to;
        }
    }

    async fn append_committed(&self, event: NewEvent) -> Result<(), PolicyError> {
        let mut tx = self.store.begin_write().await.map_err(port_error)?;
        tx.append(event).await.map_err(port_error)?;
        tx.commit().await.map_err(port_error)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // consume（§7.6）
    // -----------------------------------------------------------------------

    async fn consume_execution_in(
        &self,
        tx: &mut WriteTx,
        record: Record,
        check: &ExecutionCheck,
        act: &ActionId,
        ctx: &ActorCtx,
    ) -> Result<ExecutionPermit, GateError> {
        let now = self.clock.now();
        // 校验先于状态：已消费的重放在 head / 目标变化后同样被拦下。
        check_execution(&record.approval, check)?;
        match record.approval.status {
            ApprovalStatus::Consumed => {
                return match record.approval.consumed_by.as_deref() {
                    Some(by) if by == act.to_string() => {
                        Ok(ExecutionPermit::AlreadyConsumedBySameAction)
                    }
                    Some(by) => match by.parse::<ActionId>() {
                        Ok(by) => Err(GateError::ConsumedByOther { by }),
                        Err(_) => Err(invalid(MismatchReason::Consumed)),
                    },
                    None => Err(invalid(MismatchReason::Consumed)),
                };
            }
            ApprovalStatus::Granted if now >= record.approval.expires_at => {
                approvals::transition(
                    tx,
                    &record,
                    ApprovalStatus::Expired,
                    approvals::expired_data(),
                    approvals::policy_actor(),
                    ctx.causation_id,
                )
                .await?;
                return Err(invalid(MismatchReason::Expired));
            }
            ApprovalStatus::Granted => {}
            ApprovalStatus::Expired => return Err(invalid(MismatchReason::Expired)),
            ApprovalStatus::Pending if now >= record.approval.expires_at => {
                approvals::transition(
                    tx,
                    &record,
                    ApprovalStatus::Expired,
                    approvals::expired_data(),
                    approvals::policy_actor(),
                    ctx.causation_id,
                )
                .await?;
                return Err(invalid(MismatchReason::Expired));
            }
            _ => return Err(invalid(MismatchReason::NotGranted)),
        }

        // 第 4 步：按当前规则重判；处置为 deny 时拦下，审批状态不变。
        if let Some(id) = execution_action_id(check) {
            if self.rules().disposition(id) == Disposition::Deny {
                return Err(invalid(MismatchReason::NotGranted));
            }
        }

        approvals::transition(
            tx,
            &record,
            ApprovalStatus::Consumed,
            approvals::consumed_data(
                record.approval.id,
                act.to_string(),
                record.approval.subject_digest,
            ),
            approvals::policy_actor(),
            ctx.causation_id,
        )
        .await?;
        Ok(ExecutionPermit::Consumed)
    }
}

enum Judgement {
    Allow(Option<ApprovalId>),
    NeedsApproval(ApprovalSpec, Option<ApprovalId>),
    Deny(DenyReason),
}

/// 第 1 步：解析判定 id（force 优先于非前缀，OQ-P5 ①）。
pub fn resolve_action_id(req: &ActionRequest) -> PolicyActionId {
    match &req.facts {
        ActionFacts::PushBranch {
            branch,
            branch_prefix,
            force,
            ..
        } => {
            let prefix = if branch_prefix.is_empty() {
                DEFAULT_BRANCH_PREFIX
            } else {
                branch_prefix.as_str()
            };
            if *force {
                PolicyActionId::ForcePush
            } else if !branch.starts_with(prefix) {
                PolicyActionId::PushOutsidePrefix
            } else {
                PolicyActionId::PushBranch
            }
        }
        ActionFacts::ResolveReviewThread {
            thread_has_human_comment: true,
            ..
        } => PolicyActionId::ResolveReviewThreadHuman,
        _ => PolicyActionId::from(req.kind),
    }
}

/// 审批形状表（§7.3 第 6 步；以 core 的 `APPROVAL_SHAPES` 为准）。
fn approval_spec(
    id: PolicyActionId,
    req: &ActionRequest,
    rules: &PolicyRules,
) -> Option<ApprovalSpec> {
    let (action, target, subject_digest) = match (&req.facts, id) {
        (
            ActionFacts::MergePr {
                repo,
                number,
                expected_head_sha,
            },
            PolicyActionId::MergePr,
        ) => (
            ApprovalAction::Forge(ForgeActionKind::MergePr),
            ApprovalTarget::Pr {
                repo: repo.clone(),
                number: *number,
            },
            SubjectDigest::git(*expected_head_sha),
        ),
        (
            ActionFacts::PushBranch {
                repo,
                branch,
                local_head_oid,
                expected_remote_oid,
                ..
            },
            PolicyActionId::ForcePush | PolicyActionId::PushOutsidePrefix,
        ) => (
            if id == PolicyActionId::ForcePush {
                ApprovalAction::ForcePush
            } else {
                ApprovalAction::PushOutsidePrefix
            },
            ApprovalTarget::Branch {
                repo: repo.clone(),
                branch: branch.clone(),
                expected_remote_oid: *expected_remote_oid,
            },
            SubjectDigest::git(*local_head_oid),
        ),
        // 其余动作：data-model 未给出可由 ActionFacts 构造的形状 → fail-closed（P-54）。
        _ => return None,
    };
    action.shape()?;
    Some(ApprovalSpec {
        action,
        target,
        subject_digest,
        delivery_id: req.delivery_id,
        ttl: rules.approval_ttl,
    })
}

fn same_subject(approval: &Approval, spec: &ApprovalSpec) -> bool {
    approval.action == spec.action
        && approval.target == spec.target
        && approval.subject_digest == spec.subject_digest
}

/// 拒绝的上报建议（policy.md §6.1，#66、#133）。`HeadMismatch` 由 engine 刷新快照后
/// 重判，不上报；其余原因都有词表码。
fn report_for(id: PolicyActionId, reason: &DenyReason) -> Option<ReportHint> {
    let code = match (id, reason) {
        (_, DenyReason::HeadMismatch) => return None,
        (PolicyActionId::MergePr, DenyReason::ActionDenied) => REPORT_MERGE_DENIED.to_owned(),
        (_, DenyReason::ApprovalShapeUndefined) => REPORT_SHAPE_UNDEFINED.to_owned(),
        (_, reason) => format!("{REPORT_ACTION_DENIED_PREFIX}{}", deny_reason_name(reason)),
    };
    Some(ReportHint {
        inbox_kind: InboxKind::Escalation,
        payload_kind: None,
        reason_code: ReasonCode::parse(&code).expect("report codes are valid literals"),
        severity: Severity::Blocker,
        attach_patch: false,
    })
}

/// `DenyReason` 的 snake_case 名，与 `policy.denied` 的 `policy.<deny_reason>` 一一对应。
/// `DenyReason` 是 non_exhaustive；新增原因在补上映射前按链路不可用上报（与审计一致）。
fn deny_reason_name(reason: &DenyReason) -> &'static str {
    match reason {
        DenyReason::ActionDenied => "action_denied",
        DenyReason::ApprovalDenied => "approval_denied",
        DenyReason::ApprovalConsumed => "approval_consumed",
        DenyReason::HeadMismatch => "head_mismatch",
        DenyReason::NoValidatedPatch => "no_validated_patch",
        DenyReason::PatchDenied => "patch_denied",
        DenyReason::MultipleApprovalsRequired => "multiple_approvals_required",
        DenyReason::ApprovalShapeUndefined => "approval_shape_undefined",
        _ => "approval_chain_unavailable",
    }
}

/// `policy.denied` 的目标与主体（#89 Q1：`ActionFacts` 带上了动作目标身份）。
fn denial_target(req: &ActionRequest) -> (PolicyTarget, Option<PolicySubject>) {
    match &req.facts {
        ActionFacts::MergePr {
            repo,
            number,
            expected_head_sha,
        } => (
            PolicyTarget::Pr {
                repo: repo.clone(),
                number: *number,
            },
            Some(PolicySubject::Head(*expected_head_sha)),
        ),
        ActionFacts::PushBranch {
            repo,
            branch,
            local_head_oid,
            ..
        } => (
            PolicyTarget::Branch {
                repo: repo.clone(),
                branch: branch.clone(),
            },
            Some(PolicySubject::Head(*local_head_oid)),
        ),
        ActionFacts::ResolveReviewThread {
            repo,
            number,
            thread_node_id,
            ..
        } => (
            PolicyTarget::ReviewThread {
                repo: repo.clone(),
                number: *number,
                thread_node_id: thread_node_id.clone(),
            },
            None,
        ),
        ActionFacts::Other { target } => (target.clone(), None),
    }
}

fn invalid(reason: MismatchReason) -> GateError {
    GateError::ApprovalInvalid { reason }
}

fn execution_action_id(check: &ExecutionCheck) -> Option<PolicyActionId> {
    match check {
        ExecutionCheck::MergePr { .. } => Some(PolicyActionId::MergePr),
        ExecutionCheck::PushBranch { branch, force, .. } => Some(if *force {
            PolicyActionId::ForcePush
        } else if !branch.starts_with(DEFAULT_BRANCH_PREFIX) {
            PolicyActionId::PushOutsidePrefix
        } else {
            PolicyActionId::PushBranch
        }),
        ExecutionCheck::Other { kind, .. } => Some(PolicyActionId::from(*kind)),
    }
}

/// 执行检查（§7.6 第 3 步）：action、target、主体与实时 head。
fn check_execution(approval: &Approval, check: &ExecutionCheck) -> Result<(), GateError> {
    match check {
        ExecutionCheck::MergePr {
            repo,
            number,
            expected_head_sha,
            live_head_sha,
        } => {
            if approval.action != ApprovalAction::Forge(ForgeActionKind::MergePr) {
                return Err(invalid(MismatchReason::ActionMismatch));
            }
            if approval.target
                != (ApprovalTarget::Pr {
                    repo: repo.clone(),
                    number: *number,
                })
            {
                return Err(invalid(MismatchReason::TargetMismatch));
            }
            if approval.subject_digest != SubjectDigest::git(*expected_head_sha) {
                return Err(invalid(MismatchReason::DigestMismatch));
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
            force,
            local_head_oid,
            expected_remote_oid,
            ..
        } => {
            let required = if *force {
                ApprovalAction::ForcePush
            } else if !branch.starts_with(DEFAULT_BRANCH_PREFIX) {
                ApprovalAction::PushOutsidePrefix
            } else {
                ApprovalAction::Forge(ForgeActionKind::PushBranch)
            };
            if approval.action != required {
                return Err(invalid(MismatchReason::ActionMismatch));
            }
            let ApprovalTarget::Branch {
                repo: target_repo,
                branch: target_branch,
                expected_remote_oid: target_remote,
            } = &approval.target
            else {
                return Err(invalid(MismatchReason::TargetMismatch));
            };
            if target_repo != repo
                || target_branch != branch
                || target_remote != expected_remote_oid
            {
                return Err(invalid(MismatchReason::TargetMismatch));
            }
            if approval.subject_digest != SubjectDigest::git(*local_head_oid) {
                return Err(invalid(MismatchReason::DigestMismatch));
            }
        }
        // 其余动作没有可由执行检查核对的形状（`Other.target` 是不透明字符串）：失败即关闭。
        ExecutionCheck::Other { kind, .. } => {
            if approval.action != ApprovalAction::Forge(*kind) || approval.action.shape().is_none()
            {
                return Err(invalid(MismatchReason::ActionMismatch));
            }
            return Err(invalid(MismatchReason::TargetMismatch));
        }
    }
    Ok(())
}

#[async_trait]
impl PolicyPort for PolicyEngine {
    async fn decide_action(
        &self,
        req: ActionRequest,
        ctx: &ActorCtx,
    ) -> Result<ActionDecision, PolicyError> {
        let rules = self.rules();
        let id = resolve_action_id(&req);
        let config_rev = rules.config_rev;
        let judgement = self.judge_action(&rules, id, &req).await?;
        Ok(match judgement {
            Judgement::Allow(approval) => ActionDecision::Allow {
                action_id: id,
                approval,
                config_rev,
            },
            Judgement::NeedsApproval(spec, pending) => ActionDecision::NeedsApproval {
                spec,
                pending,
                config_rev,
            },
            Judgement::Deny(reason) => {
                let (target, subject_digest) = denial_target(&req);
                let subject = match req.delivery_id {
                    Some(delivery) => Subject::parse(&delivery.to_string()),
                    None => Subject::parse(&req.project_id.to_string()),
                }
                .expect("delivery and project ids are valid subjects");
                let denial = ActionDenial {
                    project_id: req.project_id,
                    subject,
                    action_id: id,
                    kind: req.kind,
                    target,
                    reason: audit::reason_code(&reason),
                    subject_digest,
                    config_rev,
                };
                self.append_committed(denial.event(ctx.causation_id))
                    .await?;
                ActionDecision::Deny {
                    action_id: id,
                    report: report_for(id, &reason),
                    reason,
                    config_rev,
                }
            }
        })
    }

    async fn request_approval(
        &self,
        spec: ApprovalSpec,
        ctx: ActorCtx,
    ) -> Result<Approval, PolicyError> {
        let rules = self.rules();
        let now = self.clock.now();
        // 有效期不超过当前规则的 approval.ttl_s（创建时固化）。
        let ttl = spec.ttl.min(rules.approval_ttl);
        let expires_at = gantry_core::time::Timestamp::from_millis(
            now.as_millis()
                .saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)),
        );
        let id: ApprovalId =
            self.ids
                .next::<Apr>()
                .map_err(|error| PolicyError::ApprovalUnavailable {
                    message: error.to_string(),
                })?;
        let candidate = Approval {
            id,
            delivery_id: spec.delivery_id,
            action: spec.action,
            target: spec.target.clone(),
            subject_digest: spec.subject_digest,
            device_id: None,
            signature: None,
            status: ApprovalStatus::Pending,
            expires_at,
            consumed_by: None,
            created_at: now,
            updated_at: now,
        };
        candidate
            .validate()
            .map_err(|error| PolicyError::InvalidRequest {
                pointer: "/spec".into(),
                message: error.to_string(),
            })?;

        let log = self.store.events();
        for record in approvals::load_all(&log).await? {
            if record.approval.status != ApprovalStatus::Pending
                || !same_subject(&record.approval, &spec)
                || record.approval.delivery_id != spec.delivery_id
            {
                continue;
            }
            let record = approvals::expire_if_due(&self.store, record, now).await?;
            if record.approval.status == ApprovalStatus::Pending {
                return Ok(record.approval);
            }
        }

        let data = ApprovalRequestedData {
            action: candidate.action,
            target: candidate.target.clone(),
            subject_digest: candidate.subject_digest,
            expires_at,
            config_rev: rules.config_rev,
            requested_by: ctx.actor.clone(),
        };
        let event = NewEvent {
            ty: EventType::ApprovalRequested,
            subject: approvals::subject(&id),
            project_id: None,
            actor: ctx.actor,
            causation_id: ctx.causation_id,
            correlation_id: candidate.delivery_id.map(|id| id.to_string()),
            data: serde_json::to_value(&data).expect("approval.requested data serializes"),
            dedup_key: None,
        };
        let mut tx = self.store.begin_write().await.map_err(port_error)?;
        let appended = tx
            .append_with(
                event,
                AppendOpts {
                    expect_subject_seq: Some(None),
                },
            )
            .await
            .map_err(port_error)?;
        tx.commit().await.map_err(port_error)?;
        let mut approval = candidate;
        if let gantry_store::Appended::New(event) = appended {
            approval.created_at = event.ts;
            approval.updated_at = event.ts;
        }
        Ok(approval)
    }

    async fn invalidate(
        &self,
        id: &ApprovalId,
        reason: InvalidateReason,
        ctx: &ActorCtx,
    ) -> Result<Approval, PolicyError> {
        let log = self.store.events();
        let record = approvals::load(&log, id)
            .await?
            .ok_or(PolicyError::NotFound)?;
        let record = approvals::expire_if_due(&self.store, record, self.clock.now()).await?;
        if !record.is_live() {
            return Err(PolicyError::Conflict {
                message: format!("approval {id} is {:?}", record.approval.status),
            });
        }
        let mut tx = self.store.begin_write().await.map_err(port_error)?;
        let next = approvals::transition(
            &mut tx,
            &record,
            ApprovalStatus::Invalidated,
            approvals::invalidated_data((&reason).into()),
            ctx.actor.clone(),
            ctx.causation_id,
        )
        .await?;
        tx.commit().await.map_err(port_error)?;
        Ok(next.approval)
    }

    async fn live_approvals_for_pr(
        &self,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Vec<Approval>, PolicyError> {
        let now = self.clock.now();
        let log = self.store.events();
        let mut live = Vec::new();
        for record in approvals::load_all(&log).await? {
            let same_pr = matches!(&record.approval.target,
                ApprovalTarget::Pr { repo: r, number: n } if r == repo && *n == number);
            if !same_pr || !record.is_live() {
                continue;
            }
            let record = approvals::expire_if_due(&self.store, record, now).await?;
            if record.is_live() {
                live.push(record.approval);
            }
        }
        Ok(live)
    }

    async fn decide_proposals(
        &self,
        proposal_ctx: ProposalContext,
        items: &[Proposal],
        ctx: &ActorCtx,
    ) -> Result<Vec<ProposalDecision>, PolicyError> {
        let rules = self.rules();
        let subject = Subject::parse(&proposal_ctx.attempt_id.to_string())
            .expect("attempt ids are valid subjects");
        let mut decisions = Vec::with_capacity(items.len());
        for item in items {
            let decision = decide_proposal(&rules, &proposal_ctx, item);
            if let ProposalDecision::Denied { reason, .. } = &decision {
                let denial = ProposalDenial {
                    project_id: proposal_ctx.project_id,
                    subject: subject.clone(),
                    proposal_id: &item.id,
                    kind: item.kind,
                    target_handle: item.target.as_deref(),
                    reason: audit::reason_code(reason),
                    config_rev: rules.config_rev,
                };
                self.append_committed(denial.event(ctx.causation_id))
                    .await?;
            }
            decisions.push(decision);
        }
        Ok(decisions)
    }

    async fn evaluate_patch(
        &self,
        facts: &PatchFacts,
        added: &dyn AddedLineSource,
        ctx: &ActorCtx,
    ) -> Result<PatchPolicyVerdict, PolicyError> {
        let rules = self.rules();
        let verdict = crate::patch::evaluate(&rules, self.build, facts, added);
        if verdict.verdict == PatchVerdict::Deny {
            let snapshot = self
                .forge
                .project_snapshot(&facts.project_id)
                .await
                .map_err(|error| PolicyError::ApprovalUnavailable {
                    message: format!("project repo unavailable for policy.denied: {error}"),
                })?;
            let event =
                audit::patch_event(facts, snapshot.project.repo, &verdict, ctx.causation_id);
            self.append_committed(event).await?;
        }
        Ok(verdict)
    }

    async fn consume_patch_approval(
        &self,
        tx: &mut dyn TxHandle,
        approval: &ApprovalId,
        check: &PatchApprovalCheck,
        ctx: &ActorCtx,
    ) -> Result<PatchPermit, GateError> {
        let tx = tx.as_any_mut().downcast_mut::<WriteTx>().ok_or_else(|| {
            GateError::Store(StoreError::Rejected {
                message: "consume_patch_approval requires a store WriteTx".into(),
            })
        })?;
        let log = self.store.events();
        let record = approvals::load(&log, approval)
            .await?
            .ok_or(invalid(MismatchReason::NotGranted))?;
        let a = &record.approval;
        let ApprovalTarget::Patch {
            repo,
            attempt_id,
            rules,
            paths,
        } = &a.target
        else {
            return Err(invalid(MismatchReason::TargetMismatch));
        };
        if a.action != ApprovalAction::PatchApplyProtected {
            return Err(invalid(MismatchReason::ActionMismatch));
        }
        let covers_rules = check
            .rules
            .iter()
            .all(|rule| rules.iter().any(|r| r == rule.as_str()));
        let covers_paths = check
            .paths
            .iter()
            .all(|path| paths.iter().any(|p| p == path.as_str()));
        if *repo != check.repo || *attempt_id != check.attempt_id || !covers_rules || !covers_paths
        {
            return Err(invalid(MismatchReason::TargetMismatch));
        }
        if a.subject_digest != SubjectDigest::sha256(check.patch_sha256) {
            return Err(invalid(MismatchReason::DigestMismatch));
        }
        let same_attempt = a.consumed_by.as_deref() == Some(check.attempt_id.to_string().as_str());
        match a.status {
            ApprovalStatus::Consumed if same_attempt => {
                Ok(PatchPermit::AlreadyConsumedBySameAttempt)
            }
            ApprovalStatus::Consumed => Err(invalid(MismatchReason::Consumed)),
            ApprovalStatus::Granted | ApprovalStatus::Pending
                if self.clock.now() >= a.expires_at =>
            {
                approvals::transition(
                    tx,
                    &record,
                    ApprovalStatus::Expired,
                    approvals::expired_data(),
                    approvals::policy_actor(),
                    ctx.causation_id,
                )
                .await?;
                Err(invalid(MismatchReason::Expired))
            }
            ApprovalStatus::Granted => {
                approvals::transition(
                    tx,
                    &record,
                    ApprovalStatus::Consumed,
                    approvals::consumed_data(
                        a.id,
                        check.attempt_id.to_string(),
                        SubjectDigest::sha256(check.patch_sha256),
                    ),
                    approvals::policy_actor(),
                    ctx.causation_id,
                )
                .await?;
                Ok(PatchPermit::Consumed)
            }
            ApprovalStatus::Expired => Err(invalid(MismatchReason::Expired)),
            _ => Err(invalid(MismatchReason::NotGranted)),
        }
    }

    fn path_rules(&self, _project: &ProjectId) -> Arc<dyn PathRuleSet> {
        Arc::new(RulePaths::new(self.rules()))
    }

    fn path_rules_snapshot(&self, _project: &ProjectId) -> PathRulesSnapshot {
        paths::snapshot(&self.rules(), self.build)
    }

    fn scan_text(&self, text: &str) -> Vec<SecretFinding> {
        secrets::scan_text(text)
    }

    async fn consume_for_execution(
        &self,
        approval: &ApprovalId,
        check: &ExecutionCheck,
        act: &ActionId,
        ctx: &ActorCtx,
    ) -> Result<ExecutionPermit, GateError> {
        let log = self.store.events();
        let record = approvals::load(&log, approval)
            .await?
            .ok_or(invalid(MismatchReason::NotGranted))?;
        let mut tx = self.store.begin_write().await.map_err(port_error)?;
        let result = self
            .consume_execution_in(&mut tx, record, check, act, ctx)
            .await;
        // 惰性过期即使返回错误也要落盘；其余错误不产生写入，提交是空操作。
        tx.commit().await.map_err(port_error)?;
        result
    }
}

/// proposals 判定（§6.3）：逐条独立，一条被拒不影响其余。
fn decide_proposal(
    rules: &PolicyRules,
    ctx: &ProposalContext,
    item: &Proposal,
) -> ProposalDecision {
    let proposal_id = item.id.clone();
    let by_disposition = |id: PolicyActionId| match rules.disposition(id) {
        Disposition::Auto => ProposalDecision::Accepted {
            proposal_id: proposal_id.clone(),
            action_id: Some(id),
        },
        Disposition::Deny => ProposalDecision::Denied {
            proposal_id: proposal_id.clone(),
            reason: DenyReason::ActionDenied,
        },
        // 由 proposal 推出的动作都没有可构造的审批形状：失败即关闭（P-54）。
        Disposition::Approve => ProposalDecision::Denied {
            proposal_id: proposal_id.clone(),
            reason: DenyReason::ApprovalShapeUndefined,
        },
    };
    match item.kind {
        ProposalKind::OpenPr => match ctx.patch {
            Some(PatchVerdict::Allow) => by_disposition(PolicyActionId::CreatePr),
            Some(PatchVerdict::NeedsApproval) => ProposalDecision::Held { proposal_id },
            Some(PatchVerdict::Deny) => ProposalDecision::Denied {
                proposal_id,
                reason: DenyReason::PatchDenied,
            },
            None => ProposalDecision::Denied {
                proposal_id,
                reason: DenyReason::NoValidatedPatch,
            },
        },
        // escalate 写 InboxItem，不是 GitHub 动作；inbox_kind 已由类型限定在允许集合内。
        ProposalKind::Escalate => ProposalDecision::Accepted {
            proposal_id,
            action_id: None,
        },
        ProposalKind::ReplyReviewThread => by_disposition(PolicyActionId::ReplyReviewThread),
        ProposalKind::ResolveReviewThread => {
            // 不知道线程是否含人类评论时按含人类评论处理（更严）。
            let human = item
                .target
                .as_deref()
                .and_then(|handle| {
                    ctx.thread_has_human_comment
                        .get(&gantry_core::ports::ThreadNodeId::new(handle))
                        .copied()
                })
                .unwrap_or(true);
            by_disposition(if human {
                PolicyActionId::ResolveReviewThreadHuman
            } else {
                PolicyActionId::ResolveReviewThread
            })
        }
    }
}
