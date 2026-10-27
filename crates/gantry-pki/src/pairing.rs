//! 配对的纯逻辑（pki.md §7.4.1、§7.4.5、§7.4.3 第 6.1 步，P-23、P-76、D-18）。
//!
//! 这里只有状态机与取值规则，不做 IO：
//!
//! - 令牌：`"gp1_"` + base64url(16 字节)，只存 SHA-256，比较用常量时间；
//! - 会话表：同一时刻至多一个 `open` 会话（新会话作废旧会话）；单次有效；过期；
//!   持有未匹配令牌的请求使当前 `open` 会话的失败次数 +1，达到上限即作废；
//! - 确认码：6 位十进制、均匀分布；
//! - 首台设备角色与安装引导期（D-18）。
//!
//! 会话表整体存为 `pki_pairing_sessions` 的一个文档（[`crate::store_schema`]），
//! 由 [`crate::pairing_service`] 在写事务里读出、修改、写回。

use std::fmt;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::entities::{DevicePlatform, DeviceRole};
use gantry_core::ids::{DeviceId, Entropy};
use gantry_core::secret::SecretString;
use gantry_core::time::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 令牌前缀（api.md 示例形态 `gp1_…`）。
pub const TOKEN_PREFIX: &str = "gp1_";

/// 新令牌：明文只在返回值里出现这一次，库里只存 [`token_hash`]。
pub fn new_token(entropy: &dyn Entropy) -> SecretString {
    let mut bytes = [0u8; 16];
    entropy.fill(&mut bytes);
    let token = format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    bytes.fill(0);
    SecretString::new(token)
}

/// 令牌的 SHA-256。
pub fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// 日志里可出现的令牌标识：哈希前 8 位十六进制（§7.4.1）。
pub fn token_log_id(hash: &[u8; 32]) -> String {
    hash[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 6 位十进制确认码，`0..1_000_000` 均匀取值（拒绝采样），左补零（§7.4.5）。
pub fn confirmation_code(entropy: &dyn Entropy) -> String {
    // 2^32 以内 1_000_000 的最大整数倍；落在其上的样本丢弃以避免取模偏差。
    const LIMIT: u32 = u32::MAX - (u32::MAX % 1_000_000);
    loop {
        let mut buf = [0u8; 4];
        entropy.fill(&mut buf);
        let value = u32::from_le_bytes(buf);
        if value < LIMIT {
            return format!("{:06}", value % 1_000_000);
        }
    }
}

fn is_desktop(platform: &DevicePlatform) -> bool {
    matches!(platform, DevicePlatform::Windows | DevicePlatform::Macos)
}

/// 配对时的初始角色（P-23、P-76、D-18）：
///
/// - 手机档案（`ios`）恒为 `operator`，即使是第一台设备；
/// - 桌面档案：库里**从未有过任何桌面设备**（含已吊销者，OQ-K11 的口径）时为 `approver`，否则 `operator`。
///
/// `desktop_ever_paired` 必须在配对的同一写事务里读取，单写者串行化保证"第一台"唯一。
pub fn initial_role(platform: &DevicePlatform, desktop_ever_paired: bool) -> DeviceRole {
    if is_desktop(platform) && !desktop_ever_paired {
        DeviceRole::Approver
    } else {
        DeviceRole::Operator
    }
}

/// 安装引导期（P-66，D-18）：持续到第一台**桌面**设备配对为止；之后恒为否（吊销也不恢复）。
pub fn bootstrap_period(desktop_ever_paired: bool) -> bool {
    !desktop_ever_paired
}

/// 配对会话标识。会话表内单调递增，随表持久化。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PairingSessionId(u64);

impl PairingSessionId {
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PairingSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pair-{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Open,
    Redeemed,
    Voided,
    Expired,
}

/// `pki_pairing_sessions` 的一行。明文令牌永不出现在这里。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingSession {
    pub id: PairingSessionId,
    pub token_sha256: [u8; 32],
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub failed_attempts: u32,
    pub state: SessionState,
    pub redeemed_device_id: Option<DeviceId>,
    pub confirmation_code: Option<String>,
    pub created_by_uid: u32,
}

impl fmt::Debug for PairingSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingSession")
            .field("id", &self.id)
            .field("token", &token_log_id(&self.token_sha256))
            .field("expires_at", &self.expires_at)
            .field("failed_attempts", &self.failed_attempts)
            .field("state", &self.state)
            .field("redeemed_device_id", &self.redeemed_device_id)
            .finish_non_exhaustive()
    }
}

/// `open_session` 的结果；`token` 只在这里可见。
#[derive(Debug)]
pub struct OpenedSession {
    pub id: PairingSessionId,
    pub token: SecretString,
    pub expires_at: Timestamp,
    /// 被本次作废的旧会话。
    pub voided: Vec<PairingSessionId>,
}

/// 配对窗口：存在未过期的 `open` 会话时为 `Some`，`api` 据此开关配对监听器。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairingWindow {
    pub session_id: PairingSessionId,
    pub expires_at: Timestamp,
}

