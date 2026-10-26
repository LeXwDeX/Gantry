//! `ModelProvider` 扩展点。
//!
//! 签名以 `docs/modules/models.md` §6 为准。`models` 内部通过 `ModelRouter`
//! 调用本端口；`engine` / `conversation` 不直接依赖提供方实现。
//!
//! `ModelProvider::complete` 是外部网络调用，不是跨越模块的持久化写入；它的
//! 归属由请求里的 `call_id` 贯穿 Egress 计量，因此不额外引入 `ActorCtx`。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::entities::TokenUsage;
use crate::ids::Ulid;
use crate::level::Effort;
use crate::settings::resource::ProviderKind;
use crate::text::UntrustedText;
use crate::worker::MaterialKind;

/// 调用标识由调用方生成，贯穿 Egress 计量与日志。
pub type CallId = Ulid;

/// 台账引用：`dlv_…`、`cnv_…:YYYY-MM-DD` 或 `unattributed`。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LedgerId(String);

impl LedgerId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn unattributed() -> Self {
        Self("unattributed".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LedgerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 具体提供方的推理深度参数。规格书没有封闭词表；用类型化字符串承载。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderEffort(String);

impl ProviderEffort {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("model {model} does not support reasoning effort {effort}")]
pub struct UnsupportedEffort {
    pub model: String,
    pub effort: Effort,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRequest {
    pub model: String,
    pub system: String,
    pub input: String,
    pub effort: ProviderEffort,
    /// JSON Schema，来源于角色的 `StructuredSchema`；不是自由形式的 Map。
    pub output_schema: Option<Value>,
    pub max_output_tokens: u32,
    pub request_timeout: Duration,
    pub call_id: CallId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderResponse {
    pub text: String,
    pub stop: StopReason,
    pub usage_reported: TokenUsage,
}

/// 直接调用的不可信材料。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Material {
    pub id: String,
    pub kind: MaterialKind,
    pub source: String,
    pub text: UntrustedText,
}

/// 直接调用用量的来源。`Metered` 为 Egress 权威计量；`ProviderReported`
/// 是计量不可用时的降级口径（models.md §9）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageSource {
    Metered,
    ProviderReported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    Refusal { category: Option<String> },
    Other(String),
}

/// 发往 Egress 回环反向代理的路由；`base_url` 已含 `svc_token`，Debug 不打印。
#[derive(Clone, PartialEq, Eq)]
pub struct EgressRoute {
    pub base_url: String,
    pub tier: crate::entities::Tier,
    pub call_id: CallId,
    pub ledger: LedgerId,
}

impl std::fmt::Debug for EgressRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressRoute")
            .field("base_url", &"***")
            .field("tier", &self.tier)
            .field("call_id", &self.call_id)
            .field("ledger", &self.ledger)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("invalid provider request: {message}")]
    InvalidRequest { message: String },
    #[error("model content refused")]
    ContentRefused { category: Option<String> },
    #[error("model output was truncated")]
    OutputTruncated,
    #[error("provider request timed out")]
    Timeout,
    #[error("provider rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("provider unavailable: {message}")]
    Unavailable { message: String },
    #[error("provider protocol error: {message}")]
    Protocol { message: String },
    #[error(transparent)]
    UnsupportedEffort(#[from] UnsupportedEffort),
}

/// 外部模型提供方扩展点。第一版为 Anthropic；M0 替身为 `FakeModelProvider`。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// 抽象推理深度 → 该模型的具体参数；不支持时 fail closed，不静默降级。
    fn map_effort(&self, model: &str, effort: Effort) -> Result<ProviderEffort, UnsupportedEffort>;

    async fn complete(
        &self,
        req: ProviderRequest,
        route: &EgressRoute,
    ) -> Result<ProviderResponse, ProviderError>;
}
