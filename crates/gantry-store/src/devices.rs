//! 内置 `devices` 投影（store.md §7.2）：`device.paired` / `updated` / `revoked`
//! 折叠成 Device，`cert_serial` 全表唯一。
//!
//! 只维护事件派生列；`last_seen` 等非事件列归 `pki` 的模块表（store.md OQ-4），
//! 这里恒为 `None`。折叠先在副本上校验、后统一修改，前置条件失败时不留下半折叠状态。

use std::collections::BTreeMap;

use gantry_core::entities::{Device, DeviceStatus};
use gantry_core::events::{
    decode_event_data, DevicePairedData, DeviceRevokedData, DeviceUpdatedData, Event, EventType,
    SchemaError,
};
use gantry_core::ids::{Dev, DeviceId};
use serde_json::Value;

use crate::{order_tag, subject_id, Order, Page, PageCursor, Paged, StoreError};

const DEVICES_CURSOR_TAG: &str = "devices";

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DeviceTable {
    rows: BTreeMap<DeviceId, Device>,
    /// `UNIQUE(cert_serial)`：当前序列号 → 设备（含已吊销的设备）。
    by_serial: BTreeMap<String, DeviceId>,
}

impl DeviceTable {
    pub(crate) fn folds(ty: EventType) -> bool {
        matches!(
            ty,
            EventType::DevicePaired | EventType::DeviceUpdated | EventType::DeviceRevoked
        )
    }

    pub(crate) fn apply(&mut self, event: &Event<Value>) -> Result<(), StoreError> {
        let id = subject_id::<Dev>(&event.subject)?;
        if event.project_id.is_some() {
            return Err(data_error("/project_id", "device events are global"));
        }
        match event.ty {
            EventType::DevicePaired => self.paired(event, id),
            EventType::DeviceUpdated => self.updated(event, id),
            EventType::DeviceRevoked => self.revoked(event, id),
            _ => Ok(()),
        }
    }

    fn paired(&mut self, event: &Event<Value>, id: DeviceId) -> Result<(), StoreError> {
        let data: DevicePairedData = decode_event_data(event).map_err(StoreError::InvalidEvent)?;
        if data.device_id != id {
            return Err(data_error(
                "/data/device_id",
                "device_id must equal subject",
            ));
        }
        if self.rows.contains_key(&id) {
            return Err(precondition(format!("device {id} already paired")));
        }
        self.check_serial_free(&data.cert_serial)?;
        self.by_serial.insert(data.cert_serial.clone(), id);
        self.rows.insert(
            id,
            Device {
                id,
                name: data.name,
                platform: data.platform,
                cert_serial: data.cert_serial,
                pubkey_fpr: data.pubkey_fpr,
                approval_pubkey: data.approval_pubkey,
                approval_pubkey_fpr: data.approval_pubkey_fpr,
                role: data.role,
                status: DeviceStatus::Active,
                last_seen: None,
                created_at: event.ts,
                updated_at: event.ts,
            },
        );
        Ok(())
    }

    fn updated(&mut self, event: &Event<Value>, id: DeviceId) -> Result<(), StoreError> {
        let data: DeviceUpdatedData = decode_event_data(event).map_err(StoreError::InvalidEvent)?;
        if data.device_id.is_some_and(|device_id| device_id != id) {
            return Err(data_error(
                "/data/device_id",
                "device_id must equal subject",
            ));
        }
        let mut next = self.active_row(id)?.clone();
        let old_serial = next.cert_serial.clone();
        let changed = data.changed;
        if let Some(name) = changed.name {
            next.name = name;
        }
        if let Some(role) = changed.role {
            if role.from != next.role {
                return Err(precondition(format!(
                    "device {id} role is {:?}, event expects {:?}",
                    next.role, role.from
                )));
            }
            next.role = role.to;
        }
        if let Some(serial) = changed.cert_serial {
            if serial != next.cert_serial {
                self.check_serial_free(&serial)?;
                next.cert_serial = serial;
            }
        }
        if let Some(fpr) = changed.pubkey_fpr {
            next.pubkey_fpr = fpr;
        }
        next.updated_at = event.ts;
        if next.cert_serial != old_serial {
            self.by_serial.remove(&old_serial);
            self.by_serial.insert(next.cert_serial.clone(), id);
        }
        self.rows.insert(id, next);
        Ok(())
    }

