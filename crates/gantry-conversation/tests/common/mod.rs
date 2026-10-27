//! 测试共用：内存 Store、引擎薄桩（conversation.md §12.1）、模型替身与构造器。
//!
//! 引擎桩只记录调用并按脚本返回：`create_delivery` 以 `source_message_id` 幂等，
//! 创建时像真实引擎一样写一条 `delivery.created`，并把消息的 refs 绑定到新交付
//! 单元，供后续 `locate_deliveries` 命中。不重现引擎的其他逻辑。

#![allow(dead_code, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::future::pending;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_conversation::{
    Conversation, ConversationDeps, CrashPoint, DeliveryHandle, EngineApi, Limits, ModelsPort,
};
use gantry_core::canon::RepoSlug;
use gantry_core::entities::{
    Actor, ActorKind, IssueRef, IssueState, Message, Ref, Seq, Stage, TokenUsage,
};
use gantry_core::events::{EventType, NewEvent, ServedBy, Subject};
use gantry_core::ids::{ConversationId, DeliveryId, Dlv, Id, IdGen, MessageId, ProjectId, Ulid};
use gantry_core::level::Level;
use gantry_core::ports::context::{ActorCtx, ClientProfile, PageQuery};
use gantry_core::ports::conversation::{ConversationService, NewUserMessage};
use gantry_core::ports::engine::{CreateDelivery, EngineError};
use gantry_core::ports::models::UsageSource;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use gantry_models::{DirectCall, DirectOutcome, ModelsError};
use gantry_store::{EventFilter, Store, StoreDeps, TypePattern};
use serde_json::{json, Value};

pub const NOW_MS: i64 = 1_700_000_000_000;
pub const REPO: &str = "acme/web";
const CASES: &str = include_str!("../../../gantry-core/fixtures/events/cases.json");

// ---------------------------------------------------------------------------
// 引擎薄桩
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineCall {
    Create(Box<CreateDelivery>),
    Locate { project: ProjectId, refs: Vec<Ref> },
}

#[derive(Clone)]
struct Known {
    handle: DeliveryHandle,
    refs: Vec<Ref>,
    terminal: bool,
}

#[derive(Default)]
struct EngineState {
    calls: Vec<EngineCall>,
    by_message: BTreeMap<MessageId, DeliveryId>,
    deliveries: Vec<Known>,
    create_error: Option<EngineError>,
}

pub struct StubEngine {
    store: Store,
    ids: Arc<IdGen>,
    state: Mutex<EngineState>,
    /// 置位后，下一次 `create_delivery` 在"已创建"之后 panic（模拟崩溃）。
    crash_after_create: AtomicBool,
}

impl StubEngine {
    pub fn calls(&self) -> Vec<EngineCall> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn creates(&self) -> Vec<CreateDelivery> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                EngineCall::Create(cmd) => Some(*cmd),
                EngineCall::Locate { .. } => None,
            })
            .collect()
    }

    pub fn delivery_count(&self) -> usize {
        self.state.lock().unwrap().by_message.len()
    }

    pub fn fail_create(&self, error: EngineError) {
        self.state.lock().unwrap().create_error = Some(error);
    }

    pub fn crash_after_next_create(&self) {
        self.crash_after_create.store(true, Ordering::SeqCst);
    }

    /// 预置一个交付单元：写 `delivery.created`（含 Issue 快照），并按 `refs` 绑定。
    pub async fn seed(
        &self,
        project: ProjectId,
        cap: Level,
        issues: &[u64],
        terminal: bool,
    ) -> DeliveryId {
        let id = self.ids.next::<Dlv>().unwrap();
        let mut refs = vec![Ref::Delivery { id }];
        refs.extend(issues.iter().map(|number| issue_ref(*number)));
        write_delivery(&self.store, project, id, "seeded", cap, None, issues).await;
        self.state.lock().unwrap().deliveries.push(Known {
            handle: handle(id, project, cap),
            refs,
            terminal,
        });
        id
    }
}

fn handle(id: DeliveryId, project: ProjectId, cap: Level) -> DeliveryHandle {
    DeliveryHandle {
        id,
        project_id: project,
        level_cap: cap,
        stage: Stage::Spec,
        open_spec_needed: None,
    }
}

