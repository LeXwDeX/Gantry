//! Gantry 桌面外壳的 Tauri 层（desktop-shell.md §6、§7.6）。
//!
//! 这里只做三件事：注册 §6.1 白名单命令、配置 WebView 安全（导航、新窗口、单实例）、
//! 把错误整形成 `ShellError`。可信逻辑都在 `gantry-desktop-shell`（状态与编排在 [`Shell`]）；
//! 尚未接上的命令返回 `shell.unavailable` / `shell.not_paired`，不伪造成功。
//!
//! 命令参数一律 `snake_case`（`rename_all`），与 `apps/desktop/src/ipc/tauri.ts` 一致。

use std::collections::BTreeMap;
use std::sync::Arc;

use gantry_desktop_shell::approval;
use gantry_desktop_shell::diag;
use gantry_desktop_shell::events::FrameSink;
use gantry_desktop_shell::http::HttpPairTransport;
use gantry_desktop_shell::ipc::{ShellError, ShellErrorCode};
use gantry_desktop_shell::keys::{platform_key_store, KeyStore, UnavailableKeyStore};
use gantry_desktop_shell::links;
use gantry_desktop_shell::session::{system_clock, Shell};
use gantry_desktop_shell::storage::{AppStore, Prefs};
use serde_json::Value;
use tauri::ipc::Channel;
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, State, Url, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

type CmdResult<T> = Result<T, ShellError>;

// ---- 身份与配对 ----

#[tauri::command(rename_all = "snake_case")]
fn identity_status(shell: State<'_, Arc<Shell>>) -> CmdResult<Value> {
    shell.track("identity_status", shell.identity_status())
}

/// `platform` 由外壳填，不接受 UI 传入（§6.1）。
#[tauri::command(rename_all = "snake_case")]
async fn pairing_begin(
    shell: State<'_, Arc<Shell>>,
    pairing_string: String,
    device_name: String,
) -> CmdResult<Value> {
    // 完整配对串内含一次性 token：进入命令即接管，函数返回时清零（desktop-shell.md §7.6）。
    let pairing_string = zeroize::Zeroizing::new(pairing_string);
    let r = shell.pairing_begin(&pairing_string, &device_name).await;
    shell.track("pairing_begin", r)
}

#[tauri::command(rename_all = "snake_case")]
async fn pairing_confirm(shell: State<'_, Arc<Shell>>, matches: bool) -> CmdResult<Value> {
    let r = shell.pairing_confirm(matches).await;
    shell.track("pairing_confirm", r)
}

/// 先弹原生确认框（WebView 不参与），确认后才删除本机密钥、证书与身份文件。
#[tauri::command(rename_all = "snake_case")]
async fn identity_reset(app: AppHandle, shell: State<'_, Arc<Shell>>) -> CmdResult<Value> {
    let confirmed = confirm(
        &app,
        "重置设备身份",
        "将删除本机的设备密钥、证书与身份文件，并清除各项目的界面状态。\n\n之后需要在服务端重新生成配对串并重新配对。",
        "删除并重置",
    )
    .await?;
    if !confirmed {
        return Err(ShellError::new(ShellErrorCode::UserCancelled, "用户取消"));
    }
    shell.track("identity_reset", shell.identity_reset())
}

#[tauri::command(rename_all = "snake_case")]
fn connection_state(shell: State<'_, Arc<Shell>>) -> CmdResult<Value> {
    Ok(shell.connection_state())
}

// ---- API 与事件流（需要已配对）----

/// `route` 是枚举 ID，`path` 必须与该路由的模板逐段吻合；UI 不能传任何头（§7.1）。
#[tauri::command(rename_all = "snake_case")]
async fn api_request(
    shell: State<'_, Arc<Shell>>,
    route: String,
    path: String,
    query: Option<Value>,
    body: Option<Value>,
    idempotency_key: Option<String>,
) -> CmdResult<Value> {
    let r = shell
        .api_request(
            &route,
            &path,
            query.as_ref(),
            body.as_ref(),
            idempotency_key.as_deref(),
        )
        .await;
    shell.track("api_request", r)
}

/// Tauri `Channel` 作为事件流的帧出口；WebView 关闭通道后 `send` 失败，运行器随之停止。
struct ChannelSink(Channel<Value>);

impl FrameSink for ChannelSink {
    fn send(&self, frame: Value) -> bool {
        self.0.send(frame).is_ok()
    }
}

