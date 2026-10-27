//! 模型路由（`models`）：服务端进程内所有模型调用的唯一出口。
//!
//! 规格见 `docs/modules/models.md`。M1 交付：
//!
//! - [`view`]：`ModelsView` 与 `resolve`（打杂 / 大脑 / 干活三类配置的解析）；
//! - [`settings`]：从 `store` 读生效设置，按修订热加载；
//! - [`prompt`]：可信指令与不可信材料的隔离包装；
//! - [`output`]：直接角色、schema 归属与输出校验；
//! - [`anthropic`]：`AnthropicProvider`（只连 Egress 回环反向代理，见 [`http`]）；
//! - [`router`]：`ModelRouter::invoke_direct`（重试、退避、期限、并发闸、令牌轮换）；
//! - [`accounting`]：按 Egress 终结证明写 `budget.charged`，以及按令牌对账。
//!
//! 硬约束：本 crate 不依赖 `gantry-secrets`，不持有任何提供方 key（P-16）。

#![forbid(unsafe_code)]

pub mod accounting;
pub mod anthropic;
pub mod error;
pub mod http;
pub mod output;
pub mod prompt;
pub mod router;
pub mod settings;
pub mod view;

pub use anthropic::{AnthropicProvider, EffortTable};
pub use error::ModelsError;
pub use router::{
    Attribution, DirectCall, DirectOutcome, ModelRouter, RouterClock, RouterDeps, TokioClock,
    UsageWarning, PROVIDER_MAX_INFLIGHT,
};
pub use settings::{build_view, EffectiveDocs, SettingsSource, StoreSettings, ViewCache};
pub use view::{ModelsView, Purpose, ResolveQuery, ResolvedModel, SandboxExec, MAX_FALLBACK};
