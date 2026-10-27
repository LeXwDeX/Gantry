//! `validate_invariants()`：`gantryd` 一侧唯一的容器参数闸（sandbox.md §7.3、§8）。
//!
//! 输入只能是 [`ContainerSpec`]；加固项本身是 [`crate::podman`] 里的常量，这里只
//! 断言调用方能填的字段落在允许范围内。任何一条不满足都返回
//! [`InvariantViolation`]，驱动据此不创建容器。

use std::collections::BTreeSet;

use gantry_core::ids::AttemptId;

use crate::spec::{
    ContainerSpec, Mount, StagingLayout, DST_HOME, DST_INPUTS, DST_JOB_JSON, DST_OUT, DST_TMP,
    DST_WORK, LABEL_JOB_ID, LABEL_MANAGED, LABEL_SANDBOX_ID,
};

/// 校验时需要的服务端事实。
#[derive(Clone, Copy, Debug)]
pub struct InvariantContext<'a> {
    pub layout: &'a StagingLayout,
    /// 已签发的 `job_token`；给出时断言它只出现在 `token_env` 声明的变量值里。
    pub job_token: Option<&'a str>,
}

/// 环境变量名里出现即拒绝的片段（大小写不敏感，§7.3「环境变量」）。
pub const FORBIDDEN_ENV_FRAGMENTS: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "PRIVATE_KEY"];

/// 显式豁免的占位 key：名字 → 唯一允许的值。
pub const PLACEHOLDER_ENV: &[(&str, &str)] = &[("ANTHROPIC_API_KEY", "gantry-egress-managed")];

