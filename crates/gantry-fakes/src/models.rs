//! `FakeModelProvider`：按规则返回固定输出或脚本化错误。
//!
//! 不做网络 IO，也不模拟提供方的重试与退避；这些属于 `models`。规则按
//! 「model / system 片段 / input 片段」逐个匹配，首个命中的规则生效。

use std::collections::BTreeSet;
use std::sync::Mutex;

use async_trait::async_trait;
use gantry_core::entities::Tier;
use gantry_core::entities::TokenUsage;
use gantry_core::level::Effort;
use gantry_core::ports::models::{
    CallId, EgressRoute, LedgerId, ModelProvider, ProviderEffort, ProviderError, ProviderRequest,
    ProviderResponse, StopReason, UnsupportedEffort,
};
use gantry_core::settings::resource::ProviderKind;

/// 一条脚本规则；未设置的字段视为通配。
#[derive(Clone, Debug)]
pub struct FakeModelRule {
    pub model: Option<String>,
    pub system_contains: Option<String>,
    pub input_contains: Option<String>,
    pub response: Result<ProviderResponse, ProviderError>,
}

impl FakeModelRule {
    pub fn response(response: ProviderResponse) -> Self {
        Self {
            model: None,
            system_contains: None,
            input_contains: None,
            response: Ok(response),
        }
    }

    pub fn error(error: ProviderError) -> Self {
        Self {
            model: None,
            system_contains: None,
            input_contains: None,
            response: Err(error),
        }
    }

    pub fn for_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn when_system_contains(mut self, fragment: impl Into<String>) -> Self {
        self.system_contains = Some(fragment.into());
        self
    }

    pub fn when_input_contains(mut self, fragment: impl Into<String>) -> Self {
        self.input_contains = Some(fragment.into());
        self
    }

    fn matches(&self, req: &ProviderRequest) -> bool {
        self.model.as_deref().is_none_or(|model| model == req.model)
            && self
                .system_contains
                .as_deref()
                .is_none_or(|fragment| req.system.contains(fragment))
            && self
                .input_contains
                .as_deref()
                .is_none_or(|fragment| req.input.contains(fragment))
    }
}

/// 一次 `complete` 的调用记录；不记录含令牌的 URL。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCall {
    pub model: String,
    pub call_id: CallId,
    pub ledger: LedgerId,
    pub tier: Tier,
    pub system: String,
    pub input: String,
}

#[derive(Default)]
struct State {
    rules: Vec<FakeModelRule>,
    calls: Vec<ModelCall>,
}

/// M0 的模型提供方替身。
pub struct FakeModelProvider {
    state: Mutex<State>,
    unsupported_efforts: BTreeSet<Effort>,
}

impl Default for FakeModelProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeModelProvider {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            unsupported_efforts: BTreeSet::new(),
        }
    }

    /// 声明该替身不支持的推理深度；`map_effort` 对它们 fail closed。
    pub fn with_unsupported_efforts(mut self, efforts: impl IntoIterator<Item = Effort>) -> Self {
        self.unsupported_efforts = efforts.into_iter().collect();
        self
    }

    pub fn push_rule(&self, rule: FakeModelRule) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .rules
            .push(rule);
    }

    pub fn with_rule(self, rule: FakeModelRule) -> Self {
        self.push_rule(rule);
        self
    }

    pub fn calls(&self) -> Vec<ModelCall> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .calls
            .clone()
    }

    /// 默认成功响应，供测试构造规则。
    pub fn fixed_response(text: impl Into<String>) -> ProviderResponse {
        ProviderResponse {
            text: text.into(),
            stop: StopReason::EndTurn,
            usage_reported: TokenUsage::new(10, 5, 0, 0, 1),
        }
    }
}

#[async_trait]
impl ModelProvider for FakeModelProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn map_effort(&self, model: &str, effort: Effort) -> Result<ProviderEffort, UnsupportedEffort> {
        if self.unsupported_efforts.contains(&effort) {
            return Err(UnsupportedEffort {
                model: model.to_owned(),
                effort,
            });
        }
        Ok(ProviderEffort::new(effort.as_str()))
    }

    async fn complete(
        &self,
        req: ProviderRequest,
        route: &EgressRoute,
    ) -> Result<ProviderResponse, ProviderError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(ModelCall {
            model: req.model.clone(),
            call_id: req.call_id,
            ledger: route.ledger.clone(),
            tier: route.tier,
            system: req.system.clone(),
            input: req.input.clone(),
        });
        if let Some(rule) = state.rules.iter().find(|rule| rule.matches(&req)) {
            return rule.response.clone();
        }
        Ok(Self::fixed_response("fake model response"))
    }
}
