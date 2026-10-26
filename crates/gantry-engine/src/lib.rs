//! 编排引擎（`docs/modules/engine.md`）。
//!
//! M1 目前落地已定义的模板与事件投影；`GraphTemplate` / `TemplateCx` /
//! `ExitRule` 及完整决策器仍未实现。

pub mod budget;
pub mod projection;
pub mod template;

pub use budget::{charge, check, ledger_for_delivery, remaining};
pub use projection::{
    AttemptProjection, DeliveryProjection, NodeProjection, Projection, ProjectionError,
    WorkflowProjection,
};
pub use template::{stage_graph, NodeSpec, StageGraph, VerdictRule};
