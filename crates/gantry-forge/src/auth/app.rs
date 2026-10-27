//! GitHub App 鉴权：App JWT → 安装令牌 → 缓存，以及权限自检（forge.md §7.12、FA §9.4–§9.5）。
//!
//! - JWT：RS256，`iss = app_id`，`iat` 回拨 `iat_backdate` 容忍时钟偏差，`exp = now + jwt_ttl`
//!   （默认 9 分钟，须小于官方上限，需核实）。私钥每次签名时经 `secrets` 读取，轮换后自动生效。
//! - 安装令牌：`POST /app/installations/{id}/access_tokens`，用 `repositories` 收窄到 Project
//!   的那一个仓库、用 `permissions` 按用途收窄（是否按预期工作需核实，FA OQ-13）；按响应的
//!   `expires_at` 提前 `refresh_margin` 刷新；同一 (Project, 用途) 的刷新 single-flight。
//! - 令牌只在进程内存里，以 `SecretString` 承载（`Debug` 脱敏）；本类型的 `Debug` 不列出缓存。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::ids::{Clock, ProjectId};
use gantry_core::ports::SecretSource;
use gantry_core::secret::{SecretName, SecretString};
use gantry_core::time::Timestamp;
use serde_json::{json, Value};

use super::{AuthError, TokenProvider, TokenPurpose};
use crate::config::{ForgeConfig, Permission};
use crate::github::{ApiError, GithubClient};

/// `forge.auth.*`（forge.md §10）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppAuthOptions {
    pub jwt_ttl: Duration,
    pub refresh_margin: Duration,
    /// `iat` 回拨量（需核实官方建议）。
    pub iat_backdate: Duration,
    pub http_timeout: Duration,
}

impl Default for AppAuthOptions {
    fn default() -> Self {
        Self {
            jwt_ttl: Duration::from_secs(9 * 60),
            refresh_margin: Duration::from_secs(5 * 60),
            iat_backdate: Duration::from_secs(60),
            http_timeout: Duration::from_secs(30),
        }
    }
}

pub struct AppAuthDeps {
    pub app_id: u64,
    /// `forge` 使用方视图（secrets.md）；私钥按 `private_key` 读取。
    pub secrets: Arc<dyn SecretSource>,
    pub private_key: SecretName,
    pub clock: Arc<dyn Clock>,
    /// GitHub REST 根地址；测试里是 `FakeGitHub::base_url()`。
    pub api_base: String,
    pub options: AppAuthOptions,
}

struct Cached {
    token: Arc<SecretString>,
    refresh_at: Timestamp,
}

type Key = (ProjectId, TokenPurpose);

/// [`TokenProvider`] 的 GitHub App 实现。
pub struct AppTokenProvider {
    config: Arc<ForgeConfig>,
    app_id: u64,
    secrets: Arc<dyn SecretSource>,
    private_key: SecretName,
    clock: Arc<dyn Clock>,
    github: GithubClient,
    opts: AppAuthOptions,
    cache: Mutex<HashMap<Key, Cached>>,
    flights: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for AppTokenProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppTokenProvider")
            .field("app_id", &self.app_id)
            .field("private_key", &self.private_key)
            .finish_non_exhaustive()
    }
}

/// 权限自检的结论（FA §9.5）。由装配方据此更新 [`crate::ProjectBinding`] 并上报。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionReport {
    /// 安装实际获批、且写路径会用到的权限。
    pub granted: BTreeSet<Permission>,
    /// 出现的 FA §9.2 高危权限（GitHub 的权限键名，需核实）。
    pub high_risk: Vec<String>,
    /// 出现高危权限且 `deny_extra_high_risk = true`：该 Project 的写路径整体拒绝。
    pub write_denied: bool,
}

impl PermissionReport {
    /// 写路径实际可用的权限：写路径被拒时为空集（入队即 `PermissionMissing`）。
    pub fn effective(&self) -> BTreeSet<Permission> {
        if self.write_denied {
            BTreeSet::new()
        } else {
            self.granted.clone()
        }
    }

    /// 缺少的写权限（上报"需要用户在 GitHub 上接受新权限"）。
    pub fn missing(&self) -> Vec<Permission> {
        WRITE_PERMISSIONS
            .iter()
            .copied()
            .filter(|permission| !self.granted.contains(permission))
            .collect()
    }
}

/// M1 写路径会用到的权限；`actions:write` 只在启用 `rerun_failed_jobs`（M2）时申请。
const WRITE_PERMISSIONS: [Permission; 3] = [
    Permission::ContentsWrite,
    Permission::PullRequestsWrite,
    Permission::IssuesWrite,
];

/// FA §9.2「不申请」的权限，出现即视为高危。键名按 REST 文档的 App 权限命名（需核实）。
const HIGH_RISK: [&str; 8] = [
    "workflows",
    "administration",
    "secrets",
    "actions_variables",
    "environments",
    "pages",
    "deployments",
    "packages",
];

