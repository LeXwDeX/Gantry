//! config.md §2.2 的单一规则表。
//!
//! `effective`（clamp）、`diff_relaxes`（放宽差分）、`required_authority`（角色与签名）
//! 与后续 V19 共用这一张表；新增键时只改这里，避免三处各写一遍。
//!
//! config.md 标注为 **[提议]** 的行保留提议状态；本表不把提议变成用户裁定。

use serde_json::Value;

use crate::entities::SettingsResource;

/// 项目覆盖许可。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectOverride {
    /// 项目可自由覆盖（§11 校验仍然适用）。
    Free,
    /// 只能收紧：项目值比全局更宽松时被 clamp / 拒绝。
    TightenOnly,
    /// 只能收窄（egress 档案的分组集合）。
    NarrowOnly,
    /// 仅全局可写；项目写入 V19 拒绝。
    GlobalOnly,
}

/// 如何判断"新值比旧值更宽松"。
#[derive(Clone, Copy)]
pub enum RelaxCmp {
    /// 数值变大 = 放宽（预算、限额、TTL、阈值）。
    NumGreaterLoosens,
    /// 数值变小 = 放宽（token 权重：同样用量计得更少）。
    NumSmallerLoosens,
    /// 枚举序号变小 = 放宽；序号越大越严格。
    EnumRank(&'static [&'static str]),
    /// glob 集合变少（新集合不是旧集合的超集）= 放宽。
    GlobsFewerLoosens,
    /// 集合变大 = 放宽（egress.profiles.*.groups）。
    SetSupersetLoosens,
    /// `true → false` = 放宽（escalation.enabled）。
    BoolFalseLoosens,
    /// `false → true` = 放宽。
    BoolTrueLoosens,
    /// 对象级比较；返回 `true` 表示放宽。
    Custom(fn(&Value, &Value) -> bool),
    /// 无方向：不得用于自动 clamp；项目覆盖只能是 `GlobalOnly` 或自由。
    Incomparable,
}

pub struct KeyRule {
    /// 点号路径；`*` 匹配恰好一段（如 `levels.*.budget`）。
    pub path: &'static str,
    pub project: ProjectOverride,
    pub relax: RelaxCmp,
    /// config.md §2.5 的"仅 approver 键"。
    pub approver_only: bool,
}

