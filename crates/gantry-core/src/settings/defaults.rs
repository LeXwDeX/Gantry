//! 随二进制编译的内置默认值。
//!
//! 数值来自 config.md，保留其 **[建议值，待调优]** 性质；这里不写真实密钥。
//! `models.*.provider` / `model` 没有默认值，保持缺失，由调用方拒绝创建交付单元。

use serde_json::Value;

use crate::entities::SettingsResource;

const MODELS: &str = include_str!("defaults/models.json");
const LEVELS: &str = include_str!("defaults/levels.json");
const POLICIES: &str = include_str!("defaults/policies.json");
const GUARDIAN: &str = include_str!("defaults/guardian.json");
const SANDBOX: &str = include_str!("defaults/sandbox.json");
const EGRESS: &str = include_str!("defaults/egress.json");
const NOTIFY: &str = include_str!("defaults/notify.json");
const GITHUB: &str = include_str!("defaults/github.json");

pub fn builtin_defaults(resource: SettingsResource) -> Value {
    let text = match resource {
        SettingsResource::Models => MODELS,
        SettingsResource::Levels => LEVELS,
        SettingsResource::Policies => POLICIES,
        SettingsResource::Guardian => GUARDIAN,
        SettingsResource::Sandbox => SANDBOX,
        SettingsResource::Egress => EGRESS,
        SettingsResource::Notify => NOTIFY,
        SettingsResource::Github => GITHUB,
    };
    serde_json::from_str(text).expect("built-in settings defaults are valid JSON")
}

/// 解析后的内置默认；models 的 provider / model 保持 `None`。
pub fn builtin_resolved(resource: SettingsResource) -> super::resource::ResolvedSettings {
    super::resource::parse_resolved(resource, &builtin_defaults(resource))
        .expect("built-in settings defaults match their typed schema")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resource_has_object_defaults_and_no_secret_values() {
        for resource in [
            SettingsResource::Models,
            SettingsResource::Levels,
            SettingsResource::Policies,
            SettingsResource::Guardian,
            SettingsResource::Sandbox,
            SettingsResource::Egress,
            SettingsResource::Notify,
            SettingsResource::Github,
        ] {
            let value = builtin_defaults(resource);
            assert!(value.is_object(), "{resource:?}");
            assert!(
                !contains_value_key(&value),
                "{resource:?} contains a secret value"
            );
            let _ = builtin_resolved(resource);
        }
    }

    #[test]
    fn models_have_no_default_provider_or_model() {
        let value = builtin_defaults(SettingsResource::Models);
        assert!(value["utility"].get("provider").is_none());
        assert!(value["utility"].get("model").is_none());
        assert!(value["worker"].get("provider").is_none());
    }

    #[test]
    fn level_defaults_are_the_documented_proposals() {
        let value = builtin_defaults(SettingsResource::Levels);
        assert_eq!(value["default_level"], "Medium");
        assert_eq!(value["none"]["daily_tokens"], 500_000);
        assert_eq!(value["low"]["budget"]["nodes"], 6);
        assert_eq!(value["medium"]["budget"]["nodes"], 20);
        assert_eq!(value["high"]["budget"]["nodes"], 40);
        assert_eq!(value["max"]["budget"]["nodes"], 80);
        assert_eq!(value["ultimate"]["budget"]["nodes"], 400);
        assert_eq!(value["token_weights"]["output"], 5.0);
    }

    fn contains_value_key(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.contains_key("value") || map.values().any(contains_value_key),
            Value::Array(items) => items.iter().any(contains_value_key),
            _ => false,
        }
    }
}
