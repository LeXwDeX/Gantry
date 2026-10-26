//! `SecretSource` / `SecretSink`：按使用方隔离的密钥读写端口。
//!
//! 签名以 `docs/modules/secrets.md` §6 为准。端口按 `Consumer` 命名空间隔离；
//! 同一 `SecretName` 在不同使用方下是不同的条目（P-81）。
//!
//! 本模块只定义类型与 trait，不读写文件。`SecretValue` 不可 `Clone` /
//! `Serialize`，`Debug` 脱敏，`Drop` 零化内存。

use std::fmt;
use std::path::PathBuf;

use crate::secret::{Consumer, SecretName, SecretString};
use crate::time::Timestamp;

use super::context::ActorCtx;

/// 密钥明文。唯一的取值入口是 [`SecretValue::expose`]。
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    pub fn expose_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.0)
    }
}

impl From<SecretString> for SecretValue {
    fn from(value: SecretString) -> Self {
        Self(value.into_secret().into_bytes())
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

impl PartialEq for SecretValue {
    fn eq(&self, other: &Self) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        // 常量时间比较，避免长度之外的前缀时序差异。
        let mut diff = 0u8;
        for (left, right) in self.0.iter().zip(other.0.iter()) {
            diff |= left ^ right;
        }
        diff == 0
    }
}

impl Eq for SecretValue {}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretVersion(u64);

impl SecretVersion {
    /// 版本从 1 起。
    pub fn new(value: u64) -> Option<Self> {
        (value >= 1).then_some(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretMeta {
    pub consumer: Consumer,
    pub name: SecretName,
    pub version: SecretVersion,
    pub updated_at: Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Secret {
    pub value: SecretValue,
    pub version: SecretVersion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Any,
    Absent,
    Version(SecretVersion),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutOutcome {
    pub meta: SecretMeta,
    pub previous: Option<SecretVersion>,
}

/// 暂存句柄。与 `ports::egress::StagedSecret` 分属不同命名空间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedSecret {
    pub consumer: Consumer,
    pub name: SecretName,
    pub version: SecretVersion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorruptReason {
    AeadFailed,
    HeaderInvalid,
    AadMismatch,
}

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("secret access denied for {who:?}: {name}")]
    Denied { who: Consumer, name: SecretName },
    #[error("secret not found")]
    NotFound,
    #[error("secret version conflict")]
    Conflict {
        expected: Expect,
        actual: Option<SecretVersion>,
    },
    #[error("secret file is corrupt: {path:?}")]
    Corrupt {
        path: PathBuf,
        reason: CorruptReason,
    },
    #[error("master key does not match secrets.meta")]
    KeyMismatch,
    #[error("master key unavailable: {0}")]
    KeyUnavailable(String),
    #[error("insecure permissions on {path:?}: {mode:o}")]
    InsecurePermissions { path: PathBuf, mode: u32 },
    #[error("unsupported secret format: {found}")]
    UnsupportedFormat { found: u8 },
    #[error("secret value exceeds {limit} bytes")]
    TooLarge { limit: usize },
    #[error("secret is not staged")]
    NotStaged,
    #[error("secret is already staged")]
    AlreadyStaged,
    #[error("secret I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// 读：仅限一个使用方的命名空间。
pub trait SecretSource: Send + Sync {
    fn get(&self, name: &SecretName) -> Result<Option<Secret>, SecretsError>;

    /// 轻量版本查询：只看目录，不解密。
    fn version(&self, name: &SecretName) -> Result<Option<SecretVersion>, SecretsError>;

    /// = settings 的 `present`。
    fn present(&self, name: &SecretName) -> Result<bool, SecretsError>;

    /// 轮换窗口内读旧版本（M3）。
    fn get_version(
        &self,
        name: &SecretName,
        version: SecretVersion,
    ) -> Result<Option<Secret>, SecretsError>;
}

/// 读写：仍限于一个使用方的命名空间。
///
/// 写方法接收 [`ActorCtx`]，用于 P-78 的审计上下文；`secrets` 本身不写事件
/// 日志，因此不接收 `TxHandle`。
pub trait SecretSink: SecretSource {
    fn put(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        ctx: &ActorCtx,
    ) -> Result<PutOutcome, SecretsError>;

    fn stage(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        ctx: &ActorCtx,
    ) -> Result<StagedSecret, SecretsError>;

    fn commit(&self, staged: &StagedSecret, ctx: &ActorCtx) -> Result<SecretMeta, SecretsError>;

    fn abort(&self, staged: &StagedSecret, ctx: &ActorCtx) -> Result<(), SecretsError>;

    /// 启动对账用；暂存版本对读者不可见。
    fn staged(&self) -> Result<Vec<StagedSecret>, SecretsError>;

    fn delete(
        &self,
        name: &SecretName,
        expect: Expect,
        ctx: &ActorCtx,
    ) -> Result<Option<SecretMeta>, SecretsError>;

    /// 仅元数据。
    fn list(&self) -> Result<Vec<SecretMeta>, SecretsError>;

    /// 返回缺失的名字。
    fn check_refs(&self, refs: &[SecretName]) -> Result<Vec<SecretName>, SecretsError>;
}