/// 打开唯一事件流，已打开则替换旧流（§6.1）。
#[tauri::command(rename_all = "snake_case")]
async fn events_open(
    shell: State<'_, Arc<Shell>>,
    on_frame: Channel<Value>,
    after_seq: Option<u64>,
) -> CmdResult<()> {
    let r = shell.events_open(after_seq, Arc::new(ChannelSink(on_frame)));
    shell.track("events_open", r)
}

#[tauri::command(rename_all = "snake_case")]
fn events_ack(shell: State<'_, Arc<Shell>>, seq: u64) -> CmdResult<()> {
    shell.events_ack(seq);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
fn events_close(shell: State<'_, Arc<Shell>>) -> CmdResult<()> {
    shell.events_close();
    Ok(())
}

// ---- 审批签名与受保护写入：只接受句柄，不接受待签内容（S-3）----

#[tauri::command(rename_all = "snake_case")]
async fn approval_prepare(shell: State<'_, Arc<Shell>>, approval_id: String) -> CmdResult<Value> {
    let r = shell.approval_prepare(&approval_id).await;
    shell.track("approval_prepare", r)
}

/// 只接受 `prepared_id`：参数里夹带 `payload` / `signature` 等一律拒绝（S-3、§12）。
#[tauri::command(rename_all = "snake_case")]
async fn approval_sign(
    shell: State<'_, Arc<Shell>>,
    request: tauri::ipc::Request<'_>,
) -> CmdResult<Value> {
    let tauri::ipc::InvokeBody::Json(args) = request.body() else {
        return Err(ShellError::bad_argument("args"));
    };
    approval::exact_args(args, &["prepared_id"])?;
    let prepared_id = args["prepared_id"]
        .as_str()
        .ok_or_else(|| ShellError::bad_argument("prepared_id"))?
        .to_string();
    let r = shell.approval_sign(&prepared_id).await;
    shell.track("approval_sign", r)
}

#[tauri::command(rename_all = "snake_case")]
fn approval_discard(shell: State<'_, Arc<Shell>>, prepared_id: String) -> CmdResult<()> {
    shell.approval_discard(&prepared_id);
    Ok(())
}

/// `kind` 决定哪些参数必带（§7.4.2）；按 `kind` 严格解析，多余参数一律拒绝（shell-core `guarded`）。
#[tauri::command(rename_all = "snake_case")]
async fn guarded_write_prepare(
    shell: State<'_, Arc<Shell>>,
    request: tauri::ipc::Request<'_>,
) -> CmdResult<Value> {
    let tauri::ipc::InvokeBody::Json(args) = request.body() else {
        return Err(ShellError::bad_argument("args"));
    };
    let r = shell.guarded_write_prepare(args).await;
    shell.track("guarded_write_prepare", r)
}

/// 只接受句柄：请求体由外壳持有，UI 无法在 prepare 之后替换（S-3）。
#[tauri::command(rename_all = "snake_case")]
async fn guarded_write_submit(
    shell: State<'_, Arc<Shell>>,
    request: tauri::ipc::Request<'_>,
) -> CmdResult<Value> {
    let tauri::ipc::InvokeBody::Json(args) = request.body() else {
        return Err(ShellError::bad_argument("args"));
    };
    approval::exact_args(args, &["pending_id"])?;
    let pending_id = args["pending_id"]
        .as_str()
        .ok_or_else(|| ShellError::bad_argument("pending_id"))?
        .to_string();
    let r = shell.guarded_write_submit(&pending_id).await;
    shell.track("guarded_write_submit", r)
}

#[tauri::command(rename_all = "snake_case")]
fn guarded_write_discard(shell: State<'_, Arc<Shell>>, pending_id: String) -> CmdResult<()> {
    shell.guarded_write_discard(&pending_id);
    Ok(())
}

// ---- 本机存储（§7.5）----

#[tauri::command(rename_all = "snake_case")]
fn prefs_get(shell: State<'_, Arc<Shell>>) -> CmdResult<Prefs> {
    Ok(shell.store().prefs())
}

#[tauri::command(rename_all = "snake_case")]
fn prefs_set(shell: State<'_, Arc<Shell>>, patch: Value) -> CmdResult<Prefs> {
    shell.track("prefs_set", shell.store().set_prefs(&patch))
}

#[tauri::command(rename_all = "snake_case")]
fn ui_state_get(shell: State<'_, Arc<Shell>>, project_id: String) -> CmdResult<Option<Value>> {
    shell.track("ui_state_get", shell.store().ui_state_get(&project_id))
}

#[tauri::command(rename_all = "snake_case")]
fn ui_state_set(shell: State<'_, Arc<Shell>>, project_id: String, state: Value) -> CmdResult<()> {
    shell.track(
        "ui_state_set",
        shell.store().ui_state_set(&project_id, &state),
    )
}

#[tauri::command(rename_all = "snake_case")]
fn ui_state_list(shell: State<'_, Arc<Shell>>) -> CmdResult<BTreeMap<String, Value>> {
    shell.store().ui_state_list()
}

// ---- 外链与诊断 ----

/// 只放行 `https`、无用户信息；非 `github.com` 主机先弹原生确认框（§6.1）。
/// 确认框会阻塞，所以是异步命令，在阻塞线程里等待用户。
#[tauri::command(rename_all = "snake_case")]
async fn open_external(app: AppHandle, url: String) -> CmdResult<()> {
    let link = links::check_external(&url)?;
    if link.needs_confirm {
        let message = format!(
            "即将在系统浏览器中打开外部网站。\n\n主机：{}\n\n请确认这是你要访问的网站。",
            link.host_display
        );
        if !confirm(&app, "打开外部链接", &message, "打开").await? {
            return Err(ShellError::new(ShellErrorCode::UserCancelled, "用户取消"));
        }
    }
    open_in_browser(&app, &link.url)
}

/// URL 由外壳拼出，永远指向 `github.com`，不需要确认框。
#[tauri::command(rename_all = "snake_case")]
fn open_github(
    app: AppHandle,
    kind: String,
    repo: String,
    number: Option<u64>,
    sha: Option<String>,
) -> CmdResult<()> {
    let url = links::GithubTarget::from_args(&kind, &repo, number, sha.as_deref())?.url()?;
    open_in_browser(&app, &url)
}

/// 外壳自己渲染的原生确认框（不在 WebView 里）。阻塞等待用户，放在阻塞线程里。
async fn confirm(app: &AppHandle, title: &str, message: &str, ok: &str) -> CmdResult<bool> {
    let dialog = app.clone();
    let (title, message, ok) = (title.to_string(), message.to_string(), ok.to_string());
    tauri::async_runtime::spawn_blocking(move || {
        dialog
            .dialog()
            .message(message)
            .title(title)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(ok, "取消".into()))
            .blocking_show()
    })
    .await
    .map_err(|_| ShellError::unavailable("确认框"))
}

