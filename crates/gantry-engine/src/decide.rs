//! 决策器的类型（engine.md §7.14.2、§7.14.3、§7.14.5）。
//!
//! 本切片（#69）只落类型与确定性 ID，不写 `decide` 的逻辑。字段以规格为准；
//! 规格引用了但 core 尚未定义的类型，在这里按下列方式处理，待对应模块落地后替换：
//! - `MessageRef`（M2 `SupplySpec`）：暂用 `MessageId`；
//! - `ResolvedModel`（models.md §6）：只取 `attempt.started` 记录的三项与 `attempt_wall_s`；
//! - `LevelPolicy` 依赖 `RiskSignals`（§7.10，M2），`DecideEnv` 暂不带它；
//! - `EngineSettings` 只带 `config_rev` 与 `project`，§10 的其余键随所需切片补齐。

use std::collections::{BTreeMap, BTreeSet};

use gantry_core::canon::{sha256, GitOid, RepoSlug, Sha256Digest, SubjectDigest};
use gantry_core::entities::{
    Actor, ApprovalAction, ApprovalStatus, AttemptStatus, BudgetLedger, Exec, FailureClass,
    FailureDetail, GithubRef, LockHolder, MergeMethod, PatchRef, Ref, Role, Seq, Tier, TokenUsage,
    Verdict,
};
use gantry_core::events::{
    InboxResolution, NewEvent, OutboxCancelReason, OutboxFailureCode, PatchRejectedData,
    PatchValidatedData,
};
use gantry_core::forge::ForgeEventData;
use gantry_core::ids::{
    ActionId, ApprovalId, AttemptId, DeliveryId, EventId, Id, IdKind, InboxId, MessageId, NodeId,
    WorkflowId, WorktreeId,
};
use gantry_core::level::{Effort, Level};
use gantry_core::ports::context::{ActorCtx, RequestId};
use gantry_core::ports::engine::{
    CancelWhy, CiClassifyRequest, FindingClassifyRequest, HealRequest,
};
use gantry_core::ports::forge::{CancelReason, EnqueueRequest, ReconcileTarget};
use gantry_core::ports::inbox::{OptionId, RaiseRequest, Resolution, ResolveTarget};
use gantry_core::ports::policy::{
    ActionDecision, ActionRequest, ApprovalSpec, InvalidateReason, ProposalDecision, ReportHint,
};
use gantry_core::ports::repo::{BranchName, LockToken};
use gantry_core::time::Timestamp;
use gantry_core::worker::{MissingAspect, OpenPrParams, PatchRejectReason, Proposal};

use crate::state::{ActionKey, FindingFingerprint, OpenInbox};
use crate::template::{stage_graph, StageGraph};

// ---------------------------------------------------------------------------
// Input（§7.14.2）
// ---------------------------------------------------------------------------

pub enum Input {
    Command(Command),
    Event(Observed),
    Report(EffectReport),
    Tick(TickReason),
}

pub enum Command {
    SetLevelCap {
        cap: Level,
        ctx: ActorCtx,
    },
    Abandon {
        reason: Option<String>,
        ctx: ActorCtx,
    },
    Pause {
        ctx: ActorCtx,
    },
    Resume {
        ctx: ActorCtx,
    },
    RetryNode {
        node_id: NodeId,
        ctx: ActorCtx,
    },
    /// M2：T4 答复。
    SupplySpec {
        inbox_id: InboxId,
        message: MessageId,
        ctx: ActorCtx,
    },
    /// M2：HealPort（P-60）。
    RequestHeal(HealRequest),
    /// M2：ClassifyPort。
    Classify(ClassifyRequest),
}

pub enum ClassifyRequest {
    Ci(CiClassifyRequest),
    Finding(FindingClassifyRequest),
}

pub struct Observed {
    pub seq: Seq,
    pub id: EventId,
    pub actor: Actor,
    pub data: ObservedData,
}

