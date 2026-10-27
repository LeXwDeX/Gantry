//! 档位上限解析（§7.3）与路由（§7.6）：纯函数，输入全部是已校验的确定性事实。
//!
//! 目标交付单元只由 `refs` 经 `locate_deliveries` 得出；模型给出的
//! `delivery_id` 提示不进入这里（§7.4、I5）。

use gantry_core::ids::{DeliveryId, InboxId};
use gantry_core::level::Level;
use gantry_core::ports::context::ClientProfile;
use gantry_core::worker::IntentKind;

use crate::ports::DeliveryHandle;

/// 实际生效的上限及其来源（回执复述用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapSource {
    /// 消息的 `level_cap`（输入区选择器）。
    Message,
    /// `@` 指向的唯一交付单元的上限。
    Delivery,
    /// `levels.default_level` 的项目生效值。
    Default,
    /// 手机档案忽略了消息里的 `level_cap`（C26、P-35）。
    MobileDefault,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    pub level: Level,
    pub source: CapSource,
}

/// §7.3：消息显式 > `@` 的唯一交付单元 > 项目默认；手机档案一律项目默认。
pub fn resolve_cap(
    profile: Option<ClientProfile>,
    message_cap: Option<Level>,
    located: &[DeliveryHandle],
    default_level: Level,
) -> Cap {
    if profile == Some(ClientProfile::Mobile) {
        return Cap {
            level: default_level,
            source: CapSource::MobileDefault,
        };
    }
    if let Some(level) = message_cap {
        return Cap {
            level,
            source: CapSource::Message,
        };
    }
    if let [only] = located {
        return Cap {
            level: only.level_cap,
            source: CapSource::Delivery,
        };
    }
    Cap {
        level: default_level,
        source: CapSource::Default,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// `@` 的交付单元当前没有未决的 `spec_needed`（OQ-3）。
    NoOpenQuestion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteDecision {
    NotConfigured,
    PureChat,
    CreateDelivery {
        cap: Level,
    },
    SupplySpec {
        delivery: DeliveryId,
        inbox: InboxId,
    },
    SetCapThenContinue {
        delivery: DeliveryId,
        cap: Level,
    },
    StatusAnswer,
    CodeAnswer,
    Ambiguous {
        candidates: Vec<DeliveryId>,
    },
    Unsupported {
        reason: UnsupportedReason,
    },
    ControlGuidance,
}

/// `route` 的输入。
#[derive(Clone, Debug)]
pub struct RouteCx<'a> {
    pub models_configured: bool,
    pub cap: Cap,
    pub located: &'a [DeliveryHandle],
    pub intent: IntentKind,
    pub needs_code: bool,
    pub profile: Option<ClientProfile>,
}

/// §7.6 路由表，自上而下命中即止。
pub fn route(cx: &RouteCx<'_>) -> RouteDecision {
    if !cx.models_configured {
        return RouteDecision::NotConfigured;
    }
    if cx.cap.level == Level::None && cx.intent != IntentKind::Control {
        return RouteDecision::PureChat;
    }
    match cx.intent {
        IntentKind::Control => RouteDecision::ControlGuidance,
        IntentKind::Query if cx.needs_code => RouteDecision::CodeAnswer,
        IntentKind::Query => RouteDecision::StatusAnswer,
        IntentKind::Task => match cx.located {
            [] => RouteDecision::CreateDelivery { cap: cx.cap.level },
            [only] => {
                if let Some(inbox) = only.open_spec_needed {
                    return RouteDecision::SupplySpec {
                        delivery: only.id,
                        inbox,
                    };
                }
                // 只有桌面档案、显式带了不同上限时才先调档（P-82）。
                let changes_cap = cx.cap.source == CapSource::Message
                    && cx.cap.level != only.level_cap
                    && cx.profile != Some(ClientProfile::Mobile);
                if changes_cap {
                    RouteDecision::SetCapThenContinue {
                        delivery: only.id,
                        cap: cx.cap.level,
                    }
                } else {
                    RouteDecision::Unsupported {
                        reason: UnsupportedReason::NoOpenQuestion,
                    }
                }
            }
            many => RouteDecision::Ambiguous {
                candidates: many.iter().map(|handle| handle.id).collect(),
            },
        },
    }
}
