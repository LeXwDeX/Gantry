//! 设备目录（pki.md §6.2 `DeviceDirectory`、§7.5）：按序列号映射设备、审批验签公钥、
//! 引导期判定与 `last_seen` 节流；同时为握手校验器提供在册查询（§7.3 第 4 步）。
//!
//! 每次都直接读库，没有任何进程内的设备或角色缓存：降级、吊销对下一个请求立即生效。

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::entities::{Device, DevicePlatform, DeviceStatus};
use gantry_core::ids::{Clock, DeviceId};
use gantry_core::time::Timestamp;
use gantry_store::{Page, Paged, ReadSnapshot, Store, StoreError, TxReader};
use p256::ecdsa::VerifyingKey;

use crate::csr::parse_approval_pubkey;
use crate::keys::CertSerial;
use crate::store_schema::{
    CertRowStatus, DeviceCertRow, DeviceStateRow, DEVICE_CERTS, DEVICE_STATE, MODULE,
};
use crate::verifier::{CertRecord, CertStatusLookup};
use crate::PkiError;

/// `pki.last_seen_throttle` 的默认值。
pub const LAST_SEEN_THROTTLE: Duration = Duration::from_secs(60);

#[async_trait]
pub trait DeviceDirectory: Send + Sync {
    /// 鉴权热路径（api.md §2.1 步骤 2）：按握手出示的序列号读库，不缓存。
    /// 序列号不在册，或是已过原 `notAfter` 的被取代证书 → `None`。已吊销设备照常返回
    /// （`status = revoked`），由 `api` 映射为 `401 device_revoked`。
    async fn by_cert_serial(&self, serial: &CertSerial) -> Result<Option<Device>, PkiError>;
    async fn get(&self, id: &DeviceId) -> Result<Option<Device>, PkiError>;
    /// 含已吊销，按设备 ID 键集分页。
    async fn list(&self, page: &Page) -> Result<Paged<Device>, PkiError>;
    /// 审批验签用的 `approval_pubkey`（P-47），不是 mTLS 公钥；设备已吊销或未登记 → `None`。
    async fn verifying_key(&self, id: &DeviceId) -> Result<Option<VerifyingKey>, PkiError>;
    /// 引导期（P-66、D-18）：库里从未有过桌面档案设备（含已吊销）时为 `true`。
    async fn bootstrap_period(&self) -> Result<bool, PkiError>;
    /// 节流写入（每设备每 `throttle` 至多一次），不产生事件；失败只记日志。
    async fn touch_last_seen(&self, id: &DeviceId);
}

/// 基于 `store` 的设备目录。
pub struct StoreDirectory {
    store: Store,
    clock: Arc<dyn Clock>,
    throttle: Duration,
    last_written: Mutex<HashMap<DeviceId, Timestamp>>,
}

