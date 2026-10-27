#![forbid(unsafe_code)]

//! `forge`：Gantry 与 GitHub 之间唯一的通道（`docs/modules/forge.md`）。
//!
//! 本 crate 按 issue #92 分片交付。当前切片只有 Outbox 入队面：
//! [`Forge`] 实现 `core::ports::OutboxPort`（入队、取消），按 forge-actions §3.2
//! 推导 `target` / `intent`，做入队时的机械校验（§7.8），再交给 `store` 的
//! `OutboxTx::enqueue` 落行。执行器、GitHub 客户端、webhook、归一化与对账在后续切片。
//!
//! 已知限制（M1 切片内）：
//! - 只接受 M1 的 5 个动作（forge.md §1.1）；其余 9 个动作在 M2 起开放，入队时拒绝。
//! - Project 绑定（仓库、安装、已获批权限）由装配方经 [`ForgeConfig`] 注入；
//!   项目绑定事件（`project.added`）的来源见 forge.md F-OQ-12，尚未定稿。

mod config;
mod outbox;

pub use config::{ForgeConfig, Permission, ProjectBinding, DEFAULT_BRANCH_PREFIX};
pub use outbox::key::{derive_target_intent, KeyError, TargetIntent};
pub use outbox::{Forge, M1_ACTIONS};
