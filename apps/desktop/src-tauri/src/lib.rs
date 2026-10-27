//! Gantry 桌面外壳的 Tauri 层（desktop-shell.md §6、§7.6）。
//!
//! 这里只做三件事：注册 §6.1 白名单命令、配置 WebView 安全（导航、新窗口、单实例）、
//! 把错误整形成 `ShellError`。可信逻辑都在 `gantry-desktop-shell`；本骨架阶段尚未接上的
//! 命令返回 `shell.unavailable` / `shell.not_paired`，不伪造成功。
//!
//! 命令参数一律 `snake_case`（`rename_all`），与 `apps/desktop/src/ipc/tauri.ts` 一致。

use std::collections::BTreeMap;

use gantry_desktop_shell::ipc::ShellError;
use gantry_desktop_shell::storage::{AppStore, Prefs};
use serde::Serialize;
use serde_json::Value;
use tauri::ipc::Channel;
use tauri::webview::NewWindowResponse;
use tauri::{Manager, State, Url, WebviewWindowBuilder};

type CmdResult<T> = Result<T, ShellError>;

#[derive(Serialize)]
struct IdentityStatus {
    state: &'static str,
    server_host: Option<String>,
    ca_fingerprint: Option<String>,
    device: Option<Value>,
    cert: Option<Value>,
    key_backend: Option<&'static str>,
}

#[derive(Serialize)]
struct ConnectionState {
    phase: &'static str,
    epoch: u64,
    last_bytes_age_ms: Option<u64>,
    cert_not_after: Option<String>,
    cert_expiring: bool,
    log_epoch: Option<u64>,
    clock_skew_ms: Option<i64>,
}

fn unpaired_status() -> IdentityStatus {
    IdentityStatus {
        state: "unpaired",
        server_host: None,
        ca_fingerprint: None,
        device: None,
        cert: None,
        key_backend: None,
    }
}

// ---- 身份与配对 ----

#[tauri::command(rename_all = "snake_case")]
fn identity_status() -> CmdResult<IdentityStatus> {
    Ok(unpaired_status())
}

#[tauri::command(rename_all = "snake_case")]
fn pairing_begin(pairing_string: String, device_name: String) -> CmdResult<Value> {
    let _ = (pairing_string, device_name);
    Err(ShellError::unavailable("pairing_begin"))
}

#[tauri::command(rename_all = "snake_case")]
fn pairing_confirm(matches: bool) -> CmdResult<IdentityStatus> {
    let _ = matches;
    Err(ShellError::unavailable("pairing_confirm"))
}

#[tauri::command(rename_all = "snake_case")]
fn identity_reset() -> CmdResult<IdentityStatus> {
    Err(ShellError::unavailable("identity_reset"))
}

#[tauri::command(rename_all = "snake_case")]
fn connection_state() -> CmdResult<ConnectionState> {
    Ok(ConnectionState {
        phase: "unpaired",
        epoch: 0,
        last_bytes_age_ms: None,
        cert_not_after: None,
        cert_expiring: false,
        log_epoch: None,
        clock_skew_ms: None,
    })
}

// ---- API 与事件流（需要已配对）----

#[tauri::command(rename_all = "snake_case")]
fn api_request(
    route: String,
    path: Option<Value>,
    query: Option<Value>,
    body: Option<Value>,
    idempotency_key: Option<String>,
) -> CmdResult<Value> {
    let _ = (route, path, query, body, idempotency_key);
    Err(ShellError::not_paired())
}

#[tauri::command(rename_all = "snake_case")]
fn events_open(on_frame: Channel<Value>, after_seq: Option<u64>) -> CmdResult<()> {
    let _ = (on_frame, after_seq);
    Err(ShellError::not_paired())
}

#[tauri::command(rename_all = "snake_case")]
fn events_ack(seq: u64) -> CmdResult<()> {
    let _ = seq;
    Err(ShellError::not_paired())
}

#[tauri::command(rename_all = "snake_case")]
fn events_close() -> CmdResult<()> {
    Ok(())
}

// ---- 审批签名与受保护写入：只接受句柄，不接受待签内容（S-3）----

#[tauri::command(rename_all = "snake_case")]
fn approval_prepare(approval_id: String) -> CmdResult<Value> {
    let _ = approval_id;
    Err(ShellError::not_paired())
}

#[tauri::command(rename_all = "snake_case")]
fn approval_sign(prepared_id: String) -> CmdResult<Value> {
    let _ = prepared_id;
    Err(ShellError::not_paired())
}