/// config.md §2.2 逐行录入。表内路径互不重叠，避免规则优先级歧义。
pub const RULES: &[KeyRule] = &[
    // models
    KeyRule {
        path: "models.providers",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: true,
    },
    KeyRule {
        path: "models.retry",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: false,
    },
    KeyRule {
        path: "models.utility",
        project: ProjectOverride::Free,
        relax: RelaxCmp::Incomparable,
        approver_only: false,
    },
    KeyRule {
        path: "models.planner",
        project: ProjectOverride::Free,
        relax: RelaxCmp::Incomparable,
        approver_only: false,
    },
    KeyRule {
        path: "models.worker",
        project: ProjectOverride::Free,
        relax: RelaxCmp::Incomparable,
        approver_only: false,
    },
    // levels
    KeyRule {
        path: "levels.default_level",
        project: ProjectOverride::Free,
        relax: RelaxCmp::Incomparable,
        approver_only: false,
    },
    KeyRule {
        path: "levels.*.graph_template",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: true,
    },
    KeyRule {
        path: "levels.*.budget",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(budget_loosens),
        approver_only: false,
    },
    KeyRule {
        path: "levels.none.daily_tokens",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::NumGreaterLoosens,
        approver_only: false,
    },
    KeyRule {
        path: "levels.token_weights",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Custom(token_weights_loosen),
        approver_only: false,
    },
    // policies
    KeyRule {
        path: "policies.actions",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(actions_loosen),
        approver_only: false,
    },
    KeyRule {
        path: "policies.unlisted_action",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::EnumRank(&["approve", "deny"]),
        approver_only: false,
    },
    KeyRule {
        path: "policies.allow_globs",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: true,
    },
    KeyRule {
        path: "policies.paths",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(paths_loosen),
        approver_only: false,
    },
    KeyRule {
        path: "policies.escalation",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(escalation_loosen),
        approver_only: false,
    },
    KeyRule {
        path: "policies.approval",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(ttl_loosens),
        approver_only: false,
    },
    KeyRule {
        path: "policies.patch",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(patch_limits_loosen),
        approver_only: false,
    },
    // guardian
    KeyRule {
        path: "guardian.triage",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(triage_loosen),
        approver_only: false,
    },
    KeyRule {
        path: "guardian.no_progress",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(no_progress_loosen),
        approver_only: false,
    },
    KeyRule {
        path: "guardian.patrol_interval_s",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::NumGreaterLoosens,
        approver_only: false,
    },
    KeyRule {
        path: "guardian.status_checklist",
        project: ProjectOverride::Free,
        relax: RelaxCmp::Incomparable,
        approver_only: false,
    },
    // sandbox
    KeyRule {
        path: "sandbox.driver",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: true,
    },
    KeyRule {
        path: "sandbox.runtime",
        project: ProjectOverride::GlobalOnly,
        // runc 比 runsc 弱；序号小 = 放宽。
        relax: RelaxCmp::EnumRank(&["runc", "runsc"]),
        approver_only: true,
    },
    KeyRule {
        path: "sandbox.base_image",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: true,
    },
    KeyRule {
        path: "sandbox.limits",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(sandbox_limits_loosen),
        approver_only: false,
    },
    KeyRule {
        path: "sandbox.max_concurrent_sandboxes",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::NumGreaterLoosens,
        approver_only: true,
    },
    KeyRule {
        path: "sandbox.prewarm",
        project: ProjectOverride::GlobalOnly,
        // pool_size 等数值变大 = 放宽；enabled 是布尔值，契约未给方向。
        relax: RelaxCmp::Custom(nested_numeric_greater),
        approver_only: false,
    },
    KeyRule {
        path: "sandbox.devcontainer",
        project: ProjectOverride::TightenOnly,
        relax: RelaxCmp::Custom(devcontainer_loosen),
        approver_only: false,
    },
    KeyRule {
        path: "sandbox.idle_timeout_s",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::NumGreaterLoosens,
        approver_only: false,
    },
    KeyRule {
        path: "sandbox.kill_grace_s",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::NumGreaterLoosens,
        approver_only: false,
    },
    KeyRule {
        path: "sandbox.io",
        project: ProjectOverride::GlobalOnly,
        // 全部是数值上限；任一上限变大 = 放宽。
        relax: RelaxCmp::Custom(nested_numeric_greater),
        approver_only: false,
    },
    // egress
    KeyRule {
        path: "egress.groups",
        project: ProjectOverride::GlobalOnly,
        // 加分组或给分组加主机 = 放宽。
        relax: RelaxCmp::Custom(named_hosts_loosen),
        approver_only: true,
    },
    KeyRule {
        path: "egress.injection",
        project: ProjectOverride::GlobalOnly,
        // 加注入项或给注入项加主机 = 放宽。
        relax: RelaxCmp::Custom(named_hosts_loosen),
        approver_only: true,
    },
    KeyRule {
        path: "egress.default_profile",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: true,
    },
    KeyRule {
        path: "egress.profiles",
        project: ProjectOverride::NarrowOnly,
        relax: RelaxCmp::Custom(profiles_loosen),
        approver_only: false,
    },
    // 整个 resource 不可覆盖
    KeyRule {
        path: "notify",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: false,
    },
    KeyRule {
        path: "github",
        project: ProjectOverride::GlobalOnly,
        relax: RelaxCmp::Incomparable,
        approver_only: true,
    },
];

/// config.md §6.1 锁定类：不可配置，出现在 `triage` 里即 V17 `locked_key`。
pub const LOCKED_TRIAGE_CLASSES: &[&str] = &[
    "security_finding",
    "touches_protected_path",
    "scope_expanding_fix",
    "repeated_or_no_progress",
    "budget_exhausted",
    "approval_needed",
    "push_after_approval",
    "design_or_api_change",
    "unknown",
];

pub fn triage_class_locked(class: &str) -> bool {
    LOCKED_TRIAGE_CLASSES.contains(&class)
}

