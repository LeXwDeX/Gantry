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

use async_trait::async_trait;
use gantry_core::ids::{Clock, DeviceId};
use gantry_core::ports::ModuleError;
use gantry_store::{ModuleSpec, Store};
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

/// 一次登记（含接管）的所有者令牌：只有当前租约的持有者能完成或删除该行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease(u64);

#[derive(Debug, PartialEq, Eq)]
pub enum Begin {
    /// 本请求获得处理权（新键，或接管租约过期的行）。
    Proceed(Lease),
    Replay(StoredResponse),
    InProgress,
    Reuse,
}

/// `api_idempotency` 表的操作。
#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// 原子登记：`INSERT … ON CONFLICT`。
    async fn begin(
        &self,
        device: &DeviceId,
        key: &str,
        fingerprint: [u8; 32],
    ) -> Result<Begin, ModuleError>;
    /// 写回首次响应并置 `done`；5xx 由调用方改调 [`IdempotencyStore::abandon`]。
    /// 租约已被接管（`lease` 不是当前持有者）时不做任何事。
    async fn finish(
        &self,
        device: &DeviceId,
        key: &str,
        lease: Lease,
        response: StoredResponse,
    ) -> Result<(), ModuleError>;
    /// 删除本次登记，允许同键重试（首次响应是 5xx）；同样只对当前持有者生效。
    async fn abandon(&self, device: &DeviceId, key: &str, lease: Lease) -> Result<(), ModuleError>;
}

enum State {
    InProgress { lease_until: i64, lease: Lease },
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
    next_lease: std::sync::atomic::AtomicU64,
}

