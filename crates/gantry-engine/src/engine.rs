//! 引擎写入口的最小实现：把 `delivery.created` 写入事件日志（engine.md §7.1）。
//!
//! 只做创建事务；决策器、调度与副作用执行仍未实现。

use gantry_core::entities::{Actor, Stage};
use gantry_core::events::{DeliveryStageChangedData, EventType, NewEvent, SchemaError, Subject};
use gantry_core::ids::{DeliveryId, IdError, ProjectId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::StageTrigger;
use gantry_core::machines::TransitionError;
use gantry_store::{Store, StoreError};

use crate::create::{created_event, NewDelivery};
use crate::stage::{advance, StageTransition};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("contract: {0}")]
    Contract(#[from] SchemaError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("id: {0}")]
    Id(#[from] IdError),
    #[error("stage: {0}")]
    Stage(#[from] TransitionError),
}

pub struct StageRequest {
    pub delivery_id: DeliveryId,
    pub project_id: ProjectId,
    pub from: Stage,
    pub level_effective: Level,
    pub trigger: StageTrigger,
    pub reason: Option<String>,
    pub actor: Actor,
}

pub struct Engine {
    store: Store,
}

impl Engine {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 在单个写事务中追加 `delivery.created`；相同 `source_message_id` 不重复追加。
    pub async fn create_delivery(&self, delivery: NewDelivery) -> Result<DeliveryId, EngineError> {
        let id_text = delivery.id.to_string();
        let event = created_event(delivery)?;
        let mut tx = self.store.begin_write().await?;
        tx.append(event).await?;
        tx.commit().await?;
        Ok(id_text.parse()?)
    }

    /// 校验转移并追加 `delivery.stage_changed`。
    pub async fn advance_stage(
        &self,
        request: StageRequest,
    ) -> Result<StageTransition, EngineError> {
        let transition = advance(request.from, request.level_effective, request.trigger, None)?;
        let subject = Subject::parse(&request.delivery_id.to_string())
            .map_err(|error| SchemaError::shape("/subject", error.to_string()))?;
        let event = NewEvent {
            ty: EventType::DeliveryStageChanged,
            subject,
            project_id: Some(request.project_id),
            actor: request.actor,
            causation_id: None,
            correlation_id: Some(request.delivery_id.to_string()),
            data: serde_json::to_value(DeliveryStageChangedData {
                from: transition.from,
                to: transition.to,
                reason: request.reason,
                stage_status: transition.status,
            })
            .map_err(|error| SchemaError::shape("/data", error.to_string()))?,
            dedup_key: None,
        };
        event.validate()?;
        let mut tx = self.store.begin_write().await?;
        tx.append(event).await?;
        tx.commit().await?;
        Ok(transition)
    }
}
