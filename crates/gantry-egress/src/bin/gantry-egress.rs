//! `gantry-egress` 进程入口：`gantry-egress [--config <path>]`（默认 `/etc/gantry/egress.json`）。
//!
//! 引导配置的形状见 `gantry_egress::daemon::Bootstrap`；密钥值从不经命令行、环境变量或配置文件传入。

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    use gantry_egress::daemon::{run, Bootstrap};

    let mut args = std::env::args().skip(1);
    let mut path = std::path::PathBuf::from("/etc/gantry/egress.json");
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--config", Some(value)) => path = value.into(),
            _ => {
                eprintln!("usage: gantry-egress [--config <path>]");
                return std::process::ExitCode::from(2);
            }
        }
    }
    let result = Bootstrap::load(&path).and_then(|config| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run(config))
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("gantry-egress: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("gantry-egress only runs on Unix");
    std::process::exit(1);
}