fn open_in_browser(app: &AppHandle, url: &str) -> CmdResult<()> {
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|_| ShellError::unavailable("系统浏览器"))
}

#[tauri::command(rename_all = "snake_case")]
fn diagnostics_snapshot(shell: State<'_, Arc<Shell>>) -> CmdResult<Value> {
    Ok(shell.diagnostics_snapshot())
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
        // 两个插件只给外壳的 Rust 代码用；能力文件不授予它们的任何 JS 侧权限（S-5）。
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // 每次 IPC 调用计数 `ipc_calls{command}`（§11 指标）；命令名收敛到白名单，其余记 `other`。
        .invoke_handler({
            let handler: Box<dyn Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync> =
                Box::new(tauri::generate_handler![
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
                ]);
            move |invoke: tauri::ipc::Invoke<tauri::Wry>| {
                if let Some(shell) = invoke.message.webview().try_state::<Arc<Shell>>() {
                    let command = diag::intern(
                        invoke.message.command(),
                        gantry_desktop_shell::ipc::COMMANDS,
                    );
                    shell.diag().count("ipc_calls", &[("command", command)]);
                }
                handler(invoke)
            }
        })
        // main 窗口在 tauri.conf.json 里 `create: false`，这里带上导航限制再创建。
        .setup(|app| {
            // 应用数据目录：identity.json、prefs.json、ui-state/（§7.5），仅当前用户可访问。
            let data_dir = app.path().app_data_dir()?;
            // 硬件密钥库不可用时 fail-closed：配对会失败并给出明确错误，不降级为软件密钥（OQ-S2）。
            let keys: Box<dyn KeyStore> = match platform_key_store(&data_dir.join("keys")) {
                Ok(store) => store,
                Err(e) => Box::new(UnavailableKeyStore(e.to_string())),
            };
            let shell = Arc::new(
                Shell::new(
                    AppStore::open(&data_dir)?,
                    keys,
                    Box::new(HttpPairTransport::default()),
                    system_clock(),
                )
                .with_log_dir(&data_dir.join("logs")),
            );
            // 证书自动续期（P-64）：每 10 分钟检查一次 [建议值]，到剩余 1/3 有效期时后台续期。
            {
                let shell = shell.clone();
                tauri::async_runtime::spawn(async move {
                    let _task = shell.spawn_renewal(std::time::Duration::from_secs(600));
                    std::future::pending::<()>().await;
                });
            }
            app.manage(shell);
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
