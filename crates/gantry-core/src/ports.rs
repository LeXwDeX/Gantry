//! 跨模块端口（P-51）。
//!
//! 共 16 组端口、17 个 trait（`SecretSource` / `SecretSink` 为两组）、115 个
//! 方法。`gantryd` 在装配时注入实现；测试替身实现同一组 trait。
//!
//! 设计约束：
//! - `core` 只依赖 `async-trait` 与 `futures-core`，无 tokio、无 IO；
//! - 所有 trait 可 `dyn`；
//! - 跨模块写接口统一接收 [`ActorCtx`]（P-78），不提供 `WriteCtx` 等别名；
//! - 端口错误可分支，不含 HTTP 协议；
//! - 已存在的实体、ID、usage、settings 类型直接复用，不在端口重复定义。
//!
//! 契约缺口见 `docs/m0-implementation.md` 与各子模块顶部注释；本模块不把
//! 未定义的业务形状宣称为已冻结。

pub mod context;
pub mod conversation;
pub mod egress;
pub mod engine;
pub mod forge;
pub mod inbox;
pub mod models;
pub mod policy;
pub mod repo;
pub mod sandbox;
pub mod secrets;
pub mod worker;

/// 端口组数（core §6.10）。
pub const PORT_GROUP_COUNT: usize = 16;

/// trait 数（`ForgeReader` … `SecretSink`）。
pub const PORT_TRAIT_COUNT: usize = 17;

/// 方法总数。
pub const PORT_METHOD_COUNT: usize = 115;

pub use context::{
    ActorCtx, CasRef, ClientProfile, Health, ModuleError, Page, PageOrder, PageQuery, PageWithSeq,
    RequestId, StoreError, TxHandle, ACTOR_CTX_FIELDS,
};

pub use conversation::{ConvError, ConversationService, NewUserMessage};

pub use egress::StagedSecret as EgressStagedSecret;
pub use egress::{
    AppliedConfig, CallUsage, EgressConfigView, EgressControl, EgressEndpoints, EgressError,
    EgressStatus, FinalUsage, InjectionId, IssueJobToken, JobTokenGrant, JobUsage, ProviderId,
    StageSecret, StagedInfo, SvcTokenGrant, UsageCursor, EGRESS_SECRET_CONSUMER,
};

pub use engine::{
    AnswerRequest, AnswerTicket, CancelWhy, CiClassifyRequest, ClassifyError, ClassifyPort,
    CreateDelivery, DeliveryControl, EngineError, FindingClassifyRequest, FindingTriage,
    Fingerprint, Glob, HealOutcome, HealPort, HealRequest, HealTask, HealTicket, ModuleStatus,
    PrRef, ProblemSpec, ResultRef, RoundId, TemplateCatalog, ThreadTarget, ValidatedProposal,
};

pub use forge::{
    Annotation, AnnotationLevel, CancelOutcome, CancelReason, ChangedFile, CiLog, CiLogQuery,
    Discussion, DiscussionComment, DiscussionQuery, DiscussionTarget, DriftKind, EnqueueError,
    EnqueueOutcome, EnqueueRequest, EnqueuedBy, EnqueuedByModule, ForgeError, ForgeReader,
    MirrorWant, OutboxEnqueueEnvelope, OutboxPort, PrRefresher, ProjectSnapshot, ReconcileReport,
    ReconcileTarget, ReviewThreadComment, ReviewThreadView, ThreadNodeId,
};

pub use inbox::{
    validate_raise_kind, ApnsEnvironment, ChooseError, DedupKey, InboxError, InboxPort, InboxQuery,
    InboxSummary, Note, OptionId, ProjectOpenCount, PushToken, RaiseError, RaiseOutcome,
    RaiseRequest, Resolution, ResolveError, ResolveTarget, SafeSummary, Written,
};

pub use models::{
    CallId, EgressRoute, LedgerId, Material, ModelProvider, ProviderEffort, ProviderError,
    ProviderRequest, ProviderResponse, StopReason, UnsupportedEffort, UsageSource,
};

pub use policy::{
    ActionDecision, ActionFacts, ActionRequest, AddedLineSource, ApprovalSpec, DenyReason,
    Disposition, ExecutionCheck, ExecutionPermit, GateError, InvalidateReason, MismatchReason,
    PatchApprovalCheck, PatchPermit, PatchPolicyVerdict, PathRuleHit, PathRuleSet, PolicyError,
    PolicyPort, ProposalContext, ProposalDecision, ReportHint, RuleId, SecretFinding,
};

pub use repo::{
    BranchName, CommitInfo, CommitMessage, CommitPatch, CommitRecord, CreateWorktree,
    DeclaredPatch, LockError, LockInfo, LockToken, MirrorGuard, MirrorHealth, MirrorInfo,
    MirrorMode, PatchOutcome, PathRulesSnapshot, PrepareWorkspace, PreparedWorkspace, RepoError,
    RepoService, RevSpec, ValidatePatch, WorkspaceStats, WorktreeLocks,
};

pub use sandbox::{
    ArtifactName, CapacitySnapshot, CasObject, DriverKind, EgressFlags, EgressRequest, ExecSpec,
    GroupName, InfraDetail, IoLimits, JobHandle, JobOutcome, JobSpecTrusted, ProfileName,
    ProgressSample, ProgressStats, RawMaterial, RecoveredJob, SandboxError, SandboxService,
    SandboxSpec, SweepReport, TerminalCause, TerminalFacts, TimerKind, Timers, TokenEnvSpec, Trust,
    UnexpectedArtifact, UnfinishedAttempt, UpstreamInput, WorkspaceMode, WorkspaceRequest,
};

pub use secrets::StagedSecret as SecretsStagedSecret;
pub use secrets::{
    CorruptReason, Expect, PutOutcome, Secret, SecretMeta, SecretSink, SecretSource, SecretValue,
    SecretVersion, SecretsError,
};

pub use worker::{AdapterCaps, AdapterError, ImageRef, ProtocolVersion, WorkerAdapter};