#[async_trait]
impl EngineApi for StubEngine {
    async fn create_delivery(&self, cmd: CreateDelivery) -> Result<DeliveryHandle, EngineError> {
        let existing = {
            let mut state = self.state.lock().unwrap();
            state.calls.push(EngineCall::Create(Box::new(cmd.clone())));
            if let Some(error) = state.create_error.clone() {
                return Err(error);
            }
            state.by_message.get(&cmd.source_message_id).copied()
        };
        let id = match existing {
            Some(id) => id,
            None => {
                let id = self.ids.next::<Dlv>().unwrap();
                write_delivery(
                    &self.store,
                    cmd.project_id,
                    id,
                    &cmd.title,
                    cmd.level_cap,
                    Some(cmd.source_message_id),
                    &[],
                )
                .await;
                let mut refs = vec![Ref::Delivery { id }];
                refs.extend(cmd.refs.iter().cloned());
                let mut state = self.state.lock().unwrap();
                state.by_message.insert(cmd.source_message_id, id);
                state.deliveries.push(Known {
                    handle: handle(id, cmd.project_id, cmd.level_cap),
                    refs,
                    terminal: false,
                });
                id
            }
        };
        if self.crash_after_create.swap(false, Ordering::SeqCst) {
            panic!("injected crash after create_delivery");
        }
        let state = self.state.lock().unwrap();
        let known = state
            .deliveries
            .iter()
            .find(|known| known.handle.id == id)
            .unwrap();
        Ok(known.handle.clone())
    }

    async fn locate_deliveries(
        &self,
        project: &ProjectId,
        refs: &[Ref],
    ) -> Result<Vec<DeliveryHandle>, EngineError> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(EngineCall::Locate {
            project: *project,
            refs: refs.to_vec(),
        });
        Ok(state
            .deliveries
            .iter()
            .filter(|known| !known.terminal && known.handle.project_id == *project)
            .filter(|known| known.refs.iter().any(|bound| refs.contains(bound)))
            .map(|known| known.handle.clone())
            .collect())
    }
}

