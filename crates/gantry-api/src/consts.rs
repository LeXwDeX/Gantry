//! 引导配置的默认值（本文 §10，api.md §3.7）。
//!
//! 全是契约的 **[提议]** 默认值（OQ-14），集中在这里，禁止散落在处理程序中。

use std::time::Duration;

/// 契约主版本（`/v1/me` 的 `server.api_version`）。
pub const API_VERSION: &str = "v1";

pub const BODY_MAX: usize = 64 * 1024;
pub const BODY_MAX_SETTINGS: usize = 256 * 1024;
pub const PAIR_MAX_BODY: usize = 16 * 1024;

pub const PAGE_DEFAULT_LIMIT: u32 = 50;
pub const PAGE_MAX_LIMIT: u32 = 200;

pub const VALIDATION_MAX_ERRORS: usize = 50;

pub const IDEMPOTENCY_KEY_MIN: usize = 16;
pub const IDEMPOTENCY_KEY_MAX: usize = 64;
pub const IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(24 * 3600);
pub const IDEMPOTENCY_LEASE: Duration = Duration::from_secs(30);

/// 每分钟额度：读 / 写 / 发消息 / 签名类 / 配对（每源 IP）。
pub const RATE_READ_PER_MIN: u32 = 600;
pub const RATE_WRITE_PER_MIN: u32 = 120;
pub const RATE_MESSAGE_PER_MIN: u32 = 30;
pub const RATE_SIGN_PER_MIN: u32 = 30;
pub const RATE_PAIR_PER_MIN: u32 = 5;
/// 空闲桶淘汰时间（本文 §7.11）。
pub const RATE_IDLE_EVICT: Duration = Duration::from_secs(15 * 60);

pub const SSE_HEARTBEAT: Duration = Duration::from_secs(15);
pub const SSE_MAX_STREAMS_PER_DEVICE: usize = 3;
pub const SSE_RETRY_MS: u64 = 3000;
pub const SSE_MAX_LAG_EVENTS: u64 = 100_000;

pub const MESSAGE_MAX_TEXT: usize = 16 * 1024;
pub const MESSAGE_MAX_REFS: usize = 20;

/// 客户端证书在剩余 1/3 有效期时续期（P-64）：`renew_after = not_before + 2/3 · 寿命`。
pub const RENEW_AFTER_NUM: i64 = 2;
pub const RENEW_AFTER_DEN: i64 = 3;
