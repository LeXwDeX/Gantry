//! `worker`：Worker 协议在沙盒内的执行者，以及服务端侧的 `WorkerAdapter` 声明。
//!
//! 规格见 `docs/modules/worker.md`，协议见 `docs/interfaces/worker-protocol.md`。
//! 本 crate 只依赖 `gantry-core` 与少量通用库（worker.md §3）。

pub mod artifacts;
pub mod claude_cli;
pub mod claude_code;
pub mod events;
pub mod layout;
pub mod patch;
pub mod prompt;
pub mod proposals;
pub mod startup;
pub mod stream;

pub use claude_code::ClaudeCodeAdapter;
