//! 配置视图的防御性复核（`docs/modules/egress.md` §7.9；`docs/interfaces/config.md` V07 / V10 /
//! V11 / V13；`docs/interfaces/config-view-injection.md`）。
//!
//! `core::egress::ConfigView` 反序列化时已经校验了 `injection` 的形态与 V13，但不复核分组
//! （V10、G5）、provider（V11）与权重（V07）。Egress 收到视图后在这里补齐；任何一项失败，
//! 整个视图被拒（`422 config_invalid`），旧视图保留。

use gantry_core::egress::{ConfigView, ProblemError, ProviderConfig};
use gantry_core::entities::TokenWeights;

use crate::host::{AllowPattern, Host, HostDenial};

/// V11 要求 provider 主机属于这个分组。
pub const MODEL_API_GROUP: &str = "model_api";

/// M1 唯一支持的 provider kind（config §3.1）。
pub const PROVIDER_KIND_ANTHROPIC: &str = "anthropic";

/// 复核通过的视图，附带预解析的允许模式与 provider 上游。
#[derive(Clone, Debug)]
pub struct ReviewedConfig {
    pub view: ConfigView,
    pub groups: Vec<(String, Vec<AllowPattern>)>,
    pub providers: Vec<(String, ProviderUpstream)>,
}

impl ReviewedConfig {
    pub fn group(&self, name: &str) -> Option<&[AllowPattern]> {
        self.groups
            .iter()
            .find(|(group, _)| group == name)
            .map(|(_, patterns)| patterns.as_slice())
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderUpstream> {
        self.providers
            .iter()
            .find(|(provider, _)| provider == id)
            .map(|(_, upstream)| upstream)
    }
}

/// provider 的固定上游：只用 HTTPS 443，主机来自配置，不来自请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderUpstream {
    pub kind: String,
    pub host: Host,
    /// `base_url` 的路径部分，不含尾部 `/`；转发路径拼在其后。
    pub base_path: String,
}

/// 复核视图；失败时返回全部问题（`pointer` 相对视图）。
pub fn review(view: ConfigView) -> Result<ReviewedConfig, Vec<ProblemError>> {
    let mut issues = Vec::new();

    // config-view-injection.md §3.2.4：M1 控制面拒绝非空 injection。
    if !view.injection.is_empty() {
        issues.push(issue(
            "/injection",
            "private registry injection is not enabled in this milestone (must be empty)",
        ));
    }

    let mut groups = Vec::new();
    for (name, group) in &view.groups {
        let mut patterns = Vec::new();
        for (index, raw) in group.hosts.iter().enumerate() {
            match AllowPattern::parse(raw) {
                Ok(pattern) => patterns.push(pattern),
                Err(denial) => issues.push(issue(
                    &format!("/groups/{}/hosts/{index}", escape(name)),
                    &format!("egress_forbidden_host: {}", describe(denial)),
                )),
            }
        }
        groups.push((name.clone(), patterns));
    }

    for (name, profile) in &view.profiles {
        for (index, group) in profile.groups.iter().enumerate() {
            if !view.groups.contains_key(group) {
                issues.push(issue(
                    &format!("/profiles/{}/groups/{index}", escape(name)),
                    &format!("unknown group {group}"),
                ));
            }
        }
    }
    if !view.profiles.contains_key(&view.default_profile) {
        issues.push(issue("/default_profile", "unknown profile"));
    }

    let model_api = groups
        .iter()
        .find(|(name, _)| name == MODEL_API_GROUP)
        .map(|(_, patterns)| patterns.as_slice());
    let mut providers = Vec::new();
    for (id, provider) in &view.providers {
        let pointer = format!("/providers/{}", escape(id));
        if !is_path_segment(id) {
            issues.push(issue(
                &pointer,
                "provider id must be a single URL path segment",
            ));
            continue;
        }
        match review_provider(provider, model_api) {
            Ok(upstream) => providers.push((id.clone(), upstream)),
            Err((field, message)) => issues.push(issue(&format!("{pointer}{field}"), &message)),
        }
    }

    if let Err(message) = review_weights(&view.token_weights) {
        issues.push(issue("/token_weights", &message));
    }

    if issues.is_empty() {
        Ok(ReviewedConfig {
            view,
            groups,
            providers,
        })
    } else {
        Err(issues)
    }
}

fn review_provider(
    provider: &ProviderConfig,
    model_api: Option<&[AllowPattern]>,
) -> Result<ProviderUpstream, (&'static str, String)> {
    if provider.kind != PROVIDER_KIND_ANTHROPIC {
        return Err((
            "/kind",
            format!("unsupported provider kind {}", provider.kind),
        ));
    }
    let (host, base_path) = parse_https_base(&provider.base_url).map_err(|m| ("/base_url", m))?;
    // V11：所有 provider 主机属于 groups.model_api。
    let allowed = model_api.is_some_and(|patterns| patterns.iter().any(|p| p.matches(&host)));
    if !allowed {
        return Err((
            "/base_url",
            format!("provider_host_not_allowed: {host} is not in group {MODEL_API_GROUP}"),
        ));
    }
    Ok(ProviderUpstream {
        kind: provider.kind.clone(),
        host,
        base_path,
    })
}

/// `https://<host>[:443][/path]`；不允许 userinfo、其他端口、查询串与片段。
fn parse_https_base(url: &str) -> Result<(Host, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("base_url must be an https URL")?;
    if rest.contains(['?', '#', '\\']) {
        return Err("base_url must not contain a query, fragment or backslash".into());
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err("base_url must not contain userinfo".into());
    }
    let host = match authority.strip_suffix(":443") {
        Some(host) => host,
        None if authority.contains(':') && !authority.starts_with('[') => {
            return Err("base_url port must be 443".into())
        }
        None => authority,
    };
    let host =
        Host::parse(host).map_err(|denial| format!("base_url host: {}", describe(denial)))?;
    let path = path.trim_end_matches('/');
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("base_url path must not contain dot segments".into());
    }
    Ok((host, path.to_owned()))
}

/// V07：四项权重齐全且为正的有限数。
fn review_weights(weights: &TokenWeights) -> Result<(), String> {
    let all = [
        weights.input,
        weights.output,
        weights.cache_read,
        weights.cache_write,
    ];
    if all.iter().all(|w| w.is_finite() && *w > 0.0) {
        Ok(())
    } else {
        Err("token_weights_invalid: weights must be finite and positive".into())
    }
}

fn is_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn describe(denial: HostDenial) -> &'static str {
    match denial {
        HostDenial::Github => "GitHub domains are hard-denied",
        HostDenial::IpLiteral => "IP literals are not allowed",
        HostDenial::Port => "only port 443 is allowed",
        HostDenial::Authority => {
            "not a valid host pattern (bare or multi-level wildcards are not allowed)"
        }
    }
}

fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn issue(pointer: &str, message: &str) -> ProblemError {
    ProblemError {
        pointer: pointer.to_owned(),
        message: message.to_owned(),
    }
}
