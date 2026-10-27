//! 本模块在 `store` 里的私有表（pki.md §7.2，store.md §7.10）。
//!
//! 模块表是"有序键值表"（`key → JSON`），与 `device.*` 事件在同一 `WriteTx` 里读写。
//! Device 的事件派生列由 `store` 的内置 `devices` 投影维护，这里只放非事件数据。

use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use gantry_store::ModuleSpec;
use serde::{Deserialize, Serialize};

use crate::keys::CertSerial;
use crate::verifier::DeviceCertStatus;

pub const MODULE: &str = "pki";

/// 设备证书表：键为序列号。续期后被取代的旧证书与吊销状态都在这里（握手时按序列号查）。
pub const DEVICE_CERTS: &str = "pki_device_certs";
/// 设备非事件状态：键为设备 ID（`pubkey_spki`、`last_seen`）。
pub const DEVICE_STATE: &str = "pki_device_state";
/// 配对会话表：整张 [`crate::pairing::SessionTable`] 存为键 [`SESSIONS_KEY`] 的一个文档
/// （同一时刻至多一个 `open` 会话，已结束的会话由后台任务清理，表很小）。
pub const PAIRING_SESSIONS: &str = "pki_pairing_sessions";
pub const SESSIONS_KEY: &str = "sessions";
/// CA 证书（根只存证书，中间 CA 私钥在 `secrets`）。
pub const CA: &str = "pki_ca";
/// 服务端证书（私钥在 `secrets`）。
pub const SERVER_CERT: &str = "pki_server_cert";

/// 交给 `Store::open*` 的注册项。
pub fn module_spec() -> ModuleSpec {
    ModuleSpec::new(
        MODULE,
        &[DEVICE_CERTS, DEVICE_STATE, PAIRING_SESSIONS, CA, SERVER_CERT],
    )
}

/// `pki_device_certs` 的一行。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCertRow {
    pub serial: CertSerial,
    pub device_id: DeviceId,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
    pub status: CertRowStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertRowStatus {
    Active,
    Superseded,
    Revoked,
}

impl From<CertRowStatus> for DeviceCertStatus {
    fn from(status: CertRowStatus) -> Self {
        match status {
            CertRowStatus::Active => Self::Active,
            CertRowStatus::Superseded => Self::Superseded,
            CertRowStatus::Revoked => Self::Revoked,
        }
    }
}

/// `pki_device_state` 的一行。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceStateRow {
    pub device_id: DeviceId,
    /// mTLS 客户端公钥的 SPKI DER，base64url（无填充）；重复公钥检查用。
    pub pubkey_spki: String,
    #[serde(default)]
    pub last_seen: Option<Timestamp>,
}
