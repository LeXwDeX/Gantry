//! 主机应急命令 `gantry device recover`（pki.md §7.8，P-39）与守护进程的控制面 `PkiControl`。
//!
//! 需要主机 root：非 root 立即拒绝，不触碰数据库。`revoke` 写 `device.revoked` 并追加吊销日志，
//! `promote` 把一台 active 的桌面设备提升为 approver；两者都带审计字段
//! （`actor = {system, host_recover}`、`via`、`host_uid`、`sudo_user`），重复执行不重复发事件。
//! 在线模式由 `gantryd` 经控制套接字调用 [`PkiControl`]，离线模式由 CLI 直接调用 [`run`]；
//! 两者共用同一份业务实现。

use std::time::Duration;

use async_trait::async_trait;
use gantry_core::entities::{Actor, ActorKind, Device, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use gantry_store::{Page, Store};

use crate::admin::{Audit, DeviceError, RevokeOutcome, StoreAdmin};
use crate::directory::{store_error, DeviceDirectory, StoreDirectory};
use crate::open::Pki;
use crate::pairing::{PairingSessionId, SessionState};
use crate::pairing_service::PairingTicket;
use crate::text::sanitize_for_terminal;
use crate::PkiError;

pub const HOST_RECOVER: &str = "host_recover";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverCmd {
    List,
    Revoke { device_id: DeviceId },
    Promote { device_id: DeviceId },
}

/// 调用方进程的身份（CLI 从 `geteuid()` 与 `SUDO_USER` 取得；只作审计，不作授权依据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEnv {
    pub euid: u32,
    pub sudo_user: Option<String>,
}

/// `list` 的一行；名字已剥离控制字符与 ANSI 转义，可直接打印到终端。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceLine {
    pub id: DeviceId,
    pub name: String,
    pub platform: DevicePlatform,
    pub role: DeviceRole,
    pub status: DeviceStatus,
    pub last_seen: Option<Timestamp>,
    pub pubkey_fpr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverOutcome {
    Listed(Vec<DeviceLine>),
    /// `already = true`：设备此前已吊销，未产生副作用。
    Revoked {
        device: Device,
        already: bool,
    },
    /// `already = true`：设备此前已是 approver，未产生副作用。
    Promoted {
        device: Device,
        already: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error("gantry device recover must run as root")]
    NotRoot,
    #[error("device not found")]
    NotFound,
    #[error("device is revoked")]
    Revoked,
    /// 手机档案设备永远不能成为 approver（P-76）。
    #[error("mobile devices cannot be approvers")]
    MobileCannotApprove,
    #[error("revocation journal: {0}")]
    Journal(String),
    #[error(transparent)]
    Internal(#[from] PkiError),
}

impl From<DeviceError> for RecoverError {
    fn from(error: DeviceError) -> Self {
        match error {
            DeviceError::NotFound => Self::NotFound,
            DeviceError::Revoked => Self::Revoked,
            DeviceError::MobileCannotApprove => Self::MobileCannotApprove,
            DeviceError::Journal(error) => Self::Journal(error.to_string()),
            DeviceError::Internal(error) => Self::Internal(error),
            other => Self::Internal(PkiError::Store(other.to_string())),
        }
    }
}

/// `gantry device recover` 的业务实现。权限检查在打开任何东西之前。
pub async fn run(
    cmd: RecoverCmd,
    env: &HostEnv,
    store: &Store,
    directory: &StoreDirectory,
    admin: &StoreAdmin,
) -> Result<RecoverOutcome, RecoverError> {
    if env.euid != 0 {
        return Err(RecoverError::NotRoot);
    }
    let actor = Actor::new(ActorKind::System, HOST_RECOVER);
    let audit = Audit {
        via: Some(HOST_RECOVER.into()),
        host_uid: Some(env.euid),
        sudo_user: env.sudo_user.clone(),
    };
    match cmd {
        RecoverCmd::List => Ok(RecoverOutcome::Listed(list(directory).await?)),
        RecoverCmd::Revoke { device_id } => {
            let mut tx = store.begin_write().await.map_err(store_error)?;
            let outcome = admin
                .revoke_with(&mut tx, &device_id, &actor, audit, true)
                .await?;
            tx.commit().await.map_err(store_error)?;
            Ok(match outcome {
                RevokeOutcome::Revoked(device) => RecoverOutcome::Revoked {
                    device,
                    already: false,
                },
                RevokeOutcome::AlreadyRevoked(device) => RecoverOutcome::Revoked {
                    device,
                    already: true,
                },
            })
        }
        RecoverCmd::Promote { device_id } => {
            let mut tx = store.begin_write().await.map_err(store_error)?;
            let before = tx
                .read()
                .device(&device_id)
                .await
                .map_err(store_error)?
                .ok_or(RecoverError::NotFound)?;
            let device = admin
                .set_role_with(&mut tx, &device_id, DeviceRole::Approver, &actor, audit)
                .await?;
            tx.commit().await.map_err(store_error)?;
            Ok(RecoverOutcome::Promoted {
                device,
                already: before.role == DeviceRole::Approver,
            })
        }
    }
}

async fn list(directory: &StoreDirectory) -> Result<Vec<DeviceLine>, PkiError> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let page = directory
            .list(&Page {
                limit: 200,
                cursor,
                ..Page::default()
            })
            .await?;
        out.extend(page.items.into_iter().map(|device| DeviceLine {
            id: device.id,
            name: sanitize_for_terminal(&device.name),
            platform: device.platform,
            role: device.role,
            status: device.status,
            last_seen: device.last_seen,
            pubkey_fpr: format!("sha256:{}", device.pubkey_fpr.to_hex()),
        }));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(out),
        }
    }
}

