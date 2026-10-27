//! 设置来源与热加载（models.md §7.8）。
//!
//! 生效值 = `core::settings::effective(内置默认, 全局, 项目覆盖)`；本模块只读，
//! 不写任何设置（I7）。视图按 `config_rev` 缓存：修订变化后在下一次调用前重建并
//! 原子替换；进行中的调用继续持有旧的 `Arc<ModelsView>`。构建失败时保留旧视图并
//! 报告 `degraded`。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gantry_core::entities::{Seq, SettingsResource, SettingsScope};
use gantry_core::ids::ProjectId;
use gantry_core::ports::context::Health;
use gantry_core::ports::engine::ModuleStatus;
use gantry_core::settings::{builtin_defaults, effective, Levels, Models};
use gantry_store::Store;
use serde_json::Value;

use crate::error::ModelsError;
use crate::view::ModelsView;

/// 某个作用域（全局或项目）在某一修订下的生效文档。
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveDocs {
    pub rev: Seq,
    pub models: Value,
    pub levels: Value,
}

/// 生效设置的来源。生产实现读 `store`；测试可以注入自己的实现。
#[async_trait]
pub trait SettingsSource: Send + Sync {
    /// 当前修订号；只用于判断是否需要重建视图。
    async fn rev(&self, project: Option<&ProjectId>) -> Result<Seq, ModelsError>;

    /// 当前的生效文档（项目覆盖已按 `core` 规则合并与收紧）。
    async fn effective(&self, project: Option<&ProjectId>) -> Result<EffectiveDocs, ModelsError>;
}

/// 从 `store` 的设置投影读取。
#[derive(Clone)]
pub struct StoreSettings {
    store: Store,
}

impl StoreSettings {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

fn store_error(error: gantry_store::StoreError) -> ModelsError {
    ModelsError::Internal {
        reason: format!("store: {error}"),
    }
}

#[async_trait]
impl SettingsSource for StoreSettings {
    async fn rev(&self, project: Option<&ProjectId>) -> Result<Seq, ModelsError> {
        let snapshot = self.store.snapshot().await.map_err(store_error)?;
        Ok(snapshot.config_rev(project))
    }

    async fn effective(&self, project: Option<&ProjectId>) -> Result<EffectiveDocs, ModelsError> {
        let snapshot = self.store.snapshot().await.map_err(store_error)?;
        let doc = |resource: SettingsResource| {
            let global = snapshot
                .settings_current(&SettingsScope::Global, resource)
                .value
                .unwrap_or_else(|| Value::Object(Default::default()));
            let project_doc = project.and_then(|project| {
                snapshot
                    .settings_current_for(&SettingsScope::Project, Some(project), resource)
                    .value
            });
            effective(
                resource,
                &builtin_defaults(resource),
                &global,
                project_doc.as_ref(),
            )
            .value
        };
        Ok(EffectiveDocs {
            rev: snapshot.config_rev(project),
            models: doc(SettingsResource::Models),
            levels: doc(SettingsResource::Levels),
        })
    }
}

/// 解析生效文档并构建视图。
pub fn build_view(docs: &EffectiveDocs) -> Result<ModelsView, ModelsError> {
    let models: Models = serde_json::from_value(docs.models.clone())
        .map_err(|error| ModelsError::config_invalid("/models", error.to_string()))?;
    let levels: Levels = serde_json::from_value(docs.levels.clone())
        .map_err(|error| ModelsError::config_invalid("/levels", error.to_string()))?;
    ModelsView::build(models, &levels, docs.rev)
}

struct CacheState {
    views: HashMap<Option<ProjectId>, Arc<ModelsView>>,
    /// 最近一次构建失败的修订与原因；成功构建后清除。
    failure: Option<(Seq, String)>,
    applied_rev: Seq,
}

/// 按作用域缓存的视图（热加载）。
pub struct ViewCache {
    source: Arc<dyn SettingsSource>,
    state: Mutex<CacheState>,
}

impl ViewCache {
    pub fn new(source: Arc<dyn SettingsSource>) -> Self {
        Self {
            source,
            state: Mutex::new(CacheState {
                views: HashMap::new(),
                failure: None,
                applied_rev: Seq::ZERO,
            }),
        }
    }

    /// 取该作用域的当前视图；修订变化时先重建。构建失败而有旧视图 → 返回旧视图；
    /// 没有旧视图 → 返回构建错误。
    pub async fn view(&self, project: Option<&ProjectId>) -> Result<Arc<ModelsView>, ModelsError> {
        let key = project.copied();
        let rev = self.source.rev(project).await?;
        {
            let state = self.lock();
            if let Some(view) = state.views.get(&key) {
                if view.rev() == rev {
                    return Ok(view.clone());
                }
            }
            if let Some((failed_rev, _)) = &state.failure {
                if *failed_rev == rev {
                    if let Some(view) = state.views.get(&key) {
                        return Ok(view.clone());
                    }
                }
            }
        }
        let built = match self.source.effective(project).await {
            Ok(docs) => build_view(&docs),
            Err(error) => Err(error),
        };
        let mut state = self.lock();
        match built {
            Ok(view) => {
                let view = Arc::new(view);
                state.applied_rev = state.applied_rev.max(view.rev());
                state.failure = None;
                state.views.insert(key, view.clone());
                Ok(view)
            }
            Err(error) => {
                state.failure = Some((rev, error.to_string()));
                match state.views.get(&key) {
                    Some(old) => Ok(old.clone()),
                    None => Err(error),
                }
            }
        }
    }

    /// `settings/status` 的 `modules.models`。
    pub fn status(&self) -> ModuleStatus {
        let state = self.lock();
        match &state.failure {
            Some((_, reason)) => ModuleStatus {
                applied_rev: state.applied_rev,
                health: Health::Degraded,
                detail: Some(reason.clone()),
            },
            None => ModuleStatus {
                applied_rev: state.applied_rev,
                health: Health::Ok,
                detail: None,
            },
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
