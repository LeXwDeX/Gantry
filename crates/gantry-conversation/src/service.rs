//! 会话服务：`ConversationService` 的实现与两条消费者（§7.1、§7.2）。
//!
//! - 同步部分（`post_user_message`）只校验 refs 并追加 `message.posted(user)`；
//! - 异步部分由 [`Conversation::run_once`] 驱动：先消费 `project.added`（建对话），
//!   再按 `seq` 依次处理用户消息。每条被消费事件的全部输出与 `advance_cursor`
//!   在同一个 `WriteTx` 里提交；崩溃后从游标重放，只会重复调用幂等接口。
//!
//! 调用对方（引擎 / 模型）发生在写事务之外，避免模型调用期间占住单写者。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::RepoSlug;
use gantry_core::entities::{
    Actor, ActorKind, AgentKind, Conversation as ConversationRow, DeliveryOrigin, Message,
    MessageRole, MessageTone, Ref, Role, Seq, SettingsResource, SettingsScope,
};
use gantry_core::events::{
    ConversationEventData, Event, EventType, NewEvent, ProjectEventData, Subject,
};
use gantry_core::ids::{Cnv, ConversationId, IdGen, MessageId, Msg, ProjectId};
use gantry_core::level::Level;
use gantry_core::ports::context::{
    ActorCtx, ClientProfile, Page, PageQuery, PageWithSeq, StoreError as PortStoreError,
};
use gantry_core::ports::conversation::{ConvError, ConversationService, NewUserMessage};
use gantry_core::ports::engine::CreateDelivery;
use gantry_core::settings::{builtin_defaults, effective};
use gantry_core::text::UntrustedText;
use gantry_core::worker::{ClassifyTarget, IntentKind, MaterialKind, StructuredOutput};
use gantry_models::prompt::TrustedText;
use gantry_models::{Attribution, DirectCall, ModelsError};
use gantry_store::{Appended, Store, StoreError, WriteTx};
use serde_json::Value;

use crate::intent::{materials, ContextItem, IntentInput, IntentParser, ModelsIntentParser};
use crate::ports::{DeliveryHandle, EngineApi, ModelsPort};
use crate::projection::{self, ConversationLog};
use crate::reply::{self, Reply};
use crate::route::{resolve_cap, route, Cap, RouteCx, RouteDecision, UnsupportedReason};

/// 消费者游标（store.md §6.1）。
pub const CURSOR_PROJECTS: &str = "conv:projects";
pub const CURSOR_USER_MESSAGES: &str = "conv:user_msgs";

/// 交付单元标题截断（§10 `TITLE_MAX_CHARS`）。
pub const TITLE_MAX_CHARS: usize = 80;

const STATUS_INSTRUCTIONS: &str = "\
Answer the user's question about the state of this Gantry project in a few sentences, using \
only the materials provided. Reply with target = \"status_answer\". If the materials do not \
contain the answer, say so. The user's question follows after the line USER MESSAGE.";

/// 内部常量（§10，编译期默认值，不是设置）。测试可以收紧期限。
#[derive(Clone, Debug)]
pub struct Limits {
    pub history_window: usize,
    pub max_refs_expanded: usize,
    pub intent_deadline: Duration,
    pub process_deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            history_window: 20,
            max_refs_expanded: 5,
            intent_deadline: Duration::from_secs(60),
            process_deadline: Duration::from_secs(90),
        }
    }
}

/// 故障注入点：返回 `true` 时放弃本次提交（模拟"输出已追加、游标未推进"时崩溃）。
/// 只供测试使用。
#[doc(hidden)]
pub trait CrashPoint: Send + Sync {
    fn before_commit(&self, consumer: &str, seq: Seq) -> bool;
}

pub struct ConversationDeps {
    pub store: Store,
    pub ids: Arc<IdGen>,
    pub engine: Arc<dyn EngineApi>,
    pub models: Arc<dyn ModelsPort>,
    pub limits: Limits,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("id generation failed: {0}")]
    Id(String),
    #[error("crash injected before commit")]
    Crashed,
}

pub struct Conversation {
    store: Store,
    ids: Arc<IdGen>,
    engine: Arc<dyn EngineApi>,
    models: Arc<dyn ModelsPort>,
    intent: Arc<dyn IntentParser>,
    limits: Limits,
    crash: Option<Arc<dyn CrashPoint>>,
}

