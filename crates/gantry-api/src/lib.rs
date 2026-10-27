#![forbid(unsafe_code)]

//! API 网关（`api`，docs/modules/api.md）。
//!
//! 服务端对客户端的唯一网络面。本 crate 按 M1 切片落地，当前包含：
//!
//! - [`registry`]：契约 §4.1 的 48 条路由（数据表），驱动鉴权链、限流、幂等策略、体积上限与手机子集；
//! - [`problem`]：统一错误 `application/problem+json` 与 30 个稳定 `code`；
//! - [`server`]：请求处理管线（本文 §7.1）与公共响应头；
//! - [`auth`]：设备映射、角色与档案；`pki` 的设备目录经窄接口 [`auth::DeviceLookup`] 接入；
//! - [`rate`]：按设备的令牌桶；
//! - [`query`]：查询参数的严格校验；
//! - [`validate`]：请求体按契约组件校验（I7：未知字段 → `unknown_field`）；
//! - [`idempotency`]：`Idempotency-Key` 登记与重放；
//! - [`control`]：交付单元控制与发消息（`DeliveryControl` / `ConversationService`，收 `ActorCtx`）。
//!
//! TLS 监听器、读路由、事件流、审批与 settings 在后续切片加入（issue #99）。

pub mod auth;
pub mod consts;
pub mod control;
pub mod devices;
pub mod handlers;
pub mod idempotency;
pub mod problem;
pub mod query;
pub mod rate;
pub mod reads;
pub mod registry;
pub mod server;
pub mod sse;
pub mod validate;

pub use auth::{AuthContext, PeerCert};
pub use problem::{FieldError, Problem, ProblemCode};
pub use registry::{Milestone, RouteSpec, ROUTES};
pub use server::{
    Admission, Admitted, ApiConfig, ApiCore, ApiResponse, Body, HandlerRequest, Reply, RequestHead,
    RouteHandler, Services,
};
