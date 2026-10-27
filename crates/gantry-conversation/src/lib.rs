//! 会话服务（`conversation`，`docs/modules/conversation.md`）。
//!
//! 主对话（每项目一个）的唯一写入者：把用户的一条消息变成"一个交付单元或一次
//! 查询"，并把后台事件变成对话里的事件行。M1 交付：
//!
//! - [`service`]：`ConversationService`（对话创建、发消息、读取）与消息流水线；
//! - [`intent`]：经 `models` 的意图解析（外部文本只进 materials，I5）；
//! - [`route`]：档位上限解析与路由，纯函数；
//! - [`rows`]：事件行的确定性模板（带 `tone`，不嵌入不可信文本）；
//! - [`ports`]：消费的引擎与模型端口（`EngineApi`、`ModelsPort`）。
//!
//! 硬约束：没有任何写设置的路径（I7）；模型输出不是权限，`control` 意图只回引导，
//! 模型给出的 `delivery_id` 不用于任何写操作（§7.4）。

#![forbid(unsafe_code)]

pub mod intent;
pub mod ports;
mod projection;
pub mod reply;
pub mod route;
pub mod rows;
pub mod service;

pub use intent::{IntentInput, IntentParser, IntentResult, ModelsIntentParser};
pub use ports::{DeliveryHandle, EngineApi, ModelsPort};
pub use route::{resolve_cap, route, Cap, CapSource, RouteCx, RouteDecision, UnsupportedReason};
pub use service::{
    Conversation, ConversationDeps, CrashPoint, Limits, ProcessError, CURSOR_EVENTS,
    CURSOR_PROJECTS, CURSOR_USER_MESSAGES,
};
