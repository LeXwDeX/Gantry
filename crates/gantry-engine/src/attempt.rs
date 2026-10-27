//! AttemptRunner：派发与终结管线（engine.md §7.8、§7.14.5；worker-protocol §4、§5、§8、§9，#72）。
//!
//! 输入是 [`AttemptPlan`]（`attempt.started` 已提交）与交付单元上下文 [`AttemptContext`]；
//! 输出是 [`Finalized`]，由执行体以 `Report::AttemptFinalized` 交回 `decide`。Runner 不写事件、
//! 不回读状态（§7.14.5 "AttemptRunner 不再回读状态"）。
//!
//! 信任边界（I5、W5）：
//! - `task.instructions` 只来自 `plan.instructions`（[`TrustedInstructions`]）；`task.title` 由服务端
//!   模板生成；`context` 只含码值。
//! - Issue / PR 正文与评论经 `ForgeReader::discussion` 取得，只放进 `SandboxSpec.raw_materials`
//!   （P-59），由 `sandbox.stage` 规范化、截断、隔离包装。
//! - 分类只用运行时事实（`TerminalFacts`、`EgressFlags`）与 V3–V5 结论；worker 自报的进度与
//!   `usage_reported` 不改变结论；用量取 `JobOutcome` 的 Egress 计量。
//!
//! 已知近似（规格缺口，见最终报告）：
//! - V3 的 `job_sha256` 无法核对：`sandbox.stage` 重新生成 nonce 并填 `materials` 后才算出
//!   `job.json` 的字节，`JobOutcome` 不回传该摘要。
//! - `exec = direct`（`models.invoke_direct`）不在本切片；收到时返回缺陷错误。
//! - 进度采样转发为 `attempt.progress`（展示用）不在本切片。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::canon::{jcs, sha256, GitOid, RepoSlug, Sha256Digest};
use gantry_core::entities::{
    role_profile, AttemptStatus, Exec, FailureClass, FailureDetail, PatchRef, Ref, Role,
    SandboxLimits, Seq, TokenUsage, TokenWeights, Verdict,
};
use gantry_core::ids::{AttemptId, Clock, DeliveryId, Entropy, NodeId, ProjectId, SandboxId};
use gantry_core::level::Level;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::forge::{
    Discussion, DiscussionQuery, DiscussionTarget, ForgeError, ForgeReader,
};
use gantry_core::ports::policy::{PolicyError, PolicyPort, ProposalContext, ProposalDecision};
use gantry_core::ports::repo::{
    CommitMessage, CommitPatch, DeclaredPatch, PatchOutcome, RepoError, RepoService, ValidatePatch,
};
use gantry_core::ports::sandbox::{
    ArtifactName, DriverKind, EgressRequest, ExecSpec, GroupName, InfraDetail, IoLimits,
    JobOutcome, ProfileName, RawMaterial, RecoveredJob, SandboxError, SandboxService, SandboxSpec,
    TerminalCause, Timers, Trust, UnfinishedAttempt, UpstreamInput, WorkspaceMode,
    WorkspaceRequest,
};
use gantry_core::ports::worker::WorkerAdapter;
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use gantry_core::worker::{
    AgentId, BundleEgressFlags, BundleFileName, BundleUsageMetered, FileArtifact, JobAttempt,
    JobConstraints, JobContext, JobExecution, JobIsolation, JobLimits, JobNode, JobOutput, JobSpec,
    JobTask, JobWorkflow, JobWorkspace, MaterialKind, PatchLimits, PatchMode, PatchRejectReason,
    PatchValidationSummary, PatchVerdict, PreviousAttempt, Proposal, ProposalDisposition,
    ProposalDispositionEntry, ProposalKind, ProposalParams, Proposals, ProposalsMode, ReportMode,
    ResultBundle, ResultBundleVersion, ResultClassification, ResultJson, ResultLogs, ResultStatus,
    ResultTerminal, ResultTerminalCause, ResultTimerKind, ResultValidation, StructuredOutput,
    StructuredSchema, ToolConstraints, UpstreamRef, ValidatedPatch, WorkerProtocol,
};

use crate::classify::{classify, Classification, PatchCheck, RunFacts};
use crate::decide::{AttemptPlan, EffectErrorClass, Finalized, OutputSignals, PatchResult};
use crate::decide::{ProposalMissing, ProposalOutcome, ProposalRef, ProposalText};
use crate::state::DeliveryState;

/// 隔离标记模板（worker-protocol §4.4）；标记行只含服务端生成的值。
pub const ISOLATION_BEGIN: &str = "<<<GANTRY-UNTRUSTED-BEGIN nonce={nonce} id={id} kind={kind} source={source} truncated={truncated}>>>";
pub const ISOLATION_END: &str = "<<<GANTRY-UNTRUSTED-END nonce={nonce} id={id}>>>";

/// 沙盒内工作区路径（worker-protocol §4.2，固定值）。
const WORKSPACE_PATH: &str = "/gantry/work";
/// 上游产物在沙盒内的挂载目录（worker-protocol §2.1）。
const UPSTREAM_DIR: &str = "/gantry/inputs/upstream";

const RESULT_JSON: &str = "result.json";
const CHANGES_PATCH: &str = "changes.patch";
const REPORT_MD: &str = "report.md";
const PROPOSALS_JSON: &str = "proposals.json";
const PROGRESS_JSONL: &str = "progress.jsonl";

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// 组装 `SandboxSpec` / `JobSpec` 所需的配置快照（engine.md §7.8 第 3 步、§10）。
///
/// 由调用方从同一份 `ConfigView` 一次取齐：`config_rev` 是这份快照的修订，原样进
/// `SandboxSpec.config_rev`（#220/#228），因此 `weights`、`limits`、`timers`、`io` 必须与它同源。
/// Runner 在派发开始时持有一个 `Arc` 快照，派发过程中调用方换配置不影响已在组装的 spec。
#[derive(Clone, Debug, PartialEq)]
pub struct RunnerConfig {
    /// 本快照所属 `ConfigView` 的修订。
    pub config_rev: Seq,
    /// `workspace.repo`：项目仓库（`EngineSettings.project.repo`）。
    pub repo: RepoSlug,
    /// `sandbox.driver`。
    pub driver: DriverKind,
    /// 可信 / 不可信任务；没有设置键（P-09：第一版不接收不可信 fork 代码），由装配方决定。
    pub trust: Trust,
    /// `sandbox.limits.*`；`wall_time_s` 会被计划的有效墙钟收紧。
    pub limits: SandboxLimits,
    /// `sandbox.idle_timeout_s` / `sandbox.kill_grace_s`。
    pub timers: Timers,
    /// `sandbox.io.*`；`materials.max_total_bytes` 也作 `discussion` 拉取的字节上限。
    pub io: IoLimits,
    /// `egress.default_profile`（或项目覆盖的档案名）。
    pub egress_profile: ProfileName,
    /// `egress.profiles.<档案>.groups`。
    pub egress_groups: Vec<GroupName>,
    /// `levels.token_weights`（P-70）。
    pub token_weights: TokenWeights,
    /// `models.worker.max_turns`（`ResolvedModel` 尚未携带它，config §3.2）。
    pub max_turns: u32,
    /// 单次 `discussion` 最多取多少条评论；没有设置键（P-59 只说"按需拉取"）。
    pub discussion_max_comments: u32,
    /// 各 role 的输出约束；没有设置键（内置注册表，sandbox stage 的契约缺口）。
    pub roles: HashMap<Role, RoleOutput>,
}

