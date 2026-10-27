//! `SettingsContext` 的替身（本文 §12.1 `FakeSettingsContext`）：能力表从
//! `fixtures/api/catalog.json` 读出，密钥存在性可设定，模块状态按夹具返回。

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gantry_api::settings::SettingsContext;
use gantry_core::entities::{SettingsResource, SettingsScope};
use gantry_core::level::{Effort, LevelKey};
use gantry_core::ports::ModuleError;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::settings::{AdapterCapability, ModelCapability, ValidationContext};
use serde_json::{Map, Value};

const CATALOG: &str = include_str!("../../../../fixtures/api/catalog.json");
const STATUS: &str = include_str!("../../../../fixtures/api/settings-status.json");

#[derive(Default)]
pub struct FakeSettingsContext {
    pub existing: Mutex<BTreeSet<String>>,
}

struct Ctx {
    catalog: Value,
    existing: BTreeSet<String>,
}

impl ValidationContext for Ctx {
    fn model_capability(&self, provider: &str, model: &str) -> Option<ModelCapability> {
        let cap = &self.catalog["models"]["capabilities"][format!("{provider}/{model}")];
        if cap.is_null() {
            return None;
        }
        Some(ModelCapability {
            tools: cap["tools"].as_bool().unwrap_or(false),
            reasoning_efforts: cap["reasoning_efforts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| serde_json::from_value::<Effort>(e.clone()).ok())
                .collect(),
        })
    }

    fn adapter(&self, name: &str) -> Option<AdapterCapability> {
        let a = &self.catalog["models"]["adapters"][name];
        if a.is_null() {
            return None;
        }
        Some(AdapterCapability {
            provider_kinds: a["provider_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect(),
            supports_fallback: a["supports_fallback"].as_bool().unwrap_or(false),
        })
    }

    fn graph_templates(&self, _level: LevelKey) -> &[String] {
        &[]
    }

    fn secret_exists(&self, _consumer: Consumer, name: &SecretName) -> bool {
        self.existing.contains(name.as_str())
    }

    fn resource(&self, _resource: SettingsResource, _scope: &SettingsScope) -> Option<&Value> {
        None
    }

    fn host_memory_mb(&self) -> Option<u64> {
        None
    }
}

#[async_trait]
impl SettingsContext for FakeSettingsContext {
    fn validation_ctx(&self) -> Arc<dyn ValidationContext + Send + Sync> {
        Arc::new(Ctx {
            catalog: serde_json::from_str(CATALOG).unwrap(),
            existing: self.existing.lock().unwrap().clone(),
        })
    }

    async fn catalog(&self) -> Result<Value, ModuleError> {
        Ok(serde_json::from_str(CATALOG).unwrap())
    }

    async fn status_modules(&self) -> Result<Map<String, Value>, ModuleError> {
        let status: Value = serde_json::from_str(STATUS).unwrap();
        Ok(status["modules"].as_object().unwrap().clone())
    }
}