fn system_actor() -> Actor {
    Actor::new(ActorKind::System, "conversation")
}

impl Conversation {
    pub fn new(deps: ConversationDeps) -> Self {
        let intent = Arc::new(ModelsIntentParser::new(
            deps.models.clone(),
            deps.ids.clone(),
        ));
        Self {
            store: deps.store,
            ids: deps.ids,
            engine: deps.engine,
            models: deps.models,
            intent,
            limits: deps.limits,
            crash: None,
        }
    }

    #[doc(hidden)]
    pub fn with_crash_point(mut self, crash: Arc<dyn CrashPoint>) -> Self {
        self.crash = Some(crash);
        self
    }

    /// 处理游标之后的全部待办：先 `project.added`，再用户消息。返回处理条数。
    pub async fn run_once(&self) -> Result<usize, ProcessError> {
        let mut done = self.consume_projects().await?;
        done += self.consume_user_messages().await?;
        Ok(done)
    }

    async fn commit(&self, mut tx: WriteTx, consumer: &str, seq: Seq) -> Result<(), ProcessError> {
        tx.advance_cursor(consumer, seq).await?;
        if self
            .crash
            .as_ref()
            .is_some_and(|crash| crash.before_commit(consumer, seq))
        {
            tx.rollback().await?;
            return Err(ProcessError::Crashed);
        }
        tx.commit().await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 对话生命周期
    // -----------------------------------------------------------------------

    async fn consume_projects(&self) -> Result<usize, ProcessError> {
        let after = self.store.cursors().load(CURSOR_PROJECTS).await?;
        let head = self.store.events().head().await?;
        let events = projection::scan(&self.store, None, &[EventType::ProjectAdded], head).await?;
        let mut done = 0;
        for event in events.into_iter().filter(|event| event.seq > after) {
            let mut tx = self.store.begin_write().await?;
            if let Ok(data) = serde_json::from_value::<ProjectEventData>(event.data.clone()) {
                self.ensure_in_tx(&mut tx, &data.id, Some(&event)).await?;
            }
            self.commit(tx, CURSOR_PROJECTS, event.seq).await?;
            done += 1;
        }
        Ok(done)
    }

    /// 在事务内确保对话存在；以 `dedup_key` 保证每个项目恰好一个（P-22）。
    async fn ensure_in_tx(
        &self,
        tx: &mut WriteTx,
        project: &ProjectId,
        cause: Option<&Event<Value>>,
    ) -> Result<ConversationId, ProcessError> {
        let id = self
            .ids
            .next::<Cnv>()
            .map_err(|error| ProcessError::Id(error.to_string()))?;
        let now = tx.now();
        let data = ConversationEventData {
            id,
            project_id: *project,
            created_at: now,
            updated_at: now,
        };
        let appended = tx
            .append(NewEvent {
                ty: EventType::ConversationCreated,
                subject: subject(&id.to_string())?,
                project_id: Some(*project),
                actor: system_actor(),
                causation_id: cause.map(|event| event.id),
                correlation_id: None,
                data: to_value(&data)?,
                dedup_key: Some(format!("conversation.created:{project}")),
            })
            .await?;
        match appended {
            Appended::New(_) => Ok(id),
            Appended::Duplicate { existing_seq } => {
                let event = self.store.events().get_by_seq(existing_seq).await?;
                event
                    .and_then(|event| {
                        serde_json::from_value::<ConversationEventData>(event.data).ok()
                    })
                    .map(|data| data.id)
                    .ok_or_else(|| {
                        ProcessError::Store(StoreError::PreconditionFailed(
                            "conversation.created dedup points to an unreadable event".into(),
                        ))
                    })
            }
        }
    }

    // -----------------------------------------------------------------------
    // 用户消息流水线（§7.2 P2–P7）
    // -----------------------------------------------------------------------

    async fn consume_user_messages(&self) -> Result<usize, ProcessError> {
        let after = self.store.cursors().load(CURSOR_USER_MESSAGES).await?;
        let head = self.store.events().head().await?;
        let events = projection::scan(&self.store, None, &[EventType::MessagePosted], head).await?;
        let mut done = 0;
        for event in events.into_iter().filter(|event| event.seq > after) {
            let message = serde_json::from_value::<Message>(event.data.clone()).ok();
            let mut tx_reply = None;
            if let Some(message) = message.filter(|message| message.role == MessageRole::User) {
                let reply = match tokio::time::timeout(
                    self.limits.process_deadline,
                    self.process(&event, &message),
                )
                .await
                {
                    Ok(reply) => reply?,
                    Err(_) => Reply::event(MessageTone::Warn, reply::TIMED_OUT),
                };
                tx_reply = Some((message, reply));
                done += 1;
            }
            let mut tx = self.store.begin_write().await?;
            if let Some((message, reply)) = tx_reply {
                self.append_reply(&mut tx, &event, &message, reply).await?;
            }
            self.commit(tx, CURSOR_USER_MESSAGES, event.seq).await?;
        }
        Ok(done)
    }

    async fn append_reply(
        &self,
        tx: &mut WriteTx,
        source: &Event<Value>,
        message: &Message,
        reply: Reply,
    ) -> Result<(), ProcessError> {
        let id = self
            .ids
            .next::<Msg>()
            .map_err(|error| ProcessError::Id(error.to_string()))?;
        let now = tx.now();
        let posted = Message {
            id,
            conversation_id: message.conversation_id,
            role: reply.role,
            agent_kind: reply.agent_kind,
            text: reply.text,
            tone: reply.tone,
            level_cap: None,
            refs: reply.refs,
            cards: Vec::new(),
            delivery_id: reply.delivery_id,
            created_at: now,
            updated_at: now,
        };
        tx.append(NewEvent {
            ty: EventType::MessagePosted,
            subject: subject(&message.conversation_id.to_string())?,
            project_id: source.project_id,
            actor: system_actor(),
            causation_id: Some(source.id),
            correlation_id: None,
            data: to_value(&posted)?,
            dedup_key: Some(format!("conv:reply:{}", message.id)),
        })
        .await?;
        Ok(())
    }

    async fn process(
        &self,
        event: &Event<Value>,
        message: &Message,
    ) -> Result<Reply, ProcessError> {
        let Some(project) = event.project_id else {
            return Ok(Reply::event(MessageTone::Warn, reply::INTENT_FAILED));
        };
        // P2：模型未配置 → 只提示，不调用模型、不创建交付单元。
        let configured = self.models.configured(&project).await.unwrap_or_default();
        if !configured {
            return Ok(Reply::event(MessageTone::Warn, reply::NOT_CONFIGURED));
        }

        // P3：上下文。目标交付单元只由 refs 决定（§7.4）。
        let located = if message.refs.is_empty() {
            Vec::new()
        } else {
            match self.engine.locate_deliveries(&project, &message.refs).await {
                Ok(located) => located,
                Err(_) => return Ok(Reply::event(MessageTone::Warn, reply::LOCATE_FAILED)),
            }
        };
        let default_level = self.default_level(&project).await?;

        // P4：档位上限。手机档案的 level_cap 已在同步部分丢弃（C26、P-35）。
        let cap = resolve_cap(None, message.level_cap, &located, default_level);

        // P5：意图。
        let context = self.context(&project, event, message, &located).await?;
        let input = IntentInput {
            project_id: project,
            conversation_id: message.conversation_id,
            user_text: message.text.clone(),
            context: context.clone(),
            known_deliveries: located.iter().map(|handle| handle.id).collect(),
            deadline: self.limits.intent_deadline,
        };
        let parsed = tokio::time::timeout(self.limits.intent_deadline, self.intent.parse(input));
        let intent = match parsed.await {
            Ok(Ok(intent)) => intent,
            Ok(Err(_)) | Err(_) => {
                return Ok(Reply::event(MessageTone::Warn, reply::INTENT_FAILED))
            }
        };

        // P6：路由（纯函数）。
        let decision = route(&RouteCx {
            models_configured: configured,
            cap,
            located: &located,
            intent: intent.intent,
            needs_code: intent.needs_code,
            profile: None,
        });

        // P7：执行。
        Ok(match decision {
            RouteDecision::NotConfigured => Reply::event(MessageTone::Warn, reply::NOT_CONFIGURED),
            RouteDecision::PureChat => {
                if intent.needs_code {
                    Reply::event(MessageTone::Info, reply::CODE_ANSWER_CLOSED)
                } else {
                    let mut answer = self.status_answer(&project, message, &context).await;
                    if intent.intent == IntentKind::Task && answer.role == MessageRole::Agent {
                        answer.text = format!("{}\n\n{}", answer.text, reply::NONE_LEVEL_NOTE);
                    }
                    answer
                }
            }
            RouteDecision::CreateDelivery { cap: level } => {
                self.create(event, message, &project, Cap { level, ..cap })
                    .await?
            }
            RouteDecision::StatusAnswer => self.status_answer(&project, message, &context).await,
            RouteDecision::CodeAnswer => Reply::event(MessageTone::Info, reply::CODE_ANSWER_CLOSED),
            RouteDecision::Ambiguous { candidates } => {
                Reply::agent(AgentKind::Planner, reply::AMBIGUOUS).with_refs(
                    candidates
                        .into_iter()
                        .map(|id| Ref::Delivery { id })
                        .collect(),
                )
            }
            RouteDecision::Unsupported {
                reason: UnsupportedReason::NoOpenQuestion,
            } => Reply::agent(AgentKind::Planner, reply::NO_OPEN_QUESTION)
                .with_refs(located.iter().map(|h| Ref::Delivery { id: h.id }).collect()),
            // M2：补规格与对话内调档（§1.1）。
            RouteDecision::SupplySpec { .. } | RouteDecision::SetCapThenContinue { .. } => {
                Reply::event(MessageTone::Info, reply::NOT_YET)
            }
            RouteDecision::ControlGuidance => {
                Reply::agent(AgentKind::Planner, reply::CONTROL_GUIDANCE)
            }
        })
    }

    async fn create(
        &self,
        event: &Event<Value>,
        message: &Message,
        project: &ProjectId,
        cap: Cap,
    ) -> Result<Reply, ProcessError> {
        let cmd = CreateDelivery {
            project_id: *project,
            title: title(&message.text, &message.refs),
            level_cap: cap.level,
            refs: message.refs.clone(),
            origin: DeliveryOrigin::Conversation,
            source_message_id: message.id,
            user_text: message.text.clone(),
            ctx: self.ctx_for(event)?,
        };
        Ok(match self.engine.create_delivery(cmd).await {
            Ok(handle) => reply::created(handle.id, cap),
            Err(error) => reply::engine_failed(&error),
        })
    }

    /// 状态问答：`classify(status_answer)`，记会话台账（P-58）。
    async fn status_answer(
        &self,
        project: &ProjectId,
        message: &Message,
        context: &[ContextItem],
    ) -> Reply {
        let call_id = match self.ids.next_ulid() {
            Ok(id) => id,
            Err(_) => return Reply::event(MessageTone::Warn, reply::STATUS_FAILED),
        };
        let call = DirectCall {
            call_id,
            project_id: *project,
            role: Role::Classify,
            attribution: Attribution::Conversation {
                conversation_id: message.conversation_id,
                purpose: "status_answer".into(),
            },
            level: None,
            critical: false,
            instructions: TrustedText::concat(&[
                &TrustedText::from_static(STATUS_INSTRUCTIONS),
                &TrustedText::from_user_input(format!("USER MESSAGE\n{}", message.text)),
            ]),
            materials: materials(context),
            schema: "gantry.worker.output.classify/1".into(),
            deadline: Some(self.limits.intent_deadline),
        };
        match self.models.invoke_direct(call).await {
            Ok(outcome) => match serde_json::from_value::<StructuredOutput>(outcome.output) {
                Ok(StructuredOutput::Classify(output))
                    if output.target == ClassifyTarget::StatusAnswer =>
                {
                    Reply::agent(AgentKind::Utility, output.text)
                }
                _ => Reply::event(MessageTone::Warn, reply::STATUS_FAILED),
            },
            Err(ModelsError::BudgetExhausted { .. }) => {
                Reply::event(MessageTone::Warn, reply::DAILY_QUOTA)
            }
            Err(_) => Reply::event(MessageTone::Warn, reply::STATUS_FAILED),
        }
    }

    /// 触发该操作的用户消息作为 `ActorCtx`（P-78）。
    fn ctx_for(&self, event: &Event<Value>) -> Result<ActorCtx, ProcessError> {
        let device = if event.actor.kind == ActorKind::Device {
            event.actor.id.parse().ok()
        } else {
            None
        };
        Ok(ActorCtx {
            actor: event.actor.clone(),
            device,
            role: None,
            client_profile: None,
            request_id: self
                .ids
                .next_ulid()
                .map_err(|error| ProcessError::Id(error.to_string()))?,
            idempotency_key: None,
            dedup_key: None,
            causation_id: Some(event.id),
        })
    }

    async fn default_level(&self, project: &ProjectId) -> Result<Level, ProcessError> {
        let snapshot = self.store.snapshot().await?;
        let global = snapshot
            .settings_current(&SettingsScope::Global, SettingsResource::Levels)
            .value
            .unwrap_or_else(|| Value::Object(Default::default()));
        let project_doc = snapshot
            .settings_current_for(
                &SettingsScope::Project,
                Some(project),
                SettingsResource::Levels,
            )
            .value;
        let levels = effective(
            SettingsResource::Levels,
            &builtin_defaults(SettingsResource::Levels),
            &global,
            project_doc.as_ref(),
        )
        .value;
        Ok(levels
            .get("default_level")
            .and_then(|value| serde_json::from_value::<Level>(value.clone()).ok())
            .unwrap_or(Level::Medium))
    }

    /// 喂给模型的材料（§7.5）：引用快照、最近的消息、进行中交付单元的标题。
    /// 全部为不可信文本；`source` 用项目仓库（非 GitHub 对象记为 `#0/<id>`）。
    async fn context(
        &self,
        project: &ProjectId,
        event: &Event<Value>,
        message: &Message,
        located: &[DeliveryHandle],
    ) -> Result<Vec<ContextItem>, ProcessError> {
        let upto = event.seq;
        let Some(repo) = projection::project_repo(&self.store, project, upto).await? else {
            return Ok(Vec::new());
        };
        let mut items = Vec::new();
        let deliveries = projection::deliveries_created(&self.store, project, upto).await?;

        // 引用的 Issue 快照（标题）。
        for reference in message.refs.iter().take(self.limits.max_refs_expanded) {
            if let Ref::Issue {
                repo: ref_repo,
                number,
            } = reference
            {
                let issue = deliveries
                    .iter()
                    .flat_map(|(_, data)| data.issues.iter())
                    .find(|issue| issue.repo == *ref_repo && issue.number == *number);
                if let Some(issue) = issue {
                    items.push(ContextItem {
                        id: format!("ref-issue-{number}"),
                        kind: MaterialKind::IssueBody,
                        source: format!("{}#{number}", issue.repo),
                        text: issue.title.clone(),
                    });
                }
            }
        }
        // 进行中交付单元的标题。
        for handle in located {
            if let Some((_, data)) = deliveries.iter().find(|(id, _)| *id == handle.id) {
                items.push(ContextItem {
                    id: format!("delivery-{}", handle.id),
                    kind: MaterialKind::UpstreamOutput,
                    source: format!("{repo}#0/{}", handle.id),
                    text: UntrustedText::new(data.title.clone(), "delivery".into(), false),
                });
            }
        }
        // 最近的历史消息（不含本条）。
        if let Some(log) =
            projection::conversation_log(&self.store, &message.conversation_id, upto).await?
        {
            let history: Vec<_> = log
                .messages
                .iter()
                .filter(|(seq, _)| *seq < upto)
                .map(|(_, message)| message)
                .collect();
            let start = history.len().saturating_sub(self.limits.history_window);
            for past in &history[start..] {
                items.push(ContextItem {
                    id: format!("history-{}", past.id),
                    kind: MaterialKind::UpstreamOutput,
                    source: format!("{repo}#0/{}", past.id),
                    text: UntrustedText::new(
                        past.text.clone(),
                        format!("message:{}", role_name(&past.role)),
                        false,
                    ),
                });
            }
        }
        Ok(items)
    }

    // -----------------------------------------------------------------------
    // refs 解析（§7.2 P1；OQ-1：只解析 store 里已有的快照）
    // -----------------------------------------------------------------------

    async fn resolves(
        &self,
        project: &ProjectId,
        repo: Option<&RepoSlug>,
        reference: &Ref,
        upto: Seq,
    ) -> Result<bool, StoreError> {
        let deliveries = projection::deliveries_created(&self.store, project, upto).await?;
        Ok(match reference {
            Ref::Delivery { id } => deliveries.iter().any(|(known, _)| known == id),
            Ref::Issue {
                repo: ref_repo,
                number,
            } => {
                repo == Some(ref_repo)
                    && deliveries.iter().any(|(_, data)| {
                        data.issues
                            .iter()
                            .any(|issue| issue.repo == *ref_repo && issue.number == *number)
                    })
            }
            // M1 的 store 还没有 PR / 工作树快照投影（OQ-1）。
            Ref::Pr { .. } | Ref::Worktree { .. } => false,
        })
    }

    async fn log(&self, cnv: &ConversationId) -> Result<(Seq, ConversationLog), ConvError> {
        let head = self.store.events().head().await.map_err(port_error)?;
        let log = projection::conversation_log(&self.store, cnv, head)
            .await
            .map_err(port_error)?
            .ok_or(ConvError::NotFound)?;
        Ok((head, log))
    }
}

#[async_trait]
impl ConversationService for Conversation {
    async fn ensure_conversation(
        &self,
        project: &ProjectId,
        _ctx: &ActorCtx,
    ) -> Result<ConversationId, ConvError> {
        let head = self.store.events().head().await.map_err(port_error)?;
        if projection::project_repo(&self.store, project, head)
            .await
            .map_err(port_error)?
            .is_none()
        {
            return Err(ConvError::NotFound);
        }
        let mut tx = self.store.begin_write().await.map_err(port_error)?;
        let id = self
            .ensure_in_tx(&mut tx, project, None)
            .await
            .map_err(process_port_error)?;
        tx.commit().await.map_err(port_error)?;
        Ok(id)
    }