/// 一个 role 的 `output.*`、`constraints.tools` 与补丁限额（worker-protocol §4.2、§4.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleOutput {
    pub structured_schema: StructuredSchema,
    pub patch: PatchMode,
    pub report: ReportMode,
    pub proposals: ProposalsMode,
    pub tools: ToolConstraints,
    /// `policies.patch.max_bytes` / `max_files`；`patch = forbidden` 时为 `None`。
    pub patch_limits: Option<PatchLimits>,
}

/// 交付单元上下文：Runner 不回读状态，执行体在发起 `RunAttempt` 时从状态取齐。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptContext {
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub level_effective: Level,
    pub graph_rev: u32,
    pub revision_round: u32,
    pub required: bool,
    /// `plan.upstream` 各节点最近一次成功尝试的结果引用。
    pub upstream: Vec<UpstreamSource>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamSource {
    pub node_id: NodeId,
    pub role: Role,
    pub attempt_id: AttemptId,
    pub result_ref: Sha256Digest,
}

impl AttemptContext {
    /// 从 `DeliveryState` 取齐上下文；计划引用的 Workflow / 节点不在状态里视为缺陷。
    pub fn from_state(state: &DeliveryState, plan: &AttemptPlan) -> Result<Self, RunnerError> {
        let workflow = state
            .workflows
            .get(&plan.workflow_id)
            .ok_or(RunnerError::Defect("plan_workflow_missing"))?;
        let node = workflow
            .nodes
            .get(&plan.node_id)
            .ok_or(RunnerError::Defect("plan_node_missing"))?;
        let mut upstream = Vec::new();
        for id in &plan.upstream {
            let Some(up) = state.node(id) else {
                return Err(RunnerError::Defect("upstream_node_missing"));
            };
            let latest = up.attempts.iter().rev().find(|attempt| {
                attempt.status == AttemptStatus::Succeeded && attempt.result_ref.is_some()
            });
            let Some(attempt) = latest else {
                return Err(RunnerError::Defect("upstream_without_result"));
            };
            upstream.push(UpstreamSource {
                node_id: up.id,
                role: up.role.clone(),
                attempt_id: attempt.id,
                result_ref: attempt.result_ref.expect("filtered above"),
            });
        }
        Ok(Self {
            project_id: state.delivery.project_id,
            delivery_id: state.delivery.id,
            level_effective: state.delivery.level_effective,
            graph_rev: workflow.graph_rev,
            revision_round: workflow.round,
            required: node.required,
            upstream,
        })
    }
}

// ---------------------------------------------------------------------------
// CAS
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CasError {
    /// 读写暂时失败（IO、锁）；可重试。
    #[error("cas unavailable: {0}")]
    Unavailable(String),
    /// 对象超过调用方给的上限。
    #[error("cas object too large: {bytes} > {max}")]
    TooLarge { bytes: u64, max: u64 },
    /// 对象与摘要不符等确定性错误。
    #[error("cas corrupt: {0}")]
    Corrupt(String),
}

/// Runner 用到的内容寻址存储子集（store.md §6.4）：读沙盒产物、写结果清单与被接受提议的文本。
#[async_trait]
pub trait ResultCas: Send + Sync {
    /// 不存在 → `None`；超过 `max` → `TooLarge`。
    async fn get(&self, digest: &Sha256Digest, max: usize) -> Result<Option<Vec<u8>>, CasError>;
    async fn put_artifact(&self, bytes: &[u8]) -> Result<Sha256Digest, CasError>;
    async fn put_bundle(&self, bundle: &ResultBundle) -> Result<Sha256Digest, CasError>;
}

fn store_cas_error(error: gantry_store::StoreError) -> CasError {
    match error {
        gantry_store::StoreError::CasTooLarge { bytes, max } => CasError::TooLarge { bytes, max },
        gantry_store::StoreError::Corrupt(message) => CasError::Corrupt(message),
        other => CasError::Unavailable(other.to_string()),
    }
}

#[async_trait]
impl ResultCas for gantry_store::Cas {
    async fn get(&self, digest: &Sha256Digest, max: usize) -> Result<Option<Vec<u8>>, CasError> {
        self.get_bytes(digest, max).await.map_err(store_cas_error)
    }

    async fn put_artifact(&self, bytes: &[u8]) -> Result<Sha256Digest, CasError> {
        self.put(bytes, gantry_store::CasKind::Artifact)
            .await
            .map_err(store_cas_error)
    }

    async fn put_bundle(&self, bundle: &ResultBundle) -> Result<Sha256Digest, CasError> {
        gantry_store::Cas::put_bundle(self, bundle)
            .await
            .map_err(store_cas_error)
    }
}

