//! `gantry github setup`（deploy.md §7.5；forge.md §7.14；P-65、P-66）。
//!
//! 分工：本命令检查入口、生成 Manifest 与提交页、轮询结果并核对获批权限；setup 会话、
//! 回调、转换端点、密钥落盘与 `github.*` 写入都在 `gantryd` 里的 `forge`，经控制通道调用：
//!
//! - `github.setup.begin {webhook_url, redirect_url, ttl_secs}` → `{state, expires_at}`；
//!   错误码 `not_bootstrap_period`（已配对设备且无 approver 授权，P-66）、`setup_in_progress`。
//! - `github.setup.status {state}` → `{status: pending|completed|expired|failed, app?, error?}`；
//!   `app = {app_id, slug, html_url, permissions, events}`，`error = {code, message}`，
//!   `failed` 时 `app` 可能只带 `html_url`（"转换成功但落盘失败"，§9）。
//!
//! 应答一律不含 `pem`、`webhook_secret`、`client_secret`；万一出现，本命令拒绝打印并报协议错误。

use std::io::Write;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::control::{ControlClient, ControlError};
use crate::doctor::{self, Status};
use crate::manifest::{self, AppOwner, Entry, ManifestError, Stage};
use crate::page;
use crate::probe::HttpProbe;

pub const DEFAULT_GITHUB_WEB: &str = "https://github.com";
/// setup 会话有效期 [建议值]（deploy.md §7.5 第 2 步）。
pub const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// 应答里绝不能出现的字段名（I6；§7.5 第 4 步）。
const SECRET_FIELDS: &[&str] = &["pem", "private_key", "webhook_secret", "client_secret"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupArgs {
    pub webhook_url: String,
    pub app_owner: String,
    pub name: Option<String>,
    pub test_mode: bool,
    /// 仅 `--test-mode`：把 GitHub 网页地址换成假服务（`FakeGitHubManifest`，§7.15）。
    pub github_web: Option<String>,
    /// 提交页写到哪里；`None` 时打印到标准输出。
    pub page_path: Option<std::path::PathBuf>,
}

impl SetupArgs {
    /// 解析 `gantry github setup` 之后的参数（install.sh 第 14 步按此调用）。
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, SetupError> {
        let mut webhook_url = None;
        let mut app_owner = None;
        let mut out = Self {
            webhook_url: String::new(),
            app_owner: String::new(),
            name: None,
            test_mode: false,
            github_web: None,
            page_path: None,
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| SetupError::Usage(format!("{flag} 需要一个值")))
            };
            match flag.as_str() {
                "--webhook-url" => webhook_url = Some(value()?),
                "--app-owner" => app_owner = Some(value()?),
                "--name" => out.name = Some(value()?),
                "--github-web" => out.github_web = Some(value()?),
                "--page" => out.page_path = Some(value()?.into()),
                "--test-mode" => out.test_mode = true,
                other => return Err(SetupError::Usage(format!("未知参数 {other}"))),
            }
        }
        out.webhook_url =
            webhook_url.ok_or_else(|| SetupError::Usage("缺少 --webhook-url".into()))?;
        out.app_owner = app_owner.unwrap_or_else(|| "user".into());
        if out.github_web.is_some() && !out.test_mode {
            return Err(SetupError::Usage(
                "--github-web 只能和 --test-mode 一起用".into(),
            ));
        }
        Ok(out)
    }
}

pub const USAGE: &str = "usage: gantry github setup --webhook-url https://<入口主机>/webhook \
[--app-owner user|org:<名>] [--name <App 名>] [--page <文件>] [--test-mode [--github-web <url>]]";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SetupError {
    #[error("{0}\n{USAGE}")]
    Usage(String),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("webhook 入口未就绪，先完成 install.sh 第 13 步（deploy.md §7.6）：\n{0}")]
    EntryNotReady(String),
    #[error("已过安装引导期（已有设备配对），需要 approver 签名授权（P-66）：{0}")]
    NotBootstrapPeriod(String),
    #[error("已有未完成的 setup 会话；等它过期或完成后重试：{0}")]
    InProgress(String),
    #[error(transparent)]
    Control(ControlError),
    #[error("控制通道应答不合规：{0}")]
    Protocol(String),
    #[error("setup 会话已过期，重新运行 gantry github setup")]
    Expired,
    #[error("App 创建失败（{code}）：{message}")]
    Failed { code: String, message: String },
    #[error("获批权限与 Manifest 不一致：{0}")]
    PermissionMismatch(String),
    #[error("写提交页失败：{0}")]
    Io(String),
}