    async fn post_user_message(
        &self,
        ctx: ActorCtx,
        cnv: ConversationId,
        msg: NewUserMessage,
    ) -> Result<Message, ConvError> {
        let (head, log) = self.log(&cnv).await?;
        let project = log.conversation.project_id;
        let repo = projection::project_repo(&self.store, &project, head)
            .await
            .map_err(port_error)?;
        for reference in &msg.refs {
            if !self
                .resolves(&project, repo.as_ref(), reference, head)
                .await
                .map_err(port_error)?
            {
                return Err(ConvError::UnresolvedRef {
                    reference: reference.clone(),
                });
            }
        }
        // 手机档案：服务端忽略 level_cap（C26、P-35）。
        let level_cap = if ctx.client_profile == Some(ClientProfile::Mobile) {
            None
        } else {
            msg.level_cap
        };
        let id: MessageId = self
            .ids
            .next::<Msg>()
            .map_err(|error| port_rejected(error.to_string()))?;
        let mut tx = self.store.begin_write().await.map_err(port_error)?;
        let now = tx.now();
        let message = Message {
            id,
            conversation_id: cnv,
            role: MessageRole::User,
            agent_kind: None,
            text: msg.text,
            tone: None,
            level_cap,
            refs: msg.refs,
            cards: Vec::new(),
            delivery_id: None,
            created_at: now,
            updated_at: now,
        };
        let appended = tx
            .append(NewEvent {
                ty: EventType::MessagePosted,
                subject: subject(&cnv.to_string()).map_err(process_port_error)?,
                project_id: Some(project),
                actor: ctx.actor.clone(),
                causation_id: ctx.causation_id,
                correlation_id: None,
                data: to_value(&message).map_err(process_port_error)?,
                dedup_key: ctx.dedup_key.clone(),
            })
            .await
            .map_err(port_error)?;
        match appended {
            Appended::New(_) => {
                tx.commit().await.map_err(port_error)?;
                Ok(message)
            }
            Appended::Duplicate { existing_seq } => {
                tx.rollback().await.map_err(port_error)?;
                let event = self
                    .store
                    .events()
                    .get_by_seq(existing_seq)
                    .await
                    .map_err(port_error)?
                    .ok_or(ConvError::NotFound)?;
                serde_json::from_value(event.data).map_err(|error| port_rejected(error.to_string()))
            }
        }
    }