/// `gantry pair` 等待到的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingOutcome {
    Redeemed {
        device: Box<Device>,
        confirmation_code: String,
    },
    /// 作废、过期或被取消。
    Closed(SessionState),
    /// 等待超时，会话仍然 open。
    TimedOut,
}

/// `gantryd` 注册到本机控制套接字的 pki 命令集（§7.8）。传输与 `SO_PEERCRED` 鉴权归 `gantryd`。
#[async_trait]
pub trait PkiControl: Send + Sync {
    async fn pair_open(&self, host_uid: u32) -> Result<PairingTicket, PkiError>;
    async fn pair_wait(
        &self,
        id: PairingSessionId,
        timeout: Duration,
    ) -> Result<PairingOutcome, PkiError>;
    async fn pair_cancel(&self, id: PairingSessionId) -> Result<(), PkiError>;
    async fn device_list(&self) -> Result<Vec<DeviceLine>, PkiError>;
    async fn device_recover(
        &self,
        cmd: RecoverCmd,
        env: HostEnv,
    ) -> Result<RecoverOutcome, RecoverError>;
}

#[async_trait]
impl PkiControl for Pki {
    async fn pair_open(&self, host_uid: u32) -> Result<PairingTicket, PkiError> {
        self.pairing().open_session(host_uid).await
    }

    async fn pair_wait(
        &self,
        id: PairingSessionId,
        timeout: Duration,
    ) -> Result<PairingOutcome, PkiError> {
        let pairing = self.pairing();
        let mut window = pairing.window();
        let wait = async {
            loop {
                let session = pairing.session(id).await?.ok_or(PkiError::NotFound)?;
                match session.state {
                    SessionState::Open => {}
                    SessionState::Redeemed => {
                        let device_id = session
                            .redeemed_device_id
                            .ok_or(PkiError::Config("redeemed session without device"))?;
                        let device = self
                            .directory()
                            .get(&device_id)
                            .await?
                            .ok_or(PkiError::NotFound)?;
                        return Ok(PairingOutcome::Redeemed {
                            device: Box::new(device),
                            confirmation_code: session.confirmation_code.unwrap_or_default(),
                        });
                    }
                    closed => return Ok(PairingOutcome::Closed(closed)),
                }
                // 兑换、作废、过期、取消都会改变窗口；窗口发送端随服务存活，不会关闭。
                if window.changed().await.is_err() {
                    return Err(PkiError::Config("pairing window closed"));
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(outcome) => outcome,
            Err(_) => Ok(PairingOutcome::TimedOut),
        }
    }

    async fn pair_cancel(&self, id: PairingSessionId) -> Result<(), PkiError> {
        self.pairing().cancel_session(id).await
    }

    async fn device_list(&self) -> Result<Vec<DeviceLine>, PkiError> {
        list(&self.directory()).await
    }

    async fn device_recover(
        &self,
        cmd: RecoverCmd,
        env: HostEnv,
    ) -> Result<RecoverOutcome, RecoverError> {
        run(cmd, &env, self.store(), &self.directory(), &self.admin()).await
    }
}
