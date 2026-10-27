//! 设备鉴权（api.md §2，本文 §7.1 步骤 2、4、5 与 §7.3）。
//!
//! 设备目录由 `pki` 提供（`DeviceDirectory`，同泳道接口，不在 `core::ports`）。
//! 在 `pki` 的目录落地之前，本模块只依赖下面这个窄接口 [`DeviceLookup`]，装配层
//! 再用适配器接上 `pki`；这样 `api` 不临时实现 `pki` 的逻辑（AGENTS.md）。

use async_trait::async_trait;
use gantry_core::entities::{Device, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::ids::DeviceId;
use gantry_core::ports::ClientProfile;
use gantry_core::time::Timestamp;

/// 握手通过的客户端叶子证书中，本模块需要的字段。由 TLS 层填入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCert {
    pub serial: String,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
}

/// 设备目录查询失败（存储不可用等）；映射为 `503 unavailable`。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("device directory unavailable")]
pub struct LookupError;

/// `pki::DeviceDirectory` 中本模块用到的读取方法。**实时读库，不缓存**（api.md §2.1）。
#[async_trait]
pub trait DeviceLookup: Send + Sync {
    async fn by_cert_serial(&self, serial: &str) -> Result<Option<Device>, LookupError>;
    async fn get(&self, id: &DeviceId) -> Result<Option<Device>, LookupError>;
    /// 步骤 2 成功后的"最近可见"更新；节流在 `pki`，失败不影响请求。
    async fn touch_last_seen(&self, _id: &DeviceId, _at: Timestamp) {}
}

/// 鉴权链产出的请求上下文（本文 §6.4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    pub device: Device,
    pub profile: ClientProfile,
    pub cert: PeerCert,
}

/// 档案由 `Device.platform` 推出（api.md §2.3）。`DevicePlatform` 是封闭枚举，
/// 未知取值在反序列化时就被拒绝，因此不存在"未知 → 视为 mobile"的分支。
pub fn profile_of(platform: &DevicePlatform) -> ClientProfile {
    match platform {
        DevicePlatform::Windows | DevicePlatform::Macos => ClientProfile::Desktop,
        DevicePlatform::Ios => ClientProfile::Mobile,
    }
}

pub fn profile_str(profile: ClientProfile) -> &'static str {
    match profile {
        ClientProfile::Desktop => "desktop",
        ClientProfile::Mobile => "mobile",
    }
}

/// 步骤 2 的判定：不在册或已吊销 → `None`（`401 device_revoked`）。
pub fn active(device: Option<Device>) -> Option<Device> {
    device.filter(|d| d.status == DeviceStatus::Active)
}

/// 角色层级：`approver ⊃ operator ⊃ viewer`（api.md §2.2）。
pub fn role_satisfies(have: DeviceRole, need: DeviceRole) -> bool {
    have >= need
}