    async fn list_conversations(
        &self,
        project: &ProjectId,
        _page: PageQuery,
    ) -> Result<Page<ConversationRow>, ConvError> {
        let head = self.store.events().head().await.map_err(port_error)?;
        let items = projection::conversation_of_project(&self.store, project, head)
            .await
            .map_err(port_error)?
            .into_iter()
            .collect();
        Ok(Page {
            items,
            next_cursor: None,
        })
    }

    /// 默认 `order = desc`；游标是最后一条的 `seq`，不随新消息漂移（§7.11）。
    async fn list_messages(
        &self,
        cnv: &ConversationId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError> {
        let before = match page.cursor.as_deref() {
            None => None,
            Some(cursor) => Some(parse_cursor(cursor)?),
        };
        let limit = match page.limit {
            0 => 50,
            limit => limit.min(200),
        } as usize;
        let (head, log) = self.log(cnv).await?;
        let mut newest_first: Vec<_> = log
            .messages
            .into_iter()
            .rev()
            .filter(|(seq, _)| before.is_none_or(|before| seq.as_u64() < before))
            .collect();
        let more = newest_first.len() > limit;
        newest_first.truncate(limit);
        let next_cursor = if more {
            newest_first
                .last()
                .map(|(seq, _)| format!("seq:{}", seq.as_u64()))
        } else {
            None
        };
        Ok(PageWithSeq {
            items: newest_first
                .into_iter()
                .map(|(_, message)| message)
                .collect(),
            next_cursor,
            last_seq: head,
        })
    }
}

/// 交付单元标题：首行去控制符后截断到 80 字符；空则用"来自 <ref>"（§7.6）。
pub fn title(text: &str, refs: &[Ref]) -> String {
    let first = text
        .lines()
        .map(|line| {
            line.chars()
                .filter(|ch| !ch.is_control())
                .collect::<String>()
        })
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let trimmed: String = first.trim().chars().take(TITLE_MAX_CHARS).collect();
    if !trimmed.is_empty() {
        return trimmed;
    }
    match refs.first() {
        Some(Ref::Issue { repo, number }) => format!("来自 {repo}#{number}"),
        Some(Ref::Pr { repo, number }) => format!("来自 {repo}#{number}"),
        Some(Ref::Delivery { id }) => format!("来自 {id}"),
        Some(Ref::Worktree { id }) => format!("来自 {id}"),
        None => "来自对话".to_owned(),
    }
}

fn role_name(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Agent => "agent",
        MessageRole::Event => "event",
    }
}

fn parse_cursor(cursor: &str) -> Result<u64, ConvError> {
    cursor
        .strip_prefix("seq:")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ConvError::InvalidRequest {
            pointer: "/cursor".into(),
            message: "unknown cursor".into(),
        })
}

