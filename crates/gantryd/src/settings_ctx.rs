//! `api` 的 settings 上下文（config.md §2.8、api.md §6.3）。
//!
//! 装配层把只读快照交给本类型：能力表、各 resource 的当前值、密钥存在性、模板登记与
//! 模块状态。`ValidationContext` 是同步接口，因此快照必须在构造时准备好。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use gantry_api::settings::SettingsContext;
use gantry_core::entities::{SettingsResource, SettingsScope};
use gantry_core::level::LevelKey;
use gantry_core::ports::ModuleError;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::settings::{AdapterCapability, ModelCapability, ValidationContext};
use serde_json::{Map, Value};

/// settings 校验与展示需要的只读数据。
#[derive(Clone, Default)]
pub struct SettingsSnapshot {
    /// `/v1/settings/catalog` 的完整响应（`models` / `levels` / `policies` 等）。
    pub catalog: Value,
    /// 每个 resource 当前生效的文档；键是 `(resource, scope)`。
    pub resources: Vec<((SettingsResource, SettingsScope), Value)>,
    /// `levels.graph_templates` 的已实现模板。
    pub graph_templates: BTreeMap<LevelKey, Vec<String>>,
    pub capabilities: BTreeMap<(String, String), ModelCapability>,
    pub adapters: BTreeMap<String, AdapterCapability>,
    /// 已存在的密钥（consumer + 名称）。
    pub secrets: BTreeSet<(Consumer, SecretName)>,
    /// `/v1/settings/status` 的 `modules` 对象。
    pub modules: Map<String, Value>,
}

/// `SettingsContext` 的生产实现；只读快照，不访问 store 或外部服务。
pub struct RuntimeSettingsContext {
    snapshot: Arc<SettingsSnapshot>,
}

impl RuntimeSettingsContext {
    pub fn new(snapshot: SettingsSnapshot) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
        }
    }
}

impl ValidationContext for RuntimeSettingsContext {
    fn model_capability(&self, provider: &str, model: &str) -> Option<ModelCapability> {
        self.snapshot
            .capabilities
            .get(&(provider.to_owned(), model.to_owned()))
            .cloned()
    }

    fn adapter(&self, name: &str) -> Option<AdapterCapability> {
        self.snapshot.adapters.get(name).cloned()
    }

    fn graph_templates(&self, level: LevelKey) -> &[String] {
        self.snapshot
            .graph_templates
            .get(&level)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn secret_exists(&self, consumer: Consumer, name: &SecretName) -> bool {
        self.snapshot.secrets.contains(&(consumer, name.clone()))
    }

    fn resource(&self, resource: SettingsResource, scope: &SettingsScope) -> Option<&Value> {
        self.snapshot
            .resources
            .iter()
            .find(|((candidate, candidate_scope), _)| {
                *candidate == resource && candidate_scope == scope
            })
            .map(|(_, value)| value)
    }

    fn host_memory_mb(&self) -> Option<u64> {
        None
    }
}

#[async_trait]
impl SettingsContext for RuntimeSettingsContext {
    fn validation_ctx(&self) -> Arc<dyn ValidationContext + Send + Sync> {
        Arc::new(Self {
            snapshot: self.snapshot.clone(),
        })
    }

    async fn catalog(&self) -> Result<Value, ModuleError> {
        Ok(self.snapshot.catalog.clone())
    }

