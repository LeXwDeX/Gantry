//! 编排引擎（`docs/modules/engine.md`）。
//!
//! M1 目前落地：`low.single` 模板、`DeliveryState` 折叠与 §7.14.2 路由、§7.14 决策器类型、
//! 预算台账与控制面写入、Low 正常路径的 `decide`（#70）、执行体（#71）、AttemptRunner 与
//! 端口副作用（#72）、运行时入口（#277）、控制命令与异常路径的决策（`control`，#73）。

pub mod actor;
pub mod attempt;
pub mod budget;
pub mod classify;
pub mod control;
pub mod create;
pub mod decide;
pub mod decider;
pub mod effects;
pub mod engine;
pub mod event_router;
pub mod outbox;
pub mod route;
pub mod runtime;
pub mod stage;
pub mod state;
pub mod template;

pub use attempt::{
    AttemptContext, AttemptPorts, AttemptRunner, ResultCas, RunnerConfig, RunnerError,
};
pub use budget::{charge, check, ledger_for_delivery, remaining};
pub use create::{budget_from_level, created_event, NewDelivery};
pub use decider::{decide, expected_actions, ActionIntent};
pub use effects::{Deliver, PortEffects};
pub use engine::{Engine, EngineError};
pub use outbox::{build as build_outbox, OutboxIntent};
pub use route::{Route, RouteError, Router};
pub use stage::{advance, StageTransition};
pub use state::{retry_not_before, DeliveryState, StateError};
pub use template::{
    stage_graph, ExitRule, HeadBinding, NodeSpec, RevisionCx, StageGraph, TemplateCx, VerdictRule,
};
