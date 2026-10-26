//! 生效值合并与"仅收紧"clamp。

use serde_json::{Map, Value};

use super::merge::merge_patch;
use super::rules::{
    compare_loosens, disposition_rank, key_rule, level_rank, number, object, project_override,
    strings, triage_class_locked, triage_default_action, KeyRule, ProjectOverride,
};
use crate::entities::SettingsResource;

pub type KeyPath = String;
pub type Clamp = KeyPath;

#[derive(Clone, Debug, PartialEq)]
pub struct Effective {
    pub value: Value,
    pub clamped: Vec<Clamp>,
}

/// 逐 resource 计算生效值：
/// `clamp(merge(builtin, global, override), against = merge(builtin, global))`。
///
/// 存储文档不得含 `null`（由调用方的 V01 拒绝）；本函数仍把 `null` 当作
/// "未设置" 而不是删除整个合并结果，避免项目输入里的 `null` 让已收紧的
/// 全局值回退到更宽的内置默认。
pub fn effective(
    resource: SettingsResource,
    builtin: &Value,
    global: &Value,
    project: Option<&Value>,
) -> Effective {
    let global_patch = strip_nulls(global);
    let global_effective = if global_patch.is_object() {
        merge_patch(builtin, &global_patch)
    } else {
        builtin.clone()
    };
    let Some(project) = project else {
        return Effective {
            value: global_effective,
            clamped: Vec::new(),
        };
    };
    let project_patch = strip_nulls(project);
    let proposed = if project_patch.is_object() {
        merge_patch(&global_effective, &project_patch)
    } else {
        global_effective.clone()
    };
    let mut clamped = Vec::new();
    let kept = clamp_value(
        &resource_path(resource),
        &global_effective,
        &proposed,
        &mut clamped,
    );
    clamped.sort();
    clamped.dedup();
    Effective {
        value: kept,
        clamped,
    }
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

/// 递归去掉对象里的 `null`；数组按原样保留。
fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, child)| !child.is_null())
                .map(|(key, child)| (key.clone(), strip_nulls(child)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// 保留全局值，或按项目值收紧。对象递归，列表整体比较；
/// 复合规则（预算、限额、动作、路径、加码、守护、出网档案）逐键 clamp，
/// 只回退确实放宽的字段，保留同一对象里已收紧的字段。
fn clamp_value(path: &str, global: &Value, proposed: &Value, clamped: &mut Vec<Clamp>) -> Value {
    if let Some(rule) = key_rule(path) {
        let kept = match rule.project {
            ProjectOverride::Free => return proposed.clone(),
            ProjectOverride::GlobalOnly => global.clone(),
            ProjectOverride::TightenOnly | ProjectOverride::NarrowOnly => {
                clamp_tightened(rule, global, proposed)
            }
        };
        if &kept != proposed {
            clamped.push(path.to_owned());
        }
        return kept;
    }

    match (global, proposed) {
        (Value::Object(global), Value::Object(proposed)) => {
            let mut result = Map::new();
            let mut keys: Vec<&String> = global.keys().chain(proposed.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child_path = format!("{path}.{key}");
                match (global.get(key), proposed.get(key)) {
                    (Some(global_child), Some(proposed_child)) => {
                        result.insert(
                            key.clone(),
                            clamp_value(&child_path, global_child, proposed_child, clamped),
                        );
                    }
                    (Some(global_child), None) => {
                        // 项目删除一个继承键：等价于回退到全局，不算放宽。
                        result.insert(key.clone(), global_child.clone());
                    }
                    (None, Some(proposed_child)) => {
                        // 项目新增键。新增规则 / 预算 / 守护阈值通常是收紧；
                        // 仅全局键与新增 egress 档案被拒绝（V19 也会拒绝）。
                        match project_override(&child_path) {
                            ProjectOverride::Free | ProjectOverride::TightenOnly => {
                                result.insert(key.clone(), proposed_child.clone());
                            }
                            ProjectOverride::NarrowOnly | ProjectOverride::GlobalOnly => {
                                clamped.push(child_path);
                            }
                        }
                    }
                    (None, None) => unreachable!(),
                }
            }
            Value::Object(result)
        }
        _ => {
            if global != proposed {
                clamped.push(path.to_owned());
                global.clone()
            } else {
                proposed.clone()
            }
        }
    }
}

fn clamp_tightened(rule: &KeyRule, global: &Value, proposed: &Value) -> Value {
    match rule.path {
        "levels.*.budget" => clamp_budget(global, proposed),
        "sandbox.limits" => clamp_numeric_object(
            global,
            proposed,
            &["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"],
        ),
        "policies.actions" => clamp_actions(global, proposed),
        "policies.paths" => clamp_paths(global, proposed),
        "policies.escalation" => clamp_escalation(global, proposed),
        "policies.approval" => clamp_numeric_object(global, proposed, &["ttl_s"]),
        "policies.patch" => clamp_numeric_object(
            global,
            proposed,
            &[
                "max_files",
                "max_bytes",
                "max_changed_lines",
                "max_file_bytes",
            ],
        ),
        "guardian.triage" => clamp_triage(global, proposed),
        "guardian.no_progress" => {
            clamp_numeric_object(global, proposed, &["same_class_repeats", "stalled_rounds"])
        }
        "egress.profiles" => clamp_profiles(global, proposed),
        _ => {
            if compare_loosens(rule, global, proposed) {
                global.clone()
            } else {
                proposed.clone()
            }
        }
    }
}

fn clamp_budget(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    for key in [
        "nodes",
        "rounds",
        "wall_time_s",
        "concurrency",
        "forge_writes",
        "forge_writes_per_hour",
    ] {
        clamp_numeric_key(&mut result, global, proposed, key);
    }
    if let Some(global_tiers) = global.get("tokens_by_tier").and_then(object) {
        let mut tiers = proposed
            .get("tokens_by_tier")
            .and_then(object)
            .cloned()
            .unwrap_or_default();
        for (tier, global_value) in global_tiers {
            match tiers.get(tier) {
                Some(candidate) if number_le(candidate, global_value) => {}
                _ => {
                    tiers.insert(tier.clone(), global_value.clone());
                }
            }
        }
        result.insert("tokens_by_tier".to_owned(), Value::Object(tiers));
    }
    Value::Object(result)
}

fn clamp_numeric_object(global: &Value, proposed: &Value, keys: &[&str]) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    for key in keys {
        clamp_numeric_key(&mut result, global, proposed, key);
    }
    Value::Object(result)
}

fn clamp_numeric_key(
    result: &mut Map<String, Value>,
    global: &Map<String, Value>,
    proposed: &Map<String, Value>,
    key: &str,
) {
    let Some(global_value) = global.get(key) else {
        return;
    };
    match proposed.get(key) {
        Some(candidate) if number_le(candidate, global_value) => {}
        _ => {
            result.insert(key.to_owned(), global_value.clone());
        }
    }
}

fn number_le(candidate: &Value, limit: &Value) -> bool {
    matches!((number(candidate), number(limit)), (Some(candidate), Some(limit)) if candidate <= limit)
}

fn clamp_actions(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    for (action, global_value) in global {
        let keep = matches!(
            (disposition_rank(proposed.get(action).unwrap_or(&Value::Null)),
             disposition_rank(global_value)),
            (Some(candidate), Some(global)) if candidate >= global
        );
        if !keep {
            result.insert(action.clone(), global_value.clone());
        }
    }
    Value::Object(result)
}

fn clamp_paths(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    for (id, global_rule) in global {
        match proposed.get(id) {
            Some(proposed_rule) => {
                let kept = clamp_path_rule(global_rule, proposed_rule);
                if &kept != proposed_rule {
                    result.insert(id.clone(), kept);
                }
            }
            None => {
                result.insert(id.clone(), global_rule.clone());
            }
        }
    }
    Value::Object(result)
}

fn clamp_path_rule(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    if let (Some(global_globs), Some(proposed_globs)) = (
        global.get("globs").and_then(strings),
        proposed.get("globs").and_then(strings),
    ) {
        if global_globs
            .iter()
            .any(|glob| !proposed_globs.contains(glob))
        {
            result.insert("globs".to_owned(), global["globs"].clone());
        }
    } else if let Some(global_globs) = global.get("globs") {
        result.insert("globs".to_owned(), global_globs.clone());
    }
    let keep_disposition = matches!(
        (
            disposition_rank(proposed.get("disposition").unwrap_or(&Value::Null)),
            disposition_rank(global.get("disposition").unwrap_or(&Value::Null))
        ),
        (Some(candidate), Some(global)) if candidate >= global
    );
    if !keep_disposition {
        if let Some(global_disposition) = global.get("disposition") {
            result.insert("disposition".to_owned(), global_disposition.clone());
        }
    }
    Value::Object(result)
}

fn clamp_escalation(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    if global.get("enabled") == Some(&Value::Bool(true))
        && proposed.get("enabled") == Some(&Value::Bool(false))
    {
        result.insert("enabled".to_owned(), Value::Bool(true));
    }
    if let Some(global_rules) = global.get("rules").and_then(object) {
        let mut rules = proposed
            .get("rules")
            .and_then(object)
            .cloned()
            .unwrap_or_default();
        for (id, global_rule) in global_rules {
            match rules.get(id) {
                Some(proposed_rule) => {
                    let kept = clamp_escalation_rule(global_rule, proposed_rule);
                    if &kept != proposed_rule {
                        rules.insert(id.clone(), kept);
                    }
                }
                None => {
                    rules.insert(id.clone(), global_rule.clone());
                }
            }
        }
        result.insert("rules".to_owned(), Value::Object(rules));
    }
    Value::Object(result)
}

fn clamp_escalation_rule(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    let keep_raise_to = matches!(
        (
            level_rank(proposed.get("raise_to")),
            level_rank(global.get("raise_to"))
        ),
        (Some(candidate), Some(global)) if candidate >= global
    );
    if !keep_raise_to {
        if let Some(global_raise_to) = global.get("raise_to") {
            result.insert("raise_to".to_owned(), global_raise_to.clone());
        }
    }
    match (global.get("when"), proposed.get("when")) {
        (Some(global_when), Some(proposed_when)) => {
            let kept = clamp_when(global_when, proposed_when);
            if &kept != proposed_when {
                result.insert("when".to_owned(), kept);
            }
        }
        (Some(global_when), None) => {
            result.insert("when".to_owned(), global_when.clone());
        }
        _ => {}
    }
    Value::Object(result)
}

fn clamp_when(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    if let Some(global_paths) = global.get("paths").and_then(strings) {
        return match proposed.get("paths").and_then(strings) {
            Some(proposed_paths)
                if global_paths
                    .iter()
                    .all(|path| proposed_paths.contains(path)) =>
            {
                Value::Object(proposed.clone())
            }
            _ => Value::Object(global.clone()),
        };
    }
    if let Some(global_labels) = global.get("labels").and_then(strings) {
        return match proposed.get("labels").and_then(strings) {
            Some(proposed_labels)
                if global_labels
                    .iter()
                    .all(|label| proposed_labels.contains(label)) =>
            {
                Value::Object(proposed.clone())
            }
            _ => Value::Object(global.clone()),
        };
    }
    if let Some(global_diff) = global.get("diff").and_then(object) {
        let Some(proposed_diff) = proposed.get("diff").and_then(object) else {
            return Value::Object(global.clone());
        };
        let mut diff = proposed_diff.clone();
        for key in ["files_gte", "lines_gte"] {
            if let Some(global_value) = global_diff.get(key) {
                match diff.get(key) {
                    Some(candidate) if number_le(candidate, global_value) => {}
                    _ => {
                        diff.insert((*key).to_owned(), global_value.clone());
                    }
                }
            }
        }
        let mut result = proposed.clone();
        result.insert("diff".to_owned(), Value::Object(diff));
        return Value::Object(result);
    }
    Value::Object(proposed.clone())
}

fn clamp_triage(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    // 基线不只看全局显式键：锁定类与 `mechanical_conflict` 的权威默认是
    // escalate，项目新增这些类后改成静默值也必须收紧。
    for (class, proposed_action) in proposed {
        let baseline = global
            .get(class)
            .and_then(Value::as_str)
            .or_else(|| triage_default_action(class));
        let keep = match baseline {
            Some("escalate") => proposed_action.as_str() == Some("escalate"),
            Some(_) => true,
            None => !triage_class_locked(class) || proposed_action.as_str() == Some("escalate"),
        };
        if !keep {
            result.insert(class.clone(), Value::String("escalate".to_owned()));
        }
    }
    for (class, global_action) in global {
        if global_action.as_str() == Some("escalate")
            && result.get(class).and_then(Value::as_str) != Some("escalate")
        {
            result.insert(class.clone(), global_action.clone());
        }
    }
    Value::Object(result)
}

fn clamp_profiles(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = Map::new();
    for (name, global_profile) in global {
        if let Some(proposed_profile) = proposed.get(name) {
            result.insert(
                name.clone(),
                clamp_profile(global_profile, proposed_profile),
            );
        }
        // 删除档案是收窄，保留删除。
    }
    // 新增档案扩大可用分组，项目不可新增（V19 也拒绝）。
    Value::Object(result)
}

fn clamp_profile(global: &Value, proposed: &Value) -> Value {
    let (Some(global), Some(proposed)) = (object(global), object(proposed)) else {
        return global.clone();
    };
    let mut result = proposed.clone();
    if let Some(global_groups) = global.get("groups").and_then(strings) {
        match proposed.get("groups").and_then(strings) {
            Some(proposed_groups)
                if proposed_groups
                    .iter()
                    .all(|group| global_groups.contains(group)) => {}
            _ => {
                if let Some(global_groups) = global.get("groups") {
                    result.insert("groups".to_owned(), global_groups.clone());
                }
            }
        }
    }
    Value::Object(result)
}

/// 由已合并的完整文档解析类型化设置；未配置的键保持 `None`。
pub fn effective_resolved(
    resource: SettingsResource,
    value: &Value,
) -> Result<super::resource::ResolvedSettings, serde_json::Error> {
    super::resource::parse_resolved(resource, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_budget_can_only_tighten() {
        let builtin = json!({"high": {"budget": {"nodes": 40}}});
        let global = json!({});
        let project = Some(&json!({"high": {"budget": {"nodes": 20}}}));
        let result = effective(SettingsResource::Levels, &builtin, &global, project);
        assert_eq!(result.value["high"]["budget"]["nodes"], 20);
        assert!(result.clamped.is_empty());
    }

    #[test]
    fn project_budget_loosening_is_clamped_and_reported() {
        let builtin = json!({"high": {"budget": {"nodes": 40}}});
        let global = json!({"high": {"budget": {"nodes": 30}}});
        let project = Some(&json!({"high": {"budget": {"nodes": 50}}}));
        let result = effective(SettingsResource::Levels, &builtin, &global, project);
        assert_eq!(result.value["high"]["budget"]["nodes"], 30);
        assert_eq!(result.clamped, vec!["levels.high.budget".to_owned()]);
    }

    #[test]
    fn global_only_subtree_and_resource_are_restored() {
        let builtin = json!({"providers": {}});
        let global = json!({"providers": {"anthropic": {"kind": "anthropic"}}});
        let project = Some(&json!({"providers": {"evil": {"kind": "anthropic"}}}));
        let result = effective(SettingsResource::Models, &builtin, &global, project);
        assert_eq!(result.value, global);
        assert_eq!(result.clamped, vec!["models.providers".to_owned()]);

        let builtin = json!({"channels": {"desktop": {"enabled": true}}});
        let global = json!({});
        let project = Some(&json!({"channels": {"desktop": {"enabled": false}}}));
        let result = effective(SettingsResource::Notify, &builtin, &global, project);
        assert_eq!(result.value["channels"]["desktop"]["enabled"], true);
        assert_eq!(result.clamped, vec!["notify".to_owned()]);
    }

    #[test]
    fn egress_profile_can_only_narrow() {
        let builtin = json!({"profiles": {"default": {"groups": ["model_api", "npm"]}}});
        let global = json!({});
        let narrow = Some(&json!({"profiles": {"default": {"groups": ["model_api"]}}}));
        let result = effective(SettingsResource::Egress, &builtin, &global, narrow);
        assert_eq!(
            result.value["profiles"]["default"]["groups"],
            json!(["model_api"])
        );
        assert!(result.clamped.is_empty());

        let wider = Some(&json!({"profiles": {"default": {"groups": ["model_api", "maven"]}}}));
        let result = effective(SettingsResource::Egress, &builtin, &global, wider);
        assert_eq!(
            result.value["profiles"]["default"]["groups"],
            json!(["model_api", "npm"])
        );
        assert_eq!(result.clamped, vec!["egress.profiles".to_owned()]);
    }

    #[test]
    fn null_storage_fails_closed_to_builtin() {
        let builtin = json!({"limits": {"mem_mb": 4096}});
        let global = json!({"limits": {"mem_mb": null}});
        let result = effective(SettingsResource::Sandbox, &builtin, &global, None);
        assert_eq!(result.value, builtin);
    }
}