/// config.md §6.1 非锁定类的允许取值。锁定类与未知类返回 `None`。
pub fn triage_allowed_actions(class: &str) -> Option<&'static [&'static str]> {
    match class {
        "ci_failure_in_scope" | "bot_finding_valid" | "mechanical_conflict" => {
            Some(&["silent_fix", "escalate"])
        }
        "bot_finding_invalid" => Some(&["silent_reply", "escalate"]),
        "nit" => Some(&["batch", "escalate"]),
        _ => None,
    }
}

/// config.md §6.1 的权威默认处置。锁定类按不可放宽的 `escalate` 处理；
/// `touches_protected_path` 的实际处置由 `policies.paths` 决定，但仍属锁定类。
pub fn triage_default_action(class: &str) -> Option<&'static str> {
    match class {
        "ci_failure_in_scope" | "bot_finding_valid" => Some("silent_fix"),
        "bot_finding_invalid" => Some("silent_reply"),
        "nit" => Some("batch"),
        "mechanical_conflict" => Some("escalate"),
        class if triage_class_locked(class) => Some("escalate"),
        _ => None,
    }
}

/// 查找最具体的匹配规则；`*` 只匹配一段。
pub fn key_rule(path: &str) -> Option<&'static KeyRule> {
    let segments: Vec<&str> = path.split('.').collect();
    RULES
        .iter()
        .filter(|rule| matches_path(rule.path, &segments))
        .max_by_key(|rule| {
            (
                rule.path.split('.').filter(|part| *part != "*").count(),
                rule.path.len(),
            )
        })
}

/// 路径本身或其最近的受约束祖先的规则。仅用于"这个值受哪条规则约束"，
/// 不用于判断路径是否是规则表里的精确键。
pub fn rule_for(path: &str) -> Option<&'static KeyRule> {
    let segments: Vec<&str> = path.split('.').collect();
    RULES
        .iter()
        .filter(|rule| {
            let parts: Vec<&str> = rule.path.split('.').collect();
            parts.len() <= segments.len()
                && parts
                    .iter()
                    .zip(segments.iter())
                    .all(|(part, segment)| *part == "*" || part == segment)
        })
        .max_by_key(|rule| {
            (
                rule.path.split('.').filter(|part| *part != "*").count(),
                rule.path.len(),
            )
        })
}

/// 项目可覆盖性；未列入规则表的路径按不可覆盖处理（fail-closed）。
pub fn project_override(path: &str) -> ProjectOverride {
    rule_for(path)
        .map(|rule| rule.project)
        .unwrap_or(ProjectOverride::GlobalOnly)
}

/// 该路径是否属于"仅 approver 键"（config.md §2.5）。未列入按否处理，
/// 因为仅 approver 键是封闭清单；未知键在 V01 / V19 拒绝。
pub fn approver_only(path: &str) -> bool {
    rule_for(path)
        .map(|rule| rule.approver_only)
        .unwrap_or(false)
}

/// 安装引导期（P-66）允许主机命令写入的键。文档明确列出的只有
/// `github.*` 与 `sandbox.base_image`；其余引导键未在契约中给出，保持拒绝。
pub fn bootstrap_writable(resource: SettingsResource, path: &str) -> bool {
    match resource {
        SettingsResource::Github => path == "github" || path.starts_with("github."),
        SettingsResource::Sandbox => path == "sandbox.base_image",
        _ => false,
    }
}

pub fn matches_path(pattern: &str, segments: &[&str]) -> bool {
    let parts: Vec<&str> = pattern.split('.').collect();
    parts.len() == segments.len()
        && parts
            .iter()
            .zip(segments)
            .all(|(part, segment)| *part == "*" || part == segment)
}

