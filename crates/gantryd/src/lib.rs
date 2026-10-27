#![forbid(unsafe_code)]

//! `gantryd`：服务端单进程（architecture §3）。
//!
//! 启动顺序：读引导配置 → 按 profile 装配模块 → 绑定控制通道 → 等 SIGTERM / SIGINT。
//! 装配见 [`assembly`]；控制通道见 [`control`]。

pub mod assembly;
pub mod config;
pub mod control;
#[cfg(feature = "fakes")]
mod fake_cmds;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use assembly::{assemble, AssemblyError, Profile};
use config::{BootConfig, ConfigError};
use control::{BindError, Control, ControlAuth, ControlSocket};

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub config: PathBuf,
    pub profile: Profile,
    /// 控制通道放行的 uid。只有 `fakes` 装配允许改（进程级测试不以 root 运行）。
    pub control_uid: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Assembly(#[from] AssemblyError),
    #[error(transparent)]
    Bind(#[from] BindError),
    #[error("signal handler: {0}")]
    Signal(std::io::Error),
}

pub const USAGE: &str =
    "usage: gantryd [--config <path>] [--profile real|fakes] [--control-uid <uid>]";

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, StartError> {
        let mut options = Self {
            config: config::DEFAULT_PATH.into(),
            profile: Profile::Real,
            control_uid: None,
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| StartError::Usage(format!("{flag} needs a value\n{USAGE}")))
            };
            match flag.as_str() {
                "--config" => options.config = value()?.into(),
                "--profile" => options.profile = value()?.parse().map_err(StartError::Usage)?,
                "--control-uid" => {
                    let uid = value()?;
                    let uid = uid
                        .parse()
                        .map_err(|_| StartError::Usage(format!("bad uid {uid:?}")))?;
                    options.control_uid = Some(uid);
                }
                other => return Err(StartError::Usage(format!("unknown flag {other}\n{USAGE}"))),
            }
        }
        if options.control_uid.is_some() && options.profile != Profile::Fakes {
            return Err(StartError::Usage(
                "--control-uid is only allowed with --profile fakes (control socket is root-only)"
                    .to_owned(),
            ));
        }
        Ok(options)
    }
}

/// 运行到收到 SIGTERM / SIGINT。启动阶段的任何失败都在绑定套接字之前或同时返回。
pub async fn run(options: Options) -> Result<(), StartError> {
    let config = BootConfig::load(&options.config)?;
    let assembly = Arc::new(assemble(options.profile)?);
    let auth = ControlAuth {
        uid: options.control_uid.unwrap_or(ControlAuth::ROOT.uid),
    };
    let socket = ControlSocket::bind(&config.server.control_socket)?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(StartError::Signal)?;
    eprintln!(
        "gantryd: profile={:?} control={:?} ready",
        options.profile,
        socket.path()
    );

    let (stop, stopped) = watch::channel(false);
    let control = Arc::new(Control::new(assembly));
    let serve = socket.serve(control, auth, stopped);
    tokio::pin!(serve);
    tokio::select! {
        _ = &mut serve => {}
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    let _ = stop.send(true);
    eprintln!("gantryd: shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, StartError> {
        Options::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn defaults_are_production() {
        let options = parse(&[]).unwrap();
        assert_eq!(options.config, PathBuf::from("/etc/gantry/gantry.toml"));
        assert_eq!(options.profile, Profile::Real);
        assert_eq!(options.control_uid, None);
    }

    #[test]
    fn control_uid_needs_fakes() {
        assert!(parse(&["--control-uid", "501"]).is_err());
        let options = parse(&["--profile", "fakes", "--control-uid", "501"]).unwrap();
        assert_eq!(options.control_uid, Some(501));
    }

    #[test]
    fn bad_flags_are_usage_errors() {
        for args in [&["--profile"][..], &["--profile", "dev"], &["--verbose"]] {
            assert!(matches!(parse(args), Err(StartError::Usage(_))), "{args:?}");
        }
    }

    #[test]
    fn real_profile_refuses_until_store_is_persistent() {
        assert!(matches!(
            assemble(Profile::Real),
            Err(AssemblyError::NotReady {
                module: "store",
                ..
            })
        ));
    }
}
