//! `EngineApi` 的生产适配（#105 A7）：把 conversation 的命令翻译成
//! [`gantry_engine::engine::Engine`] 的写入口与定位读取。
//!
//! 只支持 M1 的 Low 模板族：`Engine::create_delivery` 会拒绝非 Low 的上限。
//! Issue 引用由 `Ref::Issue` 补齐最小 `IssueRef`（同步离线，`synced_at` 取生成 ID 的
//! 时间）；完整 Issue 快照同步归后续里程碑。

use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{IssueRef, IssueState, Ref, Seq};
use gantry_core::ids::{DeliveryId, Dlv, IdError, IdGen, ProjectId};
use gantry_core::level::Level;
use gantry_core::ports::engine::{CreateDelivery, EngineError};
use gantry_core::settings::resource::LevelEntry;
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::{DeliverySummary, Engine, EngineError as EngineCoreError};
use gantry_store::StoreError as EngineStoreError;

use crate::ports::{DeliveryHandle, EngineApi};

/// 生产适配：`conversation` 依赖 `engine`（P-59），trait 定义在消费方。
pub struct EngineApiAdapter {
    engine: Arc<Engine>,
    ids: Arc<IdGen>,
    low: LevelEntry,
    config_rev: Seq,
}

impl EngineApiAdapter {
    pub fn new(engine: Arc<Engine>, ids: Arc<IdGen>, low: LevelEntry, config_rev: Seq) -> Self {
        Self {
            engine,
            ids,
            low,
            config_rev,
        }
    }
}

fn store_error(error: EngineStoreError) -> gantry_core::ports::StoreError {
    match error {
        EngineStoreError::Busy
        | EngineStoreError::DiskFull
        | EngineStoreError::AlreadyRunning
        | EngineStoreError::SchemaTooNew { .. } => {
            gantry_core::ports::StoreError::Unavailable { retry_after: None }
        }
        other => gantry_core::ports::StoreError::InvalidState {
            message: other.to_string(),
        },
    }
}

fn engine_error(error: EngineCoreError) -> EngineError {
    match error {
        EngineCoreError::NotConfigured => EngineError::NotConfigured,
        EngineCoreError::Store(error) => EngineError::Store(store_error(error)),
        EngineCoreError::NotFound => EngineError::NotFound,
        EngineCoreError::Validation(message) => EngineError::Validation {
            pointer: "engine".into(),
            message: message.into(),
        },
        other => EngineError::InvalidState {
            message: other.to_string(),
        },
    }
}

fn id_error(error: IdError) -> EngineError {
    EngineError::InvalidState {
        message: format!("id generation failed: {error}"),
    }
}

fn handle(summary: DeliverySummary) -> DeliveryHandle {
    DeliveryHandle {
        id: summary.id,
        project_id: summary.project_id,
        level_cap: summary.level_cap,
        stage: summary.stage,
        open_spec_needed: summary.open_spec_needed,
    }
}

#[async_trait]
impl EngineApi for EngineApiAdapter {
    async fn create_delivery(&self, cmd: CreateDelivery) -> Result<DeliveryHandle, EngineError> {
        let id: DeliveryId = self.ids.next::<Dlv>().map_err(id_error)?;
        let now = Timestamp::from_millis(id.timestamp_ms() as i64);
        let issues = cmd
            .refs
            .iter()
            .filter_map(|reference| match reference {
                Ref::Issue { repo, number } => Some(IssueRef {
                    repo: repo.clone(),
                    number: *number,
                    node_id: format!("I_{number}"),
                    title: UntrustedText::new(format!("#{number}"), "github".into(), false),
                    state: IssueState::Open,
                    state_reason: None,
                    synced_at: now,
                }),
                _ => None,
            })
            .collect();
        let new = NewDelivery {
            id,
            project_id: cmd.project_id,
            title: cmd.title,
            user_text: cmd.user_text,
            level_cap: cmd.level_cap,
            level_effective: Level::Low,
            origin: cmd.origin,
            source_message_id: cmd.source_message_id,
            issues,
            budget: budget_from_level(&self.low),
            config_rev: self.config_rev,
            actor: cmd.ctx.actor,
        };
        let id = self
            .engine
            .create_delivery(new)
            .await
            .map_err(engine_error)?;
        let summary = self
            .engine
            .delivery_summary(&id)
            .await
            .map_err(engine_error)?
            .ok_or(EngineError::NotFound)?;
        Ok(handle(summary))
    }

    async fn locate_deliveries(
        &self,
        project: &ProjectId,
        refs: &[Ref],
    ) -> Result<Vec<DeliveryHandle>, EngineError> {
        Ok(self
            .engine
            .locate_deliveries(project, refs)
            .await
            .map_err(engine_error)?
            .into_iter()
            .map(handle)
            .collect())
    }
}
