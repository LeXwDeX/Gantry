//! 设备管理（pki.md §6.2 `DeviceAdmin`、§7.5–§7.7）：改名、改角色、吊销（含
//! `revocations.journal`）、设备证书续期与恢复后重放。
//!
//! `set_role` / `revoke` 在**调用方**的写事务里执行（`api` 已在同一事务里让 `policy`
//! 消费审批）；调用方回滚则不留任何副作用——除了吊销日志里多出的一行，重放时照样吊销，
//! 偏向安全（§7.6）。

use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{Actor, ActorKind, Device, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::events::{
    DeviceChanged, DeviceRevokedData, DeviceUpdatedData, EventType, NewEvent, RoleChange, Subject,
};
use gantry_core::ids::{Clock, DeviceId, Entropy};
use gantry_core::time::Timestamp;
use gantry_store::{Page, Store, WriteTx};

use crate::config::CertPolicy;
use crate::csr::validate_csr;
use crate::directory::{encode_spki, store_error};
use crate::journal::{JournalEntry, JournalError, RevocationJournal, JOURNAL_VERSION};
use crate::keys::CertSerial;
use crate::pairing_service::{FieldError, Issuer};
use crate::store_schema::{
    CertRowStatus, DeviceCertRow, DeviceStateRow, DEVICE_CERTS, DEVICE_STATE, MODULE,
};
use crate::text::validate_device_name;
use crate::PkiError;

/// 主机应急命令等写入的审计字段（§7.8：`via`、`host_uid`、`sudo_user`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Audit {
    pub via: Option<String>,
    pub host_uid: Option<u32>,
    pub sudo_user: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("device not found")]
    NotFound,
    /// `api` 映射为 `409 conflict / self_role_change`。
    #[error("a device cannot change its own role")]
    SelfRoleChange,
    /// `api` 映射为 `409 conflict / mobile_cannot_approve`（P-76）。
    #[error("mobile devices cannot be approvers")]
    MobileCannotApprove,
    #[error("device is revoked")]
    Revoked,
    #[error("device request invalid: {0:?}")]
    Validation(Vec<FieldError>),
    /// 吊销日志追加或复制失败：吊销不提交（`api` 映射为 `503`）。
    #[error("revocation journal: {0}")]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Internal(#[from] PkiError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked(Device),
    AlreadyRevoked(Device),
}

