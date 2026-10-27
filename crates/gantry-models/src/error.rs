//! `ModelsError`（models.md §9）。
//!
//! 错误信息只含配置路径、角色与错误类别；不含提示词、模型输出、认证头或
//! `svc_token`（§8 日志脱敏）。

use gantry_core::entities::Tier;
use gantry_core::level::Effort;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelsError {
    /// 层级的 provider / model 缺失，或 provider id 不存在。`model` 没有默认值。
    #[error("models.{tier} is not configured: {reason}")]
    NotConfigured { tier: &'static str, reason: String },
    /// 配置自相矛盾（降级链引用不存在的提供方、重试次数为 0 等）。
    #[error("models config is invalid at {pointer}: {reason}")]
    ConfigInvalid { pointer: String, reason: String },
    /// 调用方传入了不属于直接调用的角色，或层级与用途不匹配。
    #[error("role {role} is not allowed here")]
    InvalidRole { role: String },
    /// `schema` 与角色不匹配。
    #[error("schema {schema} does not belong to role {role}")]
    InvalidSchema { role: String, schema: String },
    /// 模型不支持所需推理深度；不降级（fail closed）。
    #[error("model {model} does not support reasoning effort {effort}")]
    EffortUnsupported { model: String, effort: Effort },
    /// 提供方返回 4xx 请求错误。
    #[error("provider rejected the request ({status})")]
    InvalidRequest { status: u16 },
    /// 内容策略拒绝；不重试不降级。
    #[error("model refused the request")]
    ContentRefused { category: Option<String> },
    /// `MaxTokens` 截断；不重试。
    #[error("model output was truncated")]
    OutputTruncated,
    /// 所有尝试的输出都不合 schema。
    #[error("model output did not match the role schema")]
    SchemaInvalid { last_error: String },
    /// 总期限到。
    #[error("direct call deadline exceeded")]
    Timeout,
    /// 会话台账当日已达 `levels.none.daily_tokens`（P-58）。
    #[error("conversation ledger daily token cap reached")]
    BudgetExhausted { used: u64, cap: u64 },
    /// 所有尝试都连接失败 / 5xx / 429。
    #[error("model provider unavailable: {reason}")]
    Unavailable { reason: String },
    /// 依赖（store、Egress 控制面）不可用。
    #[error("models dependency failed: {reason}")]
    Internal { reason: String },
}

impl ModelsError {
    pub(crate) fn not_configured(tier: Tier, reason: impl Into<String>) -> Self {
        Self::NotConfigured {
            tier: tier.as_str(),
            reason: reason.into(),
        }
    }

    pub(crate) fn config_invalid(pointer: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ConfigInvalid {
            pointer: pointer.into(),
            reason: reason.into(),
        }
    }

    /// 稳定的错误类别，供日志与指标使用。
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured { .. } => "not_configured",
            Self::ConfigInvalid { .. } => "config_invalid",
            Self::InvalidRole { .. } => "invalid_role",
            Self::InvalidSchema { .. } => "invalid_schema",
            Self::EffortUnsupported { .. } => "effort_unsupported",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::ContentRefused { .. } => "content_refused",
            Self::OutputTruncated => "output_truncated",
            Self::SchemaInvalid { .. } => "schema_invalid",
            Self::Timeout => "timeout",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::Unavailable { .. } => "unavailable",
            Self::Internal { .. } => "internal",
        }
    }
}
