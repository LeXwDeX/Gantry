//! `RuntimeSettingsContext` 的组件测试：只读快照到 `ValidationContext` / catalog / status。

use std::collections::{BTreeMap, BTreeSet};

use gantry_api::settings::SettingsContext;
use gantry_core::entities::{SettingsResource, SettingsScope};
use gantry_core::level::{Effort, LevelKey};
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::settings::{AdapterCapability, ModelCapability};
use gantryd::settings_ctx::{RuntimeSettingsContext, SettingsSnapshot};
use serde_json::{json, Map, Value};

fn snapshot() -> SettingsSnapshot {
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        ("anthropic".to_owned(), "utility-from-settings".to_owned()),
        ModelCapability {
            tools: false,
            reasoning_efforts: vec![Effort::Low, Effort::High],
        },
    );
    let mut adapters = BTreeMap::new();
    adapters.insert(
        "claude-code".to_owned(),
        AdapterCapability {
            provider_kinds: vec!["anthropic".to_owned()],
            supports_fallback: false,
        },
    );
    let mut graph_templates = BTreeMap::new();
    graph_templates.insert(LevelKey::Low, vec!["low.single".to_owned()]);
    let mut secrets = BTreeSet::new();
    secrets.insert((
        Consumer::Egress,
        SecretName::parse("model.anthropic.main").expect("secret name"),
    ));
    let mut modules = Map::new();
    modules.insert(
        "engine".to_owned(),
        json!({"applied_rev": 1, "health": "ok", "unresolved_secret_refs": []}),
    );
    SettingsSnapshot {
        catalog: json!({"schema_version": 1, "models": {"capabilities": {}}}),
        resources: vec![(
            (SettingsResource::Models, SettingsScope::Global),
            json!({"worker": {"provider": "anthropic", "model": "worker-model"}}),
        )],
        graph_templates,
        capabilities,
        adapters,
        secrets,
        modules,
    }
}

#[test]
fn validation_context_uses_the_snapshot() {
    let context = RuntimeSettingsContext::new(snapshot());
    let validation = context.validation_ctx();
    let capability = validation
        .model_capability("anthropic", "utility-from-settings")
        .expect("capability");
    assert_eq!(
        capability.reasoning_efforts,
        vec![Effort::Low, Effort::High]
    );
    assert!(validation
        .model_capability("anthropic", "missing")
        .is_none());
    assert_eq!(validation.graph_templates(LevelKey::Low), ["low.single"]);
    assert!(validation.secret_exists(
        Consumer::Egress,
        &SecretName::parse("model.anthropic.main").expect("secret")
    ));
    assert!(!validation.secret_exists(
        Consumer::Forge,
        &SecretName::parse("model.anthropic.main").expect("secret")
    ));
    let resource = validation
        .resource(SettingsResource::Models, &SettingsScope::Global)
        .expect("models resource");
    assert_eq!(resource["worker"]["model"], "worker-model");
}

#[tokio::test]
async fn catalog_and_status_are_returned_verbatim() {
    let context = RuntimeSettingsContext::new(snapshot());
    let Value::Object(catalog) = context.catalog().await.expect("catalog") else {
        panic!("catalog must be an object");
    };
    assert_eq!(catalog["schema_version"], 1);
    let modules = context.status_modules().await.expect("status");
    assert_eq!(modules["engine"]["health"], "ok");
}
