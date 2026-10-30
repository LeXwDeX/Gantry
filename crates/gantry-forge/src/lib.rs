#![forbid(unsafe_code)]

//! `forge`：Gantry 与 GitHub 之间唯一的通道（`docs/modules/forge.md`）。
//!
//! 本 crate 按 issue #92 分片交付。已有：
//! - Outbox 入队面：[`Forge`] 实现 `core::ports::OutboxPort`（入队、取消），按
//!   forge-actions §3.2 推导 `target` / `intent`，做入队时的机械校验（§7.8）。
//! - Outbox 执行器：[`Executor`] 按 FA §2.3"先查后做"执行 M1 的 5 个动作（`push_branch`
//!   走 git 传输），执行前经 `PolicyPort` 消费审批（P-54），临时错误留在 `queued` 退避
//!   （P-53）。GitHub 客户端是 crate 私有的（I2）。
//! - 镜像仓拉取：[`Executor::sync_mirror`] 与 [`Reader`] 共用同一受保护 fetch（forge.md §7.10）。
//!
//! - App 鉴权：[`AppTokenProvider`]（App JWT、安装令牌缓存与 single-flight 刷新、权限自检，
//!   forge.md §7.12）。
//!
//! - 读取：[`Reader`] 实现 `ForgeReader` 的本地快照（读 store）、`discussion`（P-59）与
//!   `sync_mirror`（与执行器共用受保护 fetch）。
//!
//! - webhook 接收：[`WebhookReceiver`]（回环监听、HMAC 双 secret、`delivery_id` 去重、先落盘再 2xx，
//!   forge.md §7.4）。
//!
//! - 归一化与自身事件归因：[`Dispatcher`]（回读 → 快照差异 → `forge.<kind>` + `forge.synced`，
//!   `forge_effect_index` 归因与绑定，forge.md §7.5–§7.6）。
//!
//! - 对账：[`Dispatcher::reconcile`]（`source = reconcile` 的同一套差异与归因，`drift(head_diverged)`，
//!   forge.md §7.7）。
//! - App 创建回调：[`setup::SetupService`]（`/setup/github/callback`，forge.md §7.14）。
//! - 二级限流惩罚期按安装持久化在 `forge_rate_state`（执行器领取时检查）。
//!
//! 已知限制（M1 切片内）：
//! - 只接受 M1 的 5 个动作（forge.md §1.1）；其余 9 个动作在 M2 起开放，入队时拒绝。
//! - Project 绑定（仓库、安装、已获批权限）由装配方经 [`ForgeConfig`] 注入；
//!   项目绑定事件（`project.added`）的来源见 forge.md F-OQ-12，尚未定稿。

mod auth;
mod config;
mod effects;
mod git;
mod github;
mod mirror;
pub mod normalize;
mod outbox;
mod reader;
pub mod setup;
pub mod webhook;

pub use auth::app::{AppAuthDeps, AppAuthOptions, AppTokenProvider, PermissionReport};
pub use auth::{AuthError, StaticTokenProvider, TokenProvider, TokenPurpose};
pub use config::{ForgeConfig, Permission, ProjectBinding, DEFAULT_BRANCH_PREFIX};
pub use normalize::{DispatchOutcome, Dispatcher, DispatcherDeps, DispatcherOptions};
pub use outbox::executor::{
    CrashPoint, ExecError, Executor, ExecutorDeps, ExecutorOptions, FaultInjector, Step,
    StepOutcome,
};
pub use outbox::key::{derive_target_intent, KeyError, TargetIntent};
pub use outbox::{Forge, M1_ACTIONS};
pub use reader::{MirrorDeps, Reader, ReaderDeps};
pub use webhook::{
    InboxRow, InboxStatus, WebhookDeps, WebhookError, WebhookOptions, WebhookReceiver, WebhookStats,
};

/// `forge` 的模块私有表（store.md §7.10）；装配方交给 `Store::open*`。
pub fn module_spec() -> gantry_store::ModuleSpec {
    gantry_store::ModuleSpec::new(
        webhook::MODULE,
        &[
            webhook::INBOX_TABLE,
            effects::EFFECT_TABLE,
            normalize::snapshot::SNAPSHOT_TABLE,
            outbox::executor::RATE_TABLE,
        ],
    )
}