pub enum ObservedData {
    Forge(ForgeEventData),
    OutboxSucceeded {
        action_id: ActionId,
        github_ref: GithubRef,
    },
    OutboxFailed {
        action_id: ActionId,
        code: OutboxFailureCode,
    },
    OutboxCancelled {
        action_id: ActionId,
        reason: OutboxCancelReason,
    },
    ApprovalRequested {
        approval_id: ApprovalId,
        action: ApprovalAction,
        subject_digest: SubjectDigest,
    },
    /// granted / consumed / invalidated / expired / denied
    Approval {
        approval_id: ApprovalId,
        status: ApprovalStatus,
    },
    /// `item`：路由器按 `inbox_id` 联查的原上报项快照；折叠会把它移出 `open_inbox`。
    InboxResolved {
        inbox_id: InboxId,
        resolution: InboxResolution,
        option_id: Option<OptionId>,
        item: Option<OpenInbox>,
    },
    InboxDismissed {
        inbox_id: InboxId,
    },
    PatchValidated(PatchValidatedData),
    PatchRejected(PatchRejectedData),
    WorktreeCreated {
        worktree_id: WorktreeId,
        branch: String,
        base_sha: GitOid,
    },
    WorktreeLocked {
        worktree_id: WorktreeId,
        holder: LockHolder,
        lease_until: Timestamp,
        fence: Seq,
    },
    WorktreeUnlocked {
        worktree_id: WorktreeId,
    },
    SettingsUpdated {
        rev: Seq,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TickReason {
    Settle,
    Recover,
    Timer { at: Timestamp, kind: TimerKind },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimerKind {
    RetryBackoff { node_id: NodeId },
    LeaseCheck { worktree_id: WorktreeId },
    LockRenew { worktree_id: WorktreeId },
    RecoveryScan,
}

pub enum EffectReport {
    ActionJudged {
        key: ActionKey,
        decision: ActionDecision,
    },
    ApprovalRequested {
        key: ActionKey,
        approval: ApprovalId,
    },
    WorktreeReady {
        worktree_id: WorktreeId,
    },
    LockAcquired {
        node_id: NodeId,
        token: LockToken,
    },
    LockBusy {
        node_id: NodeId,
        holder: LockHolder,
        until: Option<Timestamp>,
    },
    ReconcileDone {
        target: ReconcileTarget,
    },
    Done {
        key: EffectKey,
    },
    AttemptFinalized(Finalized),
    EffectFailed {
        key: EffectKey,
        error: EffectErrorClass,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectErrorClass {
    Retryable,
    PermissionMissing,
    Defect { what: &'static str },
}

/// AttemptRunner 走完 V1–V6 后的结论。
pub struct Finalized {
    pub attempt_id: AttemptId,
    pub node_id: NodeId,
    pub status: AttemptStatus,
    pub failure_class: Option<FailureClass>,
    pub failure_detail: Option<FailureDetail>,
    pub result_ref: Option<Sha256Digest>,
    pub usage_metered: TokenUsage,
    pub weighted_tokens: u64,
    pub wall_time_s: u64,
    pub verdict: Option<Verdict>,
    pub patch: PatchResult,
    pub proposals: Vec<ProposalOutcome>,
    pub signals: OutputSignals,
}

pub enum PatchResult {
    None,
    NoChange,
    Committed {
        commit: GitOid,
        tree: GitOid,
        sha256: Sha256Digest,
    },
    /// M2。
    Held {
        sha256: Sha256Digest,
        tree: GitOid,
    },
    Denied {
        report: Option<ReportHint>,
        patch: PatchRef,
    },
    Rejected {
        reason: PatchRejectReason,
    },
    BaseConflict {
        branch_head: GitOid,
    },
}

pub struct ProposalOutcome {
    pub proposal_id: String,
    pub decision: ProposalDecision,
    pub open_pr: Option<Proposal>,
}

/// M1 恒为默认值（§7.14.8）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSignals {
    pub blocking_questions: bool,
    pub missing: Vec<MissingAspect>,
    pub arbiter_escalate: bool,
    pub fingerprints: BTreeSet<FindingFingerprint>,
}

// ---------------------------------------------------------------------------
// DecideEnv（§7.14.3）
// ---------------------------------------------------------------------------

/// 副作用的身份：在途去重、`DecideSeed` 与 `EffectFailed` 回报。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectKey {
    Judge(ActionKey),
    Approval(ActionKey),
    Worktree(DeliveryId),
    RemoveWorktree(WorktreeId),
    Lock(WorktreeId),
    Renew(WorktreeId, Seq),
    Unlock(WorktreeId, Seq),
    Run(AttemptId),
    Cancel(AttemptId),
    Release(AttemptId),
    Invalidate(ApprovalId),
    CancelOutbox(ActionId),
    Reconcile(ReconcileKey),
    Timer(TimerKind),
}

/// `ReconcileTarget` 没有全序；`EffectKey` 要进 `BTreeSet`，按（种类, ID, 编号）排序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconcileKey(pub ReconcileTarget);

impl ReconcileKey {
    fn rank(&self) -> (u8, String, u64) {
        match self.0 {
            ReconcileTarget::Pr { project, number } => (0, project.to_string(), number),
            ReconcileTarget::Issue { project, number } => (1, project.to_string(), number),
            ReconcileTarget::Delivery(id) => (2, id.to_string(), 0),
            ReconcileTarget::Project(id) => (3, id.to_string(), 0),
        }
    }
}

impl PartialOrd for ReconcileKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReconcileKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// "输入身份"（§7.14.3）：`Command` 用请求 ID，`Event` 用事件 ID，`Report` 用其 `EffectKey`，
/// `Tick` 用 `TickReason`。由执行体构造——只有它知道某个回报对应哪个已发出的副作用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputIdentity {
    Command(RequestId),
    Event(EventId),
    Report(EffectKey),
    Tick(TickReason),
}

impl InputIdentity {
    /// 规范编码；同一身份在任何进程里得到同一串字节。
    fn encode(&self) -> String {
        match self {
            InputIdentity::Command(id) => format!("command:{id}"),
            InputIdentity::Event(id) => format!("event:{id}"),
            InputIdentity::Report(key) => format!("report:{key:?}"),
            InputIdentity::Tick(reason) => format!("tick:{reason:?}"),
        }
    }
}

/// 32 字节种子：`sha256(delivery_id ‖ 输入身份 ‖ state.last_seq)`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecideSeed(pub Sha256Digest);

impl DecideSeed {
    pub fn derive(delivery: &DeliveryId, input: &InputIdentity, last_seq: Seq) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(delivery.to_string().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(input.encode().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&last_seq.as_u64().to_be_bytes());
        Self(sha256(&bytes))
    }
}

/// 确定性 ID：时间部分 = `now`，随机 80 位 = `sha256(seed ‖ 计数器)` 截断。
/// 每次决策只在入口取一次，计数器在本次决策内单调。
#[derive(Debug)]
pub struct IdMint {
    now: Timestamp,
    seed: DecideSeed,
    counter: u64,
}

impl IdMint {
    pub fn new(now: Timestamp, seed: DecideSeed) -> Self {
        Self {
            now,
            seed,
            counter: 0,
        }
    }

    pub fn next_id<K: IdKind>(&mut self) -> Id<K> {
        let mut bytes = Vec::with_capacity(40);
        bytes.extend_from_slice(self.seed.0.as_bytes());
        bytes.extend_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        let digest = sha256(&bytes);
        let mut random = [0u8; 16];
        random[6..].copy_from_slice(&digest.as_bytes()[..10]);
        let random80 = u128::from_be_bytes(random);
        let ms = u64::try_from(self.now.as_millis()).unwrap_or(0);
        // 时间戳超出 48 位只会出现在伪造时钟上；截断而不是 panic。
        Id::from_parts(ms & ((1 << 48) - 1), random80).expect("48-bit time and 80-bit random")
    }
}

/// models.md §6 `ResolvedModel` 中引擎写进 `attempt.started` 的部分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub model: String,
    pub reasoning_effort: Effort,
    /// `SandboxExec.attempt_wall_s`：有效 `wall_time_s` 的一项（§7.8，#70 C）。
    pub attempt_wall_s: u64,
}

/// `models.resolve` 结果表的键：（用途, 档位, tier, critical）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelKey {
    pub sandbox: bool,
    pub level: Level,
    pub tier: Tier,
    pub critical: bool,
}

pub type ModelTable = BTreeMap<ModelKey, ResolvedModel>;

/// `EffectiveSettings` 中引擎读取的子集（§10）。两次决策之间整体切换（§7.13）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineSettings {
    pub config_rev: Seq,
    /// 本交付单元所属项目的事实（§7.14.3，#70）。
    pub project: ProjectFacts,
    /// `sandbox.limits.wall_time_s` 与 `sandbox.kill_grace_s`（§10，#70 C）。
    pub sandbox: SandboxLimits,
}

