//! 引导配置 `/etc/gantry/gantry.toml`（deploy.md §10；config.md §1.1）。
//!
//! 只含进程启动前就要知道的项：路径、监听地址、套接字。重启生效，不热加载；
//! 模型、档位、策略等全部在 settings（I7），这里出现即拒绝（`deny_unknown_fields`）。
//! 默认值与必填项照 deploy.md §10 的表。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/gantry/gantry.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path:?}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid {path:?}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid {key}: {message}")]
    Invalid { key: &'static str, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootConfig {
    pub server: Server,
    pub forge: Forge,
    #[serde(default)]
    pub sandbox: Sandbox,
    #[serde(default)]
    pub egress: Egress,
    pub backup: Backup,
    #[serde(default)]
    pub upgrade: Upgrade,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_secrets_dir")]
    pub secrets_dir: PathBuf,
    pub api: Api,
    #[serde(default)]
    pub pair: Pair,
    #[serde(default = "default_control_socket")]
    pub control_socket: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    #[serde(default = "default_api_listen")]
    pub listen: SocketAddr,
    pub public_url: String,
    /// 由 `install.sh --api-host` 写入；至少一项（证书 SAN）。
    pub san: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    #[serde(default = "default_pair_listen")]
    pub listen: SocketAddr,
}

