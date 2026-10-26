//! 写权限：放宽、仅 approver 键与秘密值写入共同决定角色与签名。
//!
//! 判定依据是**新旧有效值的变化**，不是"请求里出现了某个键"：
//! 原样重写一个 approver-only 键不应要求重新签名（C27）。

use serde_json::Value;

use crate::entities::{DeviceRole, SettingsResource};
use crate::secret::{Consumer, SecretName};

use super::clamp::KeyPath;
use super::rules::rule_for;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretWriteKind {
    Set,
}

/// 不含值的秘密写入意图；值留在请求体里，由 `api` 转交 `secrets` / Egress。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretWriteIntent {
    pub name: SecretName,
    pub consumer: Consumer,
    pub write: SecretWriteKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authority {
    pub required_role: DeviceRole,
    pub requires_signature: bool,
}

/// 缺少旧值时的保守回退（core.md §6.7 的公开签名）：把任何"仅 approver 键"
/// 的出现都视为变化。正常写入应使用 [`required_authority_for_change`]，
/// 以比较新旧有效值，避免原样重写也要求签名。
pub fn required_authority(
    resource: SettingsResource,
    relaxes: &[KeyPath],
    new_value: &Value,
    writes: &[SecretWriteIntent],
) -> Authority {
    required_authority_for_change(resource, relaxes, &Value::Null, new_value, writes)
}

/// 比较新旧有效值：仅 approver 键只在**值发生变化**（新增 / 删除 / 修改）时
/// 要求 approver；未变化的键不触发签名（C27）。
///
/// `relaxes` 与 `writes` 由调用方从 [`super::diff_relaxes`] 与
/// [`super::split_secret_values`] 得到；本函数不重新解释文档。
pub fn required_authority_for_change(
    resource: SettingsResource,
    relaxes: &[KeyPath],
    old_value: &Value,
    new_value: &Value,
    writes: &[SecretWriteIntent],
) -> Authority {
    let approver_only_changed =
        approver_only_changed(&resource_path(resource), old_value, new_value);
    if !relaxes.is_empty() || !writes.is_empty() || approver_only_changed {
        Authority {
            required_role: DeviceRole::Approver,
            requires_signature: true,
        }
    } else {
        Authority {
            required_role: DeviceRole::Operator,
            requires_signature: false,
        }
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

fn approver_only_changed(root: &str, old: &Value, new: &Value) -> bool {
    fn walk(path: &str, old: &Value, new: &Value) -> bool {
        if let Some(rule) = rule_for(path) {
            if rule.approver_only {
                return old != new;
            }
        }
        match (old, new) {
            (Value::Object(old), Value::Object(new)) => {
                let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
                keys.sort();
                keys.dedup();
                keys.into_iter().any(|key| {
                    walk(
                        &format!("{path}.{key}"),
                        old.get(key).unwrap_or(&Value::Null),
                        new.get(key).unwrap_or(&Value::Null),
                    )
                })
            }
            (Value::Null, Value::Object(new)) => new
                .keys()
                .any(|key| walk(&format!("{path}.{key}"), &Value::Null, &new[key])),
            (Value::Object(old), Value::Null) => old
                .keys()
                .any(|key| walk(&format!("{path}.{key}"), &old[key], &Value::Null)),
            _ => false,
        }
    }
    walk(root, old, new)
}

/// 供调用方判断某个路径是否属于仅 approver 键；例如设置页展示。
pub fn is_approver_only(path: &str) -> bool {
    rule_for(path)
        .map(|rule| rule.approver_only)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::DeviceRole;
    use crate::secret::Consumer;
    use crate::settings::rules::{compare_loosens, key_rule};
    use serde_json::json;

    fn intent(name: &str, consumer: Consumer) -> SecretWriteIntent {
        SecretWriteIntent {
            name: name.parse().unwrap(),
            consumer,
            write: SecretWriteKind::Set,
        }
    }

    #[test]
    fn loosening_requires_approver_and_signature() {
        let authority = required_authority_for_change(
            crate::entities::SettingsResource::Policies,
            &["policies.actions.merge_pr".to_owned()],
            &json!({"actions": {"merge_pr": "deny"}}),
            &json!({"actions": {"merge_pr": "approve"}}),
            &[],
        );
        assert_eq!(authority.required_role, DeviceRole::Approver);
        assert!(authority.requires_signature);
    }

    #[test]
    fn tightening_is_operator_without_signature() {
        let authority = required_authority_for_change(
            crate::entities::SettingsResource::Policies,
            &[],
            &json!({"actions": {"merge_pr": "approve"}}),
            &json!({"actions": {"merge_pr": "deny"}}),
            &[],
        );
        assert_eq!(authority.required_role, DeviceRole::Operator);
        assert!(!authority.requires_signature);
    }

    #[test]
    fn unchanged_approver_key_does_not_require_signature() {
        let old = json!({"providers": {"anthropic": {"base_url": "https://api.anthropic.com"}}});
        let authority = required_authority_for_change(
            crate::entities::SettingsResource::Models,
            &[],
            &old,
            &old,
            &[],
        );
        assert_eq!(authority.required_role, DeviceRole::Operator);
        assert!(!authority.requires_signature);
    }

    #[test]
    fn changed_approver_key_requires_signature() {
        let authority = required_authority_for_change(
            crate::entities::SettingsResource::Models,
            &[],
            &json!({"providers": {"anthropic": {"base_url": "https://api.anthropic.com"}}}),
            &json!({"providers": {"anthropic": {"base_url": "https://evil.example"}}}),
            &[],
        );
        assert_eq!(authority.required_role, DeviceRole::Approver);
        assert!(authority.requires_signature);
    }

    #[test]
    fn adding_or_removing_approver_key_counts_as_change() {
        let authority = required_authority_for_change(
            crate::entities::SettingsResource::Github,
            &[],
            &json!({}),
            &json!({"app": {"app_id": 1}}),
            &[],
        );
        assert!(authority.requires_signature);
    }

    #[test]
    fn secret_value_requires_signature() {
        let authority = required_authority_for_change(
            crate::entities::SettingsResource::Models,
            &[],
            &json!({}),
            &json!({"providers": {"anthropic": {"credential": {"secret_ref": "model.x"}}}}),
            &[intent("model.x", Consumer::Egress)],
        );
        assert_eq!(authority.required_role, DeviceRole::Approver);
        assert!(authority.requires_signature);
    }

    #[test]
    fn rule_comparison_matches_relax_cmp() {
        let rule = key_rule("policies.actions").unwrap();
        assert!(compare_loosens(
            rule,
            &json!({"merge_pr": "deny"}),
            &json!({"merge_pr": "approve"})
        ));
        assert!(!compare_loosens(
            rule,
            &json!({"merge_pr": "approve"}),
            &json!({"merge_pr": "deny"})
        ));
    }
}
