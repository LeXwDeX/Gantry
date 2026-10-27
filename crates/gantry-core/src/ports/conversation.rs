//! `ConversationService`（`api` 的对话 facade）。
//!
//! 签名以 `docs/modules/conversation.md` §6 为准。写方法使用 [`ActorCtx`]；
//! 手机档案的限制由实现读取 `ctx.client_profile` 判定。

use async_trait::async_trait;

use crate::entities::{Conversation, Message, Ref};
use crate::ids::{ConversationId, ProjectId};
use crate::level::Level;

use super::context::{ActorCtx, PageQuery, PageWithSeq, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewUserMessage {
    pub text: String,
    pub level_cap: Option<Level>,
    pub refs: Vec<Ref>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConvError {
    #[error("conversation not found")]
    NotFound,
    #[error("reference does not resolve to a snapshot")]
    UnresolvedRef { reference: Ref },
    #[error("invalid conversation request at {pointer}: {message}")]
    InvalidRequest { pointer: String, message: String },
    #[error("forbidden")]
    Forbidden,
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[async_trait]
pub trait ConversationService: Send + Sync {
    /// 项目绑定后创建；幂等返回现有对话。
    async fn ensure_conversation(
        &self,
        project: &ProjectId,
        ctx: &ActorCtx,
    ) -> Result<ConversationId, ConvError>;

    /// POST /v1/conversations/{id}/messages 的同步部分。
    async fn post_user_message(
        &self,
        ctx: ActorCtx,
        cnv: ConversationId,
        msg: NewUserMessage,
    ) -> Result<Message, ConvError>;

    async fn list_conversations(
        &self,
        project: &ProjectId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Conversation>, ConvError>;

    async fn list_messages(
        &self,
        cnv: &ConversationId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError>;
}
