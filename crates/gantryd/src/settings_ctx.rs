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
