//! 幂等键登记（api.md §3.3，本文 §7.6，管线步骤 11）。
//!
//! 作用域 `(device_id, key)`；指纹 `SHA-256(method ‖ path ‖ JCS(body))`（无 body 按空串）。
//! 同键同指纹且已完成 → 原样重放；同键不同指纹 → `422 idempotency_key_reuse`；
//! 首次仍在处理且租约未到期 → `409 idempotency_in_progress`；租约过期 → 接管。
//! 首次响应 5xx 不登记（删除该行），`4xx` 登记并重放。保留 24 h。
//!
//! 本模块唯一的自有表 `api_idempotency` 要等 `store` 的 SQLite 与模块表注册
//! （store.md §7.10）落地；在那之前用 [`MemoryIdempotency`]，接口按表的语义设计，
//! 换实现时处理程序不变。业务事件另有 `dedup_key` 兜底（崩溃后完成记录丢失也
//! 不会重复追加）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gantry_core::ids::{Clock, DeviceId};
use http::{HeaderName, HeaderValue, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::consts::{IDEMPOTENCY_LEASE, IDEMPOTENCY_RETENTION};

/// 请求指纹。
pub fn fingerprint(method: &str, path: &str, body: Option<&Value>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    hasher.update([0]);
    if let Some(body) = body {
        // JCS 只在数值越界等极端输入上失败；退回原文序列化同样能区分内容。
        let bytes = gantry_core::canon::jcs(body)
            .unwrap_or_else(|_| serde_json::to_vec(body).unwrap_or_default());
        hasher.update(bytes);
    }
    hasher.finalize().into()
}

/// 首次响应的快照：状态码、需重放的头与正文。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResponse {
    pub status: StatusCode,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Begin {
    /// 本请求获得处理权（新键，或接管租约过期的行）。
    Proceed,
    Replay(StoredResponse),
    InProgress,
    Reuse,
}

/// `api_idempotency` 表的操作。
pub trait IdempotencyStore: Send + Sync {
    /// 原子登记：`INSERT … ON CONFLICT`。
    fn begin(&self, device: &DeviceId, key: &str, fingerprint: [u8; 32]) -> Begin;
    /// 写回首次响应并置 `done`；5xx 由调用方改调 [`IdempotencyStore::abandon`]。
    fn finish(&self, device: &DeviceId, key: &str, response: StoredResponse);
    /// 删除本次登记，允许同键重试（首次响应是 5xx）。
    fn abandon(&self, device: &DeviceId, key: &str);
}

enum State {
    InProgress { lease_until: i64 },
    Done(StoredResponse),
}

struct Row {
    fingerprint: [u8; 32],
    state: State,
    expires_at: i64,
}

/// 进程内实现；重启即丢失（完成记录丢失时由业务事件的 `dedup_key` 兜底）。
pub struct MemoryIdempotency {
    clock: Arc<dyn Clock>,
    rows: Mutex<HashMap<(String, String), Row>>,
}

impl MemoryIdempotency {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            rows: Mutex::new(HashMap::new()),
        }
    }

    fn now(&self) -> i64 {
        self.clock.now().as_millis()
    }

    /// 后台清理（本文 §7.6：每 10 分钟）；也在每次登记时顺带执行。
    pub fn purge_expired(&self) {
        let now = self.now();
        self.lock().retain(|_, row| row.expires_at > now);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Row>> {
        self.rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn key_of(device: &DeviceId, key: &str) -> (String, String) {
    (device.to_string(), key.to_owned())
}

fn ms(d: std::time::Duration) -> i64 {
    d.as_millis() as i64
}

impl IdempotencyStore for MemoryIdempotency {
    fn begin(&self, device: &DeviceId, key: &str, fingerprint: [u8; 32]) -> Begin {
        let now = self.now();
        let mut rows = self.lock();
        rows.retain(|_, row| row.expires_at > now);
        let slot = key_of(device, key);
        if let Some(row) = rows.get(&slot) {
            if row.fingerprint != fingerprint {
                return Begin::Reuse;
            }
            match &row.state {
                State::Done(resp) => return Begin::Replay(resp.clone()),
                State::InProgress { lease_until } if *lease_until > now => {
                    return Begin::InProgress
                }
                State::InProgress { .. } => {}
            }
        }
        rows.insert(
            slot,
            Row {
                fingerprint,
                state: State::InProgress {
                    lease_until: now + ms(IDEMPOTENCY_LEASE),
                },
                expires_at: now + ms(IDEMPOTENCY_RETENTION),
            },
        );
        Begin::Proceed
    }

    fn finish(&self, device: &DeviceId, key: &str, response: StoredResponse) {
        if let Some(row) = self.lock().get_mut(&key_of(device, key)) {
            row.state = State::Done(response);
        }
    }

    fn abandon(&self, device: &DeviceId, key: &str) {
        self.lock().remove(&key_of(device, key));
    }
}
