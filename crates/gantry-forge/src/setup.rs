//! App Manifest 创建回调（`gantry github setup` 的服务端部分，forge.md §7.14，P-65、P-66）。
//!
//! 主机命令经控制通道调 [`SetupService::begin`] 登记一次性 setup 会话（随机 `state`、有效期），
//! 再轮询 [`SetupService::status`]；GitHub 在用户提交 Manifest 后把浏览器重定向到同一 webhook
//! 入口的 `GET /setup/github/callback?code=…&state=…`，由 [`SetupService::callback`] 处理：
//!
//! 1. 无待完成会话、`state` 不匹配或已过期 → 404，不落盘（不给探测者信号）；
//! 2. 用 `code` 调 `POST /app-manifests/{code}/conversions` 换回 App ID、私钥、webhook secret
//!    （端点与返回结构需核实）；查 `{slug}[bot]` 的用户 ID 拼提交身份（P-69，格式需核实）；
//! 3. 私钥与 webhook secret 经 `secrets` 的 `forge` 使用方写入；`github.app.*` 与
//!    `github.commit_identity` 经装配方注入的 [`GithubSettingsWriter`] 写入（设置落盘与审计
//!    事件归设置模块，P-66）；
//! 4. 会话作废；回应不含任何密钥的完成页。`status` 的应答同样不含密钥（deploy.md §7.5）。
//!
//! 是否处于引导期 / 有 approver 授权由装配方判定后传给 `begin`（P-66），forge 不读设备表。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::{Clock, Entropy, Ulid};
use gantry_core::ports::{ActorCtx, Expect, SecretSink, SecretValue};
use gantry_core::secret::SecretName;
use gantry_core::time::Timestamp;
use serde_json::{json, Value};

use crate::github::GithubClient;

/// 写入 `github.app.*` 与 `github.commit_identity`（config §2.5、P-66）。实现方（设置模块）
/// 负责落盘与审计事件；forge 只给出换回的非密钥字段与密钥的引用名。
pub trait GithubSettingsWriter: Send + Sync {
    fn write_app(&self, settings: &GithubAppSettings) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubAppSettings {
    pub app_id: u64,
    pub slug: String,
    /// `secrets` 里的引用名，不是密钥本身。
    pub private_key_ref: SecretName,
    pub webhook_secret_ref: SecretName,
    pub commit_identity_name: String,
    pub commit_identity_email: String,
}

/// `github.setup.status` 的应答（deploy.md §7.5；不含密钥）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupStatus {
    Pending,
    Completed { app: Value },
    Expired,
    Failed { code: String, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    #[error("not in bootstrap period")]
    NotBootstrapPeriod,
    #[error("another setup session is in progress")]
    SetupInProgress,
}

impl SetupError {
    /// 控制通道错误码（deploy.md §7.5）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotBootstrapPeriod => "not_bootstrap_period",
            Self::SetupInProgress => "setup_in_progress",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupTicket {
    pub state: String,
    pub expires_at: Timestamp,
}

pub struct SetupDeps {
    /// `forge` 使用方的写视图（引导期写入路径）。
    pub secrets: Arc<dyn SecretSink>,
    pub private_key: SecretName,
    pub webhook_secret: SecretName,
    pub settings: Arc<dyn GithubSettingsWriter>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
    /// GitHub REST 根地址；测试里是 `FakeGitHub::base_url()`。
    pub api_base: String,
    pub http_timeout: Duration,
}

struct Session {
    state: String,
    expires_at: Timestamp,
    status: SetupStatus,
}

pub struct SetupService {
    deps: SetupDeps,
    github: GithubClient,
    sessions: Mutex<Vec<Session>>,
}

/// 回调的 HTTP 应答。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackResponse {
    pub status: u16,
    pub body: String,
}

impl SetupService {
    pub fn new(deps: SetupDeps) -> Self {
        let github = GithubClient::new(deps.api_base.clone(), deps.http_timeout);
        Self {
            deps,
            github,
            sessions: Mutex::new(Vec::new()),
        }
    }

