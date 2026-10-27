//! 令牌来源（forge.md §7.12）。
//!
//! [`TokenProvider`] 是执行器依赖的接口；[`app::AppTokenProvider`] 是 GitHub App 实现
//! （App JWT → 安装令牌 → 缓存，以及权限自检）。
//! 令牌用 `SecretString` 承载：`Debug` 脱敏、不实现 `Serialize` / `Clone`。

pub(crate) mod app;

use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::ids::ProjectId;
use gantry_core::secret::SecretString;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenPurpose {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("installation unreachable")]
    Unreachable,
    /// App 私钥缺失或无法解析（forge.md §7.13：不崩溃，写路径关闭）。
    #[error("credentials unavailable")]
    CredentialsMissing,
    /// GitHub 拒绝 App JWT 或安装不存在（401 / 403 / 404）。
    #[error("credentials rejected")]
    Rejected,
    #[error("malformed github response")]
    Malformed,
    #[error("project not bound")]
    ProjectUnknown,
}

#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(
        &self,
        project: &ProjectId,
        purpose: TokenPurpose,
    ) -> Result<Arc<SecretString>, AuthError>;

    /// 收到 401 后调用：丢弃缓存，下一次 `token` 重新换取。
    fn invalidate(&self, project: &ProjectId);
}

/// 固定令牌：用于测试。
pub struct StaticTokenProvider {
    token: Arc<SecretString>,
}

impl StaticTokenProvider {
    pub fn new(token: SecretString) -> Self {
        Self {
            token: Arc::new(token),
        }
    }
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn token(
        &self,
        _project: &ProjectId,
        _purpose: TokenPurpose,
    ) -> Result<Arc<SecretString>, AuthError> {
        Ok(self.token.clone())
    }

    fn invalidate(&self, _project: &ProjectId) {}
}
