//! 路径规则的唯一实现（policy.md §6.2、§7.2）。
//!
//! `guardian` 的 `protected` 判定、`engine` 的加码规则 `when.paths` 与本模块的
//! `evaluate_patch` 共用 [`RulePaths`]，避免多处匹配语义漂移。这里报告的是
//! **配置上的**处置；M1 把 `approve` 规则按 deny 执行是 `evaluate_patch` 的事。

use std::sync::Arc;

use gantry_core::ports::{Disposition, PathRuleHit, PathRuleSet, PathRulesSnapshot};
use gantry_core::worker::RepoPath;

use crate::rules::{PolicyBuild, PolicyRules};

/// 某一作用域生效规则上的路径求值器。
#[derive(Clone, Debug)]
pub struct RulePaths {
    rules: Arc<PolicyRules>,
}

impl RulePaths {
    pub fn new(rules: Arc<PolicyRules>) -> Self {
        Self { rules }
    }

    /// 单条路径求值：命中规则中处置最严的一条；同严格度取规则 id 最小者。
    pub fn evaluate_one(&self, path: &RepoPath) -> PathRuleHit {
        let mut best: Option<(&crate::rules::PathRule, Disposition)> = None;
        for rule in &self.rules.paths {
            if !rule.globs.matches(path.as_str()) {
                continue;
            }
            // `paths` 已按 id 排序，严格更高才替换。
            if best.is_none_or(|(_, current)| rule.disposition > current) {
                best = Some((rule, rule.disposition));
            }
        }
        PathRuleHit {
            path: path.clone(),
            rule_id: best.map(|(rule, _)| rule.id.clone()),
            disposition: best.map_or(Disposition::Auto, |(_, disposition)| disposition),
            outside_allow: !self.rules.allow_globs.matches(path.as_str()),
        }
    }
}

impl PathRuleSet for RulePaths {
    fn evaluate(&self, paths: &[RepoPath]) -> Vec<PathRuleHit> {
        paths.iter().map(|path| self.evaluate_one(path)).collect()
    }
}

/// `JobSpec.constraints.paths` 与沙盒外观用的快照（worker-protocol §4.5）。
///
/// 快照反映**执行时的**处置：`patch_approvals = false`（M1）时 `approve` 规则
/// 也一律拒绝，所以并入 `deny`，`approval_required` 为空。
pub fn snapshot(rules: &PolicyRules, build: PolicyBuild) -> PathRulesSnapshot {
    let mut deny = Vec::new();
    let mut approval_required = Vec::new();
    for rule in &rules.paths {
        let target = match rule.disposition {
            Disposition::Approve if build.patch_approvals => &mut approval_required,
            Disposition::Auto => continue,
            Disposition::Approve | Disposition::Deny => &mut deny,
        };
        target.extend(rule.globs.patterns().map(str::to_owned));
    }
    PathRulesSnapshot {
        allow: rules.allow_globs.patterns().map(str::to_owned).collect(),
        deny,
        approval_required,
    }
}
