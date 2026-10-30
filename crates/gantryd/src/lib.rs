#![forbid(unsafe_code)]
// 控制通道是 Unix 套接字，服务端只在 Linux 上运行；Windows 上本库为空。
#![cfg(unix)]

//! `gantryd`：服务端单进程（architecture §3）。
//!
//! 启动顺序：读引导配置 → 装配模块 → 注册 SIGTERM / SIGINT 接收器 → 绑定
//! 控制通道 → 等信号或引擎退出。注册先于绑定，套接字可连接之后送达的信号不会
//! 按默认动作杀死进程（#401）。装配见 [`assembly`]；控制通道见 [`control`]。

pub mod assembly;
pub mod cli;
pub mod config;
pub mod control;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use assembly::{assemble_real, Assembly, AssemblyError};
use config::{BootConfig, ConfigError};
use control::{BindError, CommandSet, Control, ControlAuth, ControlSocket};

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub config: PathBuf,
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
    /// 引擎主循环只在存储错误时退出（engine.md §7.15）。
    #[error("engine stopped: {0}")]
    Engine(String),
}

pub const USAGE: &str = "usage: gantryd [--config <path>]";

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, StartError> {
        let mut options = Self {
            config: config::DEFAULT_PATH.into(),
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--config" => {
                    let path = args.next().ok_or_else(|| {
                        StartError::Usage(format!("--config needs a value\n{USAGE}"))
                    })?;
                    options.config = path.into();
                }
                other => return Err(StartError::Usage(format!("unknown flag {other}\n{USAGE}"))),
            }
        }
        Ok(options)
    }
}

/// 服务端入口：真实装配，控制通道只放行 root。
pub async fn run(options: Options) -> Result<(), StartError> {
    let config = BootConfig::load(&options.config)?;
    let assembly = Arc::new(assemble_real(&config).await?);
    serve(&config, assembly, Vec::new(), ControlAuth::ROOT).await
}

/// 注册信号接收器，再绑定控制通道并服务到收到 SIGTERM / SIGINT。装配由调用方
/// 给出：服务端用 [`assemble_real`]，进程级测试用替身装配。
pub async fn serve(
    config: &BootConfig,
    assembly: Arc<Assembly>,
    commands: Vec<Arc<dyn CommandSet>>,
    auth: ControlAuth,
) -> Result<(), StartError> {
    // 订阅必须先于 bind：`signal` 在此安装进程级处理器并建立接收器，绑定之后、
    // select 首次 poll 之前送达的信号会被缓冲，不会按默认动作终止进程。
    // `ctrl_c()` 要等首次 poll 才注册，不能覆盖这个窗口。
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(StartError::Signal)?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(StartError::Signal)?;
    let socket = ControlSocket::bind(&config.server.control_socket)?;
    eprintln!(
        "gantryd: profile={:?} control={:?} ready",
        assembly.profile,
        socket.path()
    );

    let (stop, stopped) = watch::channel(false);
    let engine = tokio::spawn(assembly.engine.clone().run(stopped_signal(stopped.clone())));
    let control = Arc::new(Control::new(assembly, commands));
    let serve = socket.serve(control, auth, stopped);
    tokio::pin!(serve);
    tokio::pin!(engine);
    let mut engine_done = None;
    tokio::select! {
        _ = &mut serve => {}
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
        result = &mut engine => engine_done = Some(result),
    }
    let _ = stop.send(true);
    eprintln!("gantryd: shutting down");
    // 引擎处理完已入队的回报后返回；在途尝试留给下次启动的 Recover（engine.md §7.15）。
    let result = match engine_done {
        Some(result) => result,
        None => engine.await,
    };
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(StartError::Engine(error.to_string())),
        Err(join) => Err(StartError::Engine(join.to_string())),
    }
}

/// `watch` 置为 `true` 时完成的 future（引擎 `run` 的停机信号）。
async fn stopped_signal(mut stopped: watch::Receiver<bool>) {
    while !*stopped.borrow() {
        if stopped.changed().await.is_err() {
            return;
        }
    }
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
        assert_eq!(
            parse(&["--config", "/x.toml"]).unwrap().config,
            PathBuf::from("/x.toml")
        );
    }

    #[test]
    fn bad_flags_are_usage_errors() {
        for args in [
            &["--config"][..],
            &["--profile", "fakes"],
            &["--control-uid", "501"],
        ] {
            assert!(matches!(parse(args), Err(StartError::Usage(_))), "{args:?}");
        }
    }
}
