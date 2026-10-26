//! `relaxes[]`：比较**新旧有效值**，避免稀疏文档替换 / 删除导致的误判。
//!
//! 只比较规则表中带方向的子树；无方向（`Incomparable`）的键变化由
//! `required_authority` 的"仅 approver 键"检查处理。

use serde_json::Value;

use crate::entities::{SettingsResource, SettingsScope};

use super::clamp::KeyPath;
use super::rules::{compare_loosens, key_rule};

/// 返回本次写入放宽的键路径。调用方必须传**合并默认值之后**的旧 / 新有效值；
/// 传稀疏文档会在项目删除覆盖时得出相反结论。
pub fn diff_relaxes(
    resource: SettingsResource,
    _scope: &SettingsScope,
    old: &Value,
    new: &Value,
) -> Vec<KeyPath> {
    let mut relaxes = Vec::new();
    walk(&resource_path(resource), old, new, &mut relaxes);
    relaxes.sort();
    relaxes.dedup();
    relaxes
}

fn resource_path(resource: SettingsResource) -> String {
    match resource {
        SettingsResource::Models => "models",
        SettingsResource::Levels => "levels",
        SettingsResource::Policies => "policies",
        SettingsResource::Guardian => "guardian",
        SettingsResource::Sandbox => "sandbox",
        SettingsResource::Egress => "egress",
        SettingsResource::Notify => "notify",
        SettingsResource::Github => "github",
    }
    .to_owned()
}

fn walk(path: &str, old: &Value, new: &Value, relaxes: &mut Vec<KeyPath>) {
    if let Some(rule) = key_rule(path) {
        if compare_loosens(rule, old, new) {
            relaxes.push(path.to_owned());
        }
        return;
    }

    let (Value::Object(old), Value::Object(new)) = (old, new) else {
        return;
    };
    let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let child_path = format!("{path}.{key}");
        match (old.get(key), new.get(key)) {
            (Some(old_child), Some(new_child)) => walk(&child_path, old_child, new_child, relaxes),
            (Some(old_child), None) => {
                // 删除一个带方向约束的键 = 回退到默认或上层值；由该键自己的
                // 规则判定是否放宽。规则表覆盖的容器都会命中，因此这里只在
                // 叶子有规则时生效。
                if let Some(rule) = key_rule(&child_path) {
                    if compare_loosens(rule, old_child, &Value::Null) {
                        relaxes.push(child_path);
                    }
                }
            }
            (None, Some(_)) => {
                // 新增带方向约束的键通常收紧；不记为放宽。
            }
            (None, None) => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resources() -> (SettingsResource, SettingsScope) {
        (SettingsResource::Policies, SettingsScope::Global)
    }

    #[test]
    fn deny_to_approve_relaxes() {
        let (resource, scope) = resources();
        let old = json!({"actions": {"merge_pr": "deny"}});
        let new = json!({"actions": {"merge_pr": "approve"}});
        assert_eq!(
            diff_relaxes(resource, &scope, &old, &new),
            vec!["policies.actions".to_owned()]
        );
    }

    #[test]
    fn approve_to_deny_tightens() {
        let (resource, scope) = resources();
        let old = json!({"actions": {"merge_pr": "approve"}});
        let new = json!({"actions": {"merge_pr": "deny"}});
        assert!(diff_relaxes(resource, &scope, &old, &new).is_empty());
    }

    #[test]
    fn larger_ttl_and_budget_relax() {
        let (_, scope) = resources();
        let relaxes = diff_relaxes(
            SettingsResource::Policies,
            &scope,
            &json!({"approval": {"ttl_s": 86400}}),
            &json!({"approval": {"ttl_s": 172800}}),
        );
        assert_eq!(relaxes, vec!["policies.approval".to_owned()]);

        let relaxes = diff_relaxes(
            SettingsResource::Levels,
            &scope,
            &json!({"high": {"budget": {"nodes": 40}}}),
            &json!({"high": {"budget": {"nodes": 50}}}),
        );
        assert_eq!(relaxes, vec!["levels.high.budget".to_owned()]);
    }

    #[test]
    fn removing_path_rule_or_escalation_rule_relaxes() {
        let (resource, scope) = resources();
        let old = json!({"paths": {"infra": {"globs": ["deploy/**"], "disposition": "approve"}}});
        let new = json!({"paths": {}});
        assert_eq!(
            diff_relaxes(resource, &scope, &old, &new),
            vec!["policies.paths".to_owned()]
        );

        let old = json!({"escalation": {"enabled": true, "rules": {"x": {
            "when": {"paths": ["src/x/**"]}, "raise_to": "High"
        }}}});
        let new = json!({"escalation": {"enabled": true, "rules": {}}});
        assert_eq!(
            diff_relaxes(resource, &scope, &old, &new),
            vec!["policies.escalation".to_owned()]
        );
    }

    #[test]
    fn triage_and_egress_relax() {
        let relaxes = diff_relaxes(
            SettingsResource::Guardian,
            &SettingsScope::Project,
            &json!({"triage": {"security_finding": "escalate"}}),
            &json!({"triage": {"security_finding": "silent_fix"}}),
        );
        assert_eq!(relaxes, vec!["guardian.triage".to_owned()]);

        let relaxes = diff_relaxes(
            SettingsResource::Egress,
            &SettingsScope::Project,
            &json!({"profiles": {"default": {"groups": ["model_api"]}}}),
            &json!({"profiles": {"default": {"groups": ["model_api", "maven"]}}}),
        );
        assert_eq!(relaxes, vec!["egress.profiles".to_owned()]);
    }

    #[test]
    fn project_delete_falls_back_to_more_permissive_global() {
        // 项目原本把 merge_pr 收紧成 deny；删除覆盖后回退到全局 approve。
        let old_effective = json!({"actions": {"merge_pr": "deny"}});
        let new_effective = json!({"actions": {"merge_pr": "approve"}});
        assert_eq!(
            diff_relaxes(
                SettingsResource::Policies,
                &SettingsScope::Project,
                &old_effective,
                &new_effective
            ),
            vec!["policies.actions".to_owned()]
        );
    }

    #[test]
    fn replacing_a_document_with_smaller_globs_relaxes() {
        let (resource, scope) = resources();
        let old = json!({"paths": {"infra": {"globs": ["deploy/**", "terraform/**"]}}});
        let new = json!({"paths": {"infra": {"globs": ["deploy/**"]}}});
        assert_eq!(
            diff_relaxes(resource, &scope, &old, &new),
            vec!["policies.paths".to_owned()]
        );
    }
}
