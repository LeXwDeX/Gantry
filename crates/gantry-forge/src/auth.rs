//! 令牌来源（forge.md §7.12）。
//!
//! 本切片只定义执行器依赖的接口；App JWT 与安装令牌缓存在后续切片实现。
//! 令牌用 `SecretString` 承载：`Debug` 脱敏、不实现 `Serialize` / `Clone`。

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
    #[error("credentials unavailable")]
    CredentialsMissing,
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

/// 固定令牌：用于测试与 App 鉴权切片落地之前的装配。
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
