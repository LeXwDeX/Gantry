# 桌面外壳（`src-tauri`）

[desktop-shell.md](../../../docs/modules/desktop-shell.md) 的实现，分成两部分：

| 目录 | 内容 | 谁来检查 |
| --- | --- | --- |
| `shell-core/`（crate `gantry-desktop-shell`） | 可信逻辑：设备双密钥、命令白名单与错误码、本机存储，以后的配对、mTLS、事件流、审批签名也放这里。不依赖 Tauri | 根 workspace 成员，CI 的 fmt / clippy / test / MSRV 都覆盖 |
| 本目录（crate `gantry-desktop`） | Tauri 2 应用：注册 §6.1 的白名单命令，配置 CSP、导航限制、单实例锁；dialog / opener 插件只给 Rust 侧用，不授予 JS 权限 | 有独立的 `Cargo.lock` 与 `target/`，不在根 workspace 里（Linux 上需要系统 WebView 开发包）；本机构建 |
| `probe/` | Secure Enclave 探针与手动 Touch ID 步骤 | 见 [probe/README.md](probe/README.md) |

## 构建与运行

```bash
npm run build --prefix apps/desktop      # 先产出 ../dist（正式前端，不含 scenario 适配器）
cargo build --manifest-path apps/desktop/src-tauri/Cargo.toml
./apps/desktop/src-tauri/target/debug/gantry-desktop
```

## 白名单从哪来

- `shell-core/src/ipc.rs` 的 `COMMANDS` 是唯一来源。`build.rs` 用它生成每条命令的 `allow-<命令>` 权限；`capabilities/main.json` 只授予这些权限，不授予 `core:*`，也不授予任何插件的 JS 侧权限。
- `shell-core/tests/ipc_whitelist.rs` 检查四者逐项一致：`COMMANDS`、`generate_handler!` 注册表、能力文件、`build.rs`。它还检查 CSP 与规格 §7.6 逐字相同，以及 `withGlobalTauri = false`、`freezePrototype = true`、只有一个 `main` 窗口、没有 `devUrl`。
- 命令参数一律是 `snake_case`（`rename_all`），与 `apps/desktop/src/ipc/tauri.ts` 一致。

## 当前进度（#102）

- 命令都已注册。状态与编排集中在 `shell-core::session::Shell`（Tauri 只转发参数，原生确认框在 Tauri 层弹）。已有实际行为的命令：
  - `identity_status`、`pairing_begin` / `pairing_confirm`（单向 TLS 固定根 → 待确认身份 → 写 `identity.json` 并用 mTLS 取 `/v1/me`）、`identity_reset`（先弹原生确认框）；
  - `api_request`（路由表校验 → mTLS 发送与重试 → 响应整形；`X-Log-Epoch` 变化时丢弃待签内容）；
  - `events_open` / `events_ack` / `events_close` / `connection_state`（唯一事件流：游标、退避重连、45 s 空闲、`close` 帧、`409 cursor_ahead`、`log_epoch` 重同步、连续 3 次 TLS 失败 → `handshake_failed`、吊销即删密钥）；
  - `approval_prepare` / `approval_sign` / `approval_discard`（时钟偏差校正、非 approver 不取 challenge、展示前自检、连接 live 才签、重检后在阻塞线程里等用户在场、`grant`；`approval_sign` 严格只收 `prepared_id`）；
  - `guarded_write_prepare` / `guarded_write_submit` / `guarded_write_discard`（settings 先 dry_run、设备动作本地组装 signing；核对内部摘要后发起审批 → 签名 → 带 `Gantry-Approval` 重发；请求体用 `Zeroizing` 保存，提交或丢弃后清零）；
  - `prefs_*`、`ui_state_*`、`open_external`、`open_github`。
  - 其余命令（诊断）返回 `shell.not_paired` 或 `shell.unavailable`，不伪造成功。
- 硬件密钥库不可用时用 `UnavailableKeyStore`：配对失败并给出 `shell.key_unavailable`，不降级为软件密钥。
- 导航只放行应用自己的来源（`tauri://localhost`、`http(s)://tauri.localhost`），新窗口一律拒绝。

## 运行时验证（需要真实窗口，归 B 级手测，尚未执行）

在正式构建的窗口里打开 Web Inspector（仅调试版有），逐项确认会失败：

1. 调用白名单外命令，比如 `__TAURI_INTERNALS__.invoke('plugin:opener|open_url', {url: 'https://example.com'})`；
2. `fetch('https://example.com')`、`new WebSocket('wss://example.com')`；
3. 插入 `<iframe src="https://example.com">`、外链 `<script src>`；
4. `location.href = 'https://example.com'`、`window.open('https://example.com')`。