pub fn compare_loosens(rule: &KeyRule, old: &Value, new: &Value) -> bool {
    match rule.relax {
        RelaxCmp::NumGreaterLoosens => number(new) > number(old) && number(old).is_some(),
        RelaxCmp::NumSmallerLoosens => {
            number(new) < number(old) && number(old).is_some() && number(new).is_some()
        }
        RelaxCmp::EnumRank(rank) => enum_rank_loosens(rank, old, new),
        RelaxCmp::GlobsFewerLoosens => globs_fewer(old, new),
        RelaxCmp::SetSupersetLoosens => set_superset(old, new),
        RelaxCmp::BoolFalseLoosens => old == &Value::Bool(true) && new == &Value::Bool(false),
        RelaxCmp::BoolTrueLoosens => old == &Value::Bool(false) && new == &Value::Bool(true),
        RelaxCmp::Custom(compare) => compare(old, new),
        RelaxCmp::Incomparable => false,
    }
}

pub(crate) fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

pub(crate) fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect()
    })
}

fn enum_rank_loosens(rank: &[&str], old: &Value, new: &Value) -> bool {
    let Some(old) = old.as_str().and_then(|v| rank.iter().position(|r| *r == v)) else {
        return false;
    };
    let Some(new) = new.as_str().and_then(|v| rank.iter().position(|r| *r == v)) else {
        return false;
    };
    new < old
}

fn globs_fewer(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (strings(old), strings(new)) else {
        return false;
    };
    old.iter().any(|glob| !new.contains(glob))
}

fn set_superset(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (strings(old), strings(new)) else {
        return false;
    };
    old.iter().all(|item| new.contains(item)) && new.len() >= old.len()
}

pub(crate) fn object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

/// 递归比较对象里的数值叶子：任一叶子变大 = 放宽。
/// 非数值字段（如 `prewarm.enabled`）不参与比较。
fn nested_numeric_greater(old: &Value, new: &Value) -> bool {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            new.iter().any(|(key, new_value)| match old.get(key) {
                Some(old_value) => nested_numeric_greater(old_value, new_value),
                None => false,
            })
        }
        _ => matches!((number(old), number(new)), (Some(old), Some(new)) if new > old),
    }
}

/// 以 id 为键的对象（egress.groups / injection）：新增条目或新增主机 = 放宽。
fn named_hosts_loosen(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    new.iter().any(|(name, new_entry)| {
        let Some(old_entry) = old.get(name) else {
            return true;
        };
        let (Some(old_hosts), Some(new_hosts)) = (
            old_entry.get("hosts").and_then(strings),
            new_entry.get("hosts").and_then(strings),
        ) else {
            return false;
        };
        new_hosts.iter().any(|host| !old_hosts.contains(host))
    })
}

fn budget_loosens(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    if let Some(old_tiers) = old.get("tokens_by_tier").and_then(object) {
        if let Some(new_tiers) = new.get("tokens_by_tier").and_then(object) {
            for (tier, value) in old_tiers {
                if number(new_tiers.get(tier).unwrap_or(&Value::Null)) > number(value) {
                    return true;
                }
            }
        }
    }
    [
        "nodes",
        "rounds",
        "wall_time_s",
        "concurrency",
        "forge_writes",
        "forge_writes_per_hour",
    ]
    .iter()
    .any(|key| {
        number(new.get(*key).unwrap_or(&Value::Null))
            > number(old.get(*key).unwrap_or(&Value::Null))
    })
}

fn token_weights_loosen(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    ["input", "output", "cache_read", "cache_write"]
        .iter()
        .any(|key| {
            let old = number(old.get(*key).unwrap_or(&Value::Null));
            let new = number(new.get(*key).unwrap_or(&Value::Null));
            matches!((old, new), (Some(old), Some(new)) if new < old)
        })
}

pub(crate) fn disposition_rank(value: &Value) -> Option<usize> {
    value
        .as_str()
        .and_then(|v| ["auto", "approve", "deny"].iter().position(|r| *r == v))
}

fn actions_loosen(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    for (action, old_value) in old {
        let Some(new_value) = new.get(action) else {
            return true;
        };
        let (Some(old_rank), Some(new_rank)) =
            (disposition_rank(old_value), disposition_rank(new_value))
        else {
            return true;
        };
        if new_rank < old_rank {
            return true;
        }
    }
    false
}

fn paths_loosen(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    for (id, old_rule) in old {
        let Some(new_rule) = new.get(id) else {
            return true;
        };
        if path_rule_loosens(old_rule, new_rule) {
            return true;
        }
    }
    false
}