    async fn status_modules(&self) -> Result<Map<String, Value>, ModuleError> {
        Ok(self.snapshot.modules.clone())
    }
}

/// 从 store、engine、egress 与 secrets 构造只读快照。
///
/// `forge_names` / `egress_names` 是当前设置里引用到的密钥名；存在性按使用方分别查询
/// （config.md §2.4）。没有 status API 的模块如实报 `unhealthy`，不虚构 `ok`。
pub async fn build_settings_context(
    store: &gantry_store::Store,
    engine: &gantry_engine::runtime::EngineRuntime,
    egress: &dyn gantry_core::ports::egress::EgressControl,
    egress_names: &[SecretName],
    forge_secrets: &dyn gantry_core::ports::secrets::SecretSource,
    forge_names: &[SecretName],
    catalog: Value,
) -> Result<RuntimeSettingsContext, ModuleError> {
    const RESOURCES: [SettingsResource; 8] = [
        SettingsResource::Models,
        SettingsResource::Levels,
        SettingsResource::Policies,
        SettingsResource::Guardian,
        SettingsResource::Sandbox,
        SettingsResource::Egress,
        SettingsResource::Notify,
        SettingsResource::Github,
    ];

    let snapshot = store.snapshot().await.map_err(|error| {
        ModuleError::Store(gantry_core::ports::StoreError::InvalidState {
            message: error.to_string(),
        })
    })?;
    let mut resources = Vec::new();
    for resource in RESOURCES {
        let entry = snapshot.settings_current_for(&SettingsScope::Global, None, resource);
        if let Some(value) = entry.value {
            resources.push(((resource, SettingsScope::Global), value));
        }
    }

    let mut secrets = BTreeSet::new();
    for name in forge_names {
        if forge_secrets.present(name).unwrap_or(false) {
            secrets.insert((Consumer::Forge, name.clone()));
        }
    }
    if !egress_names.is_empty() {
        let present = egress.secret_present(egress_names).await.map_err(|error| {
            ModuleError::Unavailable {
                message: error.to_string(),
            }
        })?;
        for name in egress_names {
            if present.get(name).copied().unwrap_or(false) {
                secrets.insert((Consumer::Egress, name.clone()));
            }
        }
    }

    let (capabilities, adapters) = parse_catalog(&catalog);
    let mut graph_templates = BTreeMap::new();
    graph_templates.insert(LevelKey::Low, engine.engine().template_catalog().templates);

    let mut modules = Map::new();
    match engine.status().await {
        Ok(status) => {
            modules.insert(
                "engine".to_owned(),
                serde_json::json!({
                    "applied_rev": status.applied_rev.as_u64(),
                    "health": if status.degraded.is_empty() { "ok" } else { "degraded" },
                    "detail": if status.degraded.is_empty() { Value::Null } else {
                        Value::String(format!("{} degraded deliveries", status.degraded.len()))
                    },
                    "unresolved_secret_refs": [],
                }),
            );
        }
        Err(error) => {
            modules.insert(
                "engine".to_owned(),
                serde_json::json!({
                    "applied_rev": 0,
                    "health": "unhealthy",
                    "detail": error.to_string(),
                    "unresolved_secret_refs": [],
                }),
            );
        }
    }
    match egress.status().await {
        Ok(status) => {
            modules.insert(
                "egress".to_owned(),
                serde_json::json!({
                    "applied_rev": status.applied_rev.as_u64(),
                    "health": match status.health {
                        gantry_core::ports::Health::Ok => "ok",
                        gantry_core::ports::Health::Degraded => "degraded",
                        gantry_core::ports::Health::Unhealthy => "unhealthy",
                    },
                    "detail": status.detail,
                    "unresolved_secret_refs": status
                        .unresolved_secret_refs
                        .iter()
                        .map(|name| name.to_string())
                        .collect::<Vec<_>>(),
                }),
            );
        }
        Err(error) => {
            modules.insert(
                "egress".to_owned(),
                serde_json::json!({
                    "applied_rev": 0,
                    "health": "unhealthy",
                    "detail": error.to_string(),
                    "unresolved_secret_refs": [],
                }),
            );
        }
    }
    for module in ["models", "policy", "guardian", "sandbox", "notify", "forge"] {
        modules.insert(
            module.to_owned(),
            serde_json::json!({
                "applied_rev": 0,
                "health": "unhealthy",
                "detail": "module has no status source in this slice",
                "unresolved_secret_refs": [],
            }),
        );
    }

    Ok(RuntimeSettingsContext::new(SettingsSnapshot {
        catalog,
        resources,
        graph_templates,
        capabilities,
        adapters,
        secrets,
        modules,
    }))
}

fn parse_catalog(
    catalog: &Value,
) -> (
    BTreeMap<(String, String), ModelCapability>,
    BTreeMap<String, AdapterCapability>,
) {
    let mut capabilities = BTreeMap::new();
    if let Some(models) = catalog["models"]["capabilities"].as_object() {
        for (key, value) in models {
            let Some((provider, model)) = key.split_once('/') else {
                continue;
            };
            let efforts = value["reasoning_efforts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|effort| serde_json::from_value(effort.clone()).ok())
                .collect();
            capabilities.insert(
                (provider.to_owned(), model.to_owned()),
                ModelCapability {
                    tools: value["tools"].as_bool().unwrap_or(false),
                    reasoning_efforts: efforts,
                },
            );
        }
    }
    let mut adapters = BTreeMap::new();
    if let Some(list) = catalog["models"]["adapters"].as_object() {
        for (name, value) in list {
            adapters.insert(
                name.clone(),
                AdapterCapability {
                    provider_kinds: value["provider_kinds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|kind| kind.as_str().map(str::to_owned))
                        .collect(),
                    supports_fallback: value["supports_fallback"].as_bool().unwrap_or(false),
                },
            );
        }
    }
    (capabilities, adapters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_parsing_reads_capabilities_and_adapters() {
        let catalog = serde_json::json!({
            "models": {
                "capabilities": {
                    "anthropic/utility": {"tools": false, "reasoning_efforts": ["low", "high"]},
                    "broken": {"tools": true}
                },
                "adapters": {
                    "claude-code": {"provider_kinds": ["anthropic"], "supports_fallback": false}
                }
            }
        });
        let (capabilities, adapters) = parse_catalog(&catalog);
        let capability = capabilities
            .get(&("anthropic".to_owned(), "utility".to_owned()))
            .expect("capability");
        assert!(!capability.tools);
        assert_eq!(
            capability.reasoning_efforts,
            vec![
                gantry_core::level::Effort::Low,
                gantry_core::level::Effort::High
            ]
        );
        assert!(!capabilities.contains_key(&("broken".to_owned(), "broken".to_owned())));
        assert_eq!(adapters["claude-code"].provider_kinds, ["anthropic"]);
        assert!(!adapters["claude-code"].supports_fallback);
    }
}
