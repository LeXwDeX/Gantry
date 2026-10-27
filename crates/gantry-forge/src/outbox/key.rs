//! 每个动作 `kind` 的 `target` / `intent` 推导：实现已移到 core（#70），forge 与引擎共用。

pub use gantry_core::forge::{derive_target_intent, KeyError, TargetIntent};
