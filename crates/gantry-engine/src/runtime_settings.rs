//! 从同一修订的已合并设置构造引擎配置，不读取存储或解析模型。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::sha256;
use gantry_core::entities::{Role, SandboxLimits, Seq, Tier};
use gantry_core::ports::sandbox::{GroupName, ProfileName, Timers, Trust};
use gantry_core::settings::resource::{Egress, Levels, Models, Policies, Sandbox};
use gantry_core::time::Timestamp;
use gantry_core::worker::{
    PatchLimits, PatchMode, ProposalKind, ProposalsMode, ReportMode, StructuredSchema,
    ToolConstraints,
};

use crate::actor::ActorConfig;
use crate::attempt::{RoleOutput, RunnerConfig};
use crate::budget::EqualShareReservation;
use crate::decide::{
    EngineConsts, EngineSettings, ModelTable, ProjectFacts, SandboxLimits as DecisionSandboxLimits,
    TemplateRegistry,
};
use crate::runtime::RuntimeSettings;

/// 引擎内部事件轮询间隔，不是设置键。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 清理工作树时的引擎内部锁租约，不是设置键。
pub const DEFAULT_CLEANUP_LEASE: Duration = Duration::from_secs(60);
/// 单次讨论读取的评论上限，不是设置键。
pub const DEFAULT_DISCUSSION_MAX_COMMENTS: u32 = 50;
/// M1 worker 当前的提议条数硬上限（worker-protocol §5.1）。
pub const M1_WORKER_MAX_PROPOSALS: u32 = 20;
const BUILD_SCHEMA: &[u8] = include_bytes!("../../../schemas/worker/output.build.schema.json");

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeSettingsError {
    #[error("model table revision differs from config revision")]
    ModelRevisionMismatch,
    #[error("worker sandbox model does not match configured worker")]
    WorkerModelMismatch,
    #[error("default egress profile not found: {0}")]
    MissingProfile(String),
    #[error("egress group not found: {0}")]
    MissingGroup(String),
    #[error("invalid setting: {0}")]
    Invalid(&'static str),
}

fn positive(value: u64, field: &'static str) -> Result<(), RuntimeSettingsError> {
    if value == 0 {
        return Err(RuntimeSettingsError::Invalid(field));
    }
    Ok(())
}

fn positive_u32(value: u64, field: &'static str) -> Result<u32, RuntimeSettingsError> {
    positive(value, field)?;
    u32::try_from(value).map_err(|_| RuntimeSettingsError::Invalid(field))
}

fn safe_millis(value: u64, field: &'static str) -> Result<(), RuntimeSettingsError> {
    // sandbox 会将毫秒转为 i64 后与时钟相加；时钟可达 Timestamp::MAX_MILLIS。
    let millis = value
        .checked_mul(1_000)
        .and_then(|value| i64::try_from(value).ok())
        .and_then(|value| Timestamp::MAX_MILLIS.checked_add(value));
    millis.ok_or(RuntimeSettingsError::Invalid(field))?;
    Ok(())
}

/// 所有设置必须来自 `config_rev` 的同一项目生效快照。
/// `models` 是调用方用同一修订调用 `models.resolve` 得到的表，空表保持空。
/// `build_schema` 必须与本版本的内置 build schema 原始字节一致。调用方还必须将同一份字节
/// 提供给 sandbox 的 `OutputSchemas`：sandbox 按 schema id 独立获取资产，本函数无法检查它。
/// `now` 是调用方提供的当前时间；租约和空闲期限只在该时点检查可序列化范围。
/// 创建交付单元时，调用方须从同一快照选档并调用 `create::budget_from_level`。
#[allow(clippy::too_many_arguments)]
pub fn from_settings(
    config_rev: Seq,
    now: Timestamp,
    project: ProjectFacts,
    models_settings: &Models,
    levels: &Levels,
    policies: &Policies,
    sandbox: &Sandbox,
    egress: &Egress,
    model_rev: Seq,
    models: ModelTable,
    build_schema: &[u8],
) -> Result<RuntimeSettings, RuntimeSettingsError> {
    if !now.is_representable() {
        return Err(RuntimeSettingsError::Invalid("now"));
    }
    if config_rev != model_rev {
        return Err(RuntimeSettingsError::ModelRevisionMismatch);
    }
    if build_schema != BUILD_SCHEMA {
        return Err(RuntimeSettingsError::Invalid("build_schema"));
    }
    for (field, value) in [
        ("project.default_branch", &project.default_branch),
        ("project.branch_prefix", &project.branch_prefix),
    ] {
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(RuntimeSettingsError::Invalid(field));
        }
    }
    if project.allowed_merge_methods.is_empty() {
        return Err(RuntimeSettingsError::Invalid(
            "project.allowed_merge_methods",
        ));
    }
    for (key, model) in &models {
        if key.sandbox && key.tier == Tier::Worker {
            let provider = models_settings.worker.provider.as_deref();
            let selected_model = models_settings.worker.model.as_deref();
            if provider.filter(|value| !value.is_empty()) != Some(model.provider_id.as_str())
                || selected_model.filter(|value| !value.is_empty()) != Some(model.model.as_str())
                || !models_settings.providers.contains_key(&model.provider_id)
                || model.attempt_wall_s == 0
                || model.attempt_wall_s != models_settings.worker.attempt_wall_s
            {
                return Err(RuntimeSettingsError::WorkerModelMismatch);
            }
        }
    }
    let profile = egress
        .profiles
        .get(&egress.default_profile)
        .ok_or_else(|| RuntimeSettingsError::MissingProfile(egress.default_profile.clone()))?;
    for group in &profile.groups {
        if !egress.groups.contains_key(group) {
            return Err(RuntimeSettingsError::MissingGroup(group.clone()));
        }
    }
    let weights = levels.token_weights;
    for (field, value) in [
        ("sandbox.limits.cpu_cores", sandbox.limits.cpu_cores),
        ("levels.token_weights.input", weights.input),
        ("levels.token_weights.output", weights.output),
        ("levels.token_weights.cache_read", weights.cache_read),
        ("levels.token_weights.cache_write", weights.cache_write),
    ] {
        if !value.is_finite() || value <= 0.0 {
            return Err(RuntimeSettingsError::Invalid(field));
        }
    }
    let io = &sandbox.io;
    for (field, value) in [
        ("sandbox.limits.mem_mb", sandbox.limits.mem_mb),
        ("sandbox.limits.disk_mb", sandbox.limits.disk_mb),
        ("sandbox.limits.wall_time_s", sandbox.limits.wall_time_s),
        ("sandbox.idle_timeout_s", sandbox.idle_timeout_s),
        (
            "models.worker.attempt_wall_s",
            models_settings.worker.attempt_wall_s,
        ),
        (
            "sandbox.io.progress.max_line_bytes",
            io.progress.max_line_bytes,
        ),
        ("sandbox.io.progress.max_lines", io.progress.max_lines),
        (
            "sandbox.io.progress.max_rate_per_s",
            io.progress.max_rate_per_s,
        ),
        (
            "sandbox.io.artifacts.result_max_bytes",
            io.artifacts.result_max_bytes,
        ),
        (
            "sandbox.io.artifacts.report_max_bytes",
            io.artifacts.report_max_bytes,
        ),
        (
            "sandbox.io.artifacts.proposals_max_bytes",
            io.artifacts.proposals_max_bytes,
        ),
        (
            "sandbox.io.artifacts.proposals_max_count",
            io.artifacts.proposals_max_count,
        ),
        (
            "sandbox.io.materials.max_item_bytes",
            io.materials.max_item_bytes,
        ),
        (
            "sandbox.io.materials.max_total_bytes",
            io.materials.max_total_bytes,
        ),
        ("sandbox.io.logs_tail_bytes", io.logs_tail_bytes),
        ("policies.patch.max_bytes", policies.patch.max_bytes),
        (
            "policies.patch.max_changed_lines",
            policies.patch.max_changed_lines,
        ),
        (
            "policies.patch.max_file_bytes",
            policies.patch.max_file_bytes,
        ),
    ] {
        positive(value, field)?;
    }
    let consts = EngineConsts::default();
    let lease = sandbox
        .limits
        .wall_time_s
        .checked_add(sandbox.kill_grace_s)
        .and_then(|value| value.checked_add(consts.lock_lease_margin_s))
        .ok_or(RuntimeSettingsError::Invalid("sandbox.lock_lease_s"))?;
    for (field, value) in [
        ("sandbox.limits.wall_time_s", sandbox.limits.wall_time_s),
        ("sandbox.idle_timeout_s", sandbox.idle_timeout_s),
        ("sandbox.kill_grace_s", sandbox.kill_grace_s),
        ("sandbox.lock_lease_s", lease),
    ] {
        safe_millis(value, field)?;
    }
    for (field, value) in [
        ("sandbox.lock_lease_s", lease),
        ("sandbox.idle_timeout_s", sandbox.idle_timeout_s),
    ] {
        now.checked_add_secs(value)
            .ok_or(RuntimeSettingsError::Invalid(field))?;
    }
    let pids = positive_u32(sandbox.limits.pids, "sandbox.limits.pids")?;
    let capacity = positive_u32(
        sandbox.max_concurrent_sandboxes,
        "sandbox.max_concurrent_sandboxes",
    )?;
    let max_turns = positive_u32(models_settings.worker.max_turns, "models.worker.max_turns")?;
    let max_files = positive_u32(policies.patch.max_files, "policies.patch.max_files")?;
    // Runner 把材料总上限传给 ForgeReader 的 u32 字节上限。
    positive_u32(
        io.materials.max_total_bytes,
        "sandbox.io.materials.max_total_bytes",
    )?;
    let role = RoleOutput {
        structured_schema: StructuredSchema {
            id: "gantry.worker.output.build/1".into(),
            path: "/gantry/inputs/output.schema.json".into(),
            sha256: sha256(build_schema),
        },
        patch: PatchMode::Optional,
        report: ReportMode::Optional,
        proposals: ProposalsMode::Allowed {
            allowed_kinds: vec![ProposalKind::OpenPr, ProposalKind::Escalate],
            max: u32::try_from(
                io.artifacts
                    .proposals_max_count
                    .min(u64::from(M1_WORKER_MAX_PROPOSALS)),
            )
            .map_err(|_| {
                RuntimeSettingsError::Invalid("sandbox.io.artifacts.proposals_max_count")
            })?,
        },
        tools: ToolConstraints {
            edit: true,
            shell: true,
        },
        patch_limits: Some(PatchLimits {
            max_bytes: policies.patch.max_bytes,
            max_files,
        }),
    };
    Ok(RuntimeSettings {
        actor: ActorConfig {
            settings: EngineSettings {
                config_rev,
                project: project.clone(),
                sandbox: DecisionSandboxLimits {
                    wall_time_s: sandbox.limits.wall_time_s,
                    kill_grace_s: sandbox.kill_grace_s,
                },
            },
            models,
            templates: TemplateRegistry,
            reservation: Arc::new(EqualShareReservation),
            sandbox_capacity: capacity,
            consts,
        },
        runner: RunnerConfig {
            config_rev,
            repo: project.repo,
            driver: sandbox.driver,
            trust: Trust::Trusted,
            limits: SandboxLimits {
                cpu_cores: sandbox.limits.cpu_cores,
                mem_mb: sandbox.limits.mem_mb,
                disk_mb: sandbox.limits.disk_mb,
                pids,
                wall_time_s: sandbox.limits.wall_time_s,
            },
            timers: Timers {
                idle_timeout_s: sandbox.idle_timeout_s,
                kill_grace_s: sandbox.kill_grace_s,
            },
            io: io.clone(),
            egress_profile: ProfileName::new(&egress.default_profile),
            egress_groups: profile.groups.iter().map(GroupName::new).collect(),
            token_weights: weights,
            max_turns,
            discussion_max_comments: DEFAULT_DISCUSSION_MAX_COMMENTS,
            roles: HashMap::from([(Role::Single, role)]),
        },
        poll_interval: DEFAULT_POLL_INTERVAL,
        cleanup_lease: DEFAULT_CLEANUP_LEASE,
    })
}
