//! 引擎写入口的最小实现：创建、阶段推进与交付控制写入事件日志（engine.md §7.1）。
//!
//! 决策器、调度与副作用执行仍未实现。读取一律经 [`DeliveryState`]：按 §7.14.2 路由表
//! 在本项目范围内归属事件后折叠（M1 收尾评审 A4）。

use std::sync::Arc;

use std::collections::BTreeSet;

use gantry_core::entities::{Actor, InboxItem, NodeStatus, Seq, Stage, StageStatus};
use gantry_core::events::{
    DeliveryAbandonedData, DeliveryLevelChangedData, DeliveryPausedData, DeliveryResumedData,
    DeliveryStageChangedData, EventType, LevelChangeKind, NewEvent, NodeRetryRequestedData,
    SchemaError, Subject,
};
use gantry_core::ids::{DeliveryId, IdError, NodeId, ProjectId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::StageTrigger;
use gantry_core::machines::TransitionError;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::engine::TemplateCatalog;
use gantry_core::ports::inbox::{InboxPort, RaiseError, RaiseRequest};
use gantry_store::{Appended, DeliveryRow, EventFilter, Store, StoreError, TypePattern};

use crate::create::{created_event, NewDelivery};
use crate::route::{RouteError, Router};
use crate::stage::{advance, StageTransition};
use crate::state::{DeliveryState, NodeState, StateError};

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
    #[error("state: {0}")]
    State(#[from] StateError),
    #[error("route: {0}")]
    Route(#[from] RouteError),
    #[error("delivery not found")]
    NotFound,
    #[error("invalid state: {0}")]
    InvalidState(&'static str),
    #[error("validation: {0}")]
    Validation(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("inbox: {0}")]
    Inbox(#[from] RaiseError),
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
    inbox: Arc<dyn InboxPort>,
}

impl Engine {
    pub fn new(store: Store, inbox: Arc<dyn InboxPort>) -> Self {
        Self { store, inbox }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 上报一项；与调用方的事件写入同一事务时由调用方传入事务，此处自开事务。
    pub async fn raise_inbox(
        &self,
        request: RaiseRequest,
        ctx: &ActorCtx,
    ) -> Result<InboxItem, EngineError> {
        let mut tx = self.store.begin_write().await?;
        let outcome = self.inbox.raise(Some(&mut tx), request, ctx).await?;
        tx.commit().await?;
        Ok(match outcome {
            gantry_core::ports::inbox::RaiseOutcome::Created(written)
            | gantry_core::ports::inbox::RaiseOutcome::Deduped(written) => written.item,
        })
    }

    /// `levels.graph_templates` 的已实现部分；M1 只有 Low 的模板族。
    pub fn template_catalog(&self) -> TemplateCatalog {
        TemplateCatalog {
            templates: vec!["low.single".to_owned()],
        }
    }

    /// 在单个写事务中追加 `delivery.created`；相同 `source_message_id` 不重复追加，
    /// 并返回已存在的那个交付单元（engine.md §6：同一条消息重复调用返回同一个交付单元）。
    pub async fn create_delivery(&self, delivery: NewDelivery) -> Result<DeliveryId, EngineError> {
        let event = created_event(delivery)?;
        let mut tx = self.store.begin_write().await?;
        let appended = tx.append(event).await?;
        tx.commit().await?;
        let subject = match appended {
            Appended::New(event) => event.subject,
            Appended::Duplicate { existing_seq } => {
                self.store
                    .events()
                    .get_by_seq(existing_seq)
                    .await?
                    .ok_or(EngineError::NotFound)?
                    .subject
            }
        };
        Ok(subject.as_str().parse()?)
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
    pub async fn load_delivery(&self, id: &DeliveryId) -> Result<Option<DeliveryRow>, EngineError> {
        Ok(self.load_state(id).await?.map(|state| state.delivery))
    }

    /// 人工重试：只接受 `failed` 节点；`failed → queued`。
    pub async fn retry_node(
        &self,
        node_id: NodeId,
        actor: Actor,
    ) -> Result<NodeState, EngineError> {
        let delivery_id = self.delivery_of_node(&node_id).await?;
        let state = self
            .load_state(&delivery_id)
            .await?
            .ok_or(EngineError::NotFound)?;
        let delivery = &state.delivery;
        ensure_active(delivery)?;
        let node = state.node(&node_id).ok_or(EngineError::NotFound)?;
        if node.status != NodeStatus::Failed {
            return Err(EngineError::InvalidState("node is not failed"));
        }
        let subject = Subject::parse(&node_id.to_string())
            .map_err(|error| SchemaError::shape("/subject", error.to_string()))?;
        let event = NewEvent {
            ty: EventType::NodeRetryRequested,
            subject,
            project_id: Some(delivery.project_id),
            actor: actor.clone(),
            causation_id: None,
            correlation_id: Some(delivery_id.to_string()),
            data: serde_json::to_value(NodeRetryRequestedData {
                node_id,
                by: actor.id,
            })
            .map_err(|error| SchemaError::shape("/data", error.to_string()))?,
            dedup_key: None,
        };
        event.validate()?;
        let mut tx = self.store.begin_write().await?;
        tx.append(event).await?;
        tx.commit().await?;
        self.load_state(&delivery_id)
            .await?
            .and_then(|state| state.node(&node_id).cloned())
            .ok_or(EngineError::NotFound)
    }

    /// 重建一个交付单元的 `DeliveryState`（见 [`load_delivery_state`]）。
    pub async fn load_state(&self, id: &DeliveryId) -> Result<Option<DeliveryState>, EngineError> {
        load_delivery_state(&self.store, id).await
    }

    /// 节点所属交付单元：只扫建图事件（`workflow.created` / `revised` 里登记了节点）。
    /// `retry_node` 只拿到 `node_id`、不知道项目，所以不能按项目收窄；按 ID 的索引属于 store（#88）。
    async fn delivery_of_node(&self, node_id: &NodeId) -> Result<DeliveryId, EngineError> {
        let filter = EventFilter {
            project_ids: None,
            include: vec![
                TypePattern::Exact(EventType::WorkflowCreated),
                TypePattern::Exact(EventType::WorkflowRevised),
            ],
            exclude: Vec::new(),
        };
        let mut router = Router::default();
        self.scan(&filter, |event| {
            router.route(event)?;
            Ok(())
        })
        .await?;
        router.node_owner(node_id).ok_or(EngineError::NotFound)
    }

    async fn scan(
        &self,
        filter: &EventFilter,
        visit: impl FnMut(&gantry_core::events::Event<serde_json::Value>) -> Result<(), EngineError>,
    ) -> Result<(), EngineError> {
        scan_events(&self.store, filter, visit).await
    }

    /// 暂停：停止派发新节点，记录暂停前的 `stage_status`。已暂停 → 幂等返回。
    pub async fn pause(&self, id: DeliveryId, actor: Actor) -> Result<DeliveryRow, EngineError> {
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
    pub async fn resume(&self, id: DeliveryId, actor: Actor) -> Result<DeliveryRow, EngineError> {
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

    /// 调整上限：不改变创建时固化的预算模板；只写 `kind = cap`，低于 `level_effective`
    /// 时由投影派生下调（engine.md §11，#48）。
    pub async fn set_level_cap(
        &self,
        id: DeliveryId,
        cap: Level,
        actor: Actor,
    ) -> Result<DeliveryRow, EngineError> {
        if cap == Level::None {
            return Err(EngineError::Validation(
                "delivery level_cap must not be None",
            ));
        }
        let current = self.current(&id).await?;
        ensure_active(&current)?;
        if cap == current.level_cap {
            return Ok(current);
        }
        let event = level_changed_event(
            &current,
            &actor,
            current.level_cap,
            cap,
            LevelChangeKind::Cap,
        )?;
        let mut tx = self.store.begin_write().await?;
        tx.append(event).await?;
        tx.commit().await?;
        self.current(&id).await
    }

    /// 放弃：非终态 → `abandoned`。取消运行中尝试、作废审批与 PR 收尾尚未实现。
    pub async fn abandon(
        &self,
        id: DeliveryId,
        reason: Option<String>,
        actor: Actor,
    ) -> Result<DeliveryRow, EngineError> {
        let current = self.current(&id).await?;
        ensure_active(&current)?;
        let data = DeliveryAbandonedData {
            reason,
            by: actor.id.clone(),
        };
        self.append(EventType::DeliveryAbandoned, &current, actor, &data)
            .await
    }

    async fn current(&self, id: &DeliveryId) -> Result<DeliveryRow, EngineError> {
        self.load_delivery(id).await?.ok_or(EngineError::NotFound)
    }

    async fn append<T: serde::Serialize>(
        &self,
        ty: EventType,
        delivery: &DeliveryRow,
        actor: Actor,
        data: &T,
    ) -> Result<DeliveryRow, EngineError> {
        let event = delivery_event(ty, delivery, actor, data)?;
        let mut tx = self.store.begin_write().await?;
        tx.append(event).await?;
        tx.commit().await?;
        self.current(&delivery.id).await
    }
}

fn level_changed_event(
    delivery: &DeliveryRow,
    actor: &Actor,
    from: Level,
    to: Level,
    kind: LevelChangeKind,
) -> Result<NewEvent, EngineError> {
    delivery_event(
        EventType::DeliveryLevelChanged,
        delivery,
        actor.clone(),
        &DeliveryLevelChangedData {
            from,
            to,
            kind,
            reason: "user".to_owned(),
        },
    )
}

fn delivery_event<T: serde::Serialize>(
    ty: EventType,
    delivery: &DeliveryRow,
    actor: Actor,
    data: &T,
) -> Result<NewEvent, EngineError> {
    let subject = Subject::parse(&delivery.id.to_string())
        .map_err(|error| SchemaError::shape("/subject", error.to_string()))?;
    let event = NewEvent {
        ty,
        subject,
        project_id: Some(delivery.project_id),
        actor,
        causation_id: None,
        correlation_id: Some(delivery.id.to_string()),
        data: serde_json::to_value(data)
            .map_err(|error| SchemaError::shape("/data", error.to_string()))?,
        dedup_key: None,
    };
    event.validate()?;
    Ok(event)
}

fn ensure_active(delivery: &DeliveryRow) -> Result<(), EngineError> {
    if matches!(delivery.stage, Stage::Closed | Stage::Abandoned) {
        return Err(EngineError::InvalidState("delivery is terminal"));
    }
    Ok(())
}

/// 重建一个交付单元的 `DeliveryState`：先按 subject 取 `delivery.created` 得到所属项目，
/// 再只扫本项目的事件（含 `project_id` 为空的全局事件），按路由表归属后折叠。
///
/// 读的是已提交日志。执行体在持有写租约（`begin_write`）之后调用它：单写者下租约期间
/// 没有别人能提交，读到的就是本事务开始时的状态（engine.md §7.1）。
pub async fn load_delivery_state(
    store: &Store,
    id: &DeliveryId,
) -> Result<Option<DeliveryState>, EngineError> {
    let subject = Subject::parse(&id.to_string())
        .map_err(|error| SchemaError::shape("/subject", error.to_string()))?;
    let Some(project_id) = store
        .events()
        .by_subject(&subject)
        .await?
        .into_iter()
        .find_map(|event| event.project_id)
    else {
        return Ok(None);
    };
    let filter = EventFilter {
        project_ids: Some(BTreeSet::from([project_id])),
        include: Vec::new(),
        exclude: Vec::new(),
    };
    let mut router = Router::default();
    let mut state: Option<DeliveryState> = None;
    scan_events(store, &filter, |event| {
        if router.route(event)?.delivery() != Some(*id) {
            return Ok(());
        }
        match &mut state {
            Some(state) => state.apply(event)?,
            None => state = Some(DeliveryState::created(event)?),
        }
        Ok(())
    })
    .await?;
    Ok(state)
}

/// 按页扫完 `filter` 命中的已提交事件。
pub(crate) async fn scan_events(
    store: &Store,
    filter: &EventFilter,
    mut visit: impl FnMut(&gantry_core::events::Event<serde_json::Value>) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let mut cursor = Seq::ZERO;
    loop {
        let page = store.events().scan(cursor, filter, 200).await?;
        for event in &page.events {
            visit(event)?;
        }
        if page.scanned_to >= page.head {
            return Ok(());
        }
        cursor = page.scanned_to;
    }
}