#[tauri::command(rename_all = "snake_case")]
fn approval_discard(prepared_id: String) -> CmdResult<()> {
    let _ = prepared_id;
    Ok(())
}

/// `kind` 决定哪些参数必带（§7.4.2）；严格校验放在 shell-core，骨架阶段尚未接上。
#[allow(clippy::too_many_arguments)]
#[tauri::command(rename_all = "snake_case")]
fn guarded_write_prepare(
    kind: String,
    scope: Option<String>,
    project_id: Option<String>,
    resource: Option<String>,
    value: Option<Value>,
    if_match: Option<String>,
    device_id: Option<String>,
    role: Option<String>,
) -> CmdResult<Value> {
    let _ = (
        kind, scope, project_id, resource, value, if_match, device_id, role,
    );
    Err(ShellError::not_paired())
}

#[tauri::command(rename_all = "snake_case")]
fn guarded_write_submit(pending_id: String) -> CmdResult<Value> {
    let _ = pending_id;
    Err(ShellError::not_paired())
}

#[tauri::command(rename_all = "snake_case")]
fn guarded_write_discard(pending_id: String) -> CmdResult<()> {
    let _ = pending_id;
    Ok(())
}

// ---- 本机存储（§7.5）----

#[tauri::command(rename_all = "snake_case")]
fn prefs_get(store: State<'_, AppStore>) -> CmdResult<Prefs> {
    Ok(store.prefs())
}

#[tauri::command(rename_all = "snake_case")]
fn prefs_set(store: State<'_, AppStore>, patch: Value) -> CmdResult<Prefs> {
    store.set_prefs(&patch)
}

#[tauri::command(rename_all = "snake_case")]
fn ui_state_get(store: State<'_, AppStore>, project_id: String) -> CmdResult<Option<Value>> {
    store.ui_state_get(&project_id)
}

#[tauri::command(rename_all = "snake_case")]
fn ui_state_set(store: State<'_, AppStore>, project_id: String, state: Value) -> CmdResult<()> {
    store.ui_state_set(&project_id, &state)
}

#[tauri::command(rename_all = "snake_case")]
fn ui_state_list(store: State<'_, AppStore>) -> CmdResult<BTreeMap<String, Value>> {
    store.ui_state_list()
}

// ---- 外链与诊断 ----

#[tauri::command(rename_all = "snake_case")]
fn open_external(url: String) -> CmdResult<()> {
    let _ = url;
    Err(ShellError::unavailable("open_external"))
}

#[tauri::command(rename_all = "snake_case")]
fn open_github(
    kind: String,
    repo: String,
    number: Option<u64>,
    sha: Option<String>,
) -> CmdResult<()> {
    let _ = (kind, repo, number, sha);
    Err(ShellError::unavailable("open_github"))
}

#[tauri::command(rename_all = "snake_case")]
fn diagnostics_snapshot() -> CmdResult<Value> {
    Err(ShellError::unavailable("diagnostics_snapshot"))
}

/// WebView 只能停留在打包进应用的来源（§7.6"导航"）。
fn is_app_origin(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        _ => false,
    }
}

pub fn run() {
    tauri::Builder::default()
        // 单实例锁：避免两个实例争用身份文件与事件流（§7.6"其他"）。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .invoke_handler(tauri::generate_handler![
            identity_status,
            pairing_begin,
            pairing_confirm,
            identity_reset,
            connection_state,
            api_request,
            events_open,
            events_ack,
            events_close,
            approval_prepare,
            approval_sign,
            approval_discard,
            guarded_write_prepare,
            guarded_write_submit,
            guarded_write_discard,
            prefs_get,
            prefs_set,
            ui_state_get,
            ui_state_set,
            ui_state_list,
            open_external,
            open_github,
            diagnostics_snapshot,
        ])
        // main 窗口在 tauri.conf.json 里 `create: false`，这里带上导航限制再创建。
        .setup(|app| {
            // 应用数据目录：identity.json、prefs.json、ui-state/（§7.5），仅当前用户可访问。
            let data_dir = app.path().app_data_dir()?;
            app.manage(AppStore::open(&data_dir)?);
            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .cloned()
                .expect("tauri.conf.json 缺少 main 窗口");
            WebviewWindowBuilder::from_config(app.handle(), &config)?
                .on_navigation(is_app_origin)
                .on_new_window(|_url, _features| NewWindowResponse::Deny)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Gantry 桌面外壳启动失败");
}