/// 有效 `wall_time_s` 与锁租约所需的沙盒设置（§7.7、§7.8）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SandboxLimits {
    pub wall_time_s: u64,
    pub kill_grace_s: u64,
}

/// 构造 `push_branch` / `create_pr` / `merge_pr` 的 payload 与 `ActionFacts` 所需的项目事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectFacts {
    pub repo: RepoSlug,
    /// `base_ref` 与 `create_pr.base`。
    pub default_branch: String,
    /// forge 的 `push.branch_prefix`（§10 默认 `gantry/`）。
    pub branch_prefix: String,
    /// 至少一项（core 校验）；M1 的 `merge_method` 取第一项。
    pub allowed_merge_methods: Vec<MergeMethod>,
}

/// 引擎内部常量（§10，编译期默认值，不是设置）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineConsts {
    pub max_attempts_per_node: u32,
    /// 重试退避上限与基数：`min(cap, base × 2^(n-1))`，加全抖动。
    pub retry_backoff_base_ms: i64,
    pub retry_backoff_cap_ms: i64,
    pub lock_lease_margin_s: u64,
    pub recovery_scan_s: u64,
    pub max_settle: u32,
}

impl Default for EngineConsts {
    fn default() -> Self {
        Self {
            max_attempts_per_node: 3,
            retry_backoff_base_ms: 2_000,
            retry_backoff_cap_ms: 300_000,
            lock_lease_margin_s: 60,
            recovery_scan_s: 60,
            max_settle: 16,
        }
    }
}