#[derive(Clone, Debug)]
pub struct RenewRequest {
    pub csr_pem: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenewResponse {
    /// `[叶子, 中间 CA, 根]`（P-64）。
    pub certificate_chain: [String; 3],
    pub not_after: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum RenewError {
    /// `422`。
    #[error("renew request invalid: {0:?}")]
    Validation(Vec<FieldError>),
    /// `401 device_revoked`：设备已吊销，或出示的证书已不在册。
    #[error("device revoked")]
    Revoked,
    #[error(transparent)]
    Internal(#[from] PkiError),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplayReport {
    pub replayed: Vec<DeviceId>,
    pub already_revoked: u32,
    pub unknown_devices: u32,
}

#[async_trait]
pub trait DeviceAdmin: Send + Sync {
    /// 自成事务；事件 `device.updated`（`changed.name`）。
    async fn rename(&self, id: &DeviceId, name: &str, actor: &Actor)
        -> Result<Device, DeviceError>;
    /// 在调用方事务内执行。角色不变时不发事件。
    async fn set_role(
        &self,
        tx: &mut WriteTx,
        id: &DeviceId,
        role: DeviceRole,
        actor: &Actor,
    ) -> Result<Device, DeviceError>;
    /// 在调用方事务内执行；吊销该设备全部证书序列号，并在返回前追加并 fsync 一行吊销日志。
    async fn revoke(
        &self,
        tx: &mut WriteTx,
        id: &DeviceId,
        actor: &Actor,
    ) -> Result<RevokeOutcome, DeviceError>;
    /// `POST /v1/devices/self/renew`（P-64）。
    async fn renew(
        &self,
        id: &DeviceId,
        presented_serial: &CertSerial,
        req: RenewRequest,
    ) -> Result<RenewResponse, RenewError>;
    /// 从备份恢复后重放 `revoked_at` 晚于 `backup_at` 的吊销（P-67、P-80），幂等。
    async fn replay_revocations(&self, backup_at: Timestamp) -> Result<ReplayReport, PkiError>;
}

pub struct StoreAdmin {
    store: Store,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    issuer: Arc<Issuer>,
    journal: RevocationJournal,
    policy: CertPolicy,
    max_csr_bytes: usize,
    device_name_max_chars: usize,
}

impl StoreAdmin {
    pub fn new(
        store: Store,
        clock: Arc<dyn Clock>,
        entropy: Arc<dyn Entropy>,
        issuer: Arc<Issuer>,
        journal: RevocationJournal,
        policy: CertPolicy,
    ) -> Self {
        Self {
            store,
            clock,
            entropy,
            issuer,
            journal,
            policy,
            max_csr_bytes: 4096,
            device_name_max_chars: 64,
        }
    }

    /// 改角色，带审计字段（`gantry device recover --promote` 用）。
    pub async fn set_role_with(
        &self,
        tx: &mut WriteTx,
        id: &DeviceId,
        role: DeviceRole,
        actor: &Actor,
        audit: Audit,
    ) -> Result<Device, DeviceError> {
        if actor.kind == ActorKind::Device && actor.id == id.to_string() {
            return Err(DeviceError::SelfRoleChange);
        }
        let device = active_device(tx, id).await?;
        if device.platform == DevicePlatform::Ios && role == DeviceRole::Approver {
            return Err(DeviceError::MobileCannotApprove);
        }
        if device.role == role {
            return Ok(device);
        }
        let changed = DeviceChanged {
            role: Some(RoleChange {
                from: device.role,
                to: role,
            }),
            ..DeviceChanged::default()
        };
        append_updated(tx, id, changed, actor, audit).await?;
        reread(tx, id).await
    }

    /// 吊销，带审计字段。`write_journal = false` 只用于从日志重放（行已在日志里）。
    pub async fn revoke_with(
        &self,
        tx: &mut WriteTx,
        id: &DeviceId,
        actor: &Actor,
        audit: Audit,
        write_journal: bool,
    ) -> Result<RevokeOutcome, DeviceError> {
        let device = tx
            .read()
            .device(id)
            .await
            .map_err(store_error)?
            .ok_or(DeviceError::NotFound)?;
        if device.status == DeviceStatus::Revoked {
            return Ok(RevokeOutcome::AlreadyRevoked(device));
        }
        let mut rows = device_cert_rows(tx, id)?;
        for row in &mut rows {
            row.status = CertRowStatus::Revoked;
            tx.module_conn(MODULE)
                .put(DEVICE_CERTS, row.serial.as_str(), row)
                .map_err(store_error)?;
        }
        let remaining_approvers = active_devices(tx)
            .await?
            .iter()
            .filter(|other| other.id != *id && other.role == DeviceRole::Approver)
            .count() as u64;
        let cert_serials: Vec<CertSerial> = rows.into_iter().map(|row| row.serial).collect();
        tx.append(device_event(
            EventType::DeviceRevoked,
            id,
            actor,
            &DeviceRevokedData {
                device_id: *id,
                cert_serials: cert_serials.iter().map(ToString::to_string).collect(),
                remaining_approvers,
                via: audit.via.clone(),
            },
        )?)
        .await
        .map_err(store_error)?;
        if write_journal {
            // 最后一步、提交之前：失败则调用方回滚，吊销不生效（失败即关闭）。
            let identity = self.store.log_identity();
            self.journal.append(&JournalEntry {
                v: JOURNAL_VERSION,
                device_id: *id,
                cert_serials,
                approval_pubkey_fpr: device.approval_pubkey_fpr,
                revoked_at: self.clock.now(),
                actor: actor.clone(),
                via: audit.via,
                log_id: identity.log_id,
                log_epoch: identity.log_epoch,
            })?;
        }
        Ok(RevokeOutcome::Revoked(reread(tx, id).await?))
    }

    /// `Pki::open` 的只读核对：日志里有、库里仍为 `active` 的设备（需要先运行恢复命令）。
    pub async fn pending_revocations(&self) -> Result<Vec<DeviceId>, PkiError> {
        let entries = self
            .journal
            .read_all()
            .map_err(|error| PkiError::Journal(error.to_string()))?;
        let mut snapshot = self.store.snapshot().await.map_err(store_error)?;
        let mut pending = Vec::new();
        for entry in entries {
            if let Some(device) = snapshot
                .device(&entry.device_id)
                .await
                .map_err(store_error)?
            {
                if device.status == DeviceStatus::Active && !pending.contains(&device.id) {
                    pending.push(device.id);
                }
            }
        }
        Ok(pending)
    }

    async fn renew_in(
        &self,
        tx: &mut WriteTx,
        id: &DeviceId,
        presented: &CertSerial,
        req: &RenewRequest,
    ) -> Result<RenewResponse, RenewError> {
        let now = self.clock.now();
        let presented_row: Option<DeviceCertRow> = tx
            .module_conn(MODULE)
            .get(DEVICE_CERTS, presented.as_str())
            .map_err(store_error)?;
        // 出示的证书必须仍然有效（api.md §4.10）：握手时有效不代表请求时有效，
        // 保留到期前建立的长连接不能绕过到期限制。
        let usable = presented_row.as_ref().is_some_and(|row| {
            row.device_id == *id
                && matches!(
                    row.status,
                    CertRowStatus::Active | CertRowStatus::Superseded
                )
                && now < row.not_after
        });
        let device = tx.read().device(id).await.map_err(store_error)?;
        let device = match device {
            Some(device) if usable && device.status == DeviceStatus::Active => device,
            _ => return Err(RenewError::Revoked),
        };

        let key = validate_csr(&req.csr_pem, self.max_csr_bytes).map_err(|error| {
            tracing::info!(%error, "renew CSR rejected");
            RenewError::Validation(vec![FieldError {
                pointer: "/csr",
                code: "invalid",
            }])
        })?;
        let fpr = key.fingerprint();
        let others = active_devices(tx).await?;
        if others
            .iter()
            .any(|other| other.id != *id && other.pubkey_fpr == fpr)
            || device.approval_pubkey_fpr == Some(fpr)
        {
            return Err(RenewError::Validation(vec![FieldError {
                pointer: "/csr",
                code: "duplicate_key",
            }]));
        }

        let serial = loop {
            let serial = CertSerial::generate(self.entropy.as_ref());
            let taken: Option<DeviceCertRow> = tx
                .module_conn(MODULE)
                .get(DEVICE_CERTS, serial.as_str())
                .map_err(store_error)?;
            if taken.is_none() {
                break serial;
            }
        };
        let leaf =
            self.issuer
                .intermediate()
                .issue_device_leaf(id, &key, &serial, now, &self.policy)?;

        // 旧的 active 证书转为 superseded，原 notAfter 之前仍可握手。
        for mut row in device_cert_rows(tx, id)? {
            if row.status == CertRowStatus::Active {
                row.status = CertRowStatus::Superseded;
                tx.module_conn(MODULE)
                    .put(DEVICE_CERTS, row.serial.as_str(), &row)
                    .map_err(store_error)?;
            }
        }
        let key_changed = fpr != device.pubkey_fpr;
        {
            let mut conn = tx.module_conn(MODULE);
            conn.insert(
                DEVICE_CERTS,
                serial.as_str(),
                &DeviceCertRow {
                    serial: serial.clone(),
                    device_id: *id,
                    not_before: leaf.not_before,
                    not_after: leaf.not_after,
                    status: CertRowStatus::Active,
                },
            )
            .map_err(store_error)?;
            if key_changed {
                let state_key = id.to_string();
                let last_seen = conn
                    .get::<DeviceStateRow>(DEVICE_STATE, &state_key)
                    .map_err(store_error)?
                    .and_then(|state| state.last_seen);
                conn.put(
                    DEVICE_STATE,
                    &state_key,
                    &DeviceStateRow {
                        device_id: *id,
                        pubkey_spki: encode_spki(key.spki_der()),
                        last_seen,
                    },
                )
                .map_err(store_error)?;
            }
        }
        // devices 投影只记当前序列号，所以每次续期都发 device.updated（OQ-K12）。
        let changed = DeviceChanged {
            cert_serial: Some(serial.to_string()),
            pubkey_fpr: key_changed.then_some(fpr),
            ..DeviceChanged::default()
        };
        append_updated(
            tx,
            id,
            changed,
            &Actor::new(ActorKind::Device, id.to_string()),
            Audit::default(),
        )
        .await
        .map_err(|error| match error {
            DeviceError::Internal(error) => RenewError::Internal(error),
            other => RenewError::Internal(PkiError::Store(other.to_string())),
        })?;
        tracing::info!(device_id = %id, serial = %serial, key_changed, "device certificate renewed");
        Ok(RenewResponse {
            certificate_chain: self.issuer.chain(&leaf),
            not_after: leaf.not_after,
        })
    }
}

#[async_trait]
impl DeviceAdmin for StoreAdmin {
    async fn rename(
        &self,
        id: &DeviceId,
        name: &str,
        actor: &Actor,
    ) -> Result<Device, DeviceError> {
        if validate_device_name(name, self.device_name_max_chars).is_err() {
            return Err(DeviceError::Validation(vec![FieldError {
                pointer: "/name",
                code: "invalid",
            }]));
        }
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let device = active_device(&mut tx, id).await?;
        if device.name == name {
            return Ok(device);
        }
        let changed = DeviceChanged {
            name: Some(name.to_owned()),
            ..DeviceChanged::default()
        };
        append_updated(&mut tx, id, changed, actor, Audit::default()).await?;
        let device = reread(&mut tx, id).await?;
        tx.commit().await.map_err(store_error)?;
        Ok(device)
    }

    async fn set_role(
        &self,
        tx: &mut WriteTx,
        id: &DeviceId,
        role: DeviceRole,
        actor: &Actor,
    ) -> Result<Device, DeviceError> {
        self.set_role_with(tx, id, role, actor, Audit::default())
            .await
    }

    async fn revoke(
        &self,
        tx: &mut WriteTx,
        id: &DeviceId,
        actor: &Actor,
    ) -> Result<RevokeOutcome, DeviceError> {
        self.revoke_with(tx, id, actor, Audit::default(), true)
            .await
    }

    async fn renew(
        &self,
        id: &DeviceId,
        presented_serial: &CertSerial,
        req: RenewRequest,
    ) -> Result<RenewResponse, RenewError> {
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        match self.renew_in(&mut tx, id, presented_serial, &req).await {
            Ok(response) => {
                tx.commit().await.map_err(store_error)?;
                Ok(response)
            }
            Err(error) => {
                tx.rollback().await.map_err(store_error)?;
                Err(error)
            }
        }
    }

    async fn replay_revocations(&self, backup_at: Timestamp) -> Result<ReplayReport, PkiError> {
        let entries = self
            .journal
            .entries_after(backup_at)
            .map_err(|error| PkiError::Journal(error.to_string()))?;
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let mut report = ReplayReport::default();
        let actor = Actor::new(ActorKind::System, "pki");
        for entry in entries {
            let audit = Audit {
                via: Some("journal_replay".into()),
                ..Audit::default()
            };
            match self
                .revoke_with(&mut tx, &entry.device_id, &actor, audit, false)
                .await
            {
                Ok(RevokeOutcome::Revoked(_)) => report.replayed.push(entry.device_id),
                Ok(RevokeOutcome::AlreadyRevoked(_)) => report.already_revoked += 1,
                Err(DeviceError::NotFound) => report.unknown_devices += 1,
                Err(error) => return Err(PkiError::Store(error.to_string())),
            }
        }
        tx.commit().await.map_err(store_error)?;
        Ok(report)
    }
}

async fn active_device(tx: &mut WriteTx, id: &DeviceId) -> Result<Device, DeviceError> {
    let device = tx
        .read()
        .device(id)
        .await
        .map_err(store_error)?
        .ok_or(DeviceError::NotFound)?;
    if device.status != DeviceStatus::Active {
        return Err(DeviceError::Revoked);
    }
    Ok(device)
}

async fn reread(tx: &mut WriteTx, id: &DeviceId) -> Result<Device, DeviceError> {
    tx.read()
        .device(id)
        .await
        .map_err(store_error)?
        .ok_or(DeviceError::NotFound)
}

async fn append_updated(
    tx: &mut WriteTx,
    id: &DeviceId,
    changed: DeviceChanged,
    actor: &Actor,
    audit: Audit,
) -> Result<(), DeviceError> {
    tx.append(device_event(
        EventType::DeviceUpdated,
        id,
        actor,
        &DeviceUpdatedData {
            device_id: Some(*id),
            changed,
            via: audit.via,
            host_uid: audit.host_uid,
            sudo_user: audit.sudo_user,
        },
    )?)
    .await
    .map_err(store_error)?;
    Ok(())
}

fn device_event(
    ty: EventType,
    id: &DeviceId,
    actor: &Actor,
    data: &impl serde::Serialize,
) -> Result<NewEvent, PkiError> {
    Ok(NewEvent {
        ty,
        subject: Subject::parse(&id.to_string())
            .map_err(|error| PkiError::Store(format!("device subject: {error}")))?,
        project_id: None,
        actor: actor.clone(),
        causation_id: None,
        correlation_id: None,
        data: serde_json::to_value(data)
            .map_err(|error| PkiError::Store(format!("encode device event: {error}")))?,
        dedup_key: None,
    })
}

/// 该设备在 `pki_device_certs` 里的全部证书行（键是序列号，按设备过滤）。
fn device_cert_rows(tx: &mut WriteTx, id: &DeviceId) -> Result<Vec<DeviceCertRow>, PkiError> {
    let conn = tx.module_conn(MODULE);
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page: Vec<(String, DeviceCertRow)> = conn
            .scan(
                DEVICE_CERTS,
                "",
                after.as_deref(),
                gantry_store::MODULE_SCAN_MAX,
            )
            .map_err(store_error)?;
        let done = page.len() < gantry_store::MODULE_SCAN_MAX as usize;
        after = page.last().map(|(key, _)| key.clone());
        out.extend(
            page.into_iter()
                .map(|(_, row)| row)
                .filter(|row| row.device_id == *id),
        );
        if done {
            return Ok(out);
        }
    }
}

pub(crate) async fn active_devices(tx: &mut WriteTx) -> Result<Vec<Device>, PkiError> {
    let mut reader = tx.read();
    let mut out = Vec::new();
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
        out.extend(
            page.items
                .into_iter()
                .filter(|device| device.status == DeviceStatus::Active),
        );
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(out),
        }
    }
}
