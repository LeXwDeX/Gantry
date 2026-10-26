//! 引擎写入口的最小实现：创建、阶段推进与交付控制写入事件日志（engine.md §7.1）。
//!
//! 决策器、调度与副作用执行仍未实现。

use gantry_core::entities::{Actor, Seq, Stage, StageStatus};
use gantry_core::events::{
    DeliveryAbandonedData, DeliveryPausedData, DeliveryResumedData, DeliveryStageChangedData,
    EventType, NewEvent, SchemaError, Subject,
};
use gantry_core::ids::{DeliveryId, IdError, ProjectId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::StageTrigger;
use gantry_core::machines::TransitionError;
use gantry_store::{EventFilter, Store, StoreError};

use crate::create::{created_event, NewDelivery};
use crate::projection::{DeliveryProjection, Projection, ProjectionError};
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
    #[error("projection: {0}")]
    Projection(#[from] ProjectionError),
    #[error("delivery not found")]
    NotFound,
    #[error("invalid state: {0}")]
    InvalidState(&'static str),
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

    /// 读取一条交付的投影；不存在时返回 `None`。
    pub async fn load_delivery(
        &self,
        id: &DeliveryId,
    ) -> Result<Option<DeliveryProjection>, EngineError> {
        let target = id.to_string();
        let filter = EventFilter {
            project_ids: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let mut projection = Projection::default();
        let mut cursor = Seq::ZERO;
        loop {
            let page = self.store.events().scan(cursor, &filter, 200).await?;
            for event in &page.events {
                if event.correlation_id.as_deref() == Some(target.as_str()) {
                    projection.apply(event)?;
                }
            }
            if page.scanned_to >= page.head {
                break;
            }
            cursor = page.scanned_to;
        }
        Ok(projection.delivery)
    }

    /// 暂停：停止派发新节点，记录暂停前的 `stage_status`。已暂停 → 幂等返回。
    pub async fn pause(
        &self,
        id: DeliveryId,
        actor: Actor,
    ) -> Result<DeliveryProjection, EngineError> {
        let current = self.current(&id).await?;
        ensure_active(&current)?;
        if current.stage_status == StageStatus::Paused {
            return Ok(current);
        }
        let data = DeliveryPausedData {
            by: actor.id.clone(),
            status_before: current.stage_status,
        };
        self.append(EventType::DeliveryPaused, &current, actor, &data)
            .await
    }

    /// 继续：恢复到暂停前的 `stage_status`。未暂停 → 非法状态。
    pub async fn resume(
        &self,
        id: DeliveryId,
        actor: Actor,
    ) -> Result<DeliveryProjection, EngineError> {
        let current = self.current(&id).await?;
        ensure_active(&current)?;
        let status_after = current
            .status_before_pause
            .ok_or(EngineError::InvalidState("delivery is not paused"))?;
        let data = DeliveryResumedData {
            by: actor.id.clone(),
            status_after,
        };
        self.append(EventType::DeliveryResumed, &current, actor, &data)
            .await
    }

    /// 放弃：非终态 → `abandoned`。取消运行中尝试、作废审批与 PR 收尾尚未实现。
    pub async fn abandon(
        &self,
        id: DeliveryId,
        reason: Option<String>,
        actor: Actor,
    ) -> Result<DeliveryProjection, EngineError> {
        let current = self.current(&id).await?;
        ensure_active(&current)?;
        let data = DeliveryAbandonedData {
            reason,
            by: actor.id.clone(),
        };
        self.append(EventType::DeliveryAbandoned, &current, actor, &data)
            .await
    }

    async fn current(&self, id: &DeliveryId) -> Result<DeliveryProjection, EngineError> {
        self.load_delivery(id).await?.ok_or(EngineError::NotFound)
    }

    async fn append<T: serde::Serialize>(
        &self,
        ty: EventType,
        delivery: &DeliveryProjection,
        actor: Actor,
        data: &T,
    ) -> Result<DeliveryProjection, EngineError> {
        let id_text = delivery.id.to_string();
        let subject = Subject::parse(&id_text)
            .map_err(|error| SchemaError::shape("/subject", error.to_string()))?;
        let event = NewEvent {
            ty,
            subject,
            project_id: Some(delivery.project_id),
            actor,
            causation_id: None,
            correlation_id: Some(id_text),
            data: serde_json::to_value(data)
                .map_err(|error| SchemaError::shape("/data", error.to_string()))?,
            dedup_key: None,
        };
        event.validate()?;
        let mut tx = self.store.begin_write().await?;
        tx.append(event).await?;
        tx.commit().await?;
        self.current(&delivery.id).await
    }
}

fn ensure_active(delivery: &DeliveryProjection) -> Result<(), EngineError> {
    if matches!(delivery.stage, Stage::Closed | Stage::Abandoned) {
        return Err(EngineError::InvalidState("delivery is terminal"));
    }
    Ok(())
}
