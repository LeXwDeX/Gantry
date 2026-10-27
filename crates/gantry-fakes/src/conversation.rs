//! `FakeConversation`：内存对话 facade 替身。
//!
//! 记录每次调用；`ensure_conversation` 按项目幂等，`post_user_message` 优先
//! 返回脚本队列里的消息，否则按输入构造一条用户消息。不实现意图解析，也不
//! 调用引擎。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use gantry_core::entities::Seq;
use gantry_core::entities::{Conversation, Message, MessageRole};
use gantry_core::ids::{ConversationId, Id, IdKind, MessageId, ProjectId};
use gantry_core::ports::context::{ActorCtx, PageOrder, PageQuery, PageWithSeq};
use gantry_core::ports::conversation::{ConvError, ConversationService, NewUserMessage};
use gantry_core::time::Timestamp;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationCall {
    Ensure {
        project: ProjectId,
        ctx: ActorCtx,
    },
    PostUserMessage {
        ctx: ActorCtx,
        conversation_id: ConversationId,
        message: NewUserMessage,
    },
    ListConversations {
        project: ProjectId,
        page: PageQuery,
    },
    ListMessages {
        conversation_id: ConversationId,
        page: PageQuery,
    },
}

#[derive(Default)]
struct State {
    conversations: BTreeMap<ConversationId, Conversation>,
    by_project: BTreeMap<ProjectId, ConversationId>,
    messages: BTreeMap<ConversationId, Vec<Message>>,
    message_seq: u64,
    dedup: BTreeMap<String, Message>,
    scripts: BTreeMap<ConversationId, VecDeque<Message>>,
    calls: Vec<ConversationCall>,
}

/// M0 的对话服务替身。
pub struct FakeConversation {
    state: Mutex<State>,
    next: AtomicU64,
}

impl Default for FakeConversation {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeConversation {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            next: AtomicU64::new(1),
        }
    }

    /// 为某个对话追加一条脚本消息；`post_user_message` 按 FIFO 取用。
    pub fn push_scripted_message(&self, conversation_id: ConversationId, message: Message) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .scripts
            .entry(conversation_id)
            .or_default()
            .push_back(message);
    }

    pub fn calls(&self) -> Vec<ConversationCall> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .calls
            .clone()
    }

    fn next_id<K: IdKind>(&self) -> Id<K> {
        let ms = now_ms();
        let random = self.next.fetch_add(1, Ordering::Relaxed) as u128;
        Id::from_parts(ms, random).expect("fake id parts are in range")
    }

    fn next_message_id(&self) -> gantry_core::ids::MessageId {
        let random = self.next.fetch_add(1, Ordering::Relaxed) as u128;
        Id::from_parts(now_ms(), random).expect("fake id parts are in range")
    }
}

fn now() -> Timestamp {
    Timestamp::from_millis(now_ms() as i64)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(1_700_000_000_000)
}

fn parse_cursor(
    cursor: &str,
    cnv: &ConversationId,
    order: PageOrder,
) -> Result<MessageId, ConvError> {
    let direction = match order {
        PageOrder::Asc => "asc",
        PageOrder::Desc => "desc",
    };
    let prefix = format!("messages:{cnv}:{direction}:");
    cursor
        .strip_prefix(&prefix)
        .and_then(|value| {
            value
                .parse::<MessageId>()
                .ok()
                .filter(|id| id.to_string() == value)
        })
        .ok_or_else(|| ConvError::InvalidRequest {
            pointer: "/cursor".into(),
            message: "unknown cursor".into(),
        })
}

