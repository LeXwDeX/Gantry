//! 设备鉴权（api.md §2，本文 §7.1 步骤 2、4、5 与 §7.3）。
//!
//! 设备目录由 `pki` 提供（`gantry_pki::directory::DeviceDirectory`，同泳道接口，
//! 不在 `core::ports`）：每请求实时读库，不缓存。

use gantry_core::entities::{Device, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::ports::ClientProfile;
use gantry_core::time::Timestamp;

/// 握手通过的客户端叶子证书中，本模块需要的字段。由 TLS 层填入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCert {
    pub serial: String,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
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