/// Podman 的网络模式关键字；沙盒只能接到一个具名网络上。
const RESERVED_NETWORKS: &[&str] = &[
    "host",
    "none",
    "bridge",
    "private",
    "slirp4netns",
    "pasta",
    "container",
    "ns",
];

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvariantViolation {
    #[error("container name must be {expected}")]
    Name { expected: String },
    #[error("label {0} is missing or wrong")]
    Label(&'static str),
    #[error("label key or value contains forbidden characters")]
    LabelChars,
    #[error("argv must be non-empty and free of NUL")]
    Argv,
    #[error("environment variable name {0:?} is not allowed")]
    EnvName(String),
    #[error("environment variable {0} contains NUL")]
    EnvValue(String),
    #[error("placeholder variable {0} must carry the placeholder value")]
    Placeholder(String),
    #[error("token_env variable {0} is declared but not set")]
    TokenEnvMissing(String),
    #[error("job_token appears outside token_env variables ({0})")]
    TokenLeak(String),
    #[error("bind mount source is outside the job directory or not one of its four paths")]
    MountSource,
    #[error("bind mount {0} is not at its fixed destination")]
    MountDestination(String),
    #[error("{0} must be mounted read-only")]
    MountWritable(&'static str),
    #[error("{0} must be mounted read-write")]
    MountReadOnly(&'static str),
    #[error("tmpfs is only allowed at /gantry/home and /gantry/tmp with a positive size")]
    Tmpfs,
    #[error("mount {0} is missing or duplicated")]
    MountSet(&'static str),
    #[error("resource limit {0} must be positive")]
    Limit(&'static str),
    #[error("stop grace must be between 1 and 600 seconds")]
    StopGrace,
    #[error("log max_bytes must be positive")]
    Log,
    #[error("network must be a named sandbox network")]
    Network,
    #[error("add-host entry is not a plain host name with a concrete address")]
    AddHost,
}

pub fn validate_invariants(
    spec: &ContainerSpec,
    ctx: InvariantContext<'_>,
) -> Result<(), InvariantViolation> {
    check_identity(spec, ctx.layout)?;
    check_argv(spec)?;
    check_env(spec, ctx.job_token)?;
    check_mounts(spec, ctx.layout)?;
    check_limits(spec)?;
    check_network(spec)?;
    Ok(())
}

fn check_identity(spec: &ContainerSpec, layout: &StagingLayout) -> Result<(), InvariantViolation> {
    let expected = layout.container_name();
    if spec.name != expected {
        return Err(InvariantViolation::Name { expected });
    }
    for (key, value) in &spec.labels {
        if !plain_text(key) || !plain_text(value) || key.is_empty() || key.contains('=') {
            return Err(InvariantViolation::LabelChars);
        }
    }
    if spec.labels.get(LABEL_MANAGED).map(String::as_str) != Some("1") {
        return Err(InvariantViolation::Label(LABEL_MANAGED));
    }
    if spec.labels.get(LABEL_SANDBOX_ID) != Some(&layout.sandbox_id().to_string()) {
        return Err(InvariantViolation::Label(LABEL_SANDBOX_ID));
    }
    if !spec
        .labels
        .get(LABEL_JOB_ID)
        .is_some_and(|value| value.parse::<AttemptId>().is_ok())
    {
        return Err(InvariantViolation::Label(LABEL_JOB_ID));
    }
    Ok(())
}

fn check_argv(spec: &ContainerSpec) -> Result<(), InvariantViolation> {
    if spec.argv.is_empty() || spec.argv.iter().any(|arg| arg.contains('\0')) {
        return Err(InvariantViolation::Argv);
    }
    Ok(())
}

fn check_env(spec: &ContainerSpec, job_token: Option<&str>) -> Result<(), InvariantViolation> {
    let token_vars: BTreeSet<&str> = spec
        .token_env
        .model_base_url
        .values()
        .chain(spec.token_env.proxy.iter())
        .map(String::as_str)
        .collect();
    for (name, value) in &spec.env {
        if !valid_env_name(name) {
            return Err(InvariantViolation::EnvName(name.clone()));
        }
        if value.contains('\0') {
            return Err(InvariantViolation::EnvValue(name.clone()));
        }
        if let Some((_, placeholder)) = PLACEHOLDER_ENV.iter().find(|(n, _)| *n == name) {
            if value != placeholder {
                return Err(InvariantViolation::Placeholder(name.clone()));
            }
            continue;
        }
        let upper = name.to_ascii_uppercase();
        if FORBIDDEN_ENV_FRAGMENTS
            .iter()
            .any(|fragment| upper.contains(fragment))
        {
            return Err(InvariantViolation::EnvName(name.clone()));
        }
        if let Some(token) = job_token.filter(|t| !t.is_empty()) {
            if value.contains(token) && !token_vars.contains(name.as_str()) {
                return Err(InvariantViolation::TokenLeak(name.clone()));
            }
        }
    }
    for var in token_vars {
        if !spec.env.contains_key(var) {
            return Err(InvariantViolation::TokenEnvMissing(var.to_owned()));
        }
    }
    Ok(())
}

fn check_mounts(spec: &ContainerSpec, layout: &StagingLayout) -> Result<(), InvariantViolation> {
    let targets = layout.bind_targets();
    let mut seen = BTreeSet::new();
    for mount in &spec.mounts {
        match mount {
            Mount::Bind { src, dst, ro } => {
                let Some((_, fixed_dst)) = targets.iter().find(|(path, _)| path == src) else {
                    return Err(InvariantViolation::MountSource);
                };
                if dst != fixed_dst {
                    return Err(InvariantViolation::MountDestination(dst.clone()));
                }
                match *fixed_dst {
                    DST_JOB_JSON | DST_INPUTS if !ro => {
                        return Err(InvariantViolation::MountWritable(fixed_dst));
                    }
                    DST_OUT if *ro => return Err(InvariantViolation::MountReadOnly(DST_OUT)),
                    _ => {}
                }
                if !seen.insert(*fixed_dst) {
                    return Err(InvariantViolation::MountSet(fixed_dst));
                }
            }
            Mount::Tmpfs { dst, size_mb } => {
                let fixed = match dst.as_str() {
                    DST_HOME => DST_HOME,
                    DST_TMP => DST_TMP,
                    _ => return Err(InvariantViolation::Tmpfs),
                };
                if *size_mb == 0 {
                    return Err(InvariantViolation::Tmpfs);
                }
                if !seen.insert(fixed) {
                    return Err(InvariantViolation::MountSet(fixed));
                }
            }
        }
    }
    for required in [
        DST_JOB_JSON,
        DST_INPUTS,
        DST_WORK,
        DST_OUT,
        DST_HOME,
        DST_TMP,
    ] {
        if !seen.contains(required) {
            return Err(InvariantViolation::MountSet(required));
        }
    }
    Ok(())
}

fn check_limits(spec: &ContainerSpec) -> Result<(), InvariantViolation> {
    let limits = &spec.limits;
    if !(limits.cpu_cores.is_finite() && limits.cpu_cores > 0.0) {
        return Err(InvariantViolation::Limit("cpu_cores"));
    }
    if limits.mem_mb == 0 {
        return Err(InvariantViolation::Limit("mem_mb"));
    }
    if limits.disk_mb == 0 {
        return Err(InvariantViolation::Limit("disk_mb"));
    }
    if limits.pids == 0 {
        return Err(InvariantViolation::Limit("pids"));
    }
    if limits.wall_time_s == 0 {
        return Err(InvariantViolation::Limit("wall_time_s"));
    }
    let grace = spec.stop_grace.as_secs();
    if !(1..=600).contains(&grace) || spec.stop_grace.subsec_nanos() != 0 {
        return Err(InvariantViolation::StopGrace);
    }
    if spec.log.max_bytes == 0 {
        return Err(InvariantViolation::Log);
    }
    Ok(())
}

fn check_network(spec: &ContainerSpec) -> Result<(), InvariantViolation> {
    let name = spec.network.network.as_str();
    let well_formed = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        });
    if !well_formed || RESERVED_NETWORKS.contains(&name) {
        return Err(InvariantViolation::Network);
    }
    for (host, ip) in &spec.network.add_hosts {
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'));
        if !host_ok || ip.is_unspecified() || ip.is_multicast() {
            return Err(InvariantViolation::AddHost);
        }
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// 标签进入 `--label=k=v`：不得含控制字符。
fn plain_text(value: &str) -> bool {
    !value.chars().any(char::is_control)
}