/// 令牌无效。无效 / 已用 / 过期 / 作废对外**完全相同**（`401 pair_token_invalid`），不给枚举 oracle；
/// 这里只为日志与指标保留内部原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("pairing token invalid")]
pub struct TokenInvalid {
    pub reason: TokenInvalidReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenInvalidReason {
    /// 没有 `open` 会话。
    NoSession,
    /// 与当前 `open` 会话不匹配（含旧会话的令牌、已用令牌）。
    Mismatch,
    /// 匹配但已过期。
    Expired,
    /// 本次失败使会话达到失败上限而作废。
    Voided,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("pairing session not found")]
    NotFound,
    #[error("pairing session is not open")]
    NotOpen,
}

/// 配对会话表。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTable {
    sessions: Vec<PairingSession>,
    next_id: u64,
}

impl SessionTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sessions(&self) -> &[PairingSession] {
        &self.sessions
    }

    pub fn get(&self, id: PairingSessionId) -> Option<&PairingSession> {
        self.sessions.iter().find(|session| session.id == id)
    }

    /// 创建会话并作废此前所有 `open` 会话（同一时刻至多一个有效令牌）。
    pub fn open(
        &mut self,
        now: Timestamp,
        ttl: Duration,
        created_by_uid: u32,
        entropy: &dyn Entropy,
    ) -> OpenedSession {
        let mut voided = Vec::new();
        for session in &mut self.sessions {
            if session.state == SessionState::Open {
                session.state = SessionState::Voided;
                voided.push(session.id);
            }
        }
        self.next_id += 1;
        let id = PairingSessionId(self.next_id);
        let token = new_token(entropy);
        let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        let expires_at = Timestamp::from_millis(now.as_millis().saturating_add(ttl_ms));
        self.sessions.push(PairingSession {
            id,
            token_sha256: token_hash(token.expose_secret()),
            created_at: now,
            expires_at,
            failed_attempts: 0,
            state: SessionState::Open,
            redeemed_device_id: None,
            confirmation_code: None,
            created_by_uid,
        });
        OpenedSession {
            id,
            token,
            expires_at,
            voided,
        }
    }

    /// 把已过期的 `open` 会话置为 `expired`；返回受影响的会话。
    pub fn expire(&mut self, now: Timestamp) -> Vec<PairingSessionId> {
        let mut expired = Vec::new();
        for session in &mut self.sessions {
            if session.state == SessionState::Open && now >= session.expires_at {
                session.state = SessionState::Expired;
                expired.push(session.id);
            }
        }
        expired
    }

    /// 核验令牌（§7.4.3 第 2 步）。成功返回当前 `open` 会话；**不消耗**令牌——
    /// 令牌只在签发成功后由 [`SessionTable::redeem`] 失效，CSR 等校验失败不消耗。
    ///
    /// 不匹配时按 §7.4.1 给当前 `open` 会话计一次失败，达到 `max_failures` 即作废。
    pub fn check(
        &mut self,
        token: &str,
        now: Timestamp,
        max_failures: u32,
    ) -> Result<PairingSessionId, TokenInvalid> {
        self.expire(now);
        let presented = token_hash(token);
        let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.state == SessionState::Open)
        else {
            // 过期 / 已用 / 作废的令牌落到这里时，区分出"曾经匹配过某个会话"只用于日志。
            let reason = if self.sessions.iter().any(|session| {
                session.state == SessionState::Expired
                    && constant_time_eq(&session.token_sha256, &presented)
            }) {
                TokenInvalidReason::Expired
            } else {
                TokenInvalidReason::NoSession
            };
            return Err(TokenInvalid { reason });
        };
        if constant_time_eq(&session.token_sha256, &presented) {
            return Ok(session.id);
        }
        session.failed_attempts = session.failed_attempts.saturating_add(1);
        if session.failed_attempts >= max_failures {
            session.state = SessionState::Voided;
            return Err(TokenInvalid {
                reason: TokenInvalidReason::Voided,
            });
        }
        Err(TokenInvalid {
            reason: TokenInvalidReason::Mismatch,
        })
    }

    /// 签发成功后在同一写事务里标记会话已兑换（令牌随之失效）。
    pub fn redeem(
        &mut self,
        id: PairingSessionId,
        device_id: DeviceId,
        confirmation_code: String,
    ) -> Result<(), SessionError> {
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or(SessionError::NotFound)?;
        if session.state != SessionState::Open {
            return Err(SessionError::NotOpen);
        }
        session.state = SessionState::Redeemed;
        session.redeemed_device_id = Some(device_id);
        session.confirmation_code = Some(confirmation_code);
        Ok(())
    }

    /// 主机取消会话（`gantry pair` 被中断）。已结束的会话取消视为成功。
    pub fn cancel(&mut self, id: PairingSessionId) -> Result<(), SessionError> {
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or(SessionError::NotFound)?;
        if session.state == SessionState::Open {
            session.state = SessionState::Voided;
        }
        Ok(())
    }

    /// 当前配对窗口。
    pub fn window(&self, now: Timestamp) -> Option<PairingWindow> {
        self.sessions
            .iter()
            .find(|session| session.state == SessionState::Open && now < session.expires_at)
            .map(|session| PairingWindow {
                session_id: session.id,
                expires_at: session.expires_at,
            })
    }

    /// 清理已结束且早于 `before` 的会话行（后台任务的"过期令牌清理"）。
    pub fn prune(&mut self, before: Timestamp) {
        self.sessions
            .retain(|session| session.state == SessionState::Open || session.expires_at >= before);
    }
}
