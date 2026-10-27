//! 进程组装（`docs/modules/egress.md` §7.1、§7.11、§10）：读引导配置，加固进程，打开密钥库、
//! `egress.db` 与审计，起控制面 socket 与三个数据面监听器。
//!
//! 引导配置归 `deploy`（文件，重启生效；§10 [提议]），本模块只定义它的形状与默认值。
//! 没有任何从环境变量、命令行或配置文件读入密钥值的路径（§7.10）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gantry_core::ids::Clock;
use gantry_core::secret::Consumer;
use gantry_core::time::Timestamp;
use serde::Deserialize;
use tokio::net::{TcpListener, UnixListener};

use crate::audit::JsonlAudit;
use crate::connect::{ConnectProxy, TunnelLimits};
use crate::control::ControlServer;
use crate::db::Db;
use crate::model_proxy::{Limits, ModelProxy};
use crate::netdeny::{Cidr, DenyNets};
use crate::registry::{Endpoints, Listener, Registry, RegistryOptions};
use crate::token::OsEntropy;
use crate::upstream::{
    roots_from_pem, system_roots, tls_config, Connector, SystemResolver, TcpDialer,
};

/// 引导配置（JSON）。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    /// `egress.db`、`audit/` 所在目录。
    pub data_dir: PathBuf,
    /// 控制面 socket（P-50）。
    #[serde(default = "default_control_socket")]
    pub control_socket: PathBuf,
    /// 允许连接控制面的对端 uid（`gantryd` 的系统用户）。
    pub control_allowed_uid: u32,
    pub listen: ListenConfig,
    /// `Some` = "只能从沙盒网段使用"可强制（P-73，需核实）。
    #[serde(default)]
    pub sandbox_cidrs: Option<Vec<String>>,
    pub secrets: SecretsBootstrap,
    /// 除系统根证书外额外信任的根（PEM 文件），通常为空。
    #[serde(default)]
    pub extra_roots_pem: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
    pub sandbox_model: SocketAddr,
    pub sandbox_connect: SocketAddr,
    /// 只能是回环地址。
    #[serde(default = "default_svc_model")]
    pub svc_model: SocketAddr,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsBootstrap {
    pub dir: PathBuf,
    pub master_key: MasterKeyConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MasterKeyConfig {
    /// `$CREDENTIALS_DIRECTORY/<name>`。
    SystemdCredential(String),
    /// 本地密钥文件（`0400`，位于密钥目录之外）。
    File(PathBuf),
}

fn default_control_socket() -> PathBuf {
    PathBuf::from("/run/gantry/egress.sock")
}

fn default_svc_model() -> SocketAddr {
    "127.0.0.1:8082".parse().expect("valid default")
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("bootstrap config: {0}")]
    Config(String),
    #[error("secrets: {0}")]
    Secrets(String),
    #[error("storage: {0}")]
    Storage(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Bootstrap {
    pub fn load(path: &Path) -> Result<Self, StartError> {
        let raw = std::fs::read(path).map_err(|e| StartError::Config(e.to_string()))?;
        let config: Self =
            serde_json::from_slice(&raw).map_err(|e| StartError::Config(e.to_string()))?;
        if !config.listen.svc_model.ip().is_loopback() {
            return Err(StartError::Config(
                "listen.svc_model must be a loopback address".into(),
            ));
        }
        Ok(config)
    }

    fn cidrs(&self) -> Result<Option<Vec<Cidr>>, StartError> {
        self.sandbox_cidrs
            .as_ref()
            .map(|cidrs| {
                cidrs
                    .iter()
                    .map(|raw| {
                        raw.parse::<Cidr>()
                            .map_err(|e| StartError::Config(e.to_string()))
                    })
                    .collect()
            })
            .transpose()
    }
}

/// 系统时钟。
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Timestamp::from_millis(ms)
    }
}