impl StoreDirectory {
    pub fn new(store: Store, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            throttle: LAST_SEEN_THROTTLE,
            last_written: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_throttle(mut self, throttle: Duration) -> Self {
        self.throttle = throttle;
        self
    }

    async fn snapshot(&self) -> Result<ReadSnapshot, PkiError> {
        self.store.snapshot().await.map_err(store_error)
    }

    /// 握手与鉴权共用的在册查询：证书行 + 所属设备。
    async fn cert_and_device(
        snapshot: &mut ReadSnapshot,
        serial: &CertSerial,
    ) -> Result<Option<(DeviceCertRow, Device)>, PkiError> {
        let row: Option<DeviceCertRow> = snapshot
            .module_conn(MODULE)
            .get(DEVICE_CERTS, serial.as_str())
            .map_err(store_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let device = snapshot
            .device(&row.device_id)
            .await
            .map_err(store_error)?;
        Ok(device.map(|device| (row, device)))
    }

    fn with_last_seen(snapshot: &ReadSnapshot, mut device: Device) -> Result<Device, PkiError> {
        let state: Option<DeviceStateRow> = snapshot
            .module_conn(MODULE)
            .get(DEVICE_STATE, &device.id.to_string())
            .map_err(store_error)?;
        device.last_seen = state.and_then(|state| state.last_seen);
        Ok(device)
    }

    async fn write_last_seen(&self, id: &DeviceId, now: Timestamp) -> Result<(), PkiError> {
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let key = id.to_string();
        let mut conn = tx.module_conn(MODULE);
        let Some(mut state) = conn
            .get::<DeviceStateRow>(DEVICE_STATE, &key)
            .map_err(store_error)?
        else {
            return Ok(());
        };
        state.last_seen = Some(now);
        conn.put(DEVICE_STATE, &key, &state).map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

#[async_trait]
impl DeviceDirectory for StoreDirectory {
    async fn by_cert_serial(&self, serial: &CertSerial) -> Result<Option<Device>, PkiError> {
        let now = self.clock.now();
        let mut snapshot = self.snapshot().await?;
        let Some((row, device)) = Self::cert_and_device(&mut snapshot, serial).await? else {
            return Ok(None);
        };
        if row.status == CertRowStatus::Superseded && now >= row.not_after {
            return Ok(None);
        }
        Self::with_last_seen(&snapshot, device).map(Some)
    }

    async fn get(&self, id: &DeviceId) -> Result<Option<Device>, PkiError> {
        let mut snapshot = self.snapshot().await?;
        match snapshot.device(id).await.map_err(store_error)? {
            Some(device) => Self::with_last_seen(&snapshot, device).map(Some),
            None => Ok(None),
        }
    }

    async fn list(&self, page: &Page) -> Result<Paged<Device>, PkiError> {
        let mut snapshot = self.snapshot().await?;
        let paged = snapshot.devices(page).await.map_err(store_error)?;
        let items = paged
            .items
            .into_iter()
            .map(|device| Self::with_last_seen(&snapshot, device))
            .collect::<Result<_, _>>()?;
        Ok(Paged {
            items,
            next_cursor: paged.next_cursor,
        })
    }

    async fn verifying_key(&self, id: &DeviceId) -> Result<Option<VerifyingKey>, PkiError> {
        let mut snapshot = self.snapshot().await?;
        let Some(device) = snapshot.device(id).await.map_err(store_error)? else {
            return Ok(None);
        };
        if device.status != DeviceStatus::Active {
            return Ok(None);
        }
        let Some(encoded) = device.approval_pubkey else {
            return Ok(None);
        };
        // 登记时已校验；读回失败说明库被改坏，失败即关闭。
        let key = parse_approval_pubkey(&encoded)
            .map_err(|error| PkiError::Crypto(format!("stored approval_pubkey: {error}")))?;
        VerifyingKey::from_sec1_bytes(&key.uncompressed_point())
            .map(Some)
            .map_err(|error| PkiError::Crypto(format!("stored approval_pubkey: {error}")))
    }

    async fn bootstrap_period(&self) -> Result<bool, PkiError> {
        let mut snapshot = self.snapshot().await?;
        let mut cursor = None;
        loop {
            let page = snapshot
                .devices(&Page {
                    limit: 200,
                    cursor,
                    ..Page::default()
                })
                .await
                .map_err(store_error)?;
            if page.items.iter().any(|device| is_desktop(&device.platform)) {
                return Ok(false);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(true),
            }
        }
    }

    async fn touch_last_seen(&self, id: &DeviceId) {
        let now = self.clock.now();
        let throttle_ms = i64::try_from(self.throttle.as_millis()).unwrap_or(i64::MAX);
        {
            let mut written = self
                .last_written
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if written.get(id).is_some_and(|last| {
                now.as_millis().saturating_sub(last.as_millis()) < throttle_ms
            }) {
                return;
            }
            written.insert(*id, now);
        }
        if let Err(error) = self.write_last_seen(id, now).await {
            tracing::warn!(device_id = %id, %error, "touch_last_seen failed");
        }
    }
}

impl CertStatusLookup for StoreDirectory {
    fn lookup(&self, serial: &CertSerial) -> Result<Option<CertRecord>, PkiError> {
        // 握手回调是同步的；内存 `store` 的读取不会挂起，挂起即视为失败（失败即关闭）。
        let found = poll_ready(async {
            let mut snapshot = self.snapshot().await?;
            Self::cert_and_device(&mut snapshot, serial).await
        })
        .ok_or(PkiError::Config("store read would block during handshake"))??;
        Ok(found.map(|(row, device)| CertRecord {
            device_id: row.device_id,
            status: row.status.into(),
            not_after: row.not_after,
            device_active: device.status == DeviceStatus::Active,
        }))
    }
}

/// 只轮询一次：就绪则返回结果，否则 `None`。
fn poll_ready<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}

pub(crate) fn is_desktop(platform: &DevicePlatform) -> bool {
    matches!(platform, DevicePlatform::Windows | DevicePlatform::Macos)
}

/// 同一写事务里读"库中是否有过桌面档案设备"（D-18、OQ-K11；已吊销也算）。
pub(crate) async fn desktop_ever_paired(reader: &mut TxReader<'_>) -> Result<bool, PkiError> {
    let mut cursor = None;
    loop {
        let page = reader
            .devices(&Page {
                limit: 200,
                cursor,
                ..Page::default()
            })
            .await
            .map_err(store_error)?;
        if page.items.iter().any(|device| is_desktop(&device.platform)) {
            return Ok(true);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(false),
        }
    }
}

pub(crate) fn encode_spki(der: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(der)
}

pub(crate) fn store_error(error: StoreError) -> PkiError {
    PkiError::Store(error.to_string())
}
