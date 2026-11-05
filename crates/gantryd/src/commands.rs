//! 生产控制通道命令（除 `status` 外）：引导期设置写入。
//!
//! 与 `deploy` 的 CLI（`gantry settings bootstrap-set`）对应；服务端只接受
//! `core::bootstrap_writable` 白名单里的键，M1 实际只开放 `sandbox.base_image`
//! （K10：点号子路径的嵌套对象留到 M2）。

use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{Actor, ActorKind, SettingsResource, SettingsScope};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::settings::{
    bootstrap_writable, builtin_defaults, parse_resolved, SETTINGS_SCHEMA_VERSION,
};
use gantry_pki::directory::DeviceDirectory;
use gantry_store::Store;
use serde_json::{json, Value};

use crate::control::{CommandSet, Response};

/// 生产命令组；`serve` 把它与调用方提供的命令一起注册。
pub struct ProductionCommands {
    store: Store,
    directory: Arc<dyn DeviceDirectory>,
}

impl ProductionCommands {
    pub fn new(store: Store, directory: Arc<dyn DeviceDirectory>) -> Self {
        Self { store, directory }
    }

    async fn bootstrap_set(&self, args: &Value) -> Response {
        match self.directory.bootstrap_period().await {
            Ok(true) => {}
            Ok(false) => {
                return Response::err(
                    "not_bootstrap_period",
                    "a device has been paired; use the desktop settings page",
                )
            }
            Err(error) => return Response::err("unavailable", error.to_string()),
        }
        let Some(resource) = args.get("resource").and_then(Value::as_str) else {
            return Response::err("bad_request", "expected {resource, values}");
        };
        let Ok(resource_id) = serde_json::from_value::<SettingsResource>(json!(resource)) else {
            return Response::err("bad_request", format!("unknown resource {resource}"));
        };
        let Some(values) = args.get("values").and_then(Value::as_object) else {
            return Response::err("bad_request", "expected {resource, values}");
        };
        if values.is_empty() {
            return Response::err("bad_request", "values must not be empty");
        }
        let mut applied = Vec::new();
        for (key, value) in values {
            if !bootstrap_key(key) {
                return Response::err("bad_request", format!("invalid key {key}"));
            }
            if !value.is_string() {
                return Response::err("bad_request", format!("value for {key} must be a string"));
            }
            let path = format!("{resource}.{key}");
            if !bootstrap_writable(resource_id, &path) {
                return Response::err(
                    "not_writable",
                    format!("{path} is not writable during bootstrap"),
                );
            }
            if path != "sandbox.base_image" {
                return Response::err(
                    "not_writable",
                    format!("{path} is not a string setting; use github setup"),
                );
            }
            applied.push(key.clone());
        }

        let snapshot = match self.store.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return Response::err("unavailable", error.to_string()),
        };
        let entry =
            snapshot.settings_current_for(&SettingsScope::Global, None, SettingsResource::Sandbox);
        let mut doc = entry
            .value
            .unwrap_or_else(|| builtin_defaults(SettingsResource::Sandbox));
        let Some(object) = doc.as_object_mut() else {
            return Response::err("unavailable", "sandbox settings are not an object");
        };
        for (key, value) in values {
            object.insert(
                key.clone(),
                Value::String(value.as_str().unwrap_or_default().to_owned()),
            );
        }
        if parse_resolved(SettingsResource::Sandbox, &doc).is_err() {
            return Response::err("validation_failed", "sandbox settings are invalid");
        }

        let mut tx = match self.store.begin_write().await {
            Ok(tx) => tx,
            Err(error) => return Response::err("unavailable", error.to_string()),
        };
        let event = NewEvent {
            ty: EventType::SettingsUpdated,
            subject: Subject::parse("settings").expect("settings subject"),
            project_id: None,
            actor: Actor::new(ActorKind::System, "gantryd"),
            causation_id: None,
            correlation_id: None,
            data: json!({
                "scope": "global",
                "resource": resource_id,
                "schema_version": SETTINGS_SCHEMA_VERSION,
                "base_rev": entry.rev.as_u64(),
                "value": doc,
                "relaxes": [],
                "secret_changes": []
            }),
            dedup_key: None,
        };
        if let Err(error) = tx.append(event).await {
            return Response::err("unavailable", error.to_string());
        }
        if let Err(error) = tx.commit().await {
            return Response::err("unavailable", error.to_string());
        }
        Response::ok(json!({ "resource": resource, "applied": applied }))
    }
}

#[async_trait]
impl CommandSet for ProductionCommands {
    async fn handle(&self, cmd: &str, args: &Value) -> Option<Response> {
        match cmd {
            "settings.bootstrap_set" => Some(self.bootstrap_set(args).await),
            _ => None,
        }
    }
}

/// 引导期写入的资源内相对键：小写字母、数字、`_`、`.`，长度 ≤ 64。
fn bootstrap_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// `github.setup` 回调完成后写 `github` 设置（config §2.5）：换回的非密钥字段与密钥引用名。
pub struct StoreGithubSettingsWriter {
    store: Store,
}

impl StoreGithubSettingsWriter {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl gantry_forge::setup::GithubSettingsWriter for StoreGithubSettingsWriter {
    async fn write_app(
        &self,
        settings: &gantry_forge::setup::GithubAppSettings,
    ) -> Result<(), String> {
        let snapshot = self
            .store
            .snapshot()
            .await
            .map_err(|error| error.to_string())?;
        let entry =
            snapshot.settings_current_for(&SettingsScope::Global, None, SettingsResource::Github);
        let mut doc = entry
            .value
            .unwrap_or_else(|| builtin_defaults(SettingsResource::Github));
        let object = doc
            .as_object_mut()
            .ok_or_else(|| "github settings are not an object".to_owned())?;
        let app = object
            .entry("app".to_owned())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| "github.app is not an object".to_owned())?;
        app.insert("app_id".to_owned(), json!(settings.app_id));
        app.insert(
            "private_key".to_owned(),
            json!({"secret_ref": settings.private_key_ref}),
        );
        app.insert(
            "webhook_secret".to_owned(),
            json!({"secret_ref": settings.webhook_secret_ref}),
        );
        object.insert(
            "commit_identity".to_owned(),
            json!({
                "name": settings.commit_identity_name,
                "email": settings.commit_identity_email,
            }),
        );
        if parse_resolved(SettingsResource::Github, &doc).is_err() {
            return Err("github settings are invalid".to_owned());
        }

        let mut tx = self
            .store
            .begin_write()
            .await
            .map_err(|error| error.to_string())?;
        let event = NewEvent {
            ty: EventType::SettingsUpdated,
            subject: Subject::parse("settings").expect("settings subject"),
            project_id: None,
            actor: Actor::new(ActorKind::System, "gantryd"),
            causation_id: None,
            correlation_id: None,
            data: json!({
                "scope": "global",
                "resource": SettingsResource::Github,
                "schema_version": SETTINGS_SCHEMA_VERSION,
                "base_rev": entry.rev.as_u64(),
                "value": doc,
                "relaxes": [],
                "secret_changes": []
            }),
            dedup_key: None,
        };
        tx.append(event).await.map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(())
    }
}
