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
    pub exit: ExitRule,
}

/// 实例化一张阶段图的上下文（engine.md §7.14.7）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateCx {
    /// 实例化时的值；`Workflow.template` 按它取（§7.4）。
    pub level_effective: Level,
    /// 视角清单（§10 常量，OQ-5）。
    pub perspectives: Vec<String>,
    /// 修正形态（§7.5）。
    pub revision: Option<RevisionCx>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionCx {
    pub round: u32,
    /// 修正 build 的上游：本轮判定节点。
    pub feedback_from: Vec<String>,
}

/// 阶段图的"图内证据"退出条件；CI、PR 状态等图外守卫在 `decide` 里查（§7.14.7）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitRule {
    /// plan：全部 required 节点 succeeded，且全部非 required 节点已进入终态（T5）。
    RequiredSucceeded,
    /// Low implement：该节点 succeeded 且补丁已提交（T6）。
    PatchCommitted { node: String },
    /// 阶段结论 ACCEPT，且结论所基于的 head 满足绑定。
    VerdictAccept { head: HeadBinding },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadBinding {
    /// spec 的 admission（T3）。
    None,
    /// implement 的 verify：基于 `Evidence.candidate_head`（T6）。
    Candidate,
    /// review：`Evidence.judged_head` = 快照 head_sha（T10 的图内部分）。
    PrHead,
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
            exit: ExitRule::PatchCommitted {
                node: "single".to_owned(),
            },
        }),
        _ => None,
    }
}
