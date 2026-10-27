//! 写类端口的测试替身：按约定把 `ctx.dedup_key` 作为所追加事件的 `dedup_key`
//! （与 engine #73 / conversation 确认的约定一致），使 `X-Event-Seq` 与 store 层去重
//! 都能被断言。业务语义只保留最小的"目标状态幂等"，不重现引擎逻辑。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use gantry_core::entities::Conversation;
use gantry_core::entities::{Delivery, Message, MessageRole, Node, NodeStatus, Stage, StageStatus};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::ProjectId;
use gantry_core::ids::{ConversationId, DeliveryId, IdGen, NodeId};
use gantry_core::level::Level;
use gantry_core::ports::context::{
    ActorCtx, ModuleError, Page, PageQuery, PageWithSeq, StoreError,
};
use gantry_core::ports::conversation::{ConvError, ConversationService, NewUserMessage};
use gantry_core::ports::engine::DeliveryControl;
use gantry_store::{Appended, Store};
use serde_json::{json, Value};

async fn append(store: &Store, ev: NewEvent) -> Result<Appended, StoreError> {
    let mut tx = store
        .begin_write()
        .await
        .map_err(|_| StoreError::Unavailable { retry_after: None })?;
    let appended = tx.append(ev).await.map_err(|e| StoreError::Rejected {
        message: e.to_string(),
    })?;
    tx.commit()
        .await
        .map_err(|_| StoreError::Unavailable { retry_after: None })?;
    Ok(appended)
}

const CASES: &str = include_str!("../../../gantry-core/fixtures/events/cases.json");

fn event(ty: EventType, subject: &str, ctx: &ActorCtx, data: Value) -> NewEvent {
    event_in(None, ty, subject, ctx, data)
}

fn event_in(
    project: Option<ProjectId>,
    ty: EventType,
    subject: &str,
    ctx: &ActorCtx,
    data: Value,
) -> NewEvent {
    NewEvent {
        ty,
        subject: Subject::parse(subject).unwrap(),
        project_id: project,
        actor: ctx.actor.clone(),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: ctx.dedup_key.clone(),
    }
}

/// 引擎替身：交付单元与节点在内存里，状态迁移时追加事件。
pub struct StoreEngine {
    pub store: Store,
    pub deliveries: Mutex<HashMap<DeliveryId, Delivery>>,
    pub nodes: Mutex<HashMap<NodeId, Node>>,
    pub calls: Mutex<Vec<(&'static str, ActorCtx)>>,
    /// 下一次调用返回存储不可用（5xx 不登记幂等键的用例）。
    pub fail_next: Mutex<bool>,
}

impl StoreEngine {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            deliveries: Mutex::default(),
            nodes: Mutex::default(),
            calls: Mutex::default(),
            fail_next: Mutex::new(false),
        }
    }

    fn enter(&self, op: &'static str, ctx: &ActorCtx) -> Result<(), ModuleError> {
        self.calls.lock().unwrap().push((op, ctx.clone()));
        if std::mem::take(&mut *self.fail_next.lock().unwrap()) {
            return Err(ModuleError::Store(StoreError::Unavailable {
                retry_after: None,
            }));
        }
        Ok(())
    }

    fn delivery(&self, id: DeliveryId) -> Result<Delivery, ModuleError> {
        self.deliveries
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(ModuleError::NotFound)
    }

    fn save(&self, d: &Delivery) {
        self.deliveries.lock().unwrap().insert(d.id, d.clone());
    }

    /// 登记交付单元：先追加 `delivery.created`（store 的投影要求先有创建事件）。
    pub async fn add_delivery(&self, d: &Delivery) {
        let cases: Value = serde_json::from_str(CASES).unwrap();
        let ctx = ActorCtx {
            actor: gantry_core::entities::Actor::new(
                gantry_core::entities::ActorKind::Engine,
                "engine",
            ),
            device: None,
            role: None,
            client_profile: None,
            request_id: gantry_core::ids::Ulid::from_parts(0, 1).unwrap(),
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        };
        let ev = event_in(
            Some(d.project_id),
            EventType::DeliveryCreated,
            &d.id.to_string(),
            &ctx,
            cases["valid"]["delivery.created"].clone(),
        );
        append(&self.store, ev).await.unwrap();
        self.save(d);
    }

    pub fn ops(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().iter().map(|c| c.0).collect()
    }
}

fn terminal(d: &Delivery) -> bool {
    matches!(d.stage, Stage::Closed | Stage::Abandoned)
}

fn invalid() -> ModuleError {
    ModuleError::InvalidState {
        message: "terminal".into(),
    }
}

#[async_trait]
impl DeliveryControl for StoreEngine {
    async fn set_level_cap(
        &self,
        ctx: ActorCtx,
        _id: DeliveryId,
        _cap: Level,
    ) -> Result<Delivery, ModuleError> {
        self.enter("set_level_cap", &ctx)?;
        Err(ModuleError::NotFound)
    }

