//! `InboxPort`（收件箱）。
//!
//! 签名以 `docs/modules/notify.md` §6.1 为准。`RaiseRequest` 的字段按文档保留；
//! 写方法额外接收 [`ActorCtx`]，用于 P-78 的审计与档案判定。
//!
//! `RaiseRequest.payload` 在规格书中写 `serde_json::Value`，但 data-model 已有
//! 强类型 `InboxPayload`；这里使用后者，避免用通用 JSON 抹平协议。

use std::fmt;

use async_trait::async_trait;

use crate::entities::{
    Actor, ApprovalStatus, InboxItem, InboxKind, InboxPayload, InboxStatus, Seq, Severity,
};
use crate::ids::{ApprovalId, DeliveryId, EventId, InboxId, MessageId, ProjectId};
use crate::text::UntrustedText;

use super::context::{ActorCtx, Page, PageQuery, StoreError, TxHandle};

/// 模板或 `UntrustedText` 构造的摘要；只通过受控构造函数产生，`Debug` 输出
/// 清理后的文本，不保留控制字符或 bidi 覆盖字符。
#[derive(Clone, PartialEq, Eq)]
pub struct SafeSummary(String);

impl SafeSummary {
    /// 去控制字符、折叠换行并截断到 1024 字节。
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let mut cleaned = String::with_capacity(value.len().min(1024));
        let mut last_was_space = false;
        for ch in value.chars() {
            if ch == '\n' || ch == '\r' {
                if !last_was_space {
                    cleaned.push(' ');
                    last_was_space = true;
                }
                continue;
            }
            if ch.is_control() || matches!(ch as u32, 0x202a..=0x202e | 0x2066..=0x2069) {
                continue;
            }
            if cleaned.len() + ch.len_utf8() > 1021 {
                if !cleaned.ends_with('…') {
                    cleaned.push('…');
                }
                break;
            }
            cleaned.push(ch);
            last_was_space = ch == ' ';
        }
        Self(cleaned)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SafeSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SafeSummary").field(&self.0).finish()
    }
}

impl fmt::Display for SafeSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<UntrustedText> for SafeSummary {
    fn from(value: UntrustedText) -> Self {
        Self::new(value.as_untrusted_str())
    }
}

/// 去重键。`notify.md` §7.2 给出模板；本类型只承载不透明字符串。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DedupKey(String);

impl DedupKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DedupKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DedupKey").field(&self.0).finish()
    }
}

impl fmt::Display for DedupKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 用户备注，最多 2048 字节。
#[derive(Clone, PartialEq, Eq)]
pub struct Note(String);

impl Note {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.len() > 2048 {
            None
        } else {
            Some(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Note").field(&self.0).finish()
    }
}

/// 上报项选项 id；词表归创建者。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OptionId(String);

impl OptionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// APNs 令牌；`Debug` 脱敏，不进入事件或读响应。
#[derive(Clone, PartialEq, Eq)]
pub struct PushToken(String);

impl PushToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PushToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PushToken(***)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApnsEnvironment {
    Production,
    Sandbox,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaiseRequest {
    pub project_id: Option<ProjectId>,
    pub delivery_id: Option<DeliveryId>,
    pub kind: InboxKind,
    pub severity: Severity,
    pub summary: SafeSummary,
    pub payload: InboxPayload,
    pub dedup_key: DedupKey,
    pub supersedes: Vec<DedupKey>,
    pub actor: Actor,
    pub causation_id: Option<EventId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written<T> {
    pub item: T,
    pub last_seq: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaiseOutcome {
    Created(Written<InboxItem>),
    Deduped(Written<InboxItem>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveTarget {
    Id(InboxId),
    Approval(ApprovalId),
    DedupKey(DedupKey),
    Delivery {
        id: DeliveryId,
        kinds: Option<Vec<InboxKind>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Answered {
        message_id: MessageId,
    },
    ApprovalSettled {
        approval_id: ApprovalId,
        status: ApprovalStatus,
    },
    DeliveryClosed,
    DeliveryAbandoned,
    Superseded {
        by: InboxId,
    },
    External {
        reason: String,
    },
    System {
        reason: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxQuery {
    pub project_id: Option<ProjectId>,
    pub delivery_id: Option<DeliveryId>,
    pub status: Option<InboxStatus>,
    pub kind: Option<InboxKind>,
    pub page: PageQuery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectOpenCount {
    pub project_id: Option<ProjectId>,
    pub open: u32,
    pub max_severity: Severity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxSummary {
    pub total_open: u32,
    pub by_project: Vec<ProjectOpenCount>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RaiseError {
    #[error("invalid inbox payload at {pointer}: {message}")]
    InvalidPayload { pointer: String, message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("invalid resolve request: {message}")]
    InvalidRequest { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InboxError {
    #[error("inbox item not found")]
    NotFound,
    #[error("invalid inbox request: {message}")]
    InvalidRequest { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChooseError {
    #[error(transparent)]
    Inbox(#[from] InboxError),
    #[error("inbox item is not open")]
    NotOpen,
    #[error("option is not known for this item")]
    OptionUnknown,
    #[error("approval_needed must use the approval route")]
    UseApprovalRoute,
    #[error("client profile cannot choose this option")]
    ProfileForbidden,
    #[error("invalid choose request: {message}")]
    InvalidRequest { message: String },
}

/// 收件箱端口。写方法 = 一个事务内“追加事件 + 更新投影”。
#[async_trait]
pub trait InboxPort: Send + Sync {
    async fn raise(
        &self,
        tx: Option<&mut dyn TxHandle>,
        req: RaiseRequest,
        ctx: &ActorCtx,
    ) -> Result<RaiseOutcome, RaiseError>;

    async fn resolve(
        &self,
        tx: Option<&mut dyn TxHandle>,
        target: ResolveTarget,
        how: Resolution,
        ctx: &ActorCtx,
    ) -> Result<Vec<Written<InboxItem>>, ResolveError>;

    async fn dismiss(
        &self,
        id: &InboxId,
        ctx: &ActorCtx,
        note: Option<Note>,
    ) -> Result<Written<InboxItem>, InboxError>;

    async fn choose(
        &self,
        id: &InboxId,
        option: &OptionId,
        note: Option<Note>,
        ctx: &ActorCtx,
    ) -> Result<Written<InboxItem>, ChooseError>;

    async fn get(&self, id: &InboxId) -> Result<Written<InboxItem>, InboxError>;

    async fn list(&self, q: InboxQuery) -> Result<Page<InboxItem>, InboxError>;

    async fn summary(&self) -> Result<InboxSummary, InboxError>;

    async fn register_push_token(
        &self,
        ctx: &ActorCtx,
        token: PushToken,
        env: ApnsEnvironment,
    ) -> Result<(), InboxError>;
}

/// `InboxPayload` 的 kind 必须与 `RaiseRequest.kind` 一致；这里是端口层的
/// 机械校验，不复制 data-model 的字段。
pub fn validate_raise_kind(payload: &InboxPayload, kind: InboxKind) -> bool {
    payload.kind() == kind
}
