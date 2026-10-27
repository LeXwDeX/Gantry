//! `ModelsView`：生效的 `models.*` 与 `levels.*` 的只读快照，以及 `resolve`
//! （models.md §6、§7.2）。
//!
//! 项目覆盖由 `core::settings::effective` 合并后再构建视图；本模块不自己合并，
//! 也不接受调用方传入的模型名（I7）。

use gantry_core::entities::{Seq, Tier, TokenWeights};
use gantry_core::ids::ProjectId;
use gantry_core::level::{Effort, Level, LevelKey};
use gantry_core::settings::{
    EffortByLevel, Fallback, Levels, Models, PlannerEffort, ProviderKind, WorkerAdapter,
};

use crate::error::ModelsError;

/// 降级链最多 3 项（config §3.4）。
pub const MAX_FALLBACK: usize = 3;

/// 解析用途：直接调用（无工具）或沙盒内运行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Direct,
    Sandbox,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveQuery {
    pub project_id: ProjectId,
    pub tier: Tier,
    /// `level_effective`；无交付单元上下文时为 `None`（OQ-7）。
    pub level: Option<Level>,
    /// 图模板标记的关键节点。
    pub critical: bool,
    pub purpose: Purpose,
}

/// 沙盒用途的执行参数；`attempt_wall_s` 不是有效墙钟（由 engine 与 sandbox 取最小）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxExec {
    pub adapter: WorkerAdapter,
    pub attempt_wall_s: u64,
    pub max_turns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModel {
    pub tier: Tier,
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    pub model: String,
    pub reasoning_effort: Effort,
    /// 原样带出，≤ 3 项。
    pub fallback: Vec<Fallback>,
    pub request_timeout_s: u64,
    /// utility / planner 的直接调用总时长；worker 为 `None`。
    pub total_timeout_s: Option<u64>,
    pub sandbox: Option<SandboxExec>,
}

/// 一次构建出的只读快照。调用全程持有同一个 `Arc<ModelsView>`（§7.3 第 2 步）。
#[derive(Clone, Debug, PartialEq)]
pub struct ModelsView {
    rev: Seq,
    models: Models,
    token_weights: TokenWeights,
    daily_tokens: u64,
}

impl ModelsView {
    /// 构建视图；配置自相矛盾 → `ConfigInvalid`，调用方保留旧视图（§7.8）。
    ///
    /// provider / model 缺失不是构建错误：它是"尚未配置"的正常状态，由 `resolve`
    /// 返回 `NotConfigured`。
    pub fn build(models: Models, levels: &Levels, rev: Seq) -> Result<Self, ModelsError> {
        if models.retry.max_attempts == 0 {
            return Err(ModelsError::config_invalid(
                "/models/retry/max_attempts",
                "max_attempts must be at least 1",
            ));
        }
        check_tier(
            &models,
            "utility",
            &models.utility.fallback,
            models.utility.request_timeout_s,
        )?;
        check_tier(
            &models,
            "planner",
            &models.planner.fallback,
            models.planner.request_timeout_s,
        )?;
        check_tier(
            &models,
            "worker",
            &models.worker.fallback,
            models.worker.request_timeout_s,
        )?;
        if models.utility.total_timeout_s == 0 || models.planner.total_timeout_s == 0 {
            return Err(ModelsError::config_invalid(
                "/models/*/total_timeout_s",
                "total_timeout_s must be positive",
            ));
        }
        check_by_level(
            "planner",
            models.planner.reasoning_effort,
            &models.planner.effort_by_level,
        )?;
        check_by_level(
            "worker",
            models.worker.reasoning_effort,
            &models.worker.effort_by_level,
        )?;
        let weights = levels.token_weights;
        for (name, value) in [
            ("input", weights.input),
            ("output", weights.output),
            ("cache_read", weights.cache_read),
            ("cache_write", weights.cache_write),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(ModelsError::config_invalid(
                    format!("/levels/token_weights/{name}"),
                    "token weight must be a positive finite number",
                ));
            }
        }
        Ok(Self {
            rev,
            models,
            token_weights: weights,
            daily_tokens: levels.none.daily_tokens,
        })
    }

    pub fn rev(&self) -> Seq {
        self.rev
    }

    pub fn models(&self) -> &Models {
        &self.models
    }

    /// `levels.token_weights`（P-70）。
    pub fn token_weights(&self) -> TokenWeights {
        self.token_weights
    }

    /// `levels.none.daily_tokens`：会话台账每日上限（P-58）。
    pub fn daily_tokens(&self) -> u64 {
        self.daily_tokens
    }

    /// 解析模型与推理深度；纯读配置，不发请求。
    pub fn resolve(&self, q: &ResolveQuery) -> Result<ResolvedModel, ModelsError> {
        let models = &self.models;
        match (q.tier, q.purpose) {
            (Tier::Utility, Purpose::Direct) => {
                let cfg = &models.utility;
                let (provider_id, provider_kind, model) =
                    self.target(Tier::Utility, &cfg.provider, &cfg.model)?;
                Ok(ResolvedModel {
                    tier: Tier::Utility,
                    provider_id,
                    provider_kind,
                    model,
                    reasoning_effort: cfg.reasoning_effort,
                    fallback: cfg.fallback.clone(),
                    request_timeout_s: cfg.request_timeout_s,
                    total_timeout_s: Some(cfg.total_timeout_s),
                    sandbox: None,
                })
            }
            (Tier::Planner, purpose) => {
                let cfg = &models.planner;
                let (provider_id, provider_kind, model) =
                    self.target(Tier::Planner, &cfg.provider, &cfg.model)?;
                let reasoning_effort = effort_for(
                    "planner",
                    cfg.reasoning_effort,
                    &cfg.effort_by_level,
                    q.level,
                    q.critical,
                )?;
                // 沙盒内的大脑角色沿用 worker 的适配器与上限（§7.2，config §3 [提议]）。
                let (total_timeout_s, sandbox) = match purpose {
                    Purpose::Direct => (Some(cfg.total_timeout_s), None),
                    Purpose::Sandbox => (None, Some(self.sandbox_exec())),
                };
                Ok(ResolvedModel {
                    tier: Tier::Planner,
                    provider_id,
                    provider_kind,
                    model,
                    reasoning_effort,
                    fallback: cfg.fallback.clone(),
                    request_timeout_s: cfg.request_timeout_s,
                    total_timeout_s,
                    sandbox,
                })
            }
            (Tier::Worker, Purpose::Sandbox) => {
                let cfg = &models.worker;
                let (provider_id, provider_kind, model) =
                    self.target(Tier::Worker, &cfg.provider, &cfg.model)?;
                let reasoning_effort = effort_for(
                    "worker",
                    cfg.reasoning_effort,
                    &cfg.effort_by_level,
                    q.level,
                    q.critical,
                )?;
                Ok(ResolvedModel {
                    tier: Tier::Worker,
                    provider_id,
                    provider_kind,
                    model,
                    reasoning_effort,
                    fallback: cfg.fallback.clone(),
                    request_timeout_s: cfg.request_timeout_s,
                    total_timeout_s: None,
                    sandbox: Some(self.sandbox_exec()),
                })
            }
            // 打杂只做直接调用；干活只在沙盒内运行（architecture §9）。
            (tier, purpose) => Err(ModelsError::InvalidRole {
                role: format!("{}/{purpose:?}", tier.as_str()),
            }),
        }
    }

    fn sandbox_exec(&self) -> SandboxExec {
        let worker = &self.models.worker;
        SandboxExec {
            adapter: worker.adapter,
            attempt_wall_s: worker.attempt_wall_s,
            max_turns: worker.max_turns,
        }
    }

    fn target(
        &self,
        tier: Tier,
        provider: &Option<String>,
        model: &Option<String>,
    ) -> Result<(String, ProviderKind, String), ModelsError> {
        let Some(provider_id) = provider else {
            return Err(ModelsError::not_configured(tier, "provider is not set"));
        };
        let Some(model) = model else {
            return Err(ModelsError::not_configured(tier, "model is not set"));
        };
        let Some(provider) = self.models.providers.get(provider_id) else {
            return Err(ModelsError::not_configured(
                tier,
                format!("provider {provider_id} does not exist"),
            ));
        };
        Ok((provider_id.clone(), provider.kind, model.clone()))
    }
}

