//! 对话与消息的读投影（§7.8）：本模块没有自己的表，全部由事件日志折叠。
//!
//! `store` 的模块投影器从 M1 后续才开放（store.md §7.10），这里按需扫描日志；
//! 消息读取使用同一内存快照；其他处理路径按 `seq ≤ upto` 折叠（§7.11）。

use std::collections::{BTreeMap, BTreeSet};

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Conversation, Message, Seq};
use gantry_core::events::{
    ConversationEventData, DeliveryCreatedData, Event, EventType, ProjectEventData, Subject,
};
use gantry_core::ids::{ConversationId, DeliveryId, ProjectId};
use gantry_store::{EventFilter, ReadSnapshot, Store, StoreError, TypePattern};
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
    for event in events.into_iter().rev() {
        let data: ProjectEventData = serde_json::from_value(event.data).map_err(|error| {
            StoreError::Corrupt(format!(
                "seq {} project envelope: {error}",
                event.seq.as_u64()
            ))
        })?;
        if data.id == *project {
            return Ok(Some(data.repo));
        }
    }
    Ok(None)
}

pub(crate) fn conversation_of_project(
    snapshot: &ReadSnapshot,
    project: &ProjectId,
    upto: Seq,
) -> Result<Option<Conversation>, StoreError> {
    let filter = EventFilter {
        project_ids: Some(BTreeSet::from([*project])),
        include: vec![TypePattern::Exact(EventType::ConversationCreated)],
        exclude: Vec::new(),
    };
    let mut after = Seq::ZERO;
    loop {
        let page = snapshot.scan(after, &filter, SCAN_LIMIT)?;
        for event in page.events.into_iter().filter(|event| event.seq <= upto) {
            let data: ConversationEventData =
                serde_json::from_value(event.data).map_err(|error| {
                    StoreError::Corrupt(format!(
                        "seq {} conversation.created: {error}",
                        event.seq.as_u64()
                    ))
                })?;
            if data.project_id == *project {
                return Ok(Some(conversation(data)));
            }
        }
        if page.scanned_to >= upto || page.scanned_to >= page.head || page.scanned_to == after {
            break;
        }
        after = page.scanned_to;
    }
    Ok(None)
}

/// 对话本身与按 `seq` 升序的全部消息（`delivery_id` 已回填）。
pub(crate) struct ConversationLog {
    pub conversation: Conversation,
    pub messages: Vec<(Seq, Message)>,
}

pub(crate) fn conversation_log(
    snapshot: &ReadSnapshot,
    id: &ConversationId,
    upto: Seq,
) -> Result<Option<ConversationLog>, StoreError> {
    let subject = Subject::parse(&id.to_string())
        .map_err(|error| StoreError::PreconditionFailed(error.to_string()))?;
    let mut conversation_row = None;
    let mut messages = Vec::new();
    for event in snapshot.by_subject(&subject) {
        if event.seq > upto {
            continue;
        }
        match event.ty {
            EventType::ConversationCreated => {
                let data: ConversationEventData =
                    serde_json::from_value(event.data).map_err(|error| {
                        StoreError::Corrupt(format!(
                            "seq {} conversation.created: {error}",
                            event.seq.as_u64()
                        ))
                    })?;
                conversation_row = Some(conversation(data));
            }
            EventType::MessagePosted => {
                let message: Message = serde_json::from_value(event.data).map_err(|error| {
                    StoreError::Corrupt(format!(
                        "seq {} message.posted: {error}",
                        event.seq.as_u64()
                    ))
                })?;
                messages.push((event.seq, message));
            }
            _ => {}
        }
    }
    let Some(conversation) = conversation_row else {
        return Ok(None);
    };
    // OQ-8：用户消息的 delivery_id 由 delivery.created.source_message_id 回填。
    let filter = EventFilter {
        project_ids: Some(BTreeSet::from([conversation.project_id])),
        include: vec![TypePattern::Exact(EventType::DeliveryCreated)],
        exclude: Vec::new(),
    };
    let mut created = BTreeMap::new();
    let mut after = Seq::ZERO;
    loop {
        let page = snapshot.scan(after, &filter, SCAN_LIMIT)?;
        for event in page.events.into_iter().filter(|event| event.seq <= upto) {
            let id = event
                .subject
                .as_str()
                .parse::<DeliveryId>()
                .map_err(|error| {
                    StoreError::Corrupt(format!(
                        "seq {} delivery.created subject: {error}",
                        event.seq.as_u64()
                    ))
                })?;
            let data: DeliveryCreatedData =
                serde_json::from_value(event.data).map_err(|error| {
                    StoreError::Corrupt(format!(
                        "seq {} delivery.created: {error}",
                        event.seq.as_u64()
                    ))
                })?;
            if let Some(source) = data.source_message_id {
                created.insert(source, id);
            }
        }
        if page.scanned_to >= upto || page.scanned_to >= page.head || page.scanned_to == after {
            break;
        }
        after = page.scanned_to;
    }
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

pub(crate) async fn deliveries_created(
    store: &Store,
    project: &ProjectId,
    upto: Seq,
) -> Result<Vec<(DeliveryId, DeliveryCreatedData)>, StoreError> {
    let events = scan(store, Some(project), &[EventType::DeliveryCreated], upto).await?;
    let mut out = Vec::new();
    for event in events {
        let id = event
            .subject
            .as_str()
            .parse::<DeliveryId>()
            .map_err(|error| {
                StoreError::Corrupt(format!(
                    "seq {} delivery.created subject: {error}",
                    event.seq.as_u64()
                ))
            })?;
        let data: DeliveryCreatedData = serde_json::from_value(event.data).map_err(|error| {
            StoreError::Corrupt(format!(
                "seq {} delivery.created: {error}",
                event.seq.as_u64()
            ))
        })?;
        out.push((id, data));
    }
    Ok(out)
}

fn conversation(data: ConversationEventData) -> Conversation {
    Conversation {
        id: data.id,
        project_id: data.project_id,
        created_at: data.created_at,
        updated_at: data.updated_at,
    }
}