impl SetupError {
    /// `2` 用法错误；其余 `1`（install.sh 统一映射为退出码 40）。
    pub fn exit_code(&self) -> u8 {
        match self {
            SetupError::Usage(_) | SetupError::Manifest(_) => 2,
            _ => 1,
        }
    }
}

impl From<ControlError> for SetupError {
    fn from(error: ControlError) -> Self {
        match error {
            ControlError::Rejected { code, message } if code == "not_bootstrap_period" => {
                SetupError::NotBootstrapPeriod(message)
            }
            ControlError::Rejected { code, message } if code == "setup_in_progress" => {
                SetupError::InProgress(message)
            }
            other => SetupError::Control(other),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedApp {
    pub app_id: u64,
    pub slug: String,
    pub html_url: String,
    pub install_url: String,
}

pub struct Env<'a> {
    pub control: &'a dyn ControlClient,
    pub probe: &'a dyn HttpProbe,
    /// 给管理员看的进度与结果（终端）。
    pub out: &'a mut dyn Write,
    /// 轮询间隔的等待；测试里不真睡。
    pub sleep: &'a mut dyn FnMut(Duration),
}

pub fn run(args: &SetupArgs, env: Env<'_>) -> Result<CreatedApp, SetupError> {
    let Env {
        control,
        probe,
        out,
        sleep,
    } = env;
    let entry = Entry::parse(&args.webhook_url)?;
    let owner = AppOwner::parse(&args.app_owner)?;
    let name = manifest::app_name(args.name.as_deref(), &entry)?;
    let github_web = args.github_web.as_deref().unwrap_or(DEFAULT_GITHUB_WEB);
    let stage = Stage::M1;

    // 1. 入口先于 App（P-65）：外部可达检查不过就不登记会话。
    let failing: Vec<String> = doctor::webhook_external(probe, &entry, false)
        .into_iter()
        .filter(|c| c.status != Status::Ok)
        .map(|c| format!("  {}：{}", c.name, c.detail))
        .collect();
    if !failing.is_empty() {
        return Err(SetupError::EntryNotReady(failing.join("\n")));
    }

    // 2. forge 登记一次性会话；引导期判断在服务端（P-66）。
    let begun = control.call(
        "github.setup.begin",
        json!({
            "webhook_url": entry.webhook_url(),
            "redirect_url": entry.redirect_url(),
            "ttl_secs": SESSION_TTL.as_secs(),
        }),
    )?;
    reject_secrets(&begun)?;
    let state = begun
        .get("state")
        .and_then(Value::as_str)
        .filter(|s| is_state_token(s))
        .ok_or_else(|| SetupError::Protocol("state 缺失或格式不对".into()))?
        .to_owned();

    // 3. 提交页。
    let manifest = manifest::build(stage, &entry, &name);
    let html = page::render(&owner.form_action(github_web, &state), &manifest);
    let io = |e: std::io::Error| SetupError::Io(e.to_string());
    match &args.page_path {
        Some(path) => {
            std::fs::write(path, &html).map_err(io)?;
            writeln!(
                out,
                "提交页已写到 {}。把它拷到任意一台有浏览器的电脑上打开，在 GitHub 上确认创建 App“{name}”。\n页面不含秘密；会话 {} 分钟内有效。",
                path.display(),
                SESSION_TTL.as_secs() / 60
            )
            .map_err(io)?;
        }
        None => {
            writeln!(
                out,
                "把下面的 HTML 存成文件，在有浏览器的电脑上打开并提交（会话 {} 分钟内有效）：\n",
                SESSION_TTL.as_secs() / 60
            )
            .map_err(io)?;
            out.write_all(html.as_bytes()).map_err(io)?;
        }
    }

    // 4–5. 轮询结果，核对获批权限。
    let max_polls = SESSION_TTL.as_secs() / POLL_INTERVAL.as_secs() + 10;
    for _ in 0..max_polls {
        let status = control.call("github.setup.status", json!({ "state": state }))?;
        reject_secrets(&status)?;
        match status.get("status").and_then(Value::as_str) {
            Some("pending") => sleep(POLL_INTERVAL),
            Some("completed") => {
                let app = parse_app(status.get("app"), stage, github_web, args.test_mode)?;
                writeln!(
                    out,
                    "已创建 App：ID {}，slug {}（{}）。\n获批权限与 Manifest 一致：{}\n下一步：打开 {}，只选择需要的仓库安装（不要选全部仓库）。",
                    app.app_id,
                    app.slug,
                    app.html_url,
                    stage
                        .permissions()
                        .iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    app.install_url
                )
                .map_err(io)?;
                return Ok(app);
            }
            Some("expired") => return Err(SetupError::Expired),
            Some("failed") => return Err(failed(&status, args.test_mode, out)),
            _ => return Err(SetupError::Protocol("status 取值未知".into())),
        }
    }
    Err(SetupError::Expired)
}

/// 失败时给出重做步骤；"转换成功但落盘失败"时凭据已丢，只能删 App 或重置密钥后重来（§7.5、§9）。
fn failed(status: &Value, test_mode: bool, out: &mut dyn Write) -> SetupError {
    let field = |name: &str| {
        status
            .get("error")
            .and_then(|e| e.get(name))
            .and_then(Value::as_str)
            .map(sanitize_line)
            .unwrap_or_default()
    };
    let code = field("code");
    let message = field("message");
    if code == "persist_failed" {
        let html_url = status
            .get("app")
            .and_then(|a| a.get("html_url"))
            .and_then(Value::as_str)
            .filter(|u| is_web_url(u, test_mode))
            .unwrap_or(
                "（GitHub 未返回地址，到 Settings → Developer settings → GitHub Apps 查找）",
            );
        let _ = writeln!(
            out,
            "App 已在 GitHub 上创建，但密钥没能写入服务端，凭据已丢失（code 只能用一次）。\n重做步骤：\n  1. 打开 {html_url}，删除这个 App（或重新生成私钥并重置 webhook secret）；\n  2. 排除落盘失败的原因（磁盘、权限，见 journalctl -u gantryd）；\n  3. 重新运行 gantry github setup。"
        );
    }
    SetupError::Failed { code, message }
}

fn parse_app(
    app: Option<&Value>,
    stage: Stage,
    github_web: &str,
    test_mode: bool,
) -> Result<CreatedApp, SetupError> {
    let bad = |what: &str| SetupError::Protocol(format!("app.{what} 缺失或格式不对"));
    let app = app.ok_or_else(|| bad("*"))?;
    let app_id = app
        .get("app_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("app_id"))?;
    let slug = app
        .get("slug")
        .and_then(Value::as_str)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .ok_or_else(|| bad("slug"))?
        .to_owned();
    let html_url = app
        .get("html_url")
        .and_then(Value::as_str)
        .filter(|u| is_web_url(u, test_mode))
        .ok_or_else(|| bad("html_url"))?
        .to_owned();
    let empty = Map::new();
    let permissions = app
        .get("permissions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let events: Vec<String> = app
        .get("events")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let problems = manifest::diff_granted(stage, permissions, &events);
    if !problems.is_empty() {
        return Err(SetupError::PermissionMismatch(problems.join("；")));
    }
    let install_url = format!(
        "{}/apps/{slug}/installations/new",
        github_web.trim_end_matches('/')
    );
    Ok(CreatedApp {
        app_id,
        slug,
        html_url,
        install_url,
    })
}

/// 应答里任何层级出现密钥字段名都拒绝，不打印。
fn reject_secrets(value: &Value) -> Result<(), SetupError> {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Object(map) => map
                .iter()
                .any(|(k, v)| SECRET_FIELDS.contains(&k.as_str()) || walk(v)),
            Value::Array(items) => items.iter().any(walk),
            _ => false,
        }
    }
    if walk(value) {
        Err(SetupError::Protocol("应答含密钥字段，已拒绝显示".into()))
    } else {
        Ok(())
    }
}

/// `state` ≥ 128 位，URL 安全（base64url / hex）：至少 22 个字符。
fn is_state_token(s: &str) -> bool {
    (22..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 只接受 https；`--test-mode` 下的假服务（§7.15）可以是 http。
fn is_web_url(s: &str, test_mode: bool) -> bool {
    (s.starts_with("https://") || (test_mode && s.starts_with("http://")))
        && s.len() <= 512
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// 服务端转来的错误信息只保留一行可打印文本。
fn sanitize_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(300).collect()
}
