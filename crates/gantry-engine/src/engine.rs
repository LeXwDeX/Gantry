//! 引擎写入口：创建交付单元与读取（engine.md §7.1）。控制命令（暂停、继续、放弃、调档、重试）
//! 经执行体由 `decide` 求值，入口是 [`crate::runtime::EngineRuntime`] 实现的 `DeliveryControl`（#73）。
//! 读取一律经 [`DeliveryState`]：按 §7.14.2 路由表在本项目范围内归属事件后折叠（M1 收尾评审 A4）。

use std::sync::Arc;

use std::collections::BTreeSet;

use gantry_core::entities::{InboxItem, InboxKind, Ref, Seq, Stage};
use gantry_core::events::{EventType, SchemaError, Subject};
use gantry_core::ids::{DeliveryId, IdError, InboxId, NodeId, ProjectId};
use gantry_core::level::Level;
use gantry_core::machines::TransitionError;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::engine::TemplateCatalog;
use gantry_core::ports::inbox::{InboxPort, RaiseError, RaiseRequest};
use gantry_store::{Appended, DeliveryRow, EventFilter, Store, StoreError, TypePattern};

use crate::create::{created_event, NewDelivery};
use crate::route::{RouteError, Router};
use crate::state::{DeliveryState, StateError};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is not configured for the requested level")]
    NotConfigured,
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

/// `locate_deliveries` 的返回项（engine.md §6 `DeliveryHandle` 的引擎侧最小集）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliverySummary {
    pub id: DeliveryId,
    pub project_id: ProjectId,
    pub level_cap: Level,
    pub stage: Stage,
    /// 未决的 `spec_needed`；引擎保证每个交付单元同时至多一个。
    pub open_spec_needed: Option<InboxId>,
}

impl DeliverySummary {
    fn from_state(state: &DeliveryState) -> Self {
        Self {
            id: state.delivery.id,
            project_id: state.delivery.project_id,
            level_cap: state.delivery.level_cap,
            stage: state.delivery.stage,
            open_spec_needed: state
                .open_inbox
                .values()
                .find(|item| item.kind == InboxKind::SpecNeeded)
                .map(|item| item.id),
        }
    }
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

    /// 定位：给定 refs，返回已绑定且未终结的交付单元（0..n 个）。只读投影，不产生事件
    /// （engine.md §6；conversation 的 P3）。空 refs 返回空集合。
    pub async fn locate_deliveries(
        &self,
        project: &ProjectId,
        refs: &[Ref],
    ) -> Result<Vec<DeliverySummary>, EngineError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let filter = EventFilter {
            project_ids: Some(BTreeSet::from([*project])),
            include: vec![TypePattern::Exact(EventType::DeliveryCreated)],
            exclude: Vec::new(),
        };
        let mut ids = BTreeSet::new();
        scan_events(&self.store, &filter, |event| {
            let id = event
                .subject
                .as_str()
                .parse::<DeliveryId>()
                .map_err(|error| SchemaError::shape("/subject", error.to_string()))?;
            ids.insert(id);
            Ok(())
        })
        .await?;
        let mut located = Vec::new();
        for id in ids {
            let Some(state) = load_delivery_state(&self.store, &id).await? else {
                continue;
            };
            if state.delivery.stage.is_terminal() || !refs_match(&state, refs) {
                continue;
            }
            located.push(DeliverySummary::from_state(&state));
        }
        Ok(located)
    }

    /// 单个交付单元的摘要；不存在时返回 `None`。供 conversation 的 `EngineApi` 适配使用。
    pub async fn delivery_summary(
        &self,
        id: &DeliveryId,
    ) -> Result<Option<DeliverySummary>, EngineError> {
        Ok(load_delivery_state(&self.store, id)
            .await?
            .map(|state| DeliverySummary::from_state(&state)))
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
    ///
    /// `level_cap` 与模板族必须匹配且模板族已在 `template_catalog` 中登记；
    /// 不匹配时返回 [`EngineError::NotConfigured`]，不写任何事件，也不静默降档。
    /// M1 只有 Low 模板族，因此 Medium 及以上一律拒绝；有效档位从 Low 起且
    /// 不超过上限（K7）。
    pub async fn create_delivery(
        &self,
        mut delivery: NewDelivery,
    ) -> Result<DeliveryId, EngineError> {
        if delivery.level_cap != Level::Low
            || !self
                .template_catalog()
                .templates
                .iter()
                .any(|template| template == &delivery.budget.template)
        {
            return Err(EngineError::NotConfigured);
        }
        delivery.level_effective = Level::Low.capped_by(delivery.level_cap);
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

    /// 读取一条交付的投影；不存在时返回 `None`。
    pub async fn load_delivery(&self, id: &DeliveryId) -> Result<Option<DeliveryRow>, EngineError> {
        Ok(self.load_state(id).await?.map(|state| state.delivery))
    }

    /// 重建一个交付单元的 `DeliveryState`（见 [`load_delivery_state`]）。
    pub async fn load_state(&self, id: &DeliveryId) -> Result<Option<DeliveryState>, EngineError> {
        load_delivery_state(&self.store, id).await
    }

    /// 节点所属交付单元：只扫建图事件（`workflow.created` / `revised` 里登记了节点）。
    /// `retry_node` 只拿到 `node_id`、不知道项目，所以不能按项目收窄；按 ID 的索引属于 store（#88）。
    pub(crate) async fn delivery_of_node(
        &self,
        node_id: &NodeId,
    ) -> Result<DeliveryId, EngineError> {
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
}

fn refs_match(state: &DeliveryState, refs: &[Ref]) -> bool {
    refs.iter().any(|reference| match reference {
        Ref::Issue { repo, number } => state
            .delivery
            .issues
            .iter()
            .any(|issue| issue.repo == *repo && issue.number == *number),
        Ref::Pr { number, .. } => state.pr.as_ref().is_some_and(|pr| pr.number == *number),
        Ref::Worktree { id } => state
            .worktree
            .as_ref()
            .is_some_and(|worktree| worktree.id == *id),
        Ref::Delivery { id } => state.delivery.id == *id,
    })
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