fn check_tier(
    models: &Models,
    tier: &str,
    fallback: &[Fallback],
    request_timeout_s: u64,
) -> Result<(), ModelsError> {
    if fallback.len() > MAX_FALLBACK {
        return Err(ModelsError::config_invalid(
            format!("/models/{tier}/fallback"),
            format!("at most {MAX_FALLBACK} fallback entries"),
        ));
    }
    for (index, entry) in fallback.iter().enumerate() {
        if !models.providers.contains_key(&entry.provider) {
            return Err(ModelsError::config_invalid(
                format!("/models/{tier}/fallback/{index}/provider"),
                format!("provider {} does not exist", entry.provider),
            ));
        }
    }
    if request_timeout_s == 0 {
        return Err(ModelsError::config_invalid(
            format!("/models/{tier}/request_timeout_s"),
            "request_timeout_s must be positive",
        ));
    }
    Ok(())
}

fn check_by_level(
    tier: &str,
    setting: PlannerEffort,
    table: &Option<EffortByLevel>,
) -> Result<(), ModelsError> {
    if setting == PlannerEffort::ByLevel && table.is_none() {
        return Err(ModelsError::config_invalid(
            format!("/models/{tier}/effort_by_level"),
            "reasoning_effort = by_level requires effort_by_level",
        ));
    }
    Ok(())
}

/// 固定值直接用；`by_level` 按档位键查表，`critical` 有值时优先（§7.2）。
fn effort_for(
    tier: &str,
    setting: PlannerEffort,
    table: &Option<EffortByLevel>,
    level: Option<Level>,
    critical: bool,
) -> Result<Effort, ModelsError> {
    let fixed = match setting {
        PlannerEffort::None => Some(Effort::None),
        PlannerEffort::Low => Some(Effort::Low),
        PlannerEffort::Medium => Some(Effort::Medium),
        PlannerEffort::High => Some(Effort::High),
        PlannerEffort::Max => Some(Effort::Max),
        PlannerEffort::ByLevel => None,
    };
    if let Some(effort) = fixed {
        return Ok(effort);
    }
    let Some(table) = table else {
        return Err(ModelsError::config_invalid(
            format!("/models/{tier}/effort_by_level"),
            "reasoning_effort = by_level requires effort_by_level",
        ));
    };
    // `level` 缺失或为 None 时取 low（OQ-7，暂用）；Ultimate 的子交付单元按 Max 解析。
    let choice = match level.map(Level::key) {
        None | Some(LevelKey::None) | Some(LevelKey::Low) => &table.low,
        Some(LevelKey::Medium) => &table.medium,
        Some(LevelKey::High) => &table.high,
        Some(LevelKey::Max) | Some(LevelKey::Ultimate) => &table.max,
    };
    Ok(match (critical, choice.critical) {
        (true, Some(effort)) => effort,
        _ => choice.default,
    })
}