fn path_rule_loosens(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return true;
    };
    let old_disposition = old.get("disposition").and_then(disposition_rank);
    let new_disposition = new.get("disposition").and_then(disposition_rank);
    if matches!((old_disposition, new_disposition), (Some(old), Some(new)) if new < old) {
        return true;
    }
    let old_globs = old.get("globs").and_then(strings);
    let new_globs = new.get("globs").and_then(strings);
    matches!((old_globs, new_globs), (Some(old), Some(new))
        if old.iter().any(|glob| !new.contains(glob)))
}

fn escalation_loosen(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    if old.get("enabled") == Some(&Value::Bool(true))
        && new.get("enabled") == Some(&Value::Bool(false))
    {
        return true;
    }
    let (Some(old_rules), Some(new_rules)) = (
        old.get("rules").and_then(object),
        new.get("rules").and_then(object),
    ) else {
        return false;
    };
    for (id, old_rule) in old_rules {
        let Some(new_rule) = new_rules.get(id) else {
            return true;
        };
        if escalation_rule_loosens(old_rule, new_rule) {
            return true;
        }
    }
    false
}

fn escalation_rule_loosens(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return true;
    };
    if level_rank(new.get("raise_to")) < level_rank(old.get("raise_to")) {
        return true;
    }
    when_loosens(
        old.get("when").unwrap_or(&Value::Null),
        new.get("when").unwrap_or(&Value::Null),
    )
}

pub(crate) fn level_rank(value: Option<&Value>) -> Option<usize> {
    value
        .and_then(Value::as_str)
        .and_then(|v| ["Medium", "High", "Max"].iter().position(|r| *r == v))
}

fn when_loosens(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return true;
    };
    if let (Some(old_paths), Some(new_paths)) = (
        old.get("paths").and_then(strings),
        new.get("paths").and_then(strings),
    ) {
        return old_paths.iter().any(|path| !new_paths.contains(path));
    }
    if let (Some(old_labels), Some(new_labels)) = (
        old.get("labels").and_then(strings),
        new.get("labels").and_then(strings),
    ) {
        return old_labels.iter().any(|label| !new_labels.contains(label));
    }
    let (Some(old_diff), Some(new_diff)) = (
        old.get("diff").and_then(object),
        new.get("diff").and_then(object),
    ) else {
        return true;
    };
    for key in ["files_gte", "lines_gte"] {
        let old = old_diff.get(key).and_then(Value::as_f64);
        let new = new_diff.get(key).and_then(Value::as_f64);
        match (old, new) {
            (Some(old), Some(new)) if new > old => return true,
            (Some(_), None) => return true,
            _ => {}
        }
    }
    false
}

fn triage_loosen(old: &Value, new: &Value) -> bool {
    let Some(new) = object(new) else {
        return false;
    };
    let old = object(old);
    // 旧值显式 escalate，新值缺失或改成任何静默值 = 放宽。
    if old.is_some_and(|old| {
        old.iter().any(|(class, action)| {
            action == "escalate" && new.get(class).and_then(Value::as_str) != Some("escalate")
        })
    }) {
        return true;
    }
    // 新增类没有旧值时按权威默认判断；锁定类即使不在默认表里也按 escalate。
    // 这只覆盖配置出现过的类，不遍历业务文本。
    new.iter().any(|(class, action)| {
        let baseline = old
            .and_then(|old| old.get(class))
            .and_then(Value::as_str)
            .or_else(|| triage_default_action(class));
        match baseline {
            Some(baseline) => baseline == "escalate" && action.as_str() != Some("escalate"),
            None => triage_class_locked(class) && action.as_str() != Some("escalate"),
        }
    })
}

fn no_progress_loosen(old: &Value, new: &Value) -> bool {
    numeric_object_greater(old, new, &["same_class_repeats", "stalled_rounds"])
}

fn sandbox_limits_loosen(old: &Value, new: &Value) -> bool {
    numeric_object_greater(
        old,
        new,
        &["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"],
    )
}

fn patch_limits_loosen(old: &Value, new: &Value) -> bool {
    numeric_object_greater(
        old,
        new,
        &[
            "max_files",
            "max_bytes",
            "max_changed_lines",
            "max_file_bytes",
        ],
    )
}

