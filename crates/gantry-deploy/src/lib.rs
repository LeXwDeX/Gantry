#![forbid(unsafe_code)]

//! `deploy` 的 Rust 子命令（deploy.md §6.1；OPS-07）。
//!
//! 本 crate 只提供库函数，`gantry` CLI 二进制（#101）解析到 `github setup` / `doctor`
//! / `settings bootstrap-set` 后分派到这里。与运行中的 `gantryd` 只经本机控制通道 [`control`] 通信。
//!
//! - [`github_setup`]：`gantry github setup`（§7.5）；
//! - [`doctor`]：`gantry doctor` 的报告框架与 M1 子集（§7.14）；
//! - [`manifest`] / [`page`]：App Manifest 与浏览器侧提交页；
//! - [`probe`]：经公网入口的 HTTP 探测（§7.6）；
//! - [`bootstrap_set`]：`gantry settings bootstrap-set`（§7.9，P-66）的客户端侧。

pub mod bootstrap_set;
pub mod control;
pub mod doctor;
pub mod github_setup;
pub mod manifest;
pub mod page;
pub mod probe;
