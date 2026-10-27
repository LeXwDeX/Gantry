//! 模型路由（`models`）：服务端进程内所有模型调用的唯一出口。
//!
//! 规格见 `docs/modules/models.md`。M1 交付：
//!
//! - [`view`]：`ModelsView` 与 `resolve`（打杂 / 大脑 / 干活三类配置的解析）；
//! - [`settings`]：从 `store` 读生效设置，按修订热加载。
//!
//! 硬约束：本 crate 不依赖 `gantry-secrets`，不持有任何提供方 key（P-16）。

#![forbid(unsafe_code)]

pub mod error;
pub mod settings;
pub mod view;

pub use error::ModelsError;
pub use settings::{build_view, EffectiveDocs, SettingsSource, StoreSettings, ViewCache};
pub use view::{ModelsView, Purpose, ResolveQuery, ResolvedModel, SandboxExec, MAX_FALLBACK};