impl AppTokenProvider {
    pub fn new(config: Arc<ForgeConfig>, deps: AppAuthDeps) -> Self {
        Self {
            config,
            app_id: deps.app_id,
            secrets: deps.secrets,
            private_key: deps.private_key,
            clock: deps.clock,
            github: GithubClient::new(deps.api_base, deps.options.http_timeout),
            opts: deps.options,
            cache: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// `GET /app/installations/{id}`，对比 FA §9.1 / §9.2（forge.md §7.12）。
    pub async fn self_check(
        &self,
        project: &ProjectId,
        deny_extra_high_risk: bool,
    ) -> Result<PermissionReport, AuthError> {
        let installation = self.installation(project)?;
        let jwt = self.app_jwt()?;
        let value = self
            .github
            .get_installation(&jwt, installation)
            .await
            .map_err(auth_error)?;
        let permissions = value
            .get("permissions")
            .and_then(Value::as_object)
            .ok_or(AuthError::Malformed)?;
        let level = |name: &str| permissions.get(name).and_then(Value::as_str);
        let granted = [
            ("contents", Permission::ContentsWrite),
            ("pull_requests", Permission::PullRequestsWrite),
            ("issues", Permission::IssuesWrite),
            ("actions", Permission::ActionsWrite),
        ]
        .into_iter()
        .filter(|(name, _)| level(name) == Some("write"))
        .map(|(_, permission)| permission)
        .collect();
        let high_risk: Vec<String> = HIGH_RISK
            .iter()
            .filter(|name| level(name).is_some())
            .map(|name| (*name).to_owned())
            .collect();
        Ok(PermissionReport {
            granted,
            write_denied: deny_extra_high_risk && !high_risk.is_empty(),
            high_risk,
        })
    }

    fn installation(&self, project: &ProjectId) -> Result<i64, AuthError> {
        self.config
            .projects
            .get(project)
            .map(|binding| binding.project.installation_id)
            .ok_or(AuthError::ProjectUnknown)
    }

    fn app_jwt(&self) -> Result<SecretString, AuthError> {
        let secret = self
            .secrets
            .get(&self.private_key)
            .map_err(|_| AuthError::CredentialsMissing)?
            .ok_or(AuthError::CredentialsMissing)?;
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(secret.value.expose())
            .map_err(|_| AuthError::CredentialsMissing)?;
        let now = self.clock.now().as_millis().div_euclid(1000);
        let claims = json!({
            "iat": now - secs(self.opts.iat_backdate),
            "exp": now + secs(self.opts.jwt_ttl),
            "iss": self.app_id.to_string(),
        });
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &key,
        )
        .map(SecretString::new)
        .map_err(|_| AuthError::CredentialsMissing)
    }

    fn cached(&self, key: &Key) -> Option<Arc<SecretString>> {
        let now = self.clock.now();
        lock(&self.cache)
            .get(key)
            .filter(|cached| now < cached.refresh_at)
            .map(|cached| cached.token.clone())
    }

    async fn mint(&self, project: &ProjectId, purpose: TokenPurpose) -> Result<Cached, AuthError> {
        let binding = self
            .config
            .projects
            .get(project)
            .ok_or(AuthError::ProjectUnknown)?;
        let repo = binding.project.repo.as_str();
        let name = repo.split_once('/').map_or(repo, |(_, name)| name);
        let body = json!({
            "repositories": [name],
            "permissions": scope(purpose),
        });
        let jwt = self.app_jwt()?;
        let value = self
            .github
            .create_installation_token(&jwt, binding.project.installation_id, &body)
            .await
            .map_err(auth_error)?;
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or(AuthError::Malformed)?;
        let expires_at = value
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|raw| Timestamp::parse_rfc3339(raw).ok())
            .ok_or(AuthError::Malformed)?;
        let margin = i64::try_from(self.opts.refresh_margin.as_millis()).unwrap_or(i64::MAX);
        Ok(Cached {
            token: Arc::new(SecretString::new(token.to_owned())),
            refresh_at: Timestamp::from_millis(expires_at.as_millis().saturating_sub(margin)),
        })
    }
}

#[async_trait]
impl TokenProvider for AppTokenProvider {
    async fn token(
        &self,
        project: &ProjectId,
        purpose: TokenPurpose,
    ) -> Result<Arc<SecretString>, AuthError> {
        let key = (*project, purpose);
        if let Some(token) = self.cached(&key) {
            return Ok(token);
        }
        let flight = lock(&self.flights).entry(key).or_default().clone();
        let _flight = flight.lock().await;
        // 等锁期间别的调用者可能已经刷新过。
        if let Some(token) = self.cached(&key) {
            return Ok(token);
        }
        let fresh = self.mint(project, purpose).await?;
        let token = fresh.token.clone();
        lock(&self.cache).insert(key, fresh);
        Ok(token)
    }

    fn invalidate(&self, project: &ProjectId) {
        lock(&self.cache).retain(|(cached, _), _| cached != project);
    }
}

/// 按用途收窄的权限集（FA §9.4）：都不含 `workflows` / `administration`。
fn scope(purpose: TokenPurpose) -> Value {
    let level = match purpose {
        TokenPurpose::Read => "read",
        TokenPurpose::Write => "write",
    };
    let mut permissions = BTreeMap::new();
    permissions.insert("metadata", "read");
    permissions.insert("checks", "read");
    for name in ["contents", "pull_requests", "issues"] {
        permissions.insert(name, level);
    }
    json!(permissions)
}

fn auth_error(error: ApiError) -> AuthError {
    match error {
        ApiError::Status {
            status: 401 | 403 | 404,
            ..
        } => AuthError::Rejected,
        ApiError::Decode(_) => AuthError::Malformed,
        ApiError::Transport(_) | ApiError::Status { .. } => AuthError::Unreachable,
    }
}

fn secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
