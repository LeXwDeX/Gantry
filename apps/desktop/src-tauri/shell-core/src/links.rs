//! 外链（desktop-shell.md §6.1 `open_external` / `open_github`、§8 S-7）。
//!
//! WebView 不能自己跳转或开新窗口，外链只经这两条命令，由外壳交给系统浏览器：
//!
//! - `open_external`：只放行 `https`，拒绝带用户信息的 URL；主机不是 `github.com` 时外壳先弹
//!   原生确认框，框内写明完整主机名（国际化域名同时显示 Unicode 与 punycode）。
//! - `open_github`：UI 只给 `kind` / `repo` / 编号或 SHA，URL 由外壳拼出，永远指向 `github.com`。

use gantry_core::canon::{GitOid, RepoSlug};
use url::Url;

use crate::ipc::ShellError;

/// 校验通过、可以交给系统浏览器的外链。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalLink {
    /// 规范化后的 URL（主机为 punycode）。
    pub url: String,
    /// 需要外壳先弹原生确认框。
    pub needs_confirm: bool,
    /// 确认框里展示的主机：Unicode 形式；与 punycode 不同时附在括号里。
    pub host_display: String,
}

const GITHUB_HOST: &str = "github.com";
/// 过长的 URL 不交给系统浏览器（也避免确认框被撑爆）。
const MAX_URL_LEN: usize = 4096;

pub fn check_external(raw: &str) -> Result<ExternalLink, ShellError> {
    let bad = || ShellError::bad_argument("url");
    if raw.len() > MAX_URL_LEN || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(bad());
    }
    // WHATWG 解析很宽松（`https:///x` 会把 `x` 当主机），先要求原文就是 `https://主机…`。
    let rest = raw.strip_prefix("https://").ok_or_else(bad)?;
    if rest.is_empty() || rest.starts_with(['/', '\\']) {
        return Err(bad());
    }
    let url = Url::parse(raw).map_err(|_| bad())?;
    if url.scheme() != "https" {
        return Err(bad());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad());
    }
    let host_ascii = match url.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        None => return Err(bad()),
    };
    let (host_unicode, idna_ok) = idna::domain_to_unicode(&host_ascii);
    if idna_ok.is_err() {
        return Err(bad());
    }
    let host_display = if host_unicode == host_ascii {
        host_ascii.clone()
    } else {
        format!("{host_unicode}（{host_ascii}）")
    };
    let needs_confirm = !(host_ascii == GITHUB_HOST && url.port().is_none());
    Ok(ExternalLink {
        url: url.into(),
        needs_confirm,
        host_display,
    })
}

/// `open_github` 的参数（与 `OpenGithubArgs` 同形）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubTarget<'a> {
    Pr { repo: &'a str, number: u64 },
    Issue { repo: &'a str, number: u64 },
    Commit { repo: &'a str, sha: &'a str },
}

impl<'a> GithubTarget<'a> {
    /// 从 IPC 的扁平参数构造；多余或缺失的参数都算错（防止 UI 夹带）。
    pub fn from_args(
        kind: &str,
        repo: &'a str,
        number: Option<u64>,
        sha: Option<&'a str>,
    ) -> Result<Self, ShellError> {
        match (kind, number, sha) {
            ("pr", Some(number), None) => Ok(Self::Pr { repo, number }),
            ("issue", Some(number), None) => Ok(Self::Issue { repo, number }),
            ("commit", None, Some(sha)) => Ok(Self::Commit { repo, sha }),
            ("pr" | "issue", _, _) => Err(ShellError::bad_argument("number")),
            ("commit", _, _) => Err(ShellError::bad_argument("sha")),
            _ => Err(ShellError::bad_argument("kind")),
        }
    }

    pub fn url(&self) -> Result<String, ShellError> {
        let repo = |r: &str| {
            // RepoSlug 允许 `.`；只由点组成的段（`..`）会被 URL 规范化吞掉，单独拒绝。
            if r.split('/').any(|part| part.bytes().all(|b| b == b'.')) {
                return Err(ShellError::bad_argument("repo"));
            }
            RepoSlug::parse(r)
                .map(|s| s.as_str().to_string())
                .map_err(|_| ShellError::bad_argument("repo"))
        };
        let number = |n: u64| {
            if n == 0 {
                Err(ShellError::bad_argument("number"))
            } else {
                Ok(n)
            }
        };
        Ok(match self {
            Self::Pr { repo: r, number: n } => {
                format!("https://{GITHUB_HOST}/{}/pull/{}", repo(r)?, number(*n)?)
            }
            Self::Issue { repo: r, number: n } => {
                format!("https://{GITHUB_HOST}/{}/issues/{}", repo(r)?, number(*n)?)
            }
            Self::Commit { repo: r, sha } => {
                GitOid::parse(sha).map_err(|_| ShellError::bad_argument("sha"))?;
                format!("https://{GITHUB_HOST}/{}/commit/{sha}", repo(r)?)
            }
        })
    }
}
