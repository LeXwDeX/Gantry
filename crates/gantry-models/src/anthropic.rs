//! `AnthropicProvider`：Messages API 的第一版提供方（models.md §1 第 9 条、§7.5）。
//!
//! 请求发往 `EgressRoute.base_url`（回环反向代理，路径里带 `svc_token`），
//! 拼上 `/v1/messages`。请求里没有认证头、没有 `tools`、没有采样参数；结构化
//! 输出用 `output_config.format`（不用强制工具选择），推理深度用
//! `output_config.effort`。认证由 Egress 注入（P-16）。
//!
//! 推理深度映射来自注入的 [`EffortTable`]（OQ-1 / OQ-8：本模块不内置模型能力
//! 数据）；表里没有的模型或取值 fail closed。

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::entities::{Tier, TokenUsage};
use gantry_core::level::Effort;
use gantry_core::ports::models::{
    EgressRoute, ModelProvider, ProviderEffort, ProviderError, ProviderRequest, ProviderResponse,
    StopReason, UnsupportedEffort,
};
use gantry_core::settings::ProviderKind;
use serde_json::{json, Value};

use crate::http::{post, HttpError, LoopbackUrl};

/// Messages API 版本头。
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

/// 模型 → 抽象推理深度 → 该模型的 `output_config.effort` 取值。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffortTable(BTreeMap<String, BTreeMap<Effort, ProviderEffort>>);

impl EffortTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 声明某模型支持的推理深度及其提供方取值。
    pub fn with_model(
        mut self,
        model: impl Into<String>,
        efforts: impl IntoIterator<Item = (Effort, &'static str)>,
    ) -> Self {
        self.0.insert(
            model.into(),
            efforts
                .into_iter()
                .map(|(effort, value)| (effort, ProviderEffort::new(value)))
                .collect(),
        );
        self
    }

    fn get(&self, model: &str, effort: Effort) -> Option<&ProviderEffort> {
        self.0.get(model)?.get(&effort)
    }
}

pub struct AnthropicProvider {
    efforts: EffortTable,
}

impl AnthropicProvider {
    pub fn new(efforts: EffortTable) -> Self {
        Self { efforts }
    }

    /// 请求体。只含规格允许的字段；类型层面没有 `tools`、采样参数。
    pub fn request_body(req: &ProviderRequest) -> Value {
        let mut output_config = json!({ "effort": req.effort.as_str() });
        if let Some(schema) = &req.output_schema {
            output_config["format"] = json!({ "type": "json_schema", "schema": schema });
        }
        json!({
            "model": req.model,
            "max_tokens": req.max_output_tokens,
            "system": req.system,
            "messages": [{ "role": "user", "content": req.input }],
            "output_config": output_config,
        })
    }
}

fn tier_header(tier: Tier) -> &'static str {
    tier.as_str()
}

fn http_error(error: HttpError) -> ProviderError {
    match error {
        HttpError::NotLoopback => ProviderError::InvalidRequest {
            message: "egress route is not a loopback URL".into(),
        },
        HttpError::Connect(reason) | HttpError::Io(reason) => ProviderError::Unavailable {
            message: format!("connection: {reason}"),
        },
        HttpError::Malformed(reason) => ProviderError::Protocol {
            message: reason.into(),
        },
    }
}

fn usage_of(body: &Value) -> TokenUsage {
    let field = |name: &str| body["usage"][name].as_u64().unwrap_or(0);
    TokenUsage::new(
        field("input_tokens"),
        field("output_tokens"),
        field("cache_read_input_tokens"),
        field("cache_creation_input_tokens"),
        1,
    )
}

fn parse_success(body: &[u8]) -> Result<ProviderResponse, ProviderError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| ProviderError::Protocol {
        message: "response is not JSON".into(),
    })?;
    let text = value["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let stop = match value["stop_reason"].as_str() {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal {
            category: value["stop_details"]["category"]
                .as_str()
                .map(str::to_owned),
        },
        Some(other) => StopReason::Other(other.to_owned()),
        None => StopReason::Other("missing".into()),
    };
    Ok(ProviderResponse {
        text,
        stop,
        usage_reported: usage_of(&value),
    })
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn map_effort(&self, model: &str, effort: Effort) -> Result<ProviderEffort, UnsupportedEffort> {
        self.efforts
            .get(model, effort)
            .cloned()
            .ok_or_else(|| UnsupportedEffort {
                model: model.to_owned(),
                effort,
            })
    }

    async fn complete(
        &self,
        req: ProviderRequest,
        route: &EgressRoute,
    ) -> Result<ProviderResponse, ProviderError> {
        let url = LoopbackUrl::parse(&route.base_url)
            .map_err(http_error)?
            .join("/v1/messages");
        let body = serde_json::to_vec(&Self::request_body(&req)).map_err(|_| {
            ProviderError::InvalidRequest {
                message: "request is not serializable".into(),
            }
        })?;
        let headers = [
            ("anthropic-version", ANTHROPIC_VERSION.to_owned()),
            ("x-gantry-ledger", route.ledger.to_string()),
            ("x-gantry-tier", tier_header(route.tier).to_owned()),
            ("x-gantry-call-id", route.call_id.to_string()),
        ];
        let response = tokio::time::timeout(req.request_timeout, post(&url, &headers, &body))
            .await
            .map_err(|_| ProviderError::Timeout)?
            .map_err(http_error)?;
        match response.status {
            200 => parse_success(&response.body),
            403 if error_code(&response.body).as_deref() == Some("token_invalid") => {
                Err(ProviderError::EgressTokenInvalid)
            }
            404 => Err(ProviderError::ModelNotFound),
            429 => Err(ProviderError::RateLimited {
                retry_after: response
                    .header("retry-after")
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs),
            }),
            408 => Err(ProviderError::Timeout),
            status if status >= 500 => Err(ProviderError::Unavailable {
                message: format!("status {status}"),
            }),
            status => Err(ProviderError::InvalidRequest {
                message: format!("status {status}"),
            }),
        }
    }
}

/// Egress 错误体 `{"error":"<code>"}` 或提供方错误体 `{"error":{"type":…}}` 的错误码。
fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    match &value["error"] {
        Value::String(code) => Some(code.clone()),
        Value::Object(map) => map.get("type")?.as_str().map(str::to_owned),
        _ => None,
    }
}
