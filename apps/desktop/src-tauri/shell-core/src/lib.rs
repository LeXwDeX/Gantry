//! Gantry 桌面外壳的可信逻辑（[desktop-shell.md](../../../../../docs/modules/desktop-shell.md)）。
//!
//! 本 crate 不依赖 Tauri，所以能在 CI 的 workspace 检查里编译与测试；Tauri 应用
//! （`apps/desktop/src-tauri`）只负责把这里的能力注册成白名单 IPC 命令。

pub mod approval;
pub mod ipc;
pub mod keys;
pub mod links;
pub mod pairing;
pub mod routes;
pub mod storage;
pub mod tls;