    /// `github.setup.begin`。`authorized`：装配方判定的"引导期或有 approver 签名授权"。
    pub fn begin(&self, ttl: Duration, authorized: bool) -> Result<SetupTicket, SetupError> {
        if !authorized {
            return Err(SetupError::NotBootstrapPeriod);
        }
        let now = self.deps.clock.now();
        let mut sessions = lock(&self.sessions);
        if sessions
            .iter()
            .any(|session| session.status == SetupStatus::Pending && now < session.expires_at)
        {
            return Err(SetupError::SetupInProgress);
        }
        let mut raw = [0u8; 32];
        self.deps.entropy.fill(&mut raw);
        let state: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let expires_at = Timestamp::from_millis(
            now.as_millis()
                .saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)),
        );
        sessions.push(Session {
            state: state.clone(),
            expires_at,
            status: SetupStatus::Pending,
        });
        Ok(SetupTicket { state, expires_at })
    }

    /// `github.setup.status`。未知 `state` 按已过期处理。
    pub fn status(&self, state: &str) -> SetupStatus {
        let now = self.deps.clock.now();
        lock(&self.sessions)
            .iter()
            .find(|session| session.state == state)
            .map_or(SetupStatus::Expired, |session| match &session.status {
                SetupStatus::Pending if now >= session.expires_at => SetupStatus::Expired,
                other => other.clone(),
            })
    }

    /// 只在有未完成的 setup 会话时响应回调路径（deploy.md §7.6 端口表）。
    pub fn has_pending(&self) -> bool {
        let now = self.deps.clock.now();
        lock(&self.sessions)
            .iter()
            .any(|session| session.status == SetupStatus::Pending && now < session.expires_at)
    }

    /// `GET /setup/github/callback?code=…&state=…`。`query` 是 `?` 之后的原文。
    pub async fn callback(&self, query: &str) -> CallbackResponse {
        let params = parse_query(query);
        let (Some(code), Some(state)) = (params.get("code"), params.get("state")) else {
            return not_found();
        };
        if code.is_empty()
            || !code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return not_found();
        }
        let now = self.deps.clock.now();
        {
            let mut sessions = lock(&self.sessions);
            let Some(session) = sessions.iter_mut().find(|session| {
                constant_eq(&session.state, state)
                    && session.status == SetupStatus::Pending
                    && now < session.expires_at
            }) else {
                return not_found();
            };
            // 先作废再转换：并发的重复回调只有一个能进入转换。
            session.status = SetupStatus::Failed {
                code: "in_progress".into(),
                message: "conversion in progress".into(),
            };
        }
        let outcome = self.convert(code).await;
        let mut sessions = lock(&self.sessions);
        let Some(session) = sessions.iter_mut().find(|session| session.state == *state) else {
            return not_found();
        };
        let ok = outcome.is_ok();
        session.status = match outcome {
            Ok(app) => SetupStatus::Completed { app },
            Err((code, message)) => SetupStatus::Failed { code, message },
        };
        if ok {
            CallbackResponse {
                status: 200,
                body:
                    "<!doctype html><title>Gantry</title><p>GitHub App 已创建，可以关闭此页。</p>"
                        .into(),
            }
        } else {
            CallbackResponse {
                status: 502,
                body: "<!doctype html><title>Gantry</title><p>GitHub App 创建未完成，请查看主机命令的输出。</p>"
                    .into(),
            }
        }
    }

    async fn convert(&self, code: &str) -> Result<Value, (String, String)> {
        let failed = |code: &str, message: &str| (code.to_owned(), message.to_owned());
        let converted = self
            .github
            .anonymous_json("POST", &format!("/app-manifests/{code}/conversions"), None)
            .await
            .map_err(|_| failed("conversion_failed", "manifest conversion failed"))?;
        let str_of = |key: &str| converted.get(key).and_then(Value::as_str);
        let (Some(app_id), Some(slug), Some(pem), Some(webhook_secret)) = (
            converted.get("id").and_then(Value::as_u64),
            str_of("slug"),
            str_of("pem"),
            str_of("webhook_secret"),
        ) else {
            return Err(failed(
                "malformed_conversion",
                "conversion response is missing fields",
            ));
        };
        let bot_login = format!("{slug}[bot]");
        let bot = self
            .github
            .anonymous_json("GET", &format!("/users/{slug}%5Bbot%5D"), None)
            .await
            .ok()
            .and_then(|user| user.get("id").and_then(Value::as_u64));
        let ctx = setup_ctx(self.deps.clock.now());
        for (name, value) in [
            (&self.deps.private_key, pem),
            (&self.deps.webhook_secret, webhook_secret),
        ] {
            self.deps
                .secrets
                .put(
                    name,
                    SecretValue::from_bytes(value.as_bytes().to_vec()),
                    Expect::Any,
                    &ctx,
                )
                .map_err(|_| failed("secrets_write_failed", "storing app credentials failed"))?;
        }
        // 提交身份（P-69，格式需核实）：查不到 bot 用户 ID 时退到不带 ID 的 noreply 地址。
        let email = match bot {
            Some(id) => format!("{id}+{bot_login}@users.noreply.github.com"),
            None => format!("{bot_login}@users.noreply.github.com"),
        };
        self.deps
            .settings
            .write_app(&GithubAppSettings {
                app_id,
                slug: slug.to_owned(),
                private_key_ref: self.deps.private_key.clone(),
                webhook_secret_ref: self.deps.webhook_secret.clone(),
                commit_identity_name: bot_login,
                commit_identity_email: email,
            })
            .map_err(|_| failed("settings_write_failed", "writing github settings failed"))?;
        // 只回非密钥字段（应答不含 pem / webhook_secret / client_secret）。
        Ok(json!({
            "app_id": app_id,
            "slug": slug,
            "html_url": str_of("html_url").unwrap_or_default(),
            "permissions": converted.get("permissions").cloned().unwrap_or(json!({})),
            "events": converted.get("events").cloned().unwrap_or(json!([])),
        }))
    }
}

fn setup_ctx(now: Timestamp) -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "forge.setup"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(u64::try_from(now.as_millis()).unwrap_or(0), 0)
            .unwrap_or_else(|_| Ulid::from_parts(0, 0).expect("zero ulid")),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn not_found() -> CallbackResponse {
    CallbackResponse {
        status: 404,
        body: String::new(),
    }
}

/// 长度相同时逐字节常量时间比较 `state`。
fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