#[async_trait]
impl ConversationService for FakeConversation {
    async fn ensure_conversation(
        &self,
        project: &ProjectId,
        ctx: &ActorCtx,
    ) -> Result<ConversationId, ConvError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(ConversationCall::Ensure {
            project: *project,
            ctx: ctx.clone(),
        });
        if let Some(id) = state.by_project.get(project) {
            return Ok(*id);
        }
        let id: ConversationId = self.next_id();
        let now = now();
        state.conversations.insert(
            id,
            Conversation {
                id,
                project_id: *project,
                created_at: now,
                updated_at: now,
            },
        );
        state.by_project.insert(*project, id);
        Ok(id)
    }

    async fn post_user_message(
        &self,
        ctx: ActorCtx,
        cnv: ConversationId,
        msg: NewUserMessage,
    ) -> Result<Message, ConvError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dedup_key = ctx.dedup_key.clone();
        state.calls.push(ConversationCall::PostUserMessage {
            ctx,
            conversation_id: cnv,
            message: msg.clone(),
        });
        if !state.conversations.contains_key(&cnv) {
            return Err(ConvError::NotFound);
        }
        if let Some(message) = dedup_key.as_ref().and_then(|key| state.dedup.get(key)) {
            return Ok(message.clone());
        }
        let now = now();
        let mut message = match state
            .scripts
            .get_mut(&cnv)
            .and_then(|queue| queue.pop_front())
        {
            Some(scripted) => scripted,
            None => Message {
                id: self.next_message_id(),
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
            },
        };
        while state
            .messages
            .values()
            .flatten()
            .any(|old| old.id == message.id)
        {
            message.id = self.next_message_id();
        }
        state.message_seq += 1;
        state.messages.entry(cnv).or_default().push(message.clone());
        if let Some(key) = dedup_key {
            state.dedup.insert(key, message.clone());
        }
        if let Some(conversation) = state.conversations.get_mut(&cnv) {
            conversation.updated_at = now;
        }
        Ok(message)
    }

    async fn list_conversations(
        &self,
        project: &ProjectId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Conversation>, ConvError> {
        if page.cursor.is_some() {
            return Err(ConvError::InvalidRequest {
                pointer: "/cursor".into(),
                message: "unknown cursor".into(),
            });
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(ConversationCall::ListConversations {
            project: *project,
            page: page.clone(),
        });
        let all: Vec<Conversation> = state
            .conversations
            .values()
            .filter(|conversation| conversation.project_id == *project)
            .cloned()
            .collect();
        Ok(PageWithSeq {
            items: all,
            next_cursor: None,
            // 此水位只计替身消息写入，不是事件日志序号。
            last_seq: Seq::from_wire(state.message_seq).expect("fake message sequence fits seq"),
        })
    }

    async fn list_messages(
        &self,
        cnv: &ConversationId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError> {
        let order = page.order.unwrap_or(PageOrder::Desc);
        let boundary = page
            .cursor
            .as_deref()
            .map(|cursor| parse_cursor(cursor, cnv, order))
            .transpose()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(ConversationCall::ListMessages {
            conversation_id: *cnv,
            page: page.clone(),
        });
        if !state.conversations.contains_key(cnv) {
            return Err(ConvError::NotFound);
        }
        let mut messages: Vec<_> = state
            .messages
            .get(cnv)
            .into_iter()
            .flatten()
            .filter(|message| {
                boundary.is_none_or(|boundary| match order {
                    PageOrder::Asc => message.id > boundary,
                    PageOrder::Desc => message.id < boundary,
                })
            })
            .cloned()
            .collect();
        messages.sort_by_key(|message| message.id);
        if order == PageOrder::Desc {
            messages.reverse();
        }
        let limit = if page.limit == 0 {
            50
        } else {
            page.limit.min(200)
        } as usize;
        let more = messages.len() > limit;
        messages.truncate(limit);
        let next_cursor = if more {
            messages.last().map(|message| match order {
                PageOrder::Asc => format!("messages:{cnv}:asc:{}", message.id),
                PageOrder::Desc => format!("messages:{cnv}:desc:{}", message.id),
            })
        } else {
            None
        };
        Ok(PageWithSeq {
            items: messages,
            next_cursor,
            last_seq: Seq::from_wire(state.message_seq).expect("fake message sequence fits seq"),
        })
    }
}
