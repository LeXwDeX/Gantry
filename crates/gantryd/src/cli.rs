//! `gantry` 主机 CLI（deploy.md §6.1；OPS-07 的落地：CLI 在 `crates/gantryd`）。
//!
//! 这里只解析参数并分派：命令逻辑在 `gantry-deploy` 等模块的库函数里，
//! 与运行中的 `gantryd` 只经控制通道（`control.sock`）交互。未接入的子命令
//! （`pair`、`device`、`pki`、`secrets`、`store`）报用法错误，并写明归哪个模块。

use std::io::Write;
use std::path::{Path, PathBuf};

use gantry_deploy::control::{UnixControlClient, DEFAULT_SOCKET};
use gantry_deploy::manifest::Entry;
use gantry_deploy::probe::CurlProbe;
use gantry_deploy::{bootstrap_set, doctor, github_setup};

use crate::config::{self, BootConfig};

pub const USAGE: &str = "usage: gantry [--config <path>] [--socket <path>] <command>
commands:
  github setup …            登记 GitHub App（gantry-deploy）
  doctor [区域] [--json]     自检
  settings bootstrap-set …  引导期写入仅 approver 键";

/// 安装状态文件（deploy.md §5）；`doctor` 从中读 `test_mode`。
pub const INSTALL_STATE: &str = "/etc/gantry/install-state.json";

/// 用法错误的退出码，与 gantry-deploy 的约定一致。
const EXIT_USAGE: u8 = 2;

struct Global {
    config: PathBuf,
    socket: Option<PathBuf>,
}

/// 运行一条命令并返回退出码。`out` 是标准输出，错误写到 `err`。
pub fn run(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let mut global = Global {
        config: config::DEFAULT_PATH.into(),
        socket: None,
    };
    let mut rest = args.into_iter().peekable();
    while let Some(flag) = rest.peek() {
        let slot = match flag.as_str() {
            "--config" => &mut global.config,
            "--socket" => global.socket.get_or_insert_with(PathBuf::new),
            _ => break,
        };
        let flag = rest.next().unwrap_or_default();
        match rest.next() {
            Some(value) => *slot = value.into(),
            None => return usage(err, &format!("{flag} needs a value")),
        }
    }
    let command: Vec<String> = rest.collect();
    match command.first().map(String::as_str) {
        Some("github") if command.get(1).map(String::as_str) == Some("setup") => {
            github(&global, command[2..].to_vec(), out, err)
        }
        Some("doctor") => doctor_cmd(&global, command[1..].to_vec(), out, err),
        Some("settings") if command.get(1).map(String::as_str) == Some("bootstrap-set") => {
            settings(&global, command[2..].to_vec(), out, err)
        }
        Some(cmd @ ("pair" | "device" | "pki" | "secrets" | "store")) => usage(
            err,
            &format!("`gantry {cmd}` is not wired yet (owned by the {cmd} module)"),
        ),
        Some(other) => usage(err, &format!("unknown command {other:?}")),
        None => usage(err, "missing command"),
    }
}

fn usage(err: &mut dyn Write, message: &str) -> u8 {
    let _ = writeln!(err, "gantry: {message}\n{USAGE}");
    EXIT_USAGE
}

/// 控制通道路径：`--socket` > 引导配置的 `server.control_socket` > 默认值。
fn socket(global: &Global) -> PathBuf {
    if let Some(path) = &global.socket {
        return path.clone();
    }
    BootConfig::load(&global.config)
        .map(|config| config.server.control_socket)
        .unwrap_or_else(|_| DEFAULT_SOCKET.into())
}

fn github(global: &Global, args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let args = match github_setup::SetupArgs::parse(args) {
        Ok(args) => args,
        Err(error) => {
            let _ = writeln!(err, "gantry: {error}");
            return error.exit_code();
        }
    };
    let control = UnixControlClient::new(socket(global));
    let probe = CurlProbe::default();
    let mut sleep = std::thread::sleep;
    let env = github_setup::Env {
        control: &control,
        probe: &probe,
        out,
        sleep: &mut sleep,
    };
    match github_setup::run(&args, env) {
        Ok(_) => 0,
        Err(error) => {
            let _ = writeln!(err, "gantry: {error}");
            error.exit_code()
        }
    }
}

fn doctor_cmd(global: &Global, args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let args = match doctor::DoctorArgs::parse(args) {
        Ok(args) => args,
        Err(message) => return usage(err, &message),
    };
    // 配置读不到时 `entry = None`，webhook 区域按 fail 报告，而不是整条命令失败。
    let entry = BootConfig::load(&global.config)
        .ok()
        .and_then(|config| Entry::parse(&config.forge.webhook.public_url).ok());
    let probe = CurlProbe::default();
    let ctx = doctor::DoctorContext {
        entry,
        test_mode: install_test_mode(Path::new(INSTALL_STATE)),
        probe: &probe,
    };
    match doctor::run(args.area.as_deref(), &ctx) {
        Ok(report) => {
            let text = if args.json {
                report.to_json().to_string()
            } else {
                report.render_text()
            };
            let _ = writeln!(out, "{text}");
            report.exit_code()
        }
        Err(error) => usage(err, &format!("unknown doctor area {:?}", error.0)),
    }
}

/// `install-state.json` 的 `test_mode`；文件缺失或无此键按 `false`。
fn install_test_mode(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|state| state.get("test_mode").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

fn settings(global: &Global, args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let args = match bootstrap_set::BootstrapSetArgs::parse(args) {
        Ok(args) => args,
        Err(error) => {
            let _ = writeln!(err, "gantry: {error}");
            return EXIT_USAGE;
        }
    };
    let control = UnixControlClient::new(socket(global));
    match bootstrap_set::run(&args, &control) {
        Ok(result) => {
            let _ = writeln!(out, "{result}");
            0
        }
        Err(error) => {
            let _ = writeln!(err, "gantry: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(args: &[&str]) -> (u8, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            args.iter().map(|arg| arg.to_string()).collect(),
            &mut out,
            &mut err,
        );
        (code, String::from_utf8(err).unwrap())
    }

    #[test]
    fn usage_errors_exit_2() {
        for args in [
            &[][..],
            &["frobnicate"],
            &["--config"],
            &["github"],
            &["settings", "bootstrap-set"],
            &["doctor", "nowhere"],
            &["github", "setup"],
        ] {
            assert_eq!(run_args(args).0, 2, "{args:?}");
        }
    }

    #[test]
    fn unwired_module_commands_name_their_owner() {
        let (code, err) = run_args(&["pair"]);
        assert_eq!(code, 2);
        assert!(err.contains("pair module"), "{err}");
    }

    #[test]
    fn test_mode_defaults_to_false() {
        assert!(!install_test_mode(Path::new(
            "/nonexistent/install-state.json"
        )));
    }
}
