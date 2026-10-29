//! `gantry doctor`（deploy.md §7.14）：`ok` / `warn` / `unchecked` / `fail`，`--json` 供自动化，
//! 退出码 `0` / `1`（有警告）/ `3`（有未检查区域）/ `2`（有失败）。
//!
//! M1 子集：`host` 区域的 `--test-mode` 标红、`webhook` 区域的外部可达检查（§7.6 验证 ②）。
//! 其余检查需要 root、systemd、真实 GitHub 或沙盒，随 M1.4（#106）逐项补齐；未实现的
//! 区域报 `unchecked`，不冒充通过。

use serde::Serialize;
use serde_json::Value;

use crate::manifest::{Entry, CALLBACK_PATH};
use crate::probe::{HttpProbe, Method};

pub const AREAS: &[&str] = &[
    "host", "pki", "github", "webhook", "egress", "backup", "version",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Unchecked,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Unchecked => "unchecked",
            Status::Fail => "fail",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub area: &'static str,
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(
        area: &'static str,
        name: &'static str,
        status: Status,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            area,
            name,
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn worst(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Ok)
    }

    pub fn exit_code(&self) -> u8 {
        match self.worst() {
            Status::Ok => 0,
            Status::Warn => 1,
            Status::Unchecked => 3,
            Status::Fail => 2,
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({ "status": self.worst(), "checks": self.checks })
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            out.push_str(&format!(
                "{:<4} {}.{}：{}\n",
                c.status.label(),
                c.area,
                c.name,
                c.detail
            ));
        }
        out
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("未知区域 {0}；可选：{areas}", areas = AREAS.join(" / "))]
pub struct UnknownArea(pub String);

/// `gantry doctor [区域] [--json]` 的参数。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DoctorArgs {
    pub area: Option<String>,
    pub json: bool,
}

impl DoctorArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut out = Self::default();
        for arg in args {
            match arg.as_str() {
                "--json" => out.json = true,
                a if a.starts_with('-') => return Err(format!("未知参数 {a}\n{USAGE}")),
                _ if out.area.is_some() => return Err(format!("只能指定一个区域\n{USAGE}")),
                _ => out.area = Some(arg),
            }
        }
        Ok(out)
    }
}

pub const USAGE: &str =
    "usage: gantry doctor [host|pki|github|webhook|egress|backup|version] [--json]";

pub struct DoctorContext<'a> {
    /// 引导配置里的公网 webhook 地址；未配置时 `webhook` 区域报 `fail`。
    pub entry: Option<Entry>,
    /// 安装是否带了 `--test-mode`（`install-state.json`）；`None` 表示状态不可读，按失败闭合。
    pub test_mode: Option<bool>,
    pub probe: &'a dyn HttpProbe,
}

pub fn run(area: Option<&str>, ctx: &DoctorContext<'_>) -> Result<Report, UnknownArea> {
    let areas: Vec<&'static str> = match area {
        None => AREAS.to_vec(),
        Some(a) => match AREAS.iter().find(|known| **known == a) {
            Some(known) => vec![*known],
            None => return Err(UnknownArea(a.to_owned())),
        },
    };
    let mut report = Report::default();
    for area in areas {
        match area {
            "host" => report.checks.push(host_test_mode(ctx.test_mode)),
            "webhook" => match &ctx.entry {
                Some(entry) => report
                    .checks
                    .extend(webhook_external(ctx.probe, entry, false)),
                None => report.checks.push(Check::new(
                    "webhook",
                    "configured",
                    Status::Fail,
                    "引导配置里没有公网 webhook 地址（install.sh --webhook-url）",
                )),
            },
            other => report.checks.push(not_implemented(other)),
        }
    }
    Ok(report)
}

fn not_implemented(area: &'static str) -> Check {
    Check::new(
        area,
        "pending",
        Status::Unchecked,
        "M1.4 补齐（#106），本次未检查",
    )
}

/// `--test-mode` 的安装在 `host` 区域标 `fail`，防止测试开关留在生产（§7.14）。
/// 状态文件缺失、损坏或缺字段时不冒充通过。
pub fn host_test_mode(test_mode: Option<bool>) -> Check {
    match test_mode {
        Some(true) => Check::new(
            "host",
            "test_mode",
            Status::Fail,
            "安装带 --test-mode，不能用于生产",
        ),
        Some(false) => Check::new("host", "test_mode", Status::Ok, "未启用 --test-mode"),
        None => Check::new(
            "host",
            "test_mode",
            Status::Fail,
            "install-state.json 缺失或不可读，无法确认 --test-mode 状态",
        ),
    }
}

/// §7.6 验证 ② 的外部部分：经公网入口发出的请求到达了 `forge`，而 API / 配对路径没有透出。
///
/// `setup_active` 为真时（`gantry github setup` 已登记会话）跳过回调路径的 `404` 断言。
/// "`404` 由入口而非 `gantryd` 返回"需要核对访问日志计数，归 M1.4。
pub fn webhook_external(probe: &dyn HttpProbe, entry: &Entry, setup_active: bool) -> Vec<Check> {
    let origin = entry.origin();
    let mut checks = vec![expect(
        probe,
        "webhook_reaches_forge",
        Method::Post,
        &entry.webhook_url(),
        401,
        "无签名的 POST /webhook 应被 forge 以 401 拒绝",
    )];
    if !setup_active {
        checks.push(expect(
            probe,
            "callback_without_session",
            Method::Get,
            &format!("{origin}{CALLBACK_PATH}"),
            404,
            "无 setup 会话时回调路径应返回 404",
        ));
    }
    checks.push(expect(
        probe,
        "api_not_exposed",
        Method::Get,
        &format!("{origin}/v1/me"),
        404,
        "入口不得转发 API 路径",
    ));
    checks.push(expect(
        probe,
        "pair_not_exposed",
        Method::Post,
        &format!("{origin}/v1/pair"),
        404,
        "入口不得转发配对路径",
    ));
    checks
}

fn expect(
    probe: &dyn HttpProbe,
    name: &'static str,
    method: Method,
    url: &str,
    want: u16,
    what: &str,
) -> Check {
    match probe.status(method, url) {
        Ok(code) if code == want => {
            Check::new("webhook", name, Status::Ok, format!("{url} → {code}"))
        }
        Ok(code) => Check::new(
            "webhook",
            name,
            Status::Fail,
            format!("{what}；实际 {url} → {code}"),
        ),
        Err(reason) => Check::new(
            "webhook",
            name,
            Status::Fail,
            format!("{what}；{url} 不可达：{reason}"),
        ),
    }
}