    fn revoked(&mut self, event: &Event<Value>, id: DeviceId) -> Result<(), StoreError> {
        let data: DeviceRevokedData = decode_event_data(event).map_err(StoreError::InvalidEvent)?;
        if data.device_id != id {
            return Err(data_error(
                "/data/device_id",
                "device_id must equal subject",
            ));
        }
        self.active_row(id)?;
        if let Some(row) = self.rows.get_mut(&id) {
            row.status = DeviceStatus::Revoked;
            row.updated_at = event.ts;
        }
        Ok(())
    }

    /// 设备必须存在且未吊销：吊销不可逆，吊销后的改名 / 改角色 / 再吊销都是前置条件失败。
    fn active_row(&self, id: DeviceId) -> Result<&Device, StoreError> {
        let row = self
            .rows
            .get(&id)
            .ok_or_else(|| precondition(format!("device {id} not found")))?;
        if row.status != DeviceStatus::Active {
            return Err(precondition(format!("device {id} is revoked")));
        }
        Ok(row)
    }

    fn check_serial_free(&self, serial: &str) -> Result<(), StoreError> {
        if self.by_serial.contains_key(serial) {
            return Err(precondition(format!(
                "cert_serial {serial} already belongs to a device"
            )));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 读取（store.md §6.2）
    // -----------------------------------------------------------------------

    pub(crate) fn get(&self, id: &DeviceId) -> Option<Device> {
        self.rows.get(id).cloned()
    }

    /// 按**当前**序列号查；续期后被取代的旧序列号由 `pki` 的证书表负责。
    pub(crate) fn by_cert_serial(&self, serial: &str) -> Option<Device> {
        self.by_serial
            .get(serial)
            .and_then(|id| self.rows.get(id))
            .cloned()
    }

    /// 全部设备（含已吊销），按 `DeviceId` 键集分页。
    pub(crate) fn list(&self, p: &Page) -> Result<Paged<Device>, StoreError> {
        if !(1..=200).contains(&p.limit) {
            return Err(precondition("page limit must be 1..=200".to_owned()));
        }
        let after = match &p.cursor {
            None => None,
            Some(cursor) => {
                let mut parts = cursor.as_str().splitn(3, ':');
                let (Some(tag), Some(order), Some(key)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Err(StoreError::InvalidCursor);
                };
                if tag != DEVICES_CURSOR_TAG || order != order_tag(p.order) {
                    return Err(StoreError::InvalidCursor);
                }
                Some(
                    key.parse::<DeviceId>()
                        .map_err(|_| StoreError::InvalidCursor)?,
                )
            }
        };
        let limit = p.limit as usize;
        let mut items: Vec<Device> = match p.order {
            Order::Asc => self
                .rows
                .values()
                .filter(|row| after.is_none_or(|after| row.id > after))
                .take(limit + 1)
                .cloned()
                .collect(),
            Order::Desc => self
                .rows
                .values()
                .rev()
                .filter(|row| after.is_none_or(|after| row.id < after))
                .take(limit + 1)
                .cloned()
                .collect(),
        };
        let next_cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|row| {
                PageCursor::new(format!(
                    "{DEVICES_CURSOR_TAG}:{}:{}",
                    order_tag(p.order),
                    row.id
                ))
            })
        } else {
            None
        };
        Ok(Paged { items, next_cursor })
    }
}

fn data_error(pointer: &'static str, message: &'static str) -> StoreError {
    StoreError::InvalidEvent(SchemaError::consistency(pointer, message))
}

fn precondition(message: String) -> StoreError {
    StoreError::PreconditionFailed(message)
}