async fn write_delivery(
    store: &Store,
    project: ProjectId,
    id: DeliveryId,
    title: &str,
    cap: Level,
    source: Option<MessageId>,
    issues: &[u64],
) {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let mut data = cases["valid"]["delivery.created"].clone();
    data["title"] = json!(title);
    data["level_cap"] = json!(cap);
    data["level_effective"] = json!(cap);
    data["source_message_id"] = json!(source);
    let issues: Vec<IssueRef> = issues
        .iter()
        .map(|number| issue_snapshot(*number, &format!("Issue {number}")))
        .collect();
    data["issues"] = serde_json::to_value(issues).unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(NewEvent {
        ty: EventType::DeliveryCreated,
        subject: Subject::parse(&id.to_string()).unwrap(),
        project_id: Some(project),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

pub fn issue_snapshot(number: u64, title: &str) -> IssueRef {
    IssueRef {
        repo: RepoSlug::parse(REPO).unwrap(),
        number,
        node_id: format!("I_{number}"),
        title: UntrustedText::new(title.into(), "github".into(), false),
        state: IssueState::Open,
        state_reason: None,
        synced_at: Timestamp::from_millis(NOW_MS),
    }
}

/// 把带标题的 Issue 快照写进 store（经一个已终结的交付单元，定位不到它）。
pub async fn seed_issue(h: &Harness, number: u64, title: &str) {
    let id = h.ids.next::<Dlv>().unwrap();
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let mut data = cases["valid"]["delivery.created"].clone();
    data["source_message_id"] = Value::Null;
    data["issues"] = serde_json::to_value(vec![issue_snapshot(number, title)]).unwrap();
    let mut tx = h.store.begin_write().await.unwrap();
    tx.append(NewEvent {
        ty: EventType::DeliveryCreated,
        subject: Subject::parse(&id.to_string()).unwrap(),
        project_id: Some(h.project),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

pub fn issue_ref(number: u64) -> Ref {
    Ref::Issue {
        repo: RepoSlug::parse(REPO).unwrap(),
        number,
    }
}

// ---------------------------------------------------------------------------
// 模型替身（经 ModelsPort；不直连提供方）
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum Script {
    Output(Value),
    Error(ModelsError),
    Hang,
}

#[derive(Default)]
struct ModelsState {
    configured: bool,
    /// (用户文本片段, 脚本)；首个命中生效。
    intent: Vec<(String, Script)>,
    classify: Option<Script>,
    calls: Vec<DirectCall>,
}

pub struct StubModels {
    state: Mutex<ModelsState>,
}

impl StubModels {
    pub fn calls(&self) -> Vec<DirectCall> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn set_configured(&self, configured: bool) {
        self.state.lock().unwrap().configured = configured;
    }

    /// 用户文本含 `fragment` 时的 intent 输出。
    pub fn on_intent(&self, fragment: &str, script: Script) {
        self.state
            .lock()
            .unwrap()
            .intent
            .push((fragment.into(), script));
    }

    pub fn on_classify(&self, script: Script) {
        self.state.lock().unwrap().classify = Some(script);
    }
}

pub fn intent(kind: &str, needs_code: bool, delivery: Option<DeliveryId>) -> Script {
    Script::Output(json!({
        "kind": "intent",
        "intent": kind,
        "needs_code": needs_code,
        "delivery_id": delivery,
    }))
}

pub fn status_answer(text: &str) -> Script {
    Script::Output(json!({"kind": "classify", "target": "status_answer", "text": text}))
}

#[async_trait]
impl ModelsPort for StubModels {
    async fn configured(&self, _project: &ProjectId) -> Result<bool, ModelsError> {
        Ok(self.state.lock().unwrap().configured)
    }

    async fn invoke_direct(&self, call: DirectCall) -> Result<DirectOutcome, ModelsError> {
        let script = {
            let mut state = self.state.lock().unwrap();
            state.calls.push(call.clone());
            match call.schema.as_str() {
                "gantry.worker.output.intent/1" => state
                    .intent
                    .iter()
                    .find(|(fragment, _)| call.instructions.as_str().contains(fragment.as_str()))
                    .map(|(_, script)| script.clone()),
                _ => state.classify.clone(),
            }
        };
        match script.unwrap_or(Script::Error(ModelsError::Unavailable {
            reason: "no script".into(),
        })) {
            Script::Output(output) => Ok(DirectOutcome {
                output,
                served_by: ServedBy {
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    effort: None,
                },
                usage: TokenUsage::new(10, 5, 0, 0, 1),
                usage_source: UsageSource::ProviderReported,
                weighted_tokens: 15,
                tries: 1,
                fell_back: false,
                charged_seq: None,
                warnings: Vec::new(),
            }),
            Script::Error(error) => Err(error),
            Script::Hang => pending().await,
        }
    }
}

// ---------------------------------------------------------------------------
// 崩溃注入
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct CrashOnce(AtomicBool);

impl CrashOnce {
    pub fn arm(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl CrashPoint for CrashOnce {
    fn before_commit(&self, consumer: &str, _seq: Seq) -> bool {
        consumer == gantry_conversation::CURSOR_USER_MESSAGES
            && self.0.swap(false, Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// 组装
// ---------------------------------------------------------------------------

pub struct Harness {
    pub store: Store,
    pub ids: Arc<IdGen>,
    pub engine: Arc<StubEngine>,
    pub models: Arc<StubModels>,
    pub conv: Conversation,
    pub project: ProjectId,
}

pub fn limits() -> Limits {
    Limits {
        intent_deadline: Duration::from_millis(200),
        process_deadline: Duration::from_secs(5),
        ..Limits::default()
    }
}

impl Harness {
    /// 同一 store 与依赖上新建一个服务实例（模拟重启）。
    pub fn restart(&self) -> Conversation {
        self.restart_with(None)
    }

    pub fn restart_with(&self, crash: Option<Arc<dyn CrashPoint>>) -> Conversation {
        let conv = Conversation::new(ConversationDeps {
            store: self.store.clone(),
            ids: self.ids.clone(),
            engine: self.engine.clone(),
            models: self.models.clone(),
            limits: limits(),
        });
        match crash {
            Some(crash) => conv.with_crash_point(crash),
            None => conv,
        }
    }

    /// 处理到没有待办为止。
    pub async fn settle(&self) {
        while self.conv.run_once().await.unwrap() > 0 {}
    }

    pub async fn add_project(&self, project: ProjectId) {
        let cases: Value = serde_json::from_str(CASES).unwrap();
        let mut data = cases["valid"]["project.added"].clone();
        data["id"] = json!(project);
        let mut tx = self.store.begin_write().await.unwrap();
        tx.append(NewEvent {
            ty: EventType::ProjectAdded,
            subject: Subject::parse(&project.to_string()).unwrap(),
            project_id: Some(project),
            actor: Actor::new(ActorKind::System, "api"),
            causation_id: None,
            correlation_id: None,
            data,
            dedup_key: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    pub async fn conversation(&self) -> ConversationId {
        self.conv
            .ensure_conversation(&self.project, &ctx())
            .await
            .unwrap()
    }

    pub async fn post(&self, text: &str, cap: Option<Level>, refs: Vec<Ref>) -> Message {
        self.post_as(desktop(), text, cap, refs).await
    }

    pub async fn post_as(
        &self,
        ctx: ActorCtx,
        text: &str,
        cap: Option<Level>,
        refs: Vec<Ref>,
    ) -> Message {
        let cnv = self.conversation().await;
        self.conv
            .post_user_message(
                ctx,
                cnv,
                NewUserMessage {
                    text: text.into(),
                    level_cap: cap,
                    refs,
                },
            )
            .await
            .unwrap()
    }

    /// 按 `seq` 升序的全部消息（已回填 delivery_id）。
    pub async fn messages(&self) -> Vec<Message> {
        let cnv = self.conversation().await;
        let mut page = self
            .conv
            .list_messages(
                &cnv,
                PageQuery {
                    cursor: None,
                    limit: 200,
                    order: None,
                },
            )
            .await
            .unwrap()
            .items;
        page.reverse();
        page
    }

    pub async fn replies(&self) -> Vec<Message> {
        self.messages()
            .await
            .into_iter()
            .filter(|message| message.role != gantry_core::entities::MessageRole::User)
            .collect()
    }

    pub async fn count(&self, ty: EventType) -> usize {
        let filter = EventFilter {
            include: vec![TypePattern::Exact(ty)],
            ..EventFilter::default()
        };
        self.store
            .events()
            .scan(Seq::ZERO, &filter, 200)
            .await
            .unwrap()
            .events
            .len()
    }
}

pub async fn harness() -> Harness {
    let clock = Arc::new(FakeClock::new(NOW_MS));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(0xc0))));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock,
        ids: ids.clone(),
    })
    .unwrap();
    let engine = Arc::new(StubEngine {
        store: store.clone(),
        ids: ids.clone(),
        state: Mutex::new(EngineState::default()),
        crash_after_create: AtomicBool::new(false),
    });
    let models = Arc::new(StubModels {
        state: Mutex::new(ModelsState {
            configured: true,
            ..ModelsState::default()
        }),
    });
    let conv = Conversation::new(ConversationDeps {
        store: store.clone(),
        ids: ids.clone(),
        engine: engine.clone(),
        models: models.clone(),
        limits: limits(),
    });
    let project: ProjectId = Id::from_parts(NOW_MS as u64, 1).unwrap();
    let h = Harness {
        store,
        ids,
        engine,
        models,
        conv,
        project,
    };
    h.add_project(project).await;
    h.settle().await;
    h
}

pub fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(NOW_MS as u64, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

pub fn device_ctx(profile: ClientProfile) -> ActorCtx {
    let device = Id::from_parts(NOW_MS as u64, 77).unwrap();
    ActorCtx {
        actor: Actor::new(ActorKind::Device, device.to_string()),
        device: Some(device),
        client_profile: Some(profile),
        ..ctx()
    }
}

pub fn desktop() -> ActorCtx {
    device_ctx(ClientProfile::Desktop)
}