/// 组装并运行，直到任一监听器退出（出错）。
pub async fn run(config: Bootstrap) -> Result<(), StartError> {
    // 读任何密钥之前加固进程：禁核心转储、不可 ptrace 附着（Linux）。
    gantry_secrets::harden_process().map_err(|e| StartError::Secrets(e.to_string()))?;

    let secrets = open_secrets(&config.secrets)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let db = Db::open(&config.data_dir.join("egress.db"))
        .map_err(|e| StartError::Storage(e.to_string()))?;
    let audit = Arc::new(
        JsonlAudit::open(config.data_dir.join("audit"), clock.clone())
            .map_err(|e| StartError::Storage(e.to_string()))?,
    );
    let cidrs = config.cidrs()?;
    let endpoints = Endpoints {
        sandbox_model_base: format!("http://{}", config.listen.sandbox_model),
        connect_proxy_authority: config.listen.sandbox_connect.to_string(),
        loopback_model_base: format!("http://{}", config.listen.svc_model),
    };
    let mut options = RegistryOptions::new(endpoints);
    options.source_check_enforced = cidrs.is_some();
    let registry = Registry::open(
        db,
        clock,
        Arc::new(OsEntropy),
        Arc::new(secrets.scoped),
        options,
    )
    .map_err(|e| StartError::Storage(e.to_string()))?;

    // 出站落点再拒绝：Egress 自己的监听地址与沙盒网段（§7.7）。
    let mut extra = Vec::new();
    for addr in [config.listen.sandbox_model, config.listen.sandbox_connect] {
        let prefix = if addr.is_ipv4() { 32 } else { 128 };
        extra.push(Cidr::new(addr.ip(), prefix).map_err(|e| StartError::Config(e.to_string()))?);
    }
    extra.extend(cidrs.clone().unwrap_or_default());
    let connector = Connector::new(
        Arc::new(SystemResolver),
        Arc::new(TcpDialer),
        DenyNets::new(extra),
        Duration::from_secs(10),
    );
    let mut roots = system_roots();
    if let Some(path) = &config.extra_roots_pem {
        let pem = std::fs::read_to_string(path)?;
        for cert in roots_from_pem(&pem)?.roots {
            roots.roots.push(cert);
        }
    }

    let model = ModelProxy::new(
        registry.clone(),
        Arc::new(secrets.reader),
        audit.clone(),
        connector.clone(),
        tls_config(roots),
        Limits::default(),
        cidrs.clone(),
    );
    let connect = ConnectProxy::new(
        registry.clone(),
        audit.clone(),
        connector,
        TunnelLimits::default(),
        cidrs,
    );
    let control = ControlServer::new(registry, audit, config.control_allowed_uid);

    let control_listener = bind_control_socket(&config.control_socket)?;
    let sandbox_model = TcpListener::bind(config.listen.sandbox_model).await?;
    let sandbox_connect = TcpListener::bind(config.listen.sandbox_connect).await?;
    let svc_model = TcpListener::bind(config.listen.svc_model).await?;

    tokio::select! {
        result = control.serve(control_listener) => result?,
        result = model.clone().serve(sandbox_model, Listener::Sandbox) => result?,
        result = model.serve(svc_model, Listener::Loopback) => result?,
        result = connect.serve(sandbox_connect) => result?,
    }
    Ok(())
}

struct OpenedSecrets {
    reader: gantry_secrets::SecretReader,
    scoped: gantry_secrets::ScopedSecrets,
}

fn open_secrets(config: &SecretsBootstrap) -> Result<OpenedSecrets, StartError> {
    let source = || -> Box<dyn gantry_secrets::MasterKeySource> {
        match &config.master_key {
            MasterKeyConfig::SystemdCredential(name) => {
                Box::new(gantry_secrets::SystemdCredential { name: name.clone() })
            }
            MasterKeyConfig::File(path) => Box::new(gantry_secrets::KeyFile { path: path.clone() }),
        }
    };
    let cfg = || gantry_secrets::SecretsConfig::new(&config.dir, vec![Consumer::Egress], source());
    if !config.dir.exists() {
        gantry_secrets::Secrets::init(cfg()).map_err(|e| StartError::Secrets(e.to_string()))?;
    }
    let secrets =
        gantry_secrets::Secrets::open(cfg()).map_err(|e| StartError::Secrets(e.to_string()))?;
    let reader = secrets
        .reader(Consumer::Egress)
        .map_err(|e| StartError::Secrets(e.to_string()))?;
    let scoped = secrets
        .scoped(Consumer::Egress)
        .map_err(|e| StartError::Secrets(e.to_string()))?;
    Ok(OpenedSecrets { reader, scoped })
}

/// 绑定控制面 socket，权限 0660（组 `gantry-link` 由 `deploy` 设定目录属组保证，§2.1）。
fn bind_control_socket(path: &Path) -> Result<UnixListener, StartError> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(StartError::Config(format!(
                "{} exists and is not a socket",
                path.display()
            )));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}