    async fn abandon(
        &self,
        ctx: ActorCtx,
        id: DeliveryId,
        reason: Option<String>,
    ) -> Result<Delivery, ModuleError> {
        self.enter("abandon", &ctx)?;
        let mut d = self.delivery(id)?;
        if d.stage == Stage::Abandoned {
            return Ok(d);
        }
        if terminal(&d) {
            return Err(invalid());
        }
        let data = json!({ "reason": reason, "by": ctx.actor.id });
        append(
            &self.store,
            event_in(
                Some(d.project_id),
                EventType::DeliveryAbandoned,
                &id.to_string(),
                &ctx,
                data,
            ),
        )
        .await?;
        d.stage = Stage::Abandoned;
        d.stage_status = StageStatus::Done;
        self.save(&d);
        Ok(d)
    }

    async fn pause(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError> {
        self.enter("pause", &ctx)?;
        let mut d = self.delivery(id)?;
        if terminal(&d) {
            return Err(invalid());
        }
        if d.stage_status == StageStatus::Paused {
            return Ok(d);
        }
        let data = json!({ "by": ctx.actor.id, "status_before": d.stage_status });
        append(
            &self.store,
            event_in(
                Some(d.project_id),
                EventType::DeliveryPaused,
                &id.to_string(),
                &ctx,
                data,
            ),
        )
        .await?;
        d.stage_status = StageStatus::Paused;
        self.save(&d);
        Ok(d)
    }

    async fn resume(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError> {
        self.enter("resume", &ctx)?;
        let mut d = self.delivery(id)?;
        if terminal(&d) {
            return Err(invalid());
        }
        if d.stage_status != StageStatus::Paused {
            return Ok(d);
        }
        let data = json!({ "by": ctx.actor.id, "status_after": "running" });
        append(
            &self.store,
            event_in(
                Some(d.project_id),
                EventType::DeliveryResumed,
                &id.to_string(),
                &ctx,
                data,
            ),
        )
        .await?;
        d.stage_status = StageStatus::Running;
        self.save(&d);
        Ok(d)
    }

    async fn retry_node(&self, ctx: ActorCtx, id: NodeId) -> Result<Node, ModuleError> {
        self.enter("retry_node", &ctx)?;
        let node = self
            .nodes
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(ModuleError::NotFound)?;
        if node.status != NodeStatus::Failed {
            return Err(invalid());
        }
        let data = json!({ "node_id": id, "by": ctx.actor.id });
        append(
            &self.store,
            event(EventType::NodeRetryRequested, &id.to_string(), &ctx, data),
        )
        .await?;
        Ok(node)
    }
}

/// 对话替身：发消息即追加 `message.posted`；`Duplicate` 时从已有事件重建消息。
pub struct StoreConversation {
    pub store: Store,
    pub ids: std::sync::Arc<IdGen>,
    pub known: Mutex<Vec<ConversationId>>,
    pub calls: Mutex<Vec<(ActorCtx, NewUserMessage)>>,
}

#[async_trait]
impl ConversationService for StoreConversation {
    async fn ensure_conversation(
        &self,
        _project: &ProjectId,
        _ctx: &ActorCtx,
    ) -> Result<ConversationId, ConvError> {
        Err(ConvError::NotFound)
    }

    async fn post_user_message(
        &self,
        ctx: ActorCtx,
        cnv: ConversationId,
        msg: NewUserMessage,
    ) -> Result<Message, ConvError> {
        self.calls.lock().unwrap().push((ctx.clone(), msg.clone()));
        if !self.known.lock().unwrap().contains(&cnv) {
            return Err(ConvError::NotFound);
        }
        if let Some(r) = msg
            .refs
            .iter()
            .find(|r| matches!(r, gantry_core::entities::Ref::Issue { number: 404, .. }))
        {
            return Err(ConvError::UnresolvedRef {
                reference: r.clone(),
            });
        }
        let now = self.store.clock_now();
        let message = Message {
            id: self.ids.next::<gantry_core::ids::Msg>().unwrap(),
            conversation_id: cnv,
            role: MessageRole::User,
            agent_kind: None,
            text: msg.text,
            tone: None,
            level_cap: msg.level_cap,
            refs: msg.refs,
            cards: Vec::new(),
            delivery_id: None,
            created_at: now,
            updated_at: now,
        };
        let data = serde_json::to_value(&message).unwrap();
        let ev = event(
            EventType::MessagePosted,
            &message.id.to_string(),
            &ctx,
            data,
        );
        match append(&self.store, ev).await? {
            Appended::New(_) => Ok(message),
            Appended::Duplicate { existing_seq } => {
                let existing = self
                    .store
                    .events()
                    .get_by_seq(existing_seq)
                    .await
                    .ok()
                    .flatten()
                    .expect("duplicate points at an event");
                Ok(serde_json::from_value(existing.data).unwrap())
            }
        }
    }

    async fn list_conversations(
        &self,
        _project: &ProjectId,
        _page: PageQuery,
    ) -> Result<Page<Conversation>, ConvError> {
        Err(ConvError::NotFound)
    }

    async fn list_messages(
        &self,
        _cnv: &ConversationId,
        _page: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError> {
        Err(ConvError::NotFound)
    }
}