impl MemoryIdempotency {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            rows: Mutex::new(HashMap::new()),
            next_lease: std::sync::atomic::AtomicU64::new(1),
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

#[async_trait]
impl IdempotencyStore for MemoryIdempotency {
    async fn begin(
        &self,
        device: &DeviceId,
        key: &str,
        fingerprint: [u8; 32],
    ) -> Result<Begin, ModuleError> {
        let now = self.now();
        let mut rows = self.lock();
        rows.retain(|_, row| row.expires_at > now);
        let slot = key_of(device, key);
        if let Some(row) = rows.get(&slot) {
            if row.fingerprint != fingerprint {
                return Ok(Begin::Reuse);
            }
            match &row.state {
                State::Done(resp) => return Ok(Begin::Replay(resp.clone())),
                State::InProgress { lease_until, .. } if *lease_until > now => {
                    return Ok(Begin::InProgress)
                }
                State::InProgress { .. } => {}
            }
        }
        let lease = Lease(
            self.next_lease
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        rows.insert(
            slot,
            Row {
                fingerprint,
                state: State::InProgress {
                    lease_until: now + ms(IDEMPOTENCY_LEASE),
                    lease,
                },
                expires_at: now + ms(IDEMPOTENCY_RETENTION),
            },
        );
        Ok(Begin::Proceed(lease))
    }

    async fn finish(
        &self,
        device: &DeviceId,
        key: &str,
        lease: Lease,
        response: StoredResponse,
    ) -> Result<(), ModuleError> {
        if let Some(row) = self.lock().get_mut(&key_of(device, key)) {
            if matches!(row.state, State::InProgress { lease: held, .. } if held == lease) {
                row.state = State::Done(response);
            }
        }
        Ok(())
    }

    async fn abandon(&self, device: &DeviceId, key: &str, lease: Lease) -> Result<(), ModuleError> {
        let mut rows = self.lock();
        let slot = key_of(device, key);
        let held = rows.get(&slot).is_some_and(
            |row| matches!(row.state, State::InProgress { lease: held, .. } if held == lease),
        );
        if held {
            rows.remove(&slot);
        }
        Ok(())
    }
}

/// `api` 的模块表注册（store.md §7.10）。
pub fn module_spec() -> ModuleSpec {
    ModuleSpec::new("api", &[TABLE])
}

const TABLE: &str = "api_idempotency";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PersistedResponse {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PersistedRow {
    fingerprint: [u8; 32],
    state: String,
    response: Option<PersistedResponse>,
    lease: u64,
    lease_until: i64,
    expires_at: i64,
}

impl PersistedRow {
    fn done(response: StoredResponse) -> Self {
        Self {
            fingerprint: [0; 32],
            state: "done".to_owned(),
            response: Some(PersistedResponse {
                status: response.status.as_u16(),
                headers: response
                    .headers
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
                    .collect(),
                body: response.body,
            }),
            lease: 0,
            lease_until: 0,
            expires_at: 0,
        }
    }

    fn stored_response(&self) -> Option<StoredResponse> {
        self.response.as_ref().and_then(|response| {
            let status = StatusCode::from_u16(response.status).ok()?;
            let headers = response
                .headers
                .iter()
                .filter_map(|(name, value)| {
                    let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
                    let value = HeaderValue::from_bytes(value).ok()?;
                    Some((name, value))
                })
                .collect();
            Some(StoredResponse {
                status,
                headers,
                body: response.body.clone(),
            })
        })
    }
}

/// 持久化实现：每次操作一个 store 写事务，只访问 `api_idempotency` 表。
pub struct SqliteIdempotency {
    store: Store,
    clock: Arc<dyn Clock>,
}

fn store_error(error: gantry_store::StoreError) -> ModuleError {
    ModuleError::Store(gantry_core::ports::StoreError::InvalidState {
        message: error.to_string(),
    })
}

impl SqliteIdempotency {
    pub fn new(store: Store, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    fn now(&self) -> i64 {
        self.clock.now().as_millis()
    }

    fn key_of(device: &DeviceId, key: &str) -> String {
        format!("{}:{key}", device.as_u128())
    }
}

#[async_trait]
impl IdempotencyStore for SqliteIdempotency {
    async fn begin(
        &self,
        device: &DeviceId,
        key: &str,
        fingerprint: [u8; 32],
    ) -> Result<Begin, ModuleError> {
        let now = self.now();
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let slot = Self::key_of(device, key);
        let (begin, write) = {
            let conn = tx.module_conn("api");
            let row = conn
                .get::<PersistedRow>(TABLE, &slot)
                .map_err(store_error)?;
            match row {
                Some(row) if row.expires_at > now => {
                    if row.fingerprint != fingerprint {
                        (Begin::Reuse, None)
                    } else if row.state == "done" {
                        match row.stored_response() {
                            Some(response) => (Begin::Replay(response), None),
                            None => (Begin::InProgress, None),
                        }
                    } else if row.lease_until > now {
                        (Begin::InProgress, None)
                    } else {
                        let lease = row.lease.wrapping_add(1).max(1);
                        let next = PersistedRow {
                            fingerprint,
                            state: "in_progress".to_owned(),
                            response: None,
                            lease,
                            lease_until: now + ms(IDEMPOTENCY_LEASE),
                            expires_at: now + ms(IDEMPOTENCY_RETENTION),
                        };
                        (Begin::Proceed(Lease(lease)), Some(next))
                    }
                }
                _ => {
                    let next = PersistedRow {
                        fingerprint,
                        state: "in_progress".to_owned(),
                        response: None,
                        lease: 1,
                        lease_until: now + ms(IDEMPOTENCY_LEASE),
                        expires_at: now + ms(IDEMPOTENCY_RETENTION),
                    };
                    (Begin::Proceed(Lease(1)), Some(next))
                }
            }
        };
        if let Some(next) = write {
            tx.module_conn("api")
                .put(TABLE, &slot, &next)
                .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(begin)
    }

    async fn finish(
        &self,
        device: &DeviceId,
        key: &str,
        lease: Lease,
        response: StoredResponse,
    ) -> Result<(), ModuleError> {
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let slot = Self::key_of(device, key);
        let update = {
            let conn = tx.module_conn("api");
            conn.get::<PersistedRow>(TABLE, &slot)
                .map_err(store_error)?
                .and_then(|row| {
                    (row.state == "in_progress" && row.lease == lease.0).then(|| PersistedRow {
                        fingerprint: row.fingerprint,
                        state: "done".to_owned(),
                        response: PersistedRow::done(response).response,
                        lease: 0,
                        lease_until: 0,
                        expires_at: row.expires_at,
                    })
                })
        };
        if let Some(row) = update {
            tx.module_conn("api")
                .put(TABLE, &slot, &row)
                .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }

    async fn abandon(&self, device: &DeviceId, key: &str, lease: Lease) -> Result<(), ModuleError> {
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let slot = Self::key_of(device, key);
        let remove = {
            let conn = tx.module_conn("api");
            conn.get::<PersistedRow>(TABLE, &slot)
                .map_err(store_error)?
                .is_some_and(|row| row.state == "in_progress" && row.lease == lease.0)
        };
        if remove {
            tx.module_conn("api")
                .delete(TABLE, &slot)
                .map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gantry_core::testing::FakeClock;

    fn resp(body: &str) -> StoredResponse {
        StoredResponse {
            status: StatusCode::OK,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn sqlite_rows_survive_reopen() {
        use gantry_core::ids::IdGen;
        use gantry_core::testing::SeqEntropy;
        use gantry_store::{StoreConfig, StoreDeps};

        let dir = std::env::temp_dir().join(format!(
            "gantry-api-idem-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let clock = Arc::new(FakeClock::new(1_700_000_000_000));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
        let deps = || StoreDeps {
            clock: clock.clone(),
            ids: ids.clone(),
        };
        let dev = DeviceId::from_parts(1, 1).unwrap();
        let key = "key-0000000000000002";

        let store = Store::open(StoreConfig::new(&dir), deps(), &[module_spec()])
            .await
            .unwrap();
        let idem = SqliteIdempotency::new(store.clone(), clock.clone());
        let Begin::Proceed(lease) = idem.begin(&dev, key, [3; 32]).await.unwrap() else {
            panic!("first request proceeds");
        };
        idem.finish(&dev, key, lease, resp("from-disk"))
            .await
            .unwrap();
        drop(idem);
        drop(store);

        let store = Store::open(StoreConfig::new(&dir), deps(), &[module_spec()])
            .await
            .unwrap();
        let idem = SqliteIdempotency::new(store, clock);
        assert_eq!(
            idem.begin(&dev, key, [3; 32]).await.unwrap(),
            Begin::Replay(resp("from-disk"))
        );
        assert_eq!(idem.begin(&dev, key, [4; 32]).await.unwrap(), Begin::Reuse);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首次请求 A 超过租约、重试 B 接管后，A 迟到的完成或删除不能覆盖 B 的记录。
    #[tokio::test]
    async fn only_the_current_lease_holder_can_finish_or_abandon() {
        let clock = Arc::new(FakeClock::new(0));
        let store = MemoryIdempotency::new(clock.clone());
        let dev = DeviceId::from_parts(1, 1).unwrap();
        let Begin::Proceed(a) = store
            .begin(&dev, "key-0000000000000001", [1; 32])
            .await
            .unwrap()
        else {
            panic!("first request proceeds");
        };
        clock.advance_secs(31);
        let Begin::Proceed(b) = store
            .begin(&dev, "key-0000000000000001", [1; 32])
            .await
            .unwrap()
        else {
            panic!("expired lease is taken over");
        };
        assert_ne!(a, b);
        store
            .finish(&dev, "key-0000000000000001", b, resp("from-b"))
            .await
            .unwrap();
        store
            .abandon(&dev, "key-0000000000000001", a)
            .await
            .unwrap();
        store
            .finish(&dev, "key-0000000000000001", a, resp("from-a"))
            .await
            .unwrap();
        assert_eq!(
            store
                .begin(&dev, "key-0000000000000001", [1; 32])
                .await
                .unwrap(),
            Begin::Replay(resp("from-b"))
        );
        assert_eq!(
            store
                .begin(&dev, "key-0000000000000001", [2; 32])
                .await
                .unwrap(),
            Begin::Reuse
        );
    }
}
