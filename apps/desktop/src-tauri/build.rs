// 应用命令的权限由命令白名单生成：每条命令一个 `allow-<命令>`，
// capabilities/main.json 只授予这些（desktop-shell.md §6.1、S-5）。
fn main() {
    let manifest = tauri_build::AppManifest::new().commands(gantry_desktop_shell::ipc::COMMANDS);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("tauri-build 失败");
}
