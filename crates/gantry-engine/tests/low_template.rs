//! `low.single` 模板的已定义形状（engine.md §7.4）。

use gantry_core::entities::{Role, Stage};
use gantry_core::level::Level;
use gantry_core::worker::PatchMode;
use gantry_engine::template::{stage_graph, VerdictRule};

#[test]
fn low_implement_is_a_single_optional_patch_node() {
    let graph = stage_graph(Level::Low, Stage::Implement).expect("low.implement must exist");
    assert_eq!(graph.template, "low.implement");
    assert_eq!(graph.verdict, VerdictRule::None);
    assert_eq!(graph.nodes.len(), 1);

    let node = &graph.nodes[0];
    assert_eq!(node.key, "single");
    assert_eq!(node.role, Role::Single);
    assert!(node.depends_on.is_empty());
    assert!(node.upstream.is_empty());
    assert!(node.required);
    assert!(!node.critical);
    assert_eq!(node.patch, PatchMode::Optional);
}

#[test]
fn low_stages_without_agent_graphs_return_none() {
    for stage in [
        Stage::Spec,
        Stage::Plan,
        Stage::Propose,
        Stage::Review,
        Stage::Merge,
        Stage::Closed,
        Stage::Abandoned,
    ] {
        assert_eq!(stage_graph(Level::Low, stage), None, "{stage:?}");
    }
}

#[test]
fn higher_levels_are_not_fabricated_in_m1() {
    for level in [
        Level::None,
        Level::Medium,
        Level::High,
        Level::Max,
        Level::Ultimate,
    ] {
        assert_eq!(stage_graph(level, Stage::Implement), None, "{level:?}");
    }
}
