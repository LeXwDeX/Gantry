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
use gantry_core::ids::{ConversationId, Id, IdKind, ProjectId};
use gantry_core::ports::context::{ActorCtx, Page, PageQuery, PageWithSeq};
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

fn page_bounds(page: &PageQuery, len: usize) -> (usize, usize) {
    let start = page
        .cursor
        .as_deref()
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(len);
    let limit = if page.limit == 0 {
        50
    } else {
        page.limit as usize
    };
    let end = start.saturating_add(limit).min(len);
    (start, end)
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
        state.calls.push(ConversationCall::PostUserMessage {
            ctx,
            conversation_id: cnv,
            message: msg.clone(),
        });
        if !state.conversations.contains_key(&cnv) {
            return Err(ConvError::NotFound);
        }
        let now = now();
        let message = match state
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
        state.messages.entry(cnv).or_default().push(message.clone());
        if let Some(conversation) = state.conversations.get_mut(&cnv) {
            conversation.updated_at = now;
        }
        Ok(message)
    }

    async fn list_conversations(
        &self,
        project: &ProjectId,
        page: PageQuery,
    ) -> Result<Page<Conversation>, ConvError> {
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
        let (start, end) = page_bounds(&page, all.len());
        let next_cursor = (end < all.len()).then(|| end.to_string());
        Ok(Page {
            items: all[start..end].to_vec(),
            next_cursor,
        })
    }

    async fn list_messages(
        &self,
        cnv: &ConversationId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(ConversationCall::ListMessages {
            conversation_id: *cnv,
            page: page.clone(),
        });
        let messages = state.messages.get(cnv).ok_or(ConvError::NotFound)?.clone();
        let (start, end) = page_bounds(&page, messages.len());
        let next_cursor = (end < messages.len()).then(|| end.to_string());
        let last_seq = if messages.is_empty() {
            Seq::ZERO
        } else {
            Seq::new(messages.len() as u64).expect("message count fits seq")
        };
        Ok(PageWithSeq {
            items: messages[start..end].to_vec(),
            next_cursor,
            last_seq,
        })
    }
}