fn subject(id: &str) -> Result<Subject, ProcessError> {
    Subject::parse(id)
        .map_err(|error| ProcessError::Store(StoreError::PreconditionFailed(error.to_string())))
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, ProcessError> {
    serde_json::to_value(value)
        .map_err(|error| ProcessError::Store(StoreError::PreconditionFailed(error.to_string())))
}

fn port_rejected(message: String) -> ConvError {
    ConvError::Store(PortStoreError::Rejected { message })
}

fn port_error(error: StoreError) -> ConvError {
    ConvError::Store(match error {
        StoreError::NotFound => PortStoreError::NotFound,
        StoreError::Poisoned => PortStoreError::Unavailable { retry_after: None },
        StoreError::Unsupported(message) => PortStoreError::Rejected {
            message: message.to_owned(),
        },
        other => PortStoreError::InvalidState {
            message: other.to_string(),
        },
    })
}

fn process_port_error(error: ProcessError) -> ConvError {
    match error {
        ProcessError::Store(error) => port_error(error),
        other => port_rejected(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_takes_first_non_empty_line_without_control_chars() {
        assert_eq!(title("\n  修复\u{7}登录  \n第二行", &[]), "修复登录");
        let long = "字".repeat(100);
        assert_eq!(title(&long, &[]).chars().count(), TITLE_MAX_CHARS);
        assert_eq!(title("   ", &[]), "来自对话");
    }
}