/// 已登记的图模板（§7.4）；M1 只有 `low.single`。
#[derive(Clone, Copy, Debug, Default)]
pub struct TemplateRegistry;

impl TemplateRegistry {
    pub fn stage_graph(
        &self,
        level: Level,
        stage: gantry_core::entities::Stage,
    ) -> Option<StageGraph> {
        stage_graph(level, stage)
    }
}

/// 预算预留策略（§6；worker-protocol OQ-14 未决）。
pub trait ReservationPolicy: Send + Sync {
    fn reserve(&self, ledger: &BudgetLedger, tier: Tier, concurrent_in_batch: usize) -> u64;
}

pub struct DecideEnv<'a> {
    pub now: Timestamp,
    pub seed: DecideSeed,
    pub settings: &'a EngineSettings,
    pub models: &'a ModelTable,
    pub templates: &'a TemplateRegistry,
    pub reservation: &'a dyn ReservationPolicy,
    pub sandbox_capacity: u32,
    pub inflight: &'a BTreeSet<EffectKey>,
    pub denied: &'a BTreeSet<ActionKey>,
    /// `open_pr` 提议原文，执行体开事务前预取（§7.14.2）；只读。
    pub proposals: &'a ProposalTexts,
    pub consts: EngineConsts,
}

impl DecideEnv<'_> {
    pub fn mint(&self) -> IdMint {
        IdMint::new(self.now, self.seed)
    }
}

/// `open_pr` 提议原文表（#125）。执行体内存，不持久化；同一键的值恒定（CAS 内容寻址）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProposalTexts(BTreeMap<ProposalRef, ProposalText>);

impl ProposalTexts {
    /// `None` = 尚未预取（或预取遇瞬时 IO 错误）；与 `Some(Missing)` 含义不同（§7.14.3）。
    pub fn get(&self, key: &ProposalRef) -> Option<&ProposalText> {
        self.0.get(key)
    }

    /// 执行体预取后登记；瞬时 IO 错误不登记。
    pub fn insert(&mut self, key: ProposalRef, text: ProposalText) {
        self.0.insert(key, text);
    }