/// 提议原文预取（engine.md §7.14.2 "提议原文预取"，worker-protocol §5.2 `text_ref`）：
/// 读 `result_ref` 指向的清单，按 `proposal_id` 找到条目再读其 `text_ref`。
/// 瞬时错误返回 `Err`，调用方不登记表项。
pub async fn fetch_proposal_text(
    cas: &dyn ResultCas,
    key: &ProposalRef,
    max_bytes: usize,
) -> Result<ProposalText, CasError> {
    let Some(bytes) = cas.get(&key.result_ref, max_bytes).await? else {
        return Ok(ProposalText::Missing(ProposalMissing::ObjectAbsent));
    };
    let Ok(bundle) = serde_json::from_slice::<ResultBundle>(&bytes) else {
        return Ok(ProposalText::Missing(ProposalMissing::ObjectAbsent));
    };
    let entry = bundle
        .validation
        .as_ref()
        .and_then(|v| v.proposals.iter().find(|p| p.id == key.proposal_id));
    let Some(entry) = entry else {
        return Ok(ProposalText::Missing(ProposalMissing::NotInResult));
    };
    let (ProposalKind::OpenPr, Some(text_ref)) = (entry.kind, &entry.text_ref) else {
        return Ok(ProposalText::Missing(ProposalMissing::Malformed));
    };
    let Some(text) = cas.get(&text_ref.sha256, max_bytes).await? else {
        return Ok(ProposalText::Missing(ProposalMissing::Malformed));
    };
    match serde_json::from_slice(&text) {
        Ok(params) => Ok(ProposalText::Found(params)),
        Err(_) => Ok(ProposalText::Missing(ProposalMissing::Malformed)),
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// Runner 未能得出 `Finalized` 的原因。执行体把它回报为 `EffectFailed{Run}`；尝试仍未终结，
/// `Tick(Recover)` 以 `resume = true` 重发（各端口按 `job_id` 幂等）。
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("sandbox: {0}")]
    Sandbox(SandboxError),
    #[error("repo: {0}")]
    Repo(RepoError),
    #[error("policy: {0}")]
    Policy(PolicyError),
    #[error("forge: {0}")]
    Forge(ForgeError),
    #[error("cas: {0}")]
    Cas(CasError),
    /// 引擎或装配缺陷（计划与状态不符、配置缺 role 等）；重试无意义。
    #[error("defect: {0}")]
    Defect(&'static str),
}

impl RunnerError {
    pub fn class(&self) -> EffectErrorClass {
        match self {
            RunnerError::Sandbox(error) => sandbox_error_class(error),
            RunnerError::Repo(error) => repo_error_class(error),
            RunnerError::Policy(error) => policy_error_class(error),
            RunnerError::Forge(error) => forge_error_class(error),
            RunnerError::Cas(CasError::Unavailable(_)) => EffectErrorClass::Retryable,
            RunnerError::Cas(_) => EffectErrorClass::Defect {
                what: "cas_object_invalid",
            },
            RunnerError::Defect(what) => EffectErrorClass::Defect { what },
        }
    }
}

pub(crate) fn sandbox_error_class(error: &SandboxError) -> EffectErrorClass {
    match error {
        SandboxError::Infra { .. } | SandboxError::ShuttingDown | SandboxError::Store(_) => {
            EffectErrorClass::Retryable
        }
        SandboxError::InvalidSpec(_) => EffectErrorClass::Defect {
            what: "sandbox_invalid_spec",
        },
        SandboxError::NotFound => EffectErrorClass::Defect {
            what: "sandbox_job_not_found",
        },
    }
}

pub(crate) fn repo_error_class(error: &RepoError) -> EffectErrorClass {
    match error {
        RepoError::MirrorMissing
        | RepoError::MirrorNeedsRebuild
        | RepoError::BaseNotInMirror { .. }
        | RepoError::WorktreeLocked { .. }
        | RepoError::Git { .. }
        | RepoError::Timeout { .. }
        | RepoError::Io { .. }
        | RepoError::Store(_)
        | RepoError::Lock(_) => EffectErrorClass::Retryable,
        RepoError::CommitIdentityMissing => EffectErrorClass::Defect {
            what: "commit_identity_missing",
        },
        RepoError::WorktreeNotFound => EffectErrorClass::Defect {
            what: "worktree_not_found",
        },
        RepoError::LockRequired | RepoError::StaleLock => EffectErrorClass::Defect {
            what: "worktree_lock_invalid",
        },
        _ => EffectErrorClass::Defect {
            what: "repo_rejected",
        },
    }
}

pub(crate) fn policy_error_class(error: &PolicyError) -> EffectErrorClass {
    match error {
        PolicyError::Store(_) | PolicyError::ApprovalUnavailable { .. } => {
            EffectErrorClass::Retryable
        }
        PolicyError::NotConfigured => EffectErrorClass::Defect {
            what: "policy_not_configured",
        },
        _ => EffectErrorClass::Defect {
            what: "policy_rejected",
        },
    }
}

pub(crate) fn forge_error_class(error: &ForgeError) -> EffectErrorClass {
    match error {
        ForgeError::RateLimited { .. } | ForgeError::Unavailable { .. } | ForgeError::Store(_) => {
            EffectErrorClass::Retryable
        }
        ForgeError::GitHub { .. } => EffectErrorClass::Retryable,
        ForgeError::NotFound | ForgeError::InvalidTarget { .. } => EffectErrorClass::Defect {
            what: "forge_target_invalid",
        },
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Runner 依赖的端口（全部经 `core::ports`，engine 不依赖对方 crate）。
#[derive(Clone)]
pub struct AttemptPorts {
    pub sandbox: Arc<dyn SandboxService>,
    pub repo: Arc<dyn RepoService>,
    pub policy: Arc<dyn PolicyPort>,
    pub forge: Arc<dyn ForgeReader>,
    pub cas: Arc<dyn ResultCas>,
    pub adapter: Arc<dyn WorkerAdapter>,
    pub clock: Arc<dyn Clock>,
    /// `isolation.nonce` 的随机源（sandbox 在 staging 时还会用自己的 CSPRNG 重新生成）。
    pub entropy: Arc<dyn Entropy>,
}

pub struct AttemptRunner {
    ports: AttemptPorts,
    config: Arc<RunnerConfig>,
}

/// 沙盒 ID 由尝试 ID 确定性派生：`submit` 按 `(job_id, sandbox_id)` 幂等，恢复时
/// "以同一 `job_id` / `sandbox_id` 重新 `submit`"（§7.13，E4）不需要额外持久化。
pub fn sandbox_id_for(attempt: &AttemptId) -> SandboxId {
    SandboxId::from_parts(attempt.timestamp_ms(), attempt.random80())
        .expect("attempt id parts are within ULID range")
}

/// V3 的结论。
struct ResultCheck {
    result: Option<ResultJson>,
    protocol_ok: bool,
    structured: Option<serde_json::Value>,
}

/// V5 的结论。
struct PatchStep {
    check: PatchCheck,
    result: PatchResult,
    protocol_ok: bool,
    summary: Option<PatchValidationSummary>,
}

impl AttemptRunner {
    pub fn new(ports: AttemptPorts, config: Arc<RunnerConfig>) -> Self {
        Self { ports, config }
    }

    pub fn config(&self) -> &Arc<RunnerConfig> {
        &self.config
    }

    /// 派发并终结一个尝试（§7.8 第 3–4 步与终结管线）。
    pub async fn run(
        &self,
        plan: &AttemptPlan,
        cx: &AttemptContext,
        ctx: &ActorCtx,
    ) -> Result<Finalized, RunnerError> {
        if plan.exec == Exec::Direct {
            return Err(RunnerError::Defect("direct_exec_not_supported"));
        }
        let sandbox_id = sandbox_id_for(&plan.attempt_id);
        let mut need_submit = true;
        if plan.resume {
            let unfinished = [UnfinishedAttempt {
                job_id: plan.attempt_id,
                sandbox_id,
                project_id: cx.project_id,
            }];
            let recovered = self
                .ports
                .sandbox
                .recover(ctx, &unfinished)
                .await
                .map_err(RunnerError::Sandbox)?;
            match recovered.first() {
                Some(RecoveredJob::Running(_) | RecoveredJob::Exited(_)) => need_submit = false,
                Some(RecoveredJob::Lost(_)) => {
                    return Ok(self.infra_failure(plan, InfraDetail::Lost));
                }
                Some(RecoveredJob::Unknown { .. }) | None => {}
            }
        }
        let job = if need_submit {
            let spec = self.assemble(plan, cx, ctx).await?;
            let job = spec.job.clone();
            match self.ports.sandbox.submit(ctx, spec).await {
                Ok(_) => {}
                Err(SandboxError::Infra { detail, .. }) => {
                    return Ok(self.infra_failure(plan, detail));
                }
                Err(error) => return Err(RunnerError::Sandbox(error)),
            }
            Some(job)
        } else {
            None
        };
        let outcome = match self.ports.sandbox.wait(&plan.attempt_id).await {
            Ok(outcome) => outcome,
            Err(SandboxError::Infra { detail, .. }) => {
                return Ok(self.infra_failure(plan, detail));
            }
            Err(error) => return Err(RunnerError::Sandbox(error)),
        };
        self.finalize(plan, cx, ctx, &outcome, job.as_ref()).await
    }

    /// 组装 `SandboxSpec`（§7.8 第 3 步）。公开以便测试断言隔离与配置快照。
    pub async fn assemble(
        &self,
        plan: &AttemptPlan,
        cx: &AttemptContext,
        _ctx: &ActorCtx,
    ) -> Result<SandboxSpec, RunnerError> {
        let config = &self.config;
        let role = config
            .roles
            .get(&plan.role)
            .ok_or(RunnerError::Defect("role_output_not_configured"))?;
        let (tier, _) = role_profile(&plan.role);
        let agent: AgentId = self
            .ports
            .adapter
            .agent_id()
            .parse()
            .map_err(|_| RunnerError::Defect("adapter_agent_id_invalid"))?;
        let read_only = plan.exec == Exec::SandboxRo;
        let mut tools = role.tools.clone();
        let mut patch_mode = role.patch;
        if read_only {
            // worker-protocol §4.3：sandbox_ro 节点 edit = false、patch = forbidden。
            tools.edit = false;
            patch_mode = PatchMode::Forbidden;
        }
        let patch_limits = if patch_mode == PatchMode::Forbidden {
            None
        } else {
            role.patch_limits.clone()
        };
        let mut upstream_inputs = Vec::new();
        let mut job_upstream: Vec<UpstreamRef> = Vec::new();
        for source in &cx.upstream {
            let files = self.bundle_files(&source.result_ref).await?;
            job_upstream.push(UpstreamRef {
                node_id: source.node_id,
                role: source.role.clone(),
                attempt_id: source.attempt_id,
                dir: format!("{UPSTREAM_DIR}/{}", source.node_id),
                files: files.clone(),
                reference: source.result_ref,
            });
            upstream_inputs.push(UpstreamInput {
                node_id: source.node_id,
                role: source.role.clone(),
                attempt_id: source.attempt_id,
                reference: source.result_ref,
                files,
            });
        }
        let job = JobSpec {
            protocol: WorkerProtocol,
            job_id: plan.attempt_id,
            created_at: self.ports.clock.now(),
            project_id: cx.project_id,
            delivery_id: cx.delivery_id,
            workflow: JobWorkflow {
                id: plan.workflow_id,
                graph_rev: cx.graph_rev,
            },
            node: JobNode {
                id: plan.node_id,
                role: plan.role.clone(),
                exec: plan.exec,
                required: cx.required,
            },
            attempt: JobAttempt { n: plan.n },
            context: JobContext {
                level_effective: cx.level_effective,
                revision_round: cx.revision_round,
                previous_attempts: plan
                    .previous
                    .iter()
                    .map(|(attempt_id, class, detail)| PreviousAttempt {
                        attempt_id: *attempt_id,
                        failure_class: *class,
                        failure_detail: detail.clone(),
                    })
                    .collect(),
            },
            execution: JobExecution {
                agent,
                tier,
                model: plan.model.model.clone(),
                reasoning_effort: plan.model.reasoning_effort,
                agent_options: BTreeMap::new(),
            },
            task: JobTask {
                title: task_title(&plan.role, plan.n),
                instructions: plan.instructions.as_str().to_owned(),
                output_language: None,
            },
            workspace: JobWorkspace {
                path: WORKSPACE_PATH.to_owned(),
                repo: config.repo.clone(),
                base_sha: plan.base_sha,
                pr_base_sha: plan.pr_base_sha,
            },
            isolation: JobIsolation {
                nonce: self.nonce(),
                begin: ISOLATION_BEGIN.to_owned(),
                end: ISOLATION_END.to_owned(),
            },
            materials: Vec::new(),
            upstream: job_upstream,
            targets: Vec::new(),
            constraints: JobConstraints {
                tools,
                paths: self.ports.policy.path_rules_snapshot(&cx.project_id),
                limits: JobLimits {
                    wall_time_s: plan.wall_time_s,
                    idle_timeout_s: config.timers.idle_timeout_s,
                    max_turns: config.max_turns,
                    tokens_total: plan.tokens_total,
                },
                patch: patch_limits,
            },
            output: JobOutput {
                structured_schema: role.structured_schema.clone(),
                patch: patch_mode,
                report: role.report,
                proposals: role.proposals.clone(),
            },
        };
        job.validate()
            .map_err(|_| RunnerError::Defect("job_spec_invalid"))?;
        let raw_materials = self.materials(plan, cx).await?;
        let adapter = &self.ports.adapter;
        let exec = ExecSpec {
            image_digest: adapter.image().digest,
            argv: adapter.command(),
            env: adapter.env(&job),
            token_env: adapter.token_env(),
        };
        let mut limits = config.limits.clone();
        limits.wall_time_s = limits.wall_time_s.min(plan.wall_time_s);
        Ok(SandboxSpec {
            job_id: plan.attempt_id,
            sandbox_id: sandbox_id_for(&plan.attempt_id),
            project_id: cx.project_id,
            delivery_id: Some(cx.delivery_id),
            conversation_id: None,
            job,
            raw_materials,
            upstream: upstream_inputs,
            exec,
            driver: config.driver,
            trust: config.trust,
            limits,
            timers: config.timers,
            io: config.io.clone(),
            egress: EgressRequest {
                profile: config.egress_profile.clone(),
                groups: config.egress_groups.clone(),
                tokens_total: plan.tokens_total,
                weights: config.token_weights,
            },
            workspace: WorkspaceRequest {
                repo: config.repo.clone(),
                base_sha: plan.base_sha,
                pr_base_sha: plan.pr_base_sha,
                mode: if read_only {
                    WorkspaceMode::Ro
                } else {
                    WorkspaceMode::Rw
                },
            },
            config_rev: config.config_rev,
        })
    }

    /// 上游结果清单里可挂载的文件名（不含进度流）。
    async fn bundle_files(&self, result_ref: &Sha256Digest) -> Result<Vec<String>, RunnerError> {
        let max = usize::try_from(self.config.io.artifacts.result_max_bytes)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let bytes = self
            .ports
            .cas
            .get(result_ref, max)
            .await
            .map_err(RunnerError::Cas)?
            .ok_or(RunnerError::Defect("upstream_bundle_missing"))?;
        let bundle: ResultBundle = serde_json::from_slice(&bytes)
            .map_err(|_| RunnerError::Defect("upstream_bundle_invalid"))?;
        Ok(bundle
            .files
            .keys()
            .filter(|name| **name != BundleFileName::ProgressJsonl)
            .map(|name| bundle_file_name(*name).to_owned())
            .collect())
    }

    /// 32 位小写十六进制 nonce（worker-protocol §4.4 第 4 步）。
    fn nonce(&self) -> String {
        let mut bytes = [0u8; 16];
        self.ports.entropy.fill(&mut bytes);
        hex(&bytes)
    }

    /// Issue / PR 正文与评论（P-59）：经 `ForgeReader::discussion` 按需拉取，只进 `raw_materials`。
    async fn materials(
        &self,
        plan: &AttemptPlan,
        cx: &AttemptContext,
    ) -> Result<Vec<RawMaterial>, RunnerError> {
        let query = DiscussionQuery {
            include_body: true,
            comments_since: None,
            max_comments: self.config.discussion_max_comments,
            max_bytes: u32::try_from(self.config.io.materials.max_total_bytes).unwrap_or(u32::MAX),
        };
        let mut out = Vec::new();
        for reference in &plan.refs {
            let (repo, number, target, body_kind, comment_kind, tag) = match reference {
                Ref::Issue { repo, number } => (
                    repo,
                    *number,
                    DiscussionTarget::Issue(*number),
                    MaterialKind::IssueBody,
                    MaterialKind::IssueComment,
                    "issue",
                ),
                Ref::Pr { repo, number } => (
                    repo,
                    *number,
                    DiscussionTarget::PullRequest(*number),
                    MaterialKind::PrBody,
                    MaterialKind::PrComment,
                    "pr",
                ),
                Ref::Worktree { .. } | Ref::Delivery { .. } => continue,
            };
            let discussion = match self
                .ports
                .forge
                .discussion(&cx.project_id, target, query.clone())
                .await
            {
                Ok(discussion) => discussion,
                // 已删除或不可见：没有材料可给，不使派发失败。
                Err(ForgeError::NotFound) => continue,
                Err(error) => return Err(RunnerError::Forge(error)),
            };
            push_discussion(
                &mut out,
                repo,
                number,
                tag,
                body_kind,
                comment_kind,
                &discussion,
            );
        }
        Ok(out)
    }

    /// 本次运行组装 SandboxSpec 所用的修订与权重（`job_token` 口径，§7.14.2）。
    fn weight_audit(&self) -> gantry_core::events::WeightAudit {
        gantry_core::events::WeightAudit::JobToken {
            config_rev: self.config.config_rev.as_u64(),
            weights: self.config.token_weights,
        }
    }

    /// 沙盒基础设施失败：`infra.<步骤>`（worker-protocol §9），没有产物与计量。
    fn infra_failure(&self, plan: &AttemptPlan, detail: InfraDetail) -> Finalized {
        let code = match detail {
            InfraDetail::SandboxCreate => "infra.sandbox_create",
            InfraDetail::WorkspacePrep => "infra.workspace_prep",
            InfraDetail::Egress => "infra.egress",
            InfraDetail::Collect => "infra.collect",
            InfraDetail::Lost => "infra.lost",
        };
        Finalized {
            attempt_id: plan.attempt_id,
            node_id: plan.node_id,
            status: AttemptStatus::Failed,
            failure_class: Some(FailureClass::InfraError),
            failure_detail: FailureDetail::parse(code).ok(),
            result_ref: None,
            usage_metered: TokenUsage::default(),
            weighted_tokens: 0,
            weight_audit: self.weight_audit(),
            wall_time_s: 0,
            verdict: None,
            patch: PatchResult::None,
            proposals: Vec::new(),
            signals: OutputSignals::default(),
        }
    }

    /// 终结管线 V1、V3–V6 与分类（§7.8，worker-protocol §8.1）。
    async fn finalize(
        &self,
        plan: &AttemptPlan,
        cx: &AttemptContext,
        ctx: &ActorCtx,
        outcome: &JobOutcome,
        submitted: Option<&JobSpec>,
    ) -> Result<Finalized, RunnerError> {
        let role = self
            .config
            .roles
            .get(&plan.role)
            .ok_or(RunnerError::Defect("role_output_not_configured"))?;
        // V1：运行时事实。
        let terminal = outcome.terminal.clone();
        let egress_capped = outcome.egress.capped;
        let upstream_errors = outcome.egress.upstream_error_summary.is_some();
        // V3：result.json。退出码非 0 或被杀时 result.json 只作诊断，分类不看它。
        let mut check = self.check_result(plan, outcome).await?;
        // V4：structured_output 按 role 解码与一致性。
        let structured = decode_structured(&plan.role, check.structured.take());
        let result_failed = check
            .result
            .as_ref()
            .is_some_and(|r| r.status != ResultStatus::Succeeded);
        let output_ok = match (&check.result, &structured) {
            (Some(r), _) if r.status != ResultStatus::Succeeded => true,
            (Some(_), Some(_)) => true,
            _ => false,
        };
        let verdict = structured.as_ref().and_then(gate_verdict);
        let mut facts = RunFacts {
            terminal,
            egress_capped,
            upstream_errors,
            protocol_ok: check.protocol_ok,
            result_failed,
            output_ok,
            patch: PatchCheck::Clean,
            verdict: None,
        };
        // V5 / V6 只在 V1–V4 通过且 status = succeeded 时执行（worker-protocol §8.1）。
        let pre = classify(&facts);
        let mut patch = PatchStep {
            check: PatchCheck::Clean,
            result: PatchResult::None,
            protocol_ok: true,
            summary: None,
        };
        let mut proposals = Vec::new();
        let mut dispositions = Vec::new();
        let collected_at = collected_at(outcome, self.ports.clock.now());
        if pre.status == AttemptStatus::Succeeded {
            let result = check.result.as_ref().expect("protocol_ok implies result");
            patch = self
                .patch_step(plan, cx, ctx, role, result, outcome, collected_at)
                .await?;
            if patch.check == PatchCheck::Clean && patch.protocol_ok {
                let (outcomes, entries) = self
                    .proposal_step(plan, cx, ctx, role, result, outcome, &patch.result)
                    .await?;
                proposals = outcomes;
                dispositions = entries;
            }
        }
        facts.protocol_ok = facts.protocol_ok && patch.protocol_ok;
        facts.patch = patch.check.clone();
        facts.verdict = verdict;
        let classification = classify(&facts);
        let result_ref = self
            .write_bundle(
                plan,
                outcome,
                submitted,
                check.result.as_ref(),
                &classification,
                collected_at,
                patch.summary.clone(),
                dispositions,
            )
            .await?;
        let succeeded = classification.status == AttemptStatus::Succeeded;
        Ok(Finalized {
            attempt_id: plan.attempt_id,
            node_id: plan.node_id,
            status: classification.status,
            failure_class: classification.class,
            failure_detail: classification
                .detail
                .as_deref()
                .and_then(|d| FailureDetail::parse(d).ok()),
            result_ref,
            usage_metered: outcome.usage_metered,
            weighted_tokens: outcome.weighted_tokens,
            weight_audit: self.weight_audit(),
            wall_time_s: wall_time_s(outcome),
            verdict: if output_ok && check.protocol_ok {
                verdict
            } else {
                None
            },
            patch: patch.result,
            proposals: if succeeded { proposals } else { Vec::new() },
            signals: OutputSignals::default(),
        })
    }

    /// V3（worker-protocol §5.2、§8.1）。
    async fn check_result(
        &self,
        plan: &AttemptPlan,
        outcome: &JobOutcome,
    ) -> Result<ResultCheck, RunnerError> {
        let invalid = ResultCheck {
            result: None,
            protocol_ok: false,
            structured: None,
        };
        let Some(file) = outcome.files.get(&ArtifactName::new(RESULT_JSON)) else {
            return Ok(invalid);
        };
        let max = usize::try_from(self.config.io.artifacts.result_max_bytes).unwrap_or(usize::MAX);
        let bytes = match self.ports.cas.get(&file.sha256, max).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(CasError::TooLarge { .. }) | Err(CasError::Corrupt(_)) => {
                return Ok(invalid)
            }
            Err(error) => return Err(RunnerError::Cas(error)),
        };
        if bytes.len() as u64 != file.bytes || sha256(&bytes) != file.sha256 {
            return Ok(invalid);
        }
        let Ok(serde_json::Value::Object(mut map)) = serde_json::from_slice(&bytes) else {
            return Ok(invalid);
        };
        // structured_output 留给 V4：其 schema 失败是 exec.output_invalid，不是 exec.protocol。
        let structured = map.remove("structured_output").filter(|v| !v.is_null());
        let Ok(result) = serde_json::from_value::<ResultJson>(serde_json::Value::Object(map))
        else {
            return Ok(invalid);
        };
        let mut ok = result.job_id == plan.attempt_id && result.summary.chars().count() <= 1000;
        ok &= match result.status {
            ResultStatus::Succeeded => structured.is_some(),
            ResultStatus::Failed | ResultStatus::Cancelled => result.error.is_some(),
        };
        // 产物声明与实际一致（双向）：声明了就必须在 files 中且哈希 / 大小相符；有文件就必须声明。
        let artifacts = &result.artifacts;
        ok &= declared_matches(
            outcome,
            CHANGES_PATCH,
            artifacts
                .patch
                .as_ref()
                .map(|p| (p.path.as_str(), p.sha256, p.bytes)),
        );
        ok &= declared_matches(
            outcome,
            REPORT_MD,
            artifacts
                .report
                .as_ref()
                .map(|p| (p.path.as_str(), p.sha256, p.bytes)),
        );
        ok &= declared_matches(
            outcome,
            PROPOSALS_JSON,
            artifacts
                .proposals
                .as_ref()
                .map(|p| (p.path.as_str(), p.sha256, p.bytes)),
        );
        if let Some(patch) = &artifacts.patch {
            ok &= patch.base_sha == plan.base_sha;
        }
        Ok(ResultCheck {
            result: Some(result),
            protocol_ok: ok,
            structured,
        })
    }

    /// V5（engine.md §7.8；worker-protocol §8.3、§8.5）。
    #[allow(clippy::too_many_arguments)]
    async fn patch_step(
        &self,
        plan: &AttemptPlan,
        cx: &AttemptContext,
        ctx: &ActorCtx,
        role: &RoleOutput,
        result: &ResultJson,
        outcome: &JobOutcome,
        collected_at: Timestamp,
    ) -> Result<PatchStep, RunnerError> {
        let clean = |result| PatchStep {
            check: PatchCheck::Clean,
            result,
            protocol_ok: true,
            summary: None,
        };
        let Some(declared) = &result.artifacts.patch else {
            if role.patch == PatchMode::Required && plan.exec == Exec::Sandbox {
                return Ok(PatchStep {
                    check: PatchCheck::Invalid,
                    result: PatchResult::None,
                    protocol_ok: true,
                    summary: None,
                });
            }
            return Ok(clean(PatchResult::None));
        };
        if plan.exec != Exec::Sandbox {
            // 只读节点不该有补丁；intake 已丢弃，这里只作防线。
            return Ok(clean(PatchResult::None));
        }
        let file = outcome
            .files
            .get(&ArtifactName::new(CHANGES_PATCH))
            .expect("V3 checked the declared patch exists");
        let req = ValidatePatch {
            project_id: cx.project_id,
            delivery_id: cx.delivery_id,
            job_id: plan.attempt_id,
            worktree_id: plan.lock.as_ref().map(|lock| lock.worktree_id),
            base_sha: plan.base_sha,
            patch: file.sha256,
            declared: DeclaredPatch {
                sha256: declared.sha256,
                bytes: declared.bytes,
                base_sha: declared.base_sha,
                files_changed: declared.files_changed,
            },
            job_paths: self.ports.policy.path_rules_snapshot(&cx.project_id),
        };
        let verdict = self
            .ports
            .repo
            .validate_patch(ctx, req)
            .await
            .map_err(RunnerError::Repo)?;
        let summary = |validated: &ValidatedPatch, tree: Option<GitOid>| PatchValidationSummary {
            verdict: validated.verdict,
            sha256: file.sha256,
            result_tree: tree,
            files: u32::try_from(validated.files.len()).unwrap_or(u32::MAX),
        };
        match verdict {
            PatchOutcome::Validated(validated) if validated.verdict == PatchVerdict::Allow => {
                let lock = plan
                    .lock
                    .clone()
                    .ok_or(RunnerError::Defect("commit_without_lock"))?;
                let record = self
                    .ports
                    .repo
                    .commit_patch(
                        ctx,
                        CommitPatch {
                            project_id: cx.project_id,
                            job_id: plan.attempt_id,
                            worktree_id: lock.worktree_id,
                            lock,
                            approval: None,
                            message: commit_message(plan),
                            timestamp: collected_at,
                        },
                    )
                    .await;
                match record {
                    Ok(record) => Ok(PatchStep {
                        check: PatchCheck::Clean,
                        summary: Some(summary(&validated, Some(record.tree_oid))),
                        result: PatchResult::Committed {
                            commit: record.commit_oid,
                            tree: record.tree_oid,
                            sha256: record.patch_sha256,
                        },
                        protocol_ok: true,
                    }),
                    Err(RepoError::BaseMoved { branch_head, .. }) => Ok(PatchStep {
                        check: PatchCheck::BaseConflict,
                        result: PatchResult::BaseConflict { branch_head },
                        protocol_ok: true,
                        summary: Some(summary(&validated, validated.result_tree)),
                    }),
                    Err(error) => Err(RunnerError::Repo(error)),
                }
            }
            // M1 的 policy v0 把 needs_approval 按 deny 处理（fail-closed，P-19）；M2 起走 Held。
            PatchOutcome::Validated(validated) => {
                let detail = deny_detail(&validated);
                let mut paths: Vec<String> = validated
                    .files
                    .iter()
                    .map(|f| f.path.as_str().to_owned())
                    .collect();
                for reason in &validated.reasons {
                    if let Some(path) = &reason.path {
                        if !paths.iter().any(|p| p == path.as_str()) {
                            paths.push(path.as_str().to_owned());
                        }
                    }
                }
                Ok(PatchStep {
                    check: PatchCheck::Denied { detail },
                    summary: Some(PatchValidationSummary {
                        verdict: PatchVerdict::Deny,
                        sha256: file.sha256,
                        result_tree: None,
                        files: u32::try_from(validated.files.len()).unwrap_or(u32::MAX),
                    }),
                    result: PatchResult::Denied {
                        // ValidatedPatch 不带 ReportHint；decide 按 failure_detail 上报（§7.8）。
                        report: None,
                        patch: PatchRef {
                            reference: validated.sha256,
                            paths,
                        },
                    },
                    protocol_ok: true,
                })
            }
            PatchOutcome::Rejected { reason, .. } => {
                let (check, protocol_ok) = match reason {
                    PatchRejectReason::NotApplicable => (PatchCheck::Invalid, true),
                    // 声明与实际不符属于 V3（exec.protocol）。
                    PatchRejectReason::DeclaredMismatch => (PatchCheck::Clean, false),
                    other => (
                        PatchCheck::Denied {
                            detail: reject_detail(other).to_owned(),
                        },
                        true,
                    ),
                };
                Ok(PatchStep {
                    check,
                    result: PatchResult::Rejected { reason },
                    protocol_ok,
                    summary: None,
                })
            }
            PatchOutcome::BaseConflict { branch_head, .. } => Ok(PatchStep {
                check: PatchCheck::BaseConflict,
                result: PatchResult::BaseConflict { branch_head },
                protocol_ok: true,
                summary: None,
            }),
            // P11：声明了补丁但无变更，视为无补丁（OQ-15）。
            PatchOutcome::NoChange => Ok(clean(PatchResult::NoChange)),
        }
    }

    /// V6（worker-protocol §8.4）：本地检查（kind、数量、target、`open_pr` 依赖）后交 `policy`。
    #[allow(clippy::too_many_arguments)]
    async fn proposal_step(
        &self,
        plan: &AttemptPlan,
        cx: &AttemptContext,
        ctx: &ActorCtx,
        role: &RoleOutput,
        result: &ResultJson,
        outcome: &JobOutcome,
        patch: &PatchResult,
    ) -> Result<(Vec<ProposalOutcome>, Vec<ProposalDispositionEntry>), RunnerError> {
        if result.artifacts.proposals.is_none() {
            return Ok((Vec::new(), Vec::new()));
        }
        let Some(file) = outcome.files.get(&ArtifactName::new(PROPOSALS_JSON)) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let max =
            usize::try_from(self.config.io.artifacts.proposals_max_bytes).unwrap_or(usize::MAX);
        let bytes = match self.ports.cas.get(&file.sha256, max).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(CasError::TooLarge { .. }) | Err(CasError::Corrupt(_)) => {
                return Ok((Vec::new(), Vec::new()))
            }
            Err(error) => return Err(RunnerError::Cas(error)),
        };
        // 整份不合 schema：没有可逐条处置的提议，不使尝试失败（§8.4）。
        let Ok(parsed) = serde_json::from_slice::<Proposals>(&bytes) else {
            return Ok((Vec::new(), Vec::new()));
        };
        if parsed.job_id != plan.attempt_id {
            return Ok((Vec::new(), Vec::new()));
        }
        let (allowed, max_count) = match &role.proposals {
            ProposalsMode::Forbidden => (Vec::new(), 0),
            ProposalsMode::Allowed { allowed_kinds, max } => (allowed_kinds.clone(), *max),
        };
        let max_count = u64::from(max_count).min(self.config.io.artifacts.proposals_max_count);
        let committed = matches!(patch, PatchResult::Committed { .. });
        let mut entries = Vec::new();
        let mut candidates: Vec<Proposal> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for (index, proposal) in parsed.proposals.into_iter().enumerate() {
            let mut reasons = Vec::new();
            if !seen.insert(proposal.id.clone()) {
                // 清单要求 id 唯一；重复的条目直接丢弃。
                continue;
            }
            if proposal.validate().is_err() {
                reasons.push("params_invalid");
            }
            if !allowed.contains(&proposal.kind) {
                reasons.push("kind_not_allowed");
            }
            if index as u64 >= max_count {
                reasons.push("over_max");
            }
            // M1 的 JobSpec 不下发 targets 句柄：任何 target 都不在白名单。
            if proposal.target.is_some() {
                reasons.push("target_unknown");
            }
            if proposal.kind == ProposalKind::OpenPr && !committed {
                reasons.push("open_pr_without_patch");
            }
            if reasons.is_empty() {
                candidates.push(proposal);
            } else {
                entries.push(ProposalDispositionEntry {
                    id: proposal.id,
                    kind: proposal.kind,
                    disposition: ProposalDisposition::Invalid,
                    reasons: Some(reasons.into_iter().map(str::to_owned).collect()),
                    text_ref: None,
                });
            }
        }
        if candidates.is_empty() {
            return Ok((Vec::new(), entries));
        }
        let decisions = self
            .ports
            .policy
            .decide_proposals(
                ProposalContext {
                    project_id: cx.project_id,
                    delivery_id: cx.delivery_id,
                    attempt_id: plan.attempt_id,
                    patch: committed.then_some(PatchVerdict::Allow),
                    thread_has_human_comment: HashMap::new(),
                },
                &candidates,
                ctx,
            )
            .await
            .map_err(RunnerError::Policy)?;
        let mut outcomes = Vec::new();
        for proposal in candidates {
            let Some(decision) = decisions
                .iter()
                .find(|d| proposal_decision_id(d) == proposal.id)
                .cloned()
            else {
                continue;
            };
            let (disposition, reasons) = match &decision {
                ProposalDecision::Accepted { .. } => (ProposalDisposition::Accepted, None),
                ProposalDecision::NeedsApproval { .. } | ProposalDecision::Held { .. } => {
                    (ProposalDisposition::NeedsApproval, None)
                }
                ProposalDecision::Denied { reason, .. } => {
                    (ProposalDisposition::Denied, Some(vec![reason.to_string()]))
                }
            };
            let accepted = disposition == ProposalDisposition::Accepted;
            let text_ref = if accepted {
                // 被接受提议的文本另存（worker-protocol §5.2 text_ref）；原样保存，forge 执行时净化。
                let value = proposal_params_value(&proposal.params)
                    .ok_or(RunnerError::Defect("proposal_params_unserializable"))?;
                let bytes = jcs(&value).map_err(|_| RunnerError::Defect("proposal_params_jcs"))?;
                let digest = self
                    .ports
                    .cas
                    .put_artifact(&bytes)
                    .await
                    .map_err(RunnerError::Cas)?;
                Some(FileArtifact {
                    sha256: digest,
                    bytes: bytes.len() as u64,
                })
            } else {
                None
            };
            entries.push(ProposalDispositionEntry {
                id: proposal.id.clone(),
                kind: proposal.kind,
                disposition,
                reasons,
                text_ref,
            });
            let open_pr =
                (accepted && proposal.kind == ProposalKind::OpenPr).then(|| proposal.clone());
            outcomes.push(ProposalOutcome {
                proposal_id: proposal.id,
                decision,
                open_pr,
            });
        }
        Ok((outcomes, entries))
    }

    /// 组装并写入 `ResultBundle`（worker-protocol §5.2），返回 `result_ref`。
    #[allow(clippy::too_many_arguments)]
    async fn write_bundle(
        &self,
        plan: &AttemptPlan,
        outcome: &JobOutcome,
        submitted: Option<&JobSpec>,
        result: Option<&ResultJson>,
        classification: &Classification,
        collected_at: Timestamp,
        patch: Option<PatchValidationSummary>,
        proposals: Vec<ProposalDispositionEntry>,
    ) -> Result<Option<Sha256Digest>, RunnerError> {
        // job_sha256：sandbox 不回传 job.json 的摘要（契约缺口）。有合法 result.json 时取其声明，
        // 否则取引擎提交的 JobSpec 的 JCS 摘要（与 staging 后的字节不同，仅作关联）。
        let job_sha256 = match (result, submitted) {
            (Some(result), _) => result.job_sha256,
            (None, Some(job)) => serde_json::to_value(job)
                .ok()
                .and_then(|v| jcs(&v).ok())
                .map(|bytes| sha256(&bytes))
                .unwrap_or_else(|| sha256(plan.attempt_id.to_string().as_bytes())),
            (None, None) => sha256(plan.attempt_id.to_string().as_bytes()),
        };
        let mut files = BTreeMap::new();
        for (name, object) in &outcome.files {
            if let Some(key) = parse_bundle_file(name.as_str()) {
                files.insert(key, object.clone());
            }
        }
        if let Some(progress) = &outcome.progress {
            files.insert(BundleFileName::ProgressJsonl, progress.clone());
        }
        let validation = result.map(|_| ResultValidation {
            patch,
            proposals,
            unexpected_artifacts: None,
        });
        let bundle = ResultBundle {
            bundle_version: ResultBundleVersion,
            job_id: plan.attempt_id,
            job_sha256,
            collected_at,
            terminal: result_terminal(&outcome.terminal),
            files,
            logs: ResultLogs {
                stdout_tail: Some(outcome.stdout_tail.clone()),
                stderr_tail: Some(outcome.stderr_tail.clone()),
            },
            usage_metered: Some(BundleUsageMetered {
                input_tokens: outcome.usage_metered.input_tokens,
                output_tokens: outcome.usage_metered.output_tokens,
                cache_read_tokens: outcome.usage_metered.cache_read_tokens,
                cache_write_tokens: outcome.usage_metered.cache_write_tokens,
                requests: outcome.usage_metered.requests,
                weighted_tokens: Some(outcome.weighted_tokens),
            }),
            usage_reported: result.map(|r| r.usage_reported),
            egress: Some(BundleEgressFlags {
                capped: outcome.egress.capped,
                incomplete: outcome.egress.incomplete,
            }),
            classification: ResultClassification {
                failure_class: classification.class,
                failure_detail: classification
                    .detail
                    .as_deref()
                    .and_then(|d| FailureDetail::parse(d).ok()),
            },
            validation,
        };
        let digest = self
            .ports
            .cas
            .put_bundle(&bundle)
            .await
            .map_err(RunnerError::Cas)?;
        Ok(Some(digest))
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 服务端生成的任务标题（worker-protocol §4.2 `task.title` 可信）：不取交付单元标题，
/// 因为它可能源自 Issue 标题（不可信）。
fn task_title(role: &Role, n: u32) -> String {
    format!("Gantry {} attempt {n}", role_name(role))
}

fn role_name(role: &Role) -> String {
    match role {
        Role::Plan => "plan".into(),
        Role::Build => "build".into(),
        Role::Verify => "verify".into(),
        Role::Single => "single".into(),
        Role::Review(view) => format!("review:{view}"),
        Role::Reasoner => "reasoner".into(),
        Role::Arbiter => "arbiter".into(),
        Role::Summarize => "summarize".into(),
        Role::Admission => "admission".into(),
        Role::Heal => "heal".into(),
        Role::Answer => "answer".into(),
        Role::Intent => "intent".into(),
        Role::Classify => "classify".into(),
    }
}

/// 提交说明：模板（§7.8 V5 "utility 辅助调用或模板"），不含任何模型产出的文字。
fn commit_message(plan: &AttemptPlan) -> CommitMessage {
    CommitMessage {
        subject: format!("gantry: {} attempt {}", role_name(&plan.role), plan.n),
        body: format!("Node: {}\nAttempt: {}", plan.node_id, plan.attempt_id),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn push_discussion(
    out: &mut Vec<RawMaterial>,
    repo: &RepoSlug,
    number: u64,
    tag: &str,
    body_kind: MaterialKind,
    comment_kind: MaterialKind,
    discussion: &Discussion,
) {
    let source = format!("{repo}#{number}");
    // 标题、作者名等自由文本只放进正文（worker-protocol §4.4"标记行不含自由文本"）。
    let mut text = format!("Title: {}\n", discussion.title.as_untrusted_str());
    text.push_str("Body:\n");
    if let Some(body) = &discussion.body {
        text.push_str(body.as_untrusted_str());
    }
    let truncated = discussion.truncated
        || discussion.title.truncated()
        || discussion
            .body
            .as_ref()
            .is_some_and(UntrustedText::truncated);
    out.push(RawMaterial {
        id: format!("{tag}-{number}-body"),
        kind: body_kind,
        source: source.clone(),
        text: UntrustedText::new(text, source.clone(), truncated),
    });
    for comment in &discussion.comments {
        let source = format!("{repo}#{number}/{}", comment.id);
        let text = format!(
            "Author: {}\n{}",
            comment.author_login,
            comment.body.as_untrusted_str()
        );
        out.push(RawMaterial {
            id: format!("{tag}-{number}-comment-{}", comment.id),
            kind: comment_kind,
            source: source.clone(),
            text: UntrustedText::new(text, source, comment.body.truncated()),
        });
    }
}

/// 声明与实际一致：`declared = (path, sha256, bytes)`。
fn declared_matches(
    outcome: &JobOutcome,
    name: &str,
    declared: Option<(&str, Sha256Digest, u64)>,
) -> bool {
    let actual = outcome.files.get(&ArtifactName::new(name));
    match (declared, actual) {
        (None, None) => true,
        (Some((path, digest, bytes)), Some(actual)) => {
            path == name && digest == actual.sha256 && bytes == actual.bytes
        }
        _ => false,
    }
}

/// V4：按 role 解码 `structured_output` 并做一致性校验；不合即 `None`。
fn decode_structured(role: &Role, value: Option<serde_json::Value>) -> Option<StructuredOutput> {
    let output: StructuredOutput = serde_json::from_value(value?).ok()?;
    if output.kind() != StructuredOutput::expected_for_role(role) {
        return None;
    }
    output.validate().ok()?;
    Some(output)
}

fn gate_verdict(output: &StructuredOutput) -> Option<Verdict> {
    match output {
        StructuredOutput::Verify(o) => Some(o.gate.verdict),
        StructuredOutput::Review(o) => Some(o.gate.verdict),
        StructuredOutput::Arbiter(o) => Some(o.gate.verdict),
        StructuredOutput::Admission(o) => Some(o.verdict),
        _ => None,
    }
}

/// 策略拒绝的子码：workflow 优先（P-48），其次受保护路径，再次第一条原因（与 policy 的
/// `ReportHint` 取码顺序一致，policy.md §6）。
fn deny_detail(validated: &ValidatedPatch) -> String {
    const WORKFLOWS: &str = "policy.path_protected:workflows";
    let codes: Vec<&str> = validated.reasons.iter().map(|r| r.code.as_str()).collect();
    if codes.contains(&WORKFLOWS) {
        return WORKFLOWS.to_owned();
    }
    if let Some(code) = codes
        .iter()
        .find(|c| c.starts_with("policy.path_protected:"))
    {
        return (*code).to_owned();
    }
    codes
        .first()
        .map(|c| (*c).to_owned())
        .unwrap_or_else(|| "policy.path_denied".to_owned())
}

/// 机械拒绝 → `policy.*` 子码（worker-protocol §8.3 末段）。
fn reject_detail(reason: PatchRejectReason) -> &'static str {
    match reason {
        PatchRejectReason::PathInvalid => "policy.path_invalid",
        PatchRejectReason::Dotgit => "policy.dotgit",
        PatchRejectReason::Binary => "policy.binary",
        PatchRejectReason::Symlink => "policy.symlink",
        PatchRejectReason::Submodule => "policy.submodule",
        PatchRejectReason::PolicyDenied => "policy.path_denied",
        PatchRejectReason::NotApplicable | PatchRejectReason::DeclaredMismatch => {
            "exec.output_invalid"
        }
    }
}

fn proposal_decision_id(decision: &ProposalDecision) -> &str {
    match decision {
        ProposalDecision::Accepted { proposal_id, .. }
        | ProposalDecision::NeedsApproval { proposal_id, .. }
        | ProposalDecision::Held { proposal_id }
        | ProposalDecision::Denied { proposal_id, .. } => proposal_id,
    }
}

fn proposal_params_value(params: &ProposalParams) -> Option<serde_json::Value> {
    match params {
        ProposalParams::OpenPr(p) => serde_json::to_value(p).ok(),
        ProposalParams::ReplyReviewThread(p) => serde_json::to_value(p).ok(),
        ProposalParams::ResolveReviewThread(p) => serde_json::to_value(p).ok(),
        ProposalParams::Escalate(p) => serde_json::to_value(p).ok(),
    }
}

fn parse_bundle_file(name: &str) -> Option<BundleFileName> {
    Some(match name {
        RESULT_JSON => BundleFileName::ResultJson,
        CHANGES_PATCH => BundleFileName::ChangesPatch,
        REPORT_MD => BundleFileName::ReportMd,
        PROPOSALS_JSON => BundleFileName::ProposalsJson,
        PROGRESS_JSONL => BundleFileName::ProgressJsonl,
        _ => return None,
    })
}

fn bundle_file_name(name: BundleFileName) -> &'static str {
    match name {
        BundleFileName::ResultJson => RESULT_JSON,
        BundleFileName::ChangesPatch => CHANGES_PATCH,
        BundleFileName::ReportMd => REPORT_MD,
        BundleFileName::ProposalsJson => PROPOSALS_JSON,
        BundleFileName::ProgressJsonl => PROGRESS_JSONL,
    }
}

fn result_terminal(facts: &gantry_core::ports::sandbox::TerminalFacts) -> ResultTerminal {
    use gantry_core::ports::sandbox::TimerKind;
    ResultTerminal {
        cause: match facts.cause {
            TerminalCause::Exited => ResultTerminalCause::Exited,
            TerminalCause::KilledTimer => ResultTerminalCause::KilledTimer,
            TerminalCause::KilledCancel => ResultTerminalCause::KilledCancel,
            TerminalCause::KilledRuntime => ResultTerminalCause::KilledRuntime,
            TerminalCause::Lost => ResultTerminalCause::Lost,
        },
        exit_code: facts.exit_code,
        signal: facts.signal,
        oom: facts.oom,
        timer: facts.timer.map(|t| match t {
            TimerKind::Wall => ResultTimerKind::Wall,
            TimerKind::Idle => ResultTimerKind::Idle,
        }),
        cancel_requested: facts.cancel_requested,
    }
}

/// 清单的 `collected_at`：沙盒停止时间截到秒（worker-protocol §5.2）；丢失时没有停止时间，
/// 取引擎此刻的时钟（规格要求"判定丢失那条事件的时间"，Runner 不写事件，近似）。
fn collected_at(outcome: &JobOutcome, now: Timestamp) -> Timestamp {
    let at = outcome.timings.stopped.unwrap_or(now);
    let ms = at.as_millis();
    Timestamp::from_millis(ms - ms.rem_euclid(1000))
}

/// 有效墙钟（秒，向上取整）：`started → stopped`，缺开始时间时取 `created`。
fn wall_time_s(outcome: &JobOutcome) -> u64 {
    let timings = &outcome.timings;
    let (Some(start), Some(stop)) = (timings.started.or(timings.created), timings.stopped) else {
        return 0;
    };
    let ms = stop.as_millis().saturating_sub(start.as_millis()).max(0) as u64;
    ms.div_ceil(1000)
}
