//! 编排引擎（`docs/modules/engine.md`）。
//!
//! M1 只落地已定义的模板部分。`GraphTemplate` / `TemplateCx` / `ExitRule`
//! 在规格中仍有未定义字段，本 crate 不伪造这些接口；后续契约确定后再补。

pub mod template;

pub use template::{stage_graph, NodeSpec, StageGraph, VerdictRule};
