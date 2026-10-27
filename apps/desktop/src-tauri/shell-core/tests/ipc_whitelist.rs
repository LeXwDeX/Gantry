//! IPC 白名单与 WebView 安全配置的静态一致性（desktop-shell.md §6.1、§7.6、§8 S-5 / S-6）。
//!
//! Tauri 应用不在根 workspace 里，CI 编译不到它；这里直接读它的配置与源码，
//! 保证"命令注册表 = 能力文件 = `ipc::COMMANDS`"，且 CSP、全局对象、原型冻结符合规格。
//! 运行时验证（WebView 内调用白名单外命令、`fetch` 外链被拦）需要真实窗口，归 B 级手测。

use std::collections::BTreeSet;
use std::path::PathBuf;

use gantry_desktop_shell::ipc::COMMANDS;
use serde_json::Value;

const SPEC_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src ipc: http://ipc.localhost; object-src 'none'; base-uri 'none'; frame-src 'none'; form-action 'none'";

fn app_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read_json(rel: &str) -> Value {
    let text = std::fs::read_to_string(app_dir().join(rel)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn commands() -> BTreeSet<String> {
    COMMANDS.iter().map(|c| c.to_string()).collect()
}

#[test]
fn capability_grants_exactly_the_whitelist() {
    let cap = read_json("capabilities/main.json");
    assert_eq!(cap["identifier"], "main");
    assert_eq!(cap["windows"], serde_json::json!(["main"]));
    assert_eq!(cap["local"], true);
    // 不给任何远程 URL IPC 能力（§7.6"能力"）。
    assert!(cap.get("remote").is_none());

    let granted: Vec<String> = cap["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            p.as_str()
                .expect("权限必须是简单标识符，不能带作用域")
                .to_string()
        })
        .collect();
    // 只有应用命令的 allow-*：没有 core:*、没有任何插件权限（fs、shell、http、opener…）。
    for p in &granted {
        assert!(
            p.starts_with("allow-") && !p.contains(':'),
            "能力文件里出现白名单外权限 {p}"
        );
    }
    let granted: BTreeSet<String> = granted
        .iter()
        .map(|p| p.trim_start_matches("allow-").replace('-', "_"))
        .collect();
    assert_eq!(granted, commands());
}

#[test]
fn handler_registers_exactly_the_whitelist() {
    let src = std::fs::read_to_string(app_dir().join("src/lib.rs")).unwrap();
    let start = src
        .find("generate_handler![")
        .expect("找不到 generate_handler!")
        + "generate_handler![".len();
    let end = start + src[start..].find(']').unwrap();
    let registered: BTreeSet<String> = src[start..end]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(registered, commands());

    // 每个命令都按 snake_case 接参数（与 src/ipc/tauri.ts 一致），没有漏掉 rename_all 的命令。
    assert!(
        !src.contains("#[tauri::command]"),
        "存在未声明 rename_all 的命令"
    );
    assert_eq!(
        src.matches("#[tauri::command(rename_all = \"snake_case\")]")
            .count(),
        COMMANDS.len()
    );
}

#[test]
fn build_script_derives_permissions_from_whitelist() {
    let build = std::fs::read_to_string(app_dir().join("build.rs")).unwrap();
    assert!(build.contains("gantry_desktop_shell::ipc::COMMANDS"));
}

#[test]
fn webview_security_config_matches_spec() {
    let conf = read_json("tauri.conf.json");
    let app = &conf["app"];
    assert_eq!(app["withGlobalTauri"], false);
    assert_eq!(app["security"]["csp"], SPEC_CSP);
    assert_eq!(app["security"]["freezePrototype"], true);
    assert_eq!(app["security"]["capabilities"], serde_json::json!(["main"]));
    assert!(app["security"]
        .get("dangerousDisableAssetCspModification")
        .is_none());
    assert!(app["security"].get("devCsp").is_none());

    // 单窗口 main，由外壳在 setup 里带导航限制创建。
    let windows = app["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["label"], "main");
    assert_eq!(windows[0]["create"], false);

    // 只加载打包资源：没有 devUrl 指向远程页面，没有插件配置。
    assert!(conf["build"].get("devUrl").is_none());
    assert!(conf.get("plugins").is_none());

    // CSP 不放任何网络来源（S-6）。
    let csp = SPEC_CSP;
    for forbidden in [
        "https:",
        "http://localhost",
        "'unsafe-inline'",
        "'unsafe-eval'",
        "*",
    ] {
        assert!(!csp.contains(forbidden), "CSP 含 {forbidden}");
    }
}
