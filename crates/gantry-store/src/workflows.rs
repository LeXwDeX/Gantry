//! workflow / node / attempt 投影（#17–#20）。

use std::collections::BTreeMap;

use gantry_core::entities::{
    role_profile, AttemptStatus, Exec, FailureClass, FailureDetail, NodeStatus, Role, Tier,
    TokenUsage, Verdict, WorkflowStatus,
};
use gantry_core::events::{
    decode_event_data, AttemptFinishedData, AttemptStartedData, Event, NodeDescriptor,
    NodeEventData, NodeRetryRequestedData, WorkflowCreatedData, WorkflowFinishedData,
    WorkflowRevisedData,
};
use gantry_core::ids::{AttemptId, ConversationId, DeliveryId, NodeId, Wfl, WorkflowId};
use gantry_core::time::Timestamp;
use serde_json::Value;

use crate::{order_tag, subject_id, Order, Page, PageCursor, Paged, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowRow {
    pub id: WorkflowId,
    pub delivery_id: Option<DeliveryId>,
    pub conversation_id: Option<ConversationId>,
    pub stage: Option<gantry_core::entities::Stage>,
    pub graph_rev: u32,
    pub status: WorkflowStatus,
    pub template: String,
    pub nodes: Vec<NodeRow>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    pub id: NodeId,
    pub workflow_id: WorkflowId,
    pub key: String,
    pub role: Role,
    pub tier: Tier,
    pub exec: Exec,
    pub depends_on: Vec<NodeId>,
    pub required: bool,
    pub critical: bool,
    pub status: NodeStatus,
    pub verdict: Option<Verdict>,
    pub superseded_by: Option<NodeId>,
    pub attempts: Vec<AttemptRow>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptRow {
    pub id: AttemptId,
    pub node_id: NodeId,
    pub n: u32,
    pub sandbox_id: Option<gantry_core::ids::SandboxId>,
    pub status: AttemptStatus,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub result_ref: Option<gantry_core::canon::Sha256Digest>,
    pub failure_class: Option<FailureClass>,
    pub failure_detail: Option<FailureDetail>,
    pub usage: TokenUsage,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl WorkflowRow {
    fn node_mut(&mut self, id: &NodeId) -> Option<&mut NodeRow> {
        self.nodes.iter_mut().find(|node| node.id == *id)
    }

    fn add_nodes(&mut self, added: &[NodeDescriptor], ts: Timestamp) {
        let mut by_key: BTreeMap<String, NodeId> = self
            .nodes
            .iter()
            .map(|node| (node.key.clone(), node.id))
            .collect();
        for node in added {
            by_key.insert(node.key.clone(), node.node_id);
        }
        for descriptor in added {
            let (tier, exec) = role_profile(&descriptor.role);
            let depends_on = descriptor
                .depends_on
                .iter()
                .filter_map(|key| by_key.get(key).copied())
                .collect();
            self.nodes.push(NodeRow {
                id: descriptor.node_id,
                workflow_id: self.id,
                key: descriptor.key.clone(),
                role: descriptor.role.clone(),
                tier,
                exec,
                depends_on,
                required: descriptor.required,
                critical: descriptor.critical,
                status: NodeStatus::Pending,
                verdict: None,
                superseded_by: None,
                attempts: Vec::new(),
                created_at: ts,
                updated_at: ts,
            });
        }
    }
}

fn decode<T: gantry_core::events::EventData>(event: &Event<Value>) -> Result<T, StoreError> {
    decode_event_data(event).map_err(StoreError::InvalidEvent)
}

/// 折叠一条图事件；不相关的类型返回 `false`。
pub(crate) fn project(
    rows: &mut BTreeMap<WorkflowId, WorkflowRow>,
    event: &Event<Value>,
) -> Result<bool, StoreError> {
    match event.ty {
        gantry_core::events::EventType::WorkflowCreated => {
            let data: WorkflowCreatedData = decode(event)?;
            let id = subject_id::<Wfl>(&event.subject)?;
            let mut row = WorkflowRow {
                id,
                delivery_id: data.delivery_id,
                conversation_id: data.conversation_id,
                stage: data.stage,
                graph_rev: data.graph_rev,
                status: WorkflowStatus::Running,
                template: data.template,
                nodes: Vec::new(),
                created_at: event.ts,
                updated_at: event.ts,
            };
            row.add_nodes(&data.nodes, event.ts);
            rows.insert(id, row);
            Ok(true)
        }
        gantry_core::events::EventType::WorkflowRevised => {
            let data: WorkflowRevisedData = decode(event)?;
            let id = subject_id::<Wfl>(&event.subject)?;
            let Some(row) = rows.get_mut(&id) else {
                return Ok(true);
            };
            row.graph_rev = data.graph_rev;
            row.updated_at = event.ts;
            for node in &mut row.nodes {
                if data.superseded.contains(&node.id) {
                    node.status = NodeStatus::Superseded;
                    node.updated_at = event.ts;
                }
            }
            let added = data.added;
            row.add_nodes(&added, event.ts);
            Ok(true)
        }
        gantry_core::events::EventType::WorkflowFinished => {
            let data: WorkflowFinishedData = decode(event)?;
            let id = subject_id::<Wfl>(&event.subject)?;
            if let Some(row) = rows.get_mut(&id) {
                row.status = data.status;
                row.updated_at = event.ts;
            }
            Ok(true)
        }
        gantry_core::events::EventType::NodeQueued
        | gantry_core::events::EventType::NodeStarted
        | gantry_core::events::EventType::NodeFinished
        | gantry_core::events::EventType::NodeSuperseded => {
            let data: NodeEventData = decode(event)?;
            for row in rows.values_mut() {
                if let Some(node) = row.node_mut(&data.node_id) {
                    node.status = data.status;
                    node.verdict = data.verdict;
                    node.superseded_by = data.superseded_by;
                    node.updated_at = event.ts;
                    row.updated_at = event.ts;
                    return Ok(true);
                }
            }
            Ok(true)
        }
        gantry_core::events::EventType::NodeRetryRequested => {
            let data: NodeRetryRequestedData = decode(event)?;
            for row in rows.values_mut() {
                if let Some(node) = row.node_mut(&data.node_id) {
                    node.status = NodeStatus::Queued;
                    node.updated_at = event.ts;
                    row.updated_at = event.ts;
                    return Ok(true);
                }
            }
            Ok(true)
        }
        gantry_core::events::EventType::AttemptStarted => {
            let data: AttemptStartedData = decode(event)?;
            for row in rows.values_mut() {
                if let Some(node) = row.node_mut(&data.node_id) {
                    node.attempts.push(AttemptRow {
                        id: data.attempt_id,
                        node_id: data.node_id,
                        n: data.n,
                        sandbox_id: None,
                        status: AttemptStatus::Running,
                        started_at: Some(event.ts),
                        ended_at: None,
                        result_ref: None,
                        failure_class: None,
                        failure_detail: None,
                        usage: TokenUsage::default(),
                        created_at: event.ts,
                        updated_at: event.ts,
                    });
                    node.updated_at = event.ts;
                    row.updated_at = event.ts;
                    return Ok(true);
                }
            }
            Ok(true)
        }
        gantry_core::events::EventType::AttemptFinished => {
            let data: AttemptFinishedData = decode(event)?;
            let attempt_id: AttemptId = subject_id::<gantry_core::ids::Att>(&event.subject)?;
            for row in rows.values_mut() {
                for node in &mut row.nodes {
                    if let Some(attempt) = node.attempts.iter_mut().find(|a| a.id == attempt_id) {
                        attempt.status = data.status;
                        attempt.ended_at = Some(event.ts);
                        attempt.result_ref = data.result_ref;
                        attempt.failure_class = data.failure_class;
                        attempt.failure_detail = attempt_detail(&data)?;
                        attempt.usage = data.usage.unwrap_or_default();
                        attempt.updated_at = event.ts;
                        node.updated_at = event.ts;
                        row.updated_at = event.ts;
                        return Ok(true);
                    }
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `GET /v1/deliveries/{id}/workflows`：按交付单元与阶段过滤，键集分页。
pub(crate) fn list(
    rows: &BTreeMap<WorkflowId, WorkflowRow>,
    delivery: &DeliveryId,
    stage: Option<gantry_core::entities::Stage>,
    p: &Page,
) -> Result<Paged<WorkflowRow>, StoreError> {
    if !(1..=200).contains(&p.limit) {
        return Err(StoreError::PreconditionFailed(
            "page limit must be 1..=200".to_owned(),
        ));
    }
    let after = match &p.cursor {
        None => None,
        Some(cursor) => {
            let mut parts = cursor.as_str().splitn(3, ':');
            match (parts.next(), parts.next(), parts.next()) {
                (Some("workflows"), Some(order), Some(key)) if order == order_tag(p.order) => Some(
                    key.parse::<WorkflowId>()
                        .map_err(|_| StoreError::InvalidCursor)?,
                ),
                _ => return Err(StoreError::InvalidCursor),
            }
        }
    };
    let keep = |row: &WorkflowRow| {
        row.delivery_id.as_ref() == Some(delivery)
            && stage.is_none_or(|stage| row.stage == Some(stage))
    };
    let limit = p.limit as usize;
    let mut items: Vec<WorkflowRow> = match p.order {
        Order::Asc => rows
            .values()
            .filter(|row| keep(row))
            .filter(|row| after.is_none_or(|id| row.id > id))
            .take(limit + 1)
            .cloned()
            .collect(),
        Order::Desc => rows
            .values()
            .rev()
            .filter(|row| keep(row))
            .filter(|row| after.is_none_or(|id| row.id < id))
            .take(limit + 1)
            .cloned()
            .collect(),
    };
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items
            .last()
            .map(|row| PageCursor::new(format!("workflows:{}:{}", order_tag(p.order), row.id)))
    } else {
        None
    };
    Ok(Paged { items, next_cursor })
}

/// `GET /v1/nodes/{id}/attempts`：默认按 `n` 升序。
pub(crate) fn attempts(
    rows: &BTreeMap<WorkflowId, WorkflowRow>,
    node: &NodeId,
    p: &Page,
) -> Result<Paged<AttemptRow>, StoreError> {
    if !(1..=200).contains(&p.limit) {
        return Err(StoreError::PreconditionFailed(
            "page limit must be 1..=200".to_owned(),
        ));
    }
    let attempts = rows
        .values()
        .flat_map(|row| row.nodes.iter())
        .find(|row| row.id == *node)
        .map(|row| row.attempts.as_slice())
        .unwrap_or_default();
    let after = match &p.cursor {
        None => None,
        Some(cursor) => {
            let mut parts = cursor.as_str().splitn(3, ':');
            match (parts.next(), parts.next(), parts.next()) {
                (Some("attempts"), Some(order), Some(key)) if order == order_tag(p.order) => {
                    Some(key.parse::<u32>().map_err(|_| StoreError::InvalidCursor)?)
                }
                _ => return Err(StoreError::InvalidCursor),
            }
        }
    };
    let limit = p.limit as usize;
    let mut items: Vec<AttemptRow> = match p.order {
        Order::Asc => attempts
            .iter()
            .filter(|row| after.is_none_or(|n| row.n > n))
            .take(limit + 1)
            .cloned()
            .collect(),
        Order::Desc => attempts
            .iter()
            .rev()
            .filter(|row| after.is_none_or(|n| row.n < n))
            .take(limit + 1)
            .cloned()
            .collect(),
    };
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items
            .last()
            .map(|row| PageCursor::new(format!("attempts:{}:{}", order_tag(p.order), row.n)))
    } else {
        None
    };
    Ok(Paged { items, next_cursor })
}

fn attempt_detail(data: &AttemptFinishedData) -> Result<Option<FailureDetail>, StoreError> {
    data.failure_detail
        .as_deref()
        .map(FailureDetail::parse)
        .transpose()
        .map_err(|_| StoreError::PreconditionFailed("invalid failure_detail".to_owned()))
}