fn ttl_loosens(old: &Value, new: &Value) -> bool {
    numeric_object_greater(old, new, &["ttl_s"])
}

fn numeric_object_greater(old: &Value, new: &Value, keys: &[&str]) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    keys.iter().any(|key| {
        number(new.get(*key).unwrap_or(&Value::Null))
            > number(old.get(*key).unwrap_or(&Value::Null))
    })
}

fn devcontainer_loosen(old: &Value, new: &Value) -> bool {
    let old = old
        .get("mode")
        .and_then(Value::as_str)
        .and_then(|v| ["off", "safe_subset"].iter().position(|r| *r == v));
    let new = new
        .get("mode")
        .and_then(Value::as_str)
        .and_then(|v| ["off", "safe_subset"].iter().position(|r| *r == v));
    matches!((old, new), (Some(old), Some(new)) if new < old)
}

fn profiles_loosen(old: &Value, new: &Value) -> bool {
    let (Some(old), Some(new)) = (object(old), object(new)) else {
        return false;
    };
    // 只比较既有档案的分组：新增档案是扩大能力，但不在 §2.2 的
    // "egress.profiles.<name>.groups 仅收窄"这一行；新增 / 删除由 V19 处理。
    for (name, old_profile) in old {
        let Some(new_profile) = new.get(name) else {
            continue;
        };
        let (Some(old_groups), Some(new_groups)) = (
            old_profile.get("groups").and_then(strings),
            new_profile.get("groups").and_then(strings),
        ) else {
            return true;
        };
        if new_groups.iter().any(|group| !old_groups.contains(group)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_table_covers_matrix_rows() {
        let expected = [
            "models.providers",
            "models.retry",
            "models.utility",
            "models.planner",
            "models.worker",
            "levels.default_level",
            "levels.*.graph_template",
            "levels.*.budget",
            "levels.none.daily_tokens",
            "levels.token_weights",
            "policies.actions",
            "policies.unlisted_action",
            "policies.allow_globs",
            "policies.paths",
            "policies.escalation",
            "policies.approval",
            "policies.patch",
            "guardian.triage",
            "guardian.no_progress",
            "guardian.patrol_interval_s",
            "guardian.status_checklist",
            "sandbox.driver",
            "sandbox.runtime",
            "sandbox.base_image",
            "sandbox.limits",
            "sandbox.max_concurrent_sandboxes",
            "sandbox.prewarm",
            "sandbox.devcontainer",
            "sandbox.idle_timeout_s",
            "sandbox.kill_grace_s",
            "sandbox.io",
            "egress.groups",
            "egress.injection",
            "egress.default_profile",
            "egress.profiles",
            "notify",
            "github",
        ];
        for path in expected {
            assert!(key_rule(path).is_some(), "missing rule for {path}");
        }
    }

    #[test]
    fn rule_lookup_is_specific() {
        assert_eq!(
            rule_for("levels.high.budget.nodes").unwrap().path,
            "levels.*.budget"
        );
        assert_eq!(
            rule_for("levels.high.graph_template").unwrap().path,
            "levels.*.graph_template"
        );
        assert!(key_rule("levels.high.budget.nodes").is_none());
        assert_eq!(rule_for("notify.routes.x").unwrap().path, "notify");
        assert_eq!(project_override("notify"), ProjectOverride::GlobalOnly);
    }

    #[test]
    fn wildcard_matches_exactly_one_segment() {
        assert!(matches_path(
            "levels.*.budget",
            &["levels", "high", "budget"]
        ));
        assert!(!matches_path("levels.*.budget", &["levels", "budget"]));
    }

    #[test]
    fn bootstrap_keys_are_closed() {
        assert!(bootstrap_writable(
            SettingsResource::Github,
            "github.app.private_key"
        ));
        assert!(bootstrap_writable(
            SettingsResource::Sandbox,
            "sandbox.base_image"
        ));
        assert!(!bootstrap_writable(
            SettingsResource::Sandbox,
            "sandbox.limits.mem_mb"
        ));
    }
}
