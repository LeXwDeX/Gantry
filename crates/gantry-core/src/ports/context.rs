//! 跨模块端口的公共上下文、事务句柄、分页与通用错误。
//!
//! 本模块不执行 IO。`ActorCtx` 的字段逐字对应 decisions.md P-78；所有跨模块
//! 写方法都接收它，不再引入 `WriteCtx` / `ActorRef` / `DeviceActor` /
//! `RequestCtx` 别名。

use std::any::Any;
use std::time::Duration;

use crate::entities::{Actor, DeviceRole};
use crate::ids::{DeviceId, EventId, Ulid};

/// 请求关联标识。线上形态是无前缀 ULID（api.md §1.5）。
pub type RequestId = Ulid;

/// CAS 引用。core §6.6 明确 `CasRef = Sha256Digest`。
pub type CasRef = crate::canon::Sha256Digest;

/// 客户端档案，由 `Device.platform` 推出（api.md §2.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientProfile {
    Desktop,
    Mobile,
}

/// 跨端口传递调用方的写事务。
///
/// `store::WriteTx` 实现它；提供方用 `as_any_mut` 下转型取回具体事务。
/// `core` 不知道 SQLite，也不依赖 `store`。
pub trait TxHandle: Send {
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// 统一的写操作上下文（P-78）。
///
/// 设备请求由 `api` 在鉴权后构造；`engine` / `guardian` / `forge` / `system`
/// 自行发起的写也生成 `request_id`，设备字段为空。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorCtx {
    pub actor: Actor,
    pub device: Option<DeviceId>,
    pub role: Option<DeviceRole>,
    pub client_profile: Option<ClientProfile>,
    pub request_id: RequestId,
    pub idempotency_key: Option<String>,
    pub dedup_key: Option<String>,
    pub causation_id: Option<EventId>,
}

/// `ActorCtx` 的字段名快照，供契约测试核对 P-78。
pub const ACTOR_CTX_FIELDS: [&str; 8] = [
    "actor",
    "device",
    "role",
    "client_profile",
    "request_id",
    "idempotency_key",
    "dedup_key",
    "causation_id",
];

/// `store` 写事务与读失败在端口层的表示。
///
/// 它刻意不包含 HTTP 状态码：`api` 负责把领域错误映射为 problem details。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("store is unavailable")]
    Unavailable { retry_after: Option<Duration> },
    #[error("store conflict: {code}")]
    Conflict { code: &'static str },
    #[error("store record not found")]
    NotFound,
    #[error("store rejected the write: {message}")]
    InvalidState { message: String },
    #[error("store rejected the request: {message}")]
    Rejected { message: String },
}

impl StoreError {
    /// 稳定的低基数错误码，供日志与指标使用。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "store_unavailable",
            Self::Conflict { .. } => "store_conflict",
            Self::NotFound => "store_not_found",
            Self::InvalidState { .. } => "store_invalid_state",
            Self::Rejected { .. } => "store_rejected",
        }
    }
}

/// 端口方法的通用分页查询（不透明游标）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageQuery {
    pub cursor: Option<String>,
    pub limit: u32,
}

/// 端口方法的通用分页响应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// 分页结果附带的最后事件序号，用于 `X-Event-Seq`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageWithSeq<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub last_seq: crate::entities::Seq,
}

/// 模块健康度（`settings/status` 的低基数枚举）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Health {
    Ok,
    Degraded,
    Unhealthy,
}

/// 多个模块共用的操作错误。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModuleError {
    #[error("record not found")]
    NotFound,
    #[error("invalid state: {message}")]
    InvalidState { message: String },
    #[error("validation failed at {pointer}: {message}")]
    Validation { pointer: String, message: String },
    #[error("conflict: {message}")]
    Conflict { message: String },
    #[error("forbidden")]
    Forbidden,
    #[error("module unavailable: {message}")]
    Unavailable { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}
