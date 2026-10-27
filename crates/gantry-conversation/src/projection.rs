//! 对话与消息的读投影（§7.8）：本模块没有自己的表，全部由事件日志折叠。
//!
//! `store` 的模块投影器从 M1 后续才开放（store.md §7.10），这里按需扫描日志；
//! 结果只依赖 `seq ≤ upto` 的事件，所以与同一读点的 `seq` 一致（§7.11）。

use std::collections::{BTreeMap, BTreeSet};

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Conversation, Message, Seq};
use gantry_core::events::{
    ConversationEventData, DeliveryCreatedData, Event, EventType, ProjectEventData, Subject,
};
use gantry_core::ids::{ConversationId, DeliveryId, MessageId, ProjectId};
use gantry_store::{EventFilter, Store, StoreError, TypePattern};
use serde_json::Value;

const SCAN_LIMIT: u32 = 200;

/// 扫描某项目（或全局）指定类型的事件，直到 `upto`（含）。
pub(crate) async fn scan(
    store: &Store,
    project: Option<&ProjectId>,
    types: &[EventType],
    upto: Seq,
) -> Result<Vec<Event<Value>>, StoreError> {
    let filter = EventFilter {
        project_ids: project.map(|project| BTreeSet::from([*project])),
        include: types.iter().copied().map(TypePattern::Exact).collect(),
        exclude: Vec::new(),
    };
    let mut after = Seq::ZERO;
    let mut out = Vec::new();
    loop {
        let page = store.events().scan(after, &filter, SCAN_LIMIT).await?;
        let done = page.scanned_to >= upto || page.scanned_to >= page.head;
        out.extend(page.events.into_iter().filter(|event| event.seq <= upto));
        if done || page.scanned_to == after {
            break;
        }
        after = page.scanned_to;
    }
    Ok(out)
}

/// 项目绑定的仓库（`project.added` / `project.updated` 的最后一条）。
pub(crate) async fn project_repo(
    store: &Store,
    project: &ProjectId,
    upto: Seq,
) -> Result<Option<RepoSlug>, StoreError> {
    let events = scan(
        store,
        Some(project),
        &[EventType::ProjectAdded, EventType::ProjectUpdated],
        upto,
    )
    .await?;
    Ok(events
        .into_iter()
        .rev()
        .filter_map(|event| serde_json::from_value::<ProjectEventData>(event.data).ok())
        .find(|data| data.id == *project)
        .map(|data| data.repo))
}

pub(crate) async fn conversation_of_project(
    store: &Store,
    project: &ProjectId,
    upto: Seq,
) -> Result<Option<Conversation>, StoreError> {
    let events = scan(
        store,
        Some(project),
        &[EventType::ConversationCreated],
        upto,
    )
    .await?;
    Ok(events
        .into_iter()
        .filter_map(|event| serde_json::from_value::<ConversationEventData>(event.data).ok())
        .find(|data| data.project_id == *project)
        .map(conversation))
}

/// 对话本身与按 `seq` 升序的全部消息（`delivery_id` 已回填）。
pub(crate) struct ConversationLog {
    pub conversation: Conversation,
    pub messages: Vec<(Seq, Message)>,
}

pub(crate) async fn conversation_log(
    store: &Store,
    id: &ConversationId,
    upto: Seq,
) -> Result<Option<ConversationLog>, StoreError> {
    let subject = Subject::parse(&id.to_string())
        .map_err(|error| StoreError::PreconditionFailed(error.to_string()))?;
    let mut conversation_row = None;
    let mut messages = Vec::new();
    for event in store.events().by_subject(&subject).await? {
        if event.seq > upto {
            continue;
        }
        match event.ty {
            EventType::ConversationCreated => {
                conversation_row = serde_json::from_value::<ConversationEventData>(event.data)
                    .ok()
                    .map(conversation);
            }
            EventType::MessagePosted => {
                if let Ok(message) = serde_json::from_value::<Message>(event.data) {
                    messages.push((event.seq, message));
                }
            }
            _ => {}
        }
    }
    let Some(conversation) = conversation_row else {
        return Ok(None);
    };
    // OQ-8：用户消息的 delivery_id 由 delivery.created.source_message_id 回填。
    let created = delivery_sources(store, &conversation.project_id, upto).await?;
    for (_, message) in &mut messages {
        if message.delivery_id.is_none() {
            message.delivery_id = created.get(&message.id).copied();
        }
    }
    Ok(Some(ConversationLog {
        conversation,
        messages,
    }))
}

/// 项目内 `source_message_id → delivery_id`。
pub(crate) async fn delivery_sources(
    store: &Store,
    project: &ProjectId,
    upto: Seq,
) -> Result<BTreeMap<MessageId, DeliveryId>, StoreError> {
    let mut out = BTreeMap::new();
    for event in deliveries_created(store, project, upto).await? {
        if let Some(source) = event.1.source_message_id {
            out.insert(source, event.0);
        }
    }
    Ok(out)
}

pub(crate) async fn deliveries_created(
    store: &Store,
    project: &ProjectId,
    upto: Seq,
) -> Result<Vec<(DeliveryId, DeliveryCreatedData)>, StoreError> {
    let events = scan(store, Some(project), &[EventType::DeliveryCreated], upto).await?;
    Ok(events
        .into_iter()
        .filter_map(|event| {
            let id = event.subject.as_str().parse::<DeliveryId>().ok()?;
            let data = serde_json::from_value::<DeliveryCreatedData>(event.data).ok()?;
            Some((id, data))
        })
        .collect())
}

fn conversation(data: ConversationEventData) -> Conversation {
    Conversation {
        id: data.id,
        project_id: data.project_id,
        created_at: data.created_at,
        updated_at: data.updated_at,
    }
}
