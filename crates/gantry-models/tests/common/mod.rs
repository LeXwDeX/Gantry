//! 集成测试共用：内存 store 与 `settings.updated` 写入。
#![allow(dead_code)]

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, SettingsResource, SettingsScope};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{Store, StoreDeps};
use serde_json::{json, Value};

pub const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
pub const OTHER_PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZB";
pub const START_MS: i64 = 1_793_000_000_000;

pub fn project() -> ProjectId {
    PROJECT.parse().unwrap()
}

pub fn other_project() -> ProjectId {
    OTHER_PROJECT.parse().unwrap()
}

pub fn store() -> (Store, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new(START_MS));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids,
    })
    .unwrap();
    (store, clock)
}

/// 追加一条 `settings.updated`；`value` 是该作用域的完整存储文档。
pub async fn put_settings(
    store: &Store,
    project: Option<&ProjectId>,
    resource: SettingsResource,
    value: Value,
) {
    let scope = if project.is_some() {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    };
    let base_rev = store
        .snapshot_sync()
        .unwrap()
        .settings_current_for(&scope, project, resource)
        .rev;
    let subject = match project {
        Some(project) => project.to_string(),
        None => "settings".to_owned(),
    };
    let event = NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::parse(&subject).unwrap(),
        project_id: project.copied(),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data: json!({
            "scope": if project.is_some() { "project" } else { "global" },
            "resource": format!("{resource:?}").to_lowercase(),
            "schema_version": 1,
            "base_rev": base_rev.as_u64(),
            "value": value,
            "relaxes": [],
            "secret_changes": [],
        }),
        dedup_key: None,
    };
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event).await.unwrap();
    tx.commit().await.unwrap();
}

/// 一份完整配置的全局 models 文档（模型 id 是占位符）。
pub fn global_models() -> Value {
    json!({
        "providers": {
            "anthropic": {
                "kind": "anthropic",
                "base_url": "https://api.anthropic.com",
                "credential": {"type": "api_key", "secret_ref": "model.anthropic.main"}
            }
        },
        "utility": {"provider": "anthropic", "model": "utility-model"},
        "planner": {"provider": "anthropic", "model": "planner-model"},
        "worker": {"provider": "anthropic", "model": "worker-model"}
    })
}
