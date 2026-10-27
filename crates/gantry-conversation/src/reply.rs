//! 回复与系统提示的确定性模板（§7.7）。模板不嵌入任何不可信文本；对象用
//! [`Ref`](gantry_core::entities::Ref) 表示。

use gantry_core::entities::{AgentKind, MessageRole, MessageTone, Ref};
use gantry_core::ids::DeliveryId;
use gantry_core::level::Level;
use gantry_core::ports::engine::EngineError;

use crate::route::{Cap, CapSource};

/// 本条消息处理后追加的一条回复。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub role: MessageRole,
    pub agent_kind: Option<AgentKind>,
    pub text: String,
    pub tone: Option<MessageTone>,
    pub refs: Vec<Ref>,
    pub delivery_id: Option<DeliveryId>,
}

impl Reply {
    /// `role = event` 的系统提示。
    pub fn event(tone: MessageTone, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Event,
            agent_kind: None,
            text: text.into(),
            tone: Some(tone),
            refs: Vec::new(),
            delivery_id: None,
        }
    }

    pub fn agent(kind: AgentKind, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Agent,
            agent_kind: Some(kind),
            text: text.into(),
            tone: None,
            refs: Vec::new(),
            delivery_id: None,
        }
    }

    pub fn with_refs(mut self, refs: Vec<Ref>) -> Self {
        self.refs = refs;
        self
    }
}

pub const NOT_CONFIGURED: &str =
    "需要先在设置页配置模型（models.planner 与 models.utility），之后才能处理对话消息。";
pub const INTENT_FAILED: &str = "暂时无法理解这条消息，请重试。";
pub const TIMED_OUT: &str = "处理超时，请重试。";
pub const STATUS_FAILED: &str = "暂时无法回答这个问题，请重试。";
pub const DAILY_QUOTA: &str = "今日问答额度已用完。";
pub const CODE_ANSWER_CLOSED: &str = "代码问答暂未开放。";
pub const NONE_LEVEL_NOTE: &str = "None 档不修改代码，要改请提高上限。";
pub const AMBIGUOUS: &str = "引用分属多个交付单元，请用 @ 指定其中一个。";
pub const NO_OPEN_QUESTION: &str = "该交付单元当前阶段不接收补充。";
pub const NOT_YET: &str = "对话内补充规格与调整上限暂未开放，请在检视器中操作。";
pub const CONTROL_GUIDANCE: &str =
    "对话不会直接执行控制操作：请在检视器使用暂停 / 继续 / 重试 / 放弃，\
或在上报卡上选择。模型、预算、策略等设置请到设置页修改。";
pub const LOCATE_FAILED: &str = "暂时无法定位引用的交付单元，请重试。";

/// 任务回执：复述实际生效的上限（§7.3、prototype §2.3）。
pub fn created(delivery: DeliveryId, cap: Cap) -> Reply {
    let note = match cap.source {
        CapSource::Message => "（取自输入区选择器）",
        CapSource::Delivery => "（沿用所引用交付单元的上限）",
        CapSource::Default => "（项目默认）",
        CapSource::MobileDefault => "（手机端使用项目默认上限）",
    };
    let mut reply = Reply::agent(
        AgentKind::Planner,
        format!("已创建交付单元，档位上限 {}{note}。", level_name(cap.level)),
    )
    .with_refs(vec![Ref::Delivery { id: delivery }]);
    reply.delivery_id = Some(delivery);
    reply
}

pub fn engine_failed(error: &EngineError) -> Reply {
    let text = match error {
        EngineError::NotConfigured => "所需档位的模型尚未配置，请到设置页配置后重试。".to_owned(),
        other => format!("创建交付单元失败：{}", engine_code(other)),
    };
    Reply::event(MessageTone::Warn, text)
}

fn engine_code(error: &EngineError) -> &'static str {
    match error {
        EngineError::NotConfigured => "not_configured",
        EngineError::InvalidState { .. } => "invalid_state",
        EngineError::Validation { .. } => "validation_failed",
        EngineError::NotFound => "not_found",
        EngineError::BudgetExhausted { .. } => "budget_exhausted",
        EngineError::Forbidden => "forbidden",
        EngineError::Conflict { .. } => "conflict",
        EngineError::Store(error) => error.code(),
    }
}

pub fn level_name(level: Level) -> &'static str {
    level.as_str()
}
