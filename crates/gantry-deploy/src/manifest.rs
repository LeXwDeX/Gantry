//! GitHub App Manifest（deploy.md §7.5 第 2 步；forge-actions §9.6）。
//!
//! 权限与事件集合只在这里定义一次；`gantry github setup` 用它生成 Manifest，
//! 也用它核对回调后 `forge` 报回的获批权限（§7.5 第 5 步）。

use serde_json::{json, Map, Value};

/// 回调路径，与 webhook 同一入口（P-65）。
pub const CALLBACK_PATH: &str = "/setup/github/callback";
pub const WEBHOOK_PATH: &str = "/webhook";
/// GitHub App 名称的长度上限（需核实，deploy.md §7.5）。
pub const MAX_APP_NAME_CHARS: usize = 34;

/// 任何阶段都不得出现的权限（P-48；forge-actions §9.2）。
pub const FORBIDDEN_PERMISSIONS: &[&str] = &["workflows", "administration"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// M1 子集：不含 `actions`、`merge_queues` 与 `merge_group`（§7.5 第 2 步）。
    M1,
    /// M2 经"接受新权限"升级后的完整集合（forge-actions §9.6）。
    M2,
}

impl Stage {
    pub fn permissions(self) -> Vec<(&'static str, &'static str)> {
        let mut permissions = vec![
            ("metadata", "read"),
            ("contents", "write"),
            ("pull_requests", "write"),
            ("issues", "write"),
            ("checks", "read"),
        ];
        if self == Stage::M2 {
            permissions.push(("actions", "write"));
            // 键名需核实（forge-actions OQ-1）。
            permissions.push(("merge_queues", "read"));
        }
        permissions
    }

    pub fn events(self) -> Vec<&'static str> {
        let mut events = vec![
            "pull_request",
            "pull_request_review",
            "pull_request_review_comment",
            "pull_request_review_thread",
            "issue_comment",
            "issues",
            "check_suite",
            "check_run",
            "push",
            "repository",
        ];
        if self == Stage::M2 {
            events.insert(8, "merge_group");
        }
        events
    }
}

/// App 拥有者：`user` 或 `org:<名>`（`--app-owner`，deploy.md §6.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppOwner {
    User,
    Org(String),
}

impl AppOwner {
    pub fn parse(raw: &str) -> Result<Self, ManifestError> {
        if raw == "user" {
            return Ok(Self::User);
        }
        match raw.strip_prefix("org:") {
            Some(org) if is_github_login(org) => Ok(Self::Org(org.to_owned())),
            _ => Err(ManifestError::Owner(raw.to_owned())),
        }
    }

    /// Manifest 提交页的表单地址（需核实，OPS-02）。`state` 由 `forge` 生成，只含 URL 安全字符。
    pub fn form_action(&self, github_web: &str, state: &str) -> String {
        let base = github_web.trim_end_matches('/');
        match self {
            Self::User => format!("{base}/settings/apps/new?state={state}"),
            Self::Org(org) => format!("{base}/organizations/{org}/settings/apps/new?state={state}"),
        }
    }
}

/// GitHub 登录名：字母数字与单个连字符，不以连字符开头或结尾，≤ 39 字符。
fn is_github_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("--webhook-url 必须是 https://<主机>/webhook，不带查询串、片段和用户信息：{0}")]
    WebhookUrl(String),
    #[error("--app-owner 必须是 user 或 org:<组织名>：{0}")]
    Owner(String),
    #[error("--name 必须是 1–{MAX_APP_NAME_CHARS} 个字母、数字、空格、- 或 _：{0}")]
    Name(String),
}

/// 已校验的公网入口：`https://<authority>/webhook`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    authority: String,
}

impl Entry {
    pub fn parse(webhook_url: &str) -> Result<Self, ManifestError> {
        let bad = || ManifestError::WebhookUrl(webhook_url.to_owned());
        let rest = webhook_url.strip_prefix("https://").ok_or_else(bad)?;
        let authority = rest.strip_suffix(WEBHOOK_PATH).ok_or_else(bad)?;
        let host = match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            Some(_) => return Err(bad()),
            None => authority,
        };
        let host_ok = !host.is_empty()
            && host.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if !host_ok {
            return Err(bad());
        }
        Ok(Self {
            authority: authority.to_ascii_lowercase(),
        })
    }

    pub fn host(&self) -> &str {
        self.authority
            .rsplit_once(':')
            .map_or(&self.authority, |(host, _)| host)
    }

    pub fn origin(&self) -> String {
        format!("https://{}", self.authority)
    }

    pub fn webhook_url(&self) -> String {
        format!("{}{WEBHOOK_PATH}", self.origin())
    }

    pub fn redirect_url(&self) -> String {
        format!("{}{CALLBACK_PATH}", self.origin())
    }
}

/// App 名称：用户给的原样校验；缺省为 `gantry-<主机名首段>`，截到上限。
pub fn app_name(explicit: Option<&str>, entry: &Entry) -> Result<String, ManifestError> {
    if let Some(name) = explicit {
        let ok = !name.trim().is_empty()
            && name.chars().count() <= MAX_APP_NAME_CHARS
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'));
        return if ok {
            Ok(name.to_owned())
        } else {
            Err(ManifestError::Name(name.to_owned()))
        };
    }
    let label = entry.host().split('.').next().unwrap_or("host");
    let mut name = format!("gantry-{label}");
    name.truncate(MAX_APP_NAME_CHARS);
    Ok(name.trim_end_matches('-').to_owned())
}

/// 生成 Manifest JSON（§7.5 第 2 步）。私有 App，不请求 OAuth。
pub fn build(stage: Stage, entry: &Entry, name: &str) -> Value {
    let permissions: Map<String, Value> = stage
        .permissions()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), Value::from(v)))
        .collect();
    json!({
        "name": name,
        "url": entry.origin(),
        "public": false,
        "hook_attributes": { "url": entry.webhook_url(), "active": true },
        "redirect_url": entry.redirect_url(),
        "request_oauth_on_install": false,
        "default_permissions": permissions,
        "default_events": stage.events(),
    })
}

/// 获批权限 / 事件与 Manifest 的差异（§7.5 第 5 步要求"完全一致"）。空表示一致。
pub fn diff_granted(
    stage: Stage,
    permissions: &Map<String, Value>,
    events: &[String],
) -> Vec<String> {
    let mut problems = Vec::new();
    let expected = stage.permissions();
    for (key, level) in &expected {
        match permissions.get(*key).and_then(Value::as_str) {
            Some(got) if got == *level => {}
            Some(got) => problems.push(format!("权限 {key} 为 {got}，应为 {level}")),
            None => problems.push(format!("缺少权限 {key}:{level}")),
        }
    }
    for key in permissions.keys() {
        if FORBIDDEN_PERMISSIONS.contains(&key.as_str()) {
            problems.push(format!("出现禁止的权限 {key}（P-48）"));
        } else if !expected.iter().any(|(k, _)| k == key) {
            problems.push(format!("多出权限 {key}"));
        }
    }
    let expected_events = stage.events();
    for event in &expected_events {
        if !events.iter().any(|e| e == event) {
            problems.push(format!("缺少事件 {event}"));
        }
    }
    for event in events {
        if !expected_events.contains(&event.as_str()) {
            problems.push(format!("多出事件 {event}"));
        }
    }
    problems
}
