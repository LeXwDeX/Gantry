//! 主机规范化、硬性拒绝表与允许模式匹配（`docs/modules/egress.md` §7.5 步 3–6、§7.7）。
//!
//! 规范化顺序：识别 IPv6 / 方括号 → IDNA（UTS #46）映射为 punycode 并小写 → 去一个尾部点 →
//! 识别 IP 字面量（含十进制 / 八进制 / 十六进制写法）→ 只允许 `[a-z0-9.-]` 的 LDH 标签 →
//! GitHub 域名族。命中任一项即拒绝，并给出审计规则 id。

use std::fmt;

/// GitHub 域名族（§7.7）。拒绝对**任意层级**子域生效。
const GITHUB_DOMAINS: &[&str] = &[
    "github.com",
    "githubusercontent.com",
    "githubassets.com",
    "github.io",
    "ghcr.io",
    "githubapp.com",
    "github.dev",
    "githubcopilot.com",
    "git.io",
];

/// `CONNECT` 只允许的端口（§7.7）。
pub const CONNECT_PORT: u16 = 443;

/// 主机或 authority 被拒绝的原因；`rule_id` 是审计 `rule` 字段的值（§6.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostDenial {
    /// GitHub 域名族（`hard.github`）。
    Github,
    /// IP 字面量，含 IPv6、十进制 / 八进制 / 十六进制与 IPv4 映射写法（`hard.ip_literal`）。
    IpLiteral,
    /// 端口不是 443（`hard.port`）。
    Port,
    /// authority 不合法：userinfo、缺端口、非 LDH 标签、IDNA 失败（`hard.authority`）。
    Authority,
}

impl HostDenial {
    pub fn rule_id(self) -> &'static str {
        match self {
            Self::Github => "hard.github",
            Self::IpLiteral => "hard.ip_literal",
            Self::Port => "hard.port",
            Self::Authority => "hard.authority",
        }
    }
}

impl fmt::Display for HostDenial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.rule_id())
    }
}

/// 规范化后的主机名：小写 punycode、无尾部点、只含 `[a-z0-9.-]`，不是 IP 字面量，也不在
/// GitHub 域名族里。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Host(String);

impl Host {
    /// 规范化并执行硬性拒绝表。
    pub fn parse(raw: &str) -> Result<Self, HostDenial> {
        let name = normalize(raw)?;
        if is_github(&name) {
            return Err(HostDenial::Github);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 解析 `CONNECT` 的 authority（`host:port`），严格：拒绝 userinfo、IP 字面量、非 443 端口。
///
/// 带 userinfo 时先判断其后的主机：落在 GitHub 域名族或是 IP 字面量就报对应的硬性规则，
/// 否则报 `hard.authority`。主机规则先于端口规则。
pub fn parse_connect_authority(raw: &str) -> Result<Host, HostDenial> {
    let (userinfo, hostport) = match raw.rsplit_once('@') {
        Some((userinfo, rest)) => (Some(userinfo), rest),
        None => (None, raw),
    };
    if hostport.starts_with('[') {
        return Err(HostDenial::IpLiteral);
    }
    let Some((host, port)) = hostport.rsplit_once(':') else {
        // 没有端口：主机若命中硬性规则仍按主机规则报告。
        Host::parse(hostport)?;
        return Err(HostDenial::Authority);
    };
    let host = Host::parse(host)?;
    if userinfo.is_some() {
        return Err(HostDenial::Authority);
    }
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HostDenial::Authority);
    }
    match port.parse::<u16>() {
        Ok(CONNECT_PORT) => Ok(host),
        Ok(_) => Err(HostDenial::Port),
        Err(_) => Err(HostDenial::Authority),
    }
}

fn normalize(raw: &str) -> Result<String, HostDenial> {
    if raw.is_empty() {
        return Err(HostDenial::Authority);
    }
    // IPv6 字面量（含 IPv4 映射写法）只能以方括号或冒号出现。
    if raw.starts_with('[') || raw.contains(':') {
        return Err(HostDenial::IpLiteral);
    }
    if raw.contains('@') || raw.contains('*') {
        return Err(HostDenial::Authority);
    }
    // UTS #46：全角字符、全角句点映射为 ASCII，大写转小写，非 ASCII 标签转 punycode。
    let ascii = idna::domain_to_ascii(raw).map_err(|_| HostDenial::Authority)?;
    let name = ascii.strip_suffix('.').unwrap_or(&ascii);
    if name.is_empty() || name.len() > 253 {
        return Err(HostDenial::Authority);
    }
    let labels: Vec<&str> = name.split('.').collect();
    // WHATWG URL 的 "ends in a number"：末尾标签是数字（十进制或 0x 十六进制）即按 IPv4 解析，
    // 覆盖 `127.1`、`2130706433`、`0x7f.0.0.1`、`017700000001` 等写法。顶级域不会是数字。
    if labels.last().is_some_and(|label| is_numeric_label(label)) {
        return Err(HostDenial::IpLiteral);
    }
    for label in &labels {
        if !is_ldh_label(label) {
            return Err(HostDenial::Authority);
        }
    }
    Ok(name.to_owned())
}

fn is_numeric_label(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if let Some(hex) = label
        .strip_prefix("0x")
        .or_else(|| label.strip_prefix("0X"))
    {
        return hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    label.bytes().all(|b| b.is_ascii_digit())
}

fn is_ldh_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// `name` 已规范化（小写、无尾部点）。
pub fn is_github(name: &str) -> bool {
    GITHUB_DOMAINS.iter().any(|domain| {
        name == *domain
            || name
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// 分组里的允许模式：`example.com` 精确匹配；`*.example.com` 只匹配恰好多一层标签的子域
/// （OQ-E8 未决，取更严的解释）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllowPattern {
    Exact(String),
    /// 存的是去掉 `*.` 之后的后缀。
    OneLabel(String),
}

impl AllowPattern {
    /// 解析配置里的分组条目；裸 `*`、多层通配、IP 字面量、GitHub 域名族一律拒绝（V10、G5）。
    pub fn parse(raw: &str) -> Result<Self, HostDenial> {
        match raw.strip_prefix("*.") {
            Some(rest) => {
                if rest.contains('*') {
                    return Err(HostDenial::Authority);
                }
                let suffix = Host::parse(rest)?;
                Ok(Self::OneLabel(suffix.0))
            }
            None => Ok(Self::Exact(Host::parse(raw)?.0)),
        }
    }

    pub fn matches(&self, host: &Host) -> bool {
        match self {
            Self::Exact(name) => host.0 == *name,
            Self::OneLabel(suffix) => host
                .0
                .strip_suffix(suffix.as_str())
                .and_then(|prefix| prefix.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty() && !label.contains('.')),
        }
    }
}
