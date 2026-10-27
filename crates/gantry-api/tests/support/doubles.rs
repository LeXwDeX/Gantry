//! 写类端口的替身：按 `docs/milestones.md` 用 `gantry-fakes` 的 `FakeEngine` /
//! `FakeConversation`（只记录调用、按脚本返回），本文件不实现任何业务语义。
//!
//! - [`ConvFaults`]：在 `FakeConversation` 外加一层故障注入（脚本化下一次错误），
//!   其余调用原样转交；
//! - [`record`]：存储夹具——预置"所属模块按 `ctx.dedup_key` 追加的事件"，
//!   用来断言 `X-Event-Seq` 的取法。事件由测试准备，不由替身产生。

use std::sync::Mutex;

use async_trait::async_trait;
use gantry_core::entities::{Actor, ActorKind, Conversation, Message, Seq};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{ConversationId, ProjectId};
use gantry_core::ports::context::{ActorCtx, PageQuery, PageWithSeq};
use gantry_core::ports::conversation::{ConvError, ConversationService, NewUserMessage};
use gantry_fakes::conversation::FakeConversation;
use gantry_store::{Appended, Store};
use serde_json::Value;

const CASES: &str = include_str!("../../../gantry-core/fixtures/events/cases.json");

/// 契约正例夹具中某类事件的 `data`。
pub fn fixture_data(ty: &str) -> Value {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    cases["valid"][ty].clone()
}

/// 追加一条夹具事件并返回其 `seq`。
pub async fn record(
    store: &Store,
    ty: EventType,
    subject: &str,
    project_id: Option<ProjectId>,
    data: Value,
    dedup_key: Option<String>,
) -> Seq {
    let mut tx = store.begin_write().await.unwrap();
    let appended = tx
        .append(NewEvent {
            ty,
            subject: Subject::parse(subject).unwrap(),
            project_id,
            actor: Actor::new(ActorKind::Engine, "engine"),
            causation_id: None,
            correlation_id: None,
            data,
            dedup_key,
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    match appended {
        Appended::New(event) => event.seq,
        Appended::Duplicate { existing_seq } => existing_seq,
    }
}

/// `FakeConversation` 外的故障注入层。
pub struct ConvFaults {
    pub inner: FakeConversation,
    pub next_error: Mutex<Option<ConvError>>,
}

impl ConvFaults {
    pub fn new() -> Self {
        Self {
            inner: FakeConversation::new(),
            next_error: Mutex::new(None),
        }
    }

    pub fn fail_next(&self, error: ConvError) {
        *self.next_error.lock().unwrap() = Some(error);
    }
}

#[async_trait]
impl ConversationService for ConvFaults {
    async fn ensure_conversation(
        &self,
        project: &ProjectId,
        ctx: &ActorCtx,
    ) -> Result<ConversationId, ConvError> {
        self.inner.ensure_conversation(project, ctx).await
    }

    async fn post_user_message(
        &self,
        ctx: ActorCtx,
        cnv: ConversationId,
        msg: NewUserMessage,
    ) -> Result<Message, ConvError> {
        let result = self.inner.post_user_message(ctx, cnv, msg).await;
        match self.next_error.lock().unwrap().take() {
            Some(error) => Err(error),
            None => result,
        }
    }

    async fn list_conversations(
        &self,
        project: &ProjectId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Conversation>, ConvError> {
        self.inner.list_conversations(project, page).await
    }

    async fn list_messages(
        &self,
        cnv: &ConversationId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError> {
        self.inner.list_messages(cnv, page).await
    }
}
