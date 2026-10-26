//! 图模板的已定义部分（engine.md §7.4）。
//!
//! 只覆盖 `low.single`：Low 的实现阶段为单节点 `single`；spec / plan / review
//! 没有 agent 图。Medium 及以上模板留给 M2，不在 M1 伪造。

use gantry_core::entities::{Role, Stage};
use gantry_core::level::Level;
use gantry_core::worker::PatchMode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSpec {
    pub key: String,
    pub role: Role,
    pub depends_on: Vec<String>,
    pub upstream: Vec<String>,
    pub required: bool,
    pub critical: bool,
    pub patch: PatchMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerdictRule {
    None,
    Single(String),
    Fold(Vec<String>),
    Arbiter(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageGraph {
    pub template: &'static str,
    pub nodes: Vec<NodeSpec>,
    pub verdict: VerdictRule,
}

/// 按（档位，阶段）返回已定义的图；没有 agent 图或尚未实现的档位返回 `None`。
pub fn stage_graph(level: Level, stage: Stage) -> Option<StageGraph> {
    match (level, stage) {
        (Level::Low, Stage::Implement) => Some(StageGraph {
            template: "low.implement",
            nodes: vec![NodeSpec {
                key: "single".to_owned(),
                role: Role::Single,
                depends_on: Vec::new(),
                upstream: Vec::new(),
                required: true,
                critical: false,
                patch: PatchMode::Optional,
            }],
            verdict: VerdictRule::None,
        }),
        _ => None,
    }
}