    pub fn contains(&self, key: &ProposalRef) -> bool {
        self.0.contains_key(key)
    }
}

/// 取自 `AttemptState` 的 `result_ref` 与 `accepted_open_pr`。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProposalRef {
    pub result_ref: Sha256Digest,
    pub proposal_id: String,
}

impl ProposalRef {
    /// 尝试带被接受的提议时的键；没有 `accepted_open_pr` 或 `result_ref` 时为 `None`。
    pub fn of(attempt: &crate::state::AttemptState) -> Option<Self> {
        Some(Self {
            result_ref: attempt.result_ref?,
            proposal_id: attempt.accepted_open_pr.clone()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalText {
    /// 原样保存；模型产出，作数据交 forge 执行时净化（I5）。
    Found(OpenPrParams),
    /// 确定性缺失：已读过 CAS，但取不到可用的该提议。
    Missing(ProposalMissing),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalMissing {
    /// CAS 中没有 `result_ref` 指向的对象。
    ObjectAbsent,
    /// 结果对象里没有该 `proposal_id`。
    NotInResult,
    /// 有该 id，但 kind ≠ open_pr 或 params 解码失败。
    Malformed,
}

// ---------------------------------------------------------------------------
// Decision（§7.14.5）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandRejection {
    InvalidState(&'static str),
    Validation(&'static str),
    NotSupportedYet(&'static str),
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Append(NewEvent),
    Raise(RaiseRequest),
    Resolve {
        target: ResolveTarget,
        how: Resolution,
    },
    Enqueue(EnqueueRequest),
    Reply(Result<(), CommandRejection>),
    Effect(Effect),
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    /// 执行器先 `resolve(base_ref)` 得到 base_sha 再 `create_worktree`（§7.14.5，#70 A）。
    EnsureWorktree {
        branch: BranchName,
        base_ref: String,
    },
    RemoveWorktree {
        worktree_id: WorktreeId,
        lock: Option<LockToken>,
    },
    AcquireLock {
        worktree_id: WorktreeId,
        node_id: NodeId,
        holder: LockHolder,
        lease_s: u64,
    },
    ReleaseLock {
        token: LockToken,
    },
    RenewLock {
        token: LockToken,
        lease_s: u64,
    },
    RunAttempt(Box<AttemptPlan>),
    CancelAttempt {
        attempt_id: AttemptId,
        why: CancelWhy,
    },
    ReleaseSandbox {
        attempt_id: AttemptId,
    },
    JudgeAction {
        key: ActionKey,
        request: ActionRequest,
    },
    RequestApproval {
        key: ActionKey,
        spec: ApprovalSpec,
    },
    InvalidateApproval {
        approval_id: ApprovalId,
        reason: InvalidateReason,
    },
    CancelOutbox {
        action_id: ActionId,
        reason: CancelReason,
    },
    ReconcileNow {
        target: ReconcileTarget,
    },
    ScheduleTimer {
        at: Timestamp,
        kind: TimerKind,
    },
}

/// 角色模板 + 用户亲手输入的原文（I5，W5）；只能由这两者构造。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedInstructions(String);

impl TrustedInstructions {
    pub fn compose(role_template: &'static str, user_text: &str) -> Self {
        Self(format!("{role_template}\n\n{user_text}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `RunAttempt` 的全部输入；AttemptRunner 不再回读状态。
#[derive(Debug, PartialEq)]
pub struct AttemptPlan {
    pub attempt_id: AttemptId,
    pub node_id: NodeId,
    pub workflow_id: WorkflowId,
    pub role: Role,
    pub exec: Exec,
    pub critical: bool,
    pub n: u32,
    pub base_sha: GitOid,
    pub pr_base_sha: Option<GitOid>,
    pub tokens_total: u64,
    pub wall_time_s: u64,
    pub model: ResolvedModel,
    pub upstream: Vec<NodeId>,
    pub previous: Vec<(AttemptId, FailureClass, Option<FailureDetail>)>,
    pub instructions: TrustedInstructions,
    pub refs: Vec<Ref>,
    pub lock: Option<LockToken>,
    pub resume: bool,
}