impl Default for Pair {
    fn default() -> Self {
        Self {
            listen: default_pair_listen(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forge {
    pub webhook: Webhook,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ingress {
    Tunnel,
    Public,
}

/// forge-actions §11 B。
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    #[serde(default = "default_webhook_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_ingress")]
    pub ingress: Ingress,
    pub public_url: String,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    #[serde(default)]
    pub ip_allowlist: bool,
    #[serde(default = "default_dedupe_retention")]
    pub dedupe_retention: String,
    #[serde(default = "default_payload_retention")]
    pub payload_retention: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    #[serde(default = "default_runtime_user")]
    pub runtime_user: String,
    #[serde(default = "default_runtime_socket")]
    pub runtime_socket: PathBuf,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            runtime_user: default_runtime_user(),
            runtime_socket: default_runtime_socket(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Egress {
    #[serde(default = "default_egress_socket")]
    pub control_socket: PathBuf,
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            control_socket: default_egress_socket(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    #[serde(default = "default_backup_dest")]
    pub dest: PathBuf,
    pub recipients: Vec<String>,
    #[serde(default)]
    pub post_hook: String,
    pub journal_target: PathBuf,
    #[serde(default = "default_keep_daily")]
    pub keep_daily: u32,
    #[serde(default = "default_keep_weekly")]
    pub keep_weekly: u32,
    #[serde(default = "default_keep_monthly")]
    pub keep_monthly: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upgrade {
    #[serde(default = "default_health_timeout_s")]
    pub health_timeout_s: u32,
}

impl Default for Upgrade {
    fn default() -> Self {
        Self {
            health_timeout_s: default_health_timeout_s(),
        }
    }
}

impl BootConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text).map_err(|error| match error {
            ConfigError::Parse { message, .. } => ConfigError::Parse {
                path: path.to_owned(),
                message,
            },
            other => other,
        })
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|error| ConfigError::Parse {
            path: PathBuf::new(),
            message: error.to_string(),
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        require_https("server.api.public_url", &self.server.api.public_url)?;
        require_https("forge.webhook.public_url", &self.forge.webhook.public_url)?;
        if self.server.api.san.is_empty() || self.server.api.san.iter().any(|s| s.is_empty()) {
            return invalid("server.api.san", "needs at least one non-empty host");
        }
        if self.backup.recipients.is_empty() {
            return invalid("backup.recipients", "needs at least one recipient");
        }
        if self.forge.webhook.max_body_bytes == 0 {
            return invalid("forge.webhook.max_body_bytes", "must be positive");
        }
        retention(
            "forge.webhook.dedupe_retention",
            &self.forge.webhook.dedupe_retention,
        )?;
        retention(
            "forge.webhook.payload_retention",
            &self.forge.webhook.payload_retention,
        )?;
        for (key, path) in [
            ("server.data_dir", &self.server.data_dir),
            ("server.secrets_dir", &self.server.secrets_dir),
            ("server.control_socket", &self.server.control_socket),
            ("sandbox.runtime_socket", &self.sandbox.runtime_socket),
            ("egress.control_socket", &self.egress.control_socket),
            ("backup.dest", &self.backup.dest),
            ("backup.journal_target", &self.backup.journal_target),
        ] {
            if !path.is_absolute() {
                return invalid(key, "must be an absolute path");
            }
        }
        Ok(())
    }
}

fn invalid<T>(key: &'static str, message: &str) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid {
        key,
        message: message.to_owned(),
    })
}

fn require_https(key: &'static str, url: &str) -> Result<(), ConfigError> {
    match url.strip_prefix("https://") {
        Some(rest) if !rest.is_empty() => Ok(()),
        _ => invalid(key, "must be an https:// URL"),
    }
}

/// 保留期写作 `<正整数><s|m|h|d>`，如 `30d`。
fn retention(key: &'static str, value: &str) -> Result<(), ConfigError> {
    let valid = value
        .strip_suffix(['s', 'm', 'h', 'd'])
        .and_then(|digits| digits.parse::<u32>().ok())
        .is_some_and(|n| n > 0);
    if valid {
        Ok(())
    } else {
        invalid(key, "expected <positive integer><s|m|h|d>, e.g. 30d")
    }
}

fn default_data_dir() -> PathBuf {
    "/var/lib/gantry".into()
}
fn default_secrets_dir() -> PathBuf {
    "/var/lib/gantry/secrets".into()
}
fn default_control_socket() -> PathBuf {
    "/run/gantry/control.sock".into()
}
fn default_api_listen() -> SocketAddr {
    ([0, 0, 0, 0], 8443).into()
}
fn default_pair_listen() -> SocketAddr {
    ([0, 0, 0, 0], 8444).into()
}
fn default_webhook_listen() -> SocketAddr {
    ([127, 0, 0, 1], 8787).into()
}
fn default_ingress() -> Ingress {
    Ingress::Tunnel
}
fn default_max_body_bytes() -> u64 {
    26_214_400
}
fn default_dedupe_retention() -> String {
    "30d".to_owned()
}
fn default_payload_retention() -> String {
    "7d".to_owned()
}
fn default_runtime_user() -> String {
    "gantry-sbx".to_owned()
}
fn default_runtime_socket() -> PathBuf {
    "/run/gantry-sbx/podman.sock".into()
}
fn default_egress_socket() -> PathBuf {
    "/run/gantry/egress.sock".into()
}
fn default_backup_dest() -> PathBuf {
    "/var/backups/gantry".into()
}
fn default_keep_daily() -> u32 {
    7
}
fn default_keep_weekly() -> u32 {
    4
}
fn default_keep_monthly() -> u32 {
    6
}
fn default_health_timeout_s() -> u32 {
    120
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[server.api]
public_url = "https://gantry.example"
san = ["gantry.example"]

[forge.webhook]
public_url = "https://hooks.example/github"

[backup]
recipients = ["age1example"]
journal_target = "/srv/journal/revocations.journal"
"#;

    #[test]
    fn minimal_file_fills_deploy_defaults() {
        let config = BootConfig::parse(MINIMAL).unwrap();
        assert_eq!(config.server.data_dir, Path::new("/var/lib/gantry"));
        assert_eq!(
            config.server.control_socket,
            Path::new("/run/gantry/control.sock")
        );
        assert_eq!(config.server.api.listen, default_api_listen());
        assert_eq!(config.server.pair.listen.port(), 8444);
        assert_eq!(config.forge.webhook.listen.to_string(), "127.0.0.1:8787");
        assert_eq!(config.forge.webhook.ingress, Ingress::Tunnel);
        assert_eq!(config.forge.webhook.max_body_bytes, 26_214_400);
        assert_eq!(config.forge.webhook.dedupe_retention, "30d");
        assert_eq!(config.sandbox.runtime_user, "gantry-sbx");
        assert_eq!(
            config.egress.control_socket,
            Path::new("/run/gantry/egress.sock")
        );
        assert_eq!(
            (
                config.backup.keep_daily,
                config.backup.keep_weekly,
                config.backup.keep_monthly
            ),
            (7, 4, 6)
        );
        assert_eq!(config.upgrade.health_timeout_s, 120);
    }

    #[test]
    fn required_keys_are_required() {
        for key in [
            "public_url = \"https://gantry.example\"",
            "recipients = [\"age1example\"]",
        ] {
            let text = MINIMAL.replace(key, "");
            assert!(BootConfig::parse(&text).is_err(), "missing {key} accepted");
        }
    }

    #[test]
    fn settings_keys_are_not_bootstrap_config() {
        let text = format!("{MINIMAL}\n[levels.low]\nnodes = 3\n");
        let error = BootConfig::parse(&text).unwrap_err().to_string();
        assert!(error.contains("levels"), "{error}");
    }

    #[test]
    fn unknown_nested_key_is_rejected() {
        let text = MINIMAL.replace("[server.api]", "[server.api]\nmtls = false");
        assert!(BootConfig::parse(&text).is_err());
    }

    #[test]
    fn values_are_validated() {
        let cases = [
            ("https://gantry.example", "http://gantry.example"),
            ("san = [\"gantry.example\"]", "san = []"),
            ("recipients = [\"age1example\"]", "recipients = []"),
            (
                "journal_target = \"/srv/journal/revocations.journal\"",
                "journal_target = \"relative.journal\"",
            ),
        ];
        for (from, to) in cases {
            let text = MINIMAL.replace(from, to);
            assert!(
                matches!(BootConfig::parse(&text), Err(ConfigError::Invalid { .. })),
                "{to} accepted"
            );
        }
        let text = MINIMAL.replace(
            "[forge.webhook]",
            "[forge.webhook]\ndedupe_retention = \"30 days\"",
        );
        assert!(matches!(
            BootConfig::parse(&text),
            Err(ConfigError::Invalid {
                key: "forge.webhook.dedupe_retention",
                ..
            })
        ));
    }
}
