//! 事件日志到引擎读模型的投影（engine.md §7.1 / §7.2）。
//!
//! 只重建 M1 已定义的部分：交付阶段、工作流 / 节点映射、尝试关联与上报状态。
//! 预算明细与 `open_inbox` 的阻塞判定尚未实现；不伪造规格里未定义的
//! `AttemptState` / `OpenInbox` / `RoundRecord`。

use std::collections::BTreeMap;

use gantry_core::entities::{
    role_profile, AttemptStatus, DeliveryOrigin, Exec, FailureClass, InboxStatus, IssueRef,
    NodeStatus, Role, Seq, Stage, StageStatus, Tier, Verdict, WorkflowStatus,
};
use gantry_core::events::{
    AttemptFinishedData, AttemptStartedData, DeliveryAbandonedData, DeliveryBudget,
    DeliveryCreatedData, DeliveryLevelChangedData, DeliveryPausedData, DeliveryResumedData,
    DeliveryStageChangedData, Event, EventType, InboxDismissedData, InboxRaisedData,
    InboxResolvedData, LevelChangeKind, NodeEventData, NodeRetryRequestedData, WorkflowCreatedData,
    WorkflowFinishedData, WorkflowRevisedData,
};
use gantry_core::ids::{AttemptId, DeliveryId, InboxId, NodeId, ProjectId, WorkflowId};
use gantry_core::level::Level;
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projection {
    pub delivery: Option<DeliveryProjection>,
    pub workflows: BTreeMap<WorkflowId, WorkflowProjection>,
    pub nodes: BTreeMap<NodeId, NodeProjection>,
    pub attempts: BTreeMap<AttemptId, AttemptProjection>,
    pub inbox: BTreeMap<InboxId, InboxStatus>,
    pub last_seq: Option<Seq>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryProjection {
    pub id: DeliveryId,
    pub project_id: ProjectId,
    pub title: String,
    pub level_cap: Level,
    pub level_effective: Level,
    pub origin: DeliveryOrigin,
    pub stage: Stage,
    pub stage_status: StageStatus,
    pub status_before_pause: Option<StageStatus>,
    pub budget: DeliveryBudget,
    pub issues: Vec<IssueRef>,
    pub config_rev: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowProjection {
    pub id: WorkflowId,
    pub stage: Option<Stage>,
    pub template: String,
    pub graph_rev: u32,
    pub status: WorkflowStatus,
    pub nodes: BTreeMap<NodeId, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeProjection {
    pub id: NodeId,
    pub workflow_id: WorkflowId,
    pub key: String,
    pub role: Role,
    pub tier: Tier,
    pub exec: Exec,
    pub required: bool,
    pub critical: bool,
    pub status: NodeStatus,
    pub verdict: Option<Verdict>,
    pub superseded_by: Option<NodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptProjection {
    pub id: AttemptId,
    pub node_id: NodeId,
    pub n: u32,
    pub status: Option<AttemptStatus>,
    pub failure_class: Option<FailureClass>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("event {ty} data does not match its contract: {source}")]
    Data {
        ty: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("event {ty} arrived before its parent state")]
    MissingParent { ty: &'static str },
}

fn decode<T: serde::de::DeserializeOwned>(
    event: &Event<Value>,
    ty: &'static str,
) -> Result<T, ProjectionError> {
    serde_json::from_value(event.data.clone())
        .map_err(|source| ProjectionError::Data { ty, source })
}

fn subject_id<T: std::str::FromStr>(
    event: &Event<Value>,
    ty: &'static str,
) -> Result<T, ProjectionError> {
    event
        .subject
        .as_str()
        .parse()
        .map_err(|_| ProjectionError::MissingParent { ty })
}

impl Projection {
    /// 应用一条事件；未知事件忽略，投影只读取已定义形状。
    pub fn apply(&mut self, event: &Event<Value>) -> Result<(), ProjectionError> {
        let ty = event.ty.as_str();
        match event.ty {
            EventType::DeliveryCreated => {
                let data: DeliveryCreatedData = decode(event, ty)?;
                self.delivery = Some(DeliveryProjection {
                    id: subject_id(event, ty)?,
                    project_id: event
                        .project_id
                        .ok_or(ProjectionError::MissingParent { ty })?,
                    title: data.title,
                    level_cap: data.level_cap,
                    level_effective: data.level_effective,
                    origin: data.origin,
                    stage: Stage::Spec,
                    stage_status: StageStatus::Running,
                    status_before_pause: None,
                    budget: data.budget,
                    issues: data.issues,
                    config_rev: data.config_rev.as_u64(),
                });
            }
            EventType::DeliveryStageChanged => {
                let data: DeliveryStageChangedData = decode(event, ty)?;
                let delivery = self
                    .delivery
                    .as_mut()
                    .ok_or(ProjectionError::MissingParent { ty })?;
                delivery.stage = data.to;
                delivery.stage_status = data.stage_status;
            }
            EventType::DeliveryLevelChanged => {
                let data: DeliveryLevelChangedData = decode(event, ty)?;
                let delivery = self
                    .delivery
                    .as_mut()
                    .ok_or(ProjectionError::MissingParent { ty })?;
                match data.kind {
                    LevelChangeKind::Cap => delivery.level_cap = data.to,
                    LevelChangeKind::Effective => delivery.level_effective = data.to,
                }
            }
            EventType::DeliveryPaused => {
                let data: DeliveryPausedData = decode(event, ty)?;
                let delivery = self
                    .delivery
                    .as_mut()
                    .ok_or(ProjectionError::MissingParent { ty })?;
                delivery.status_before_pause = Some(data.status_before);
                delivery.stage_status = StageStatus::Paused;
            }
            EventType::DeliveryResumed => {
                let data: DeliveryResumedData = decode(event, ty)?;
                let delivery = self
                    .delivery
                    .as_mut()
                    .ok_or(ProjectionError::MissingParent { ty })?;
                delivery.stage_status = data.status_after;
                delivery.status_before_pause = None;
            }
            EventType::DeliveryAbandoned => {
                let _: DeliveryAbandonedData = decode(event, ty)?;
                let delivery = self
                    .delivery
                    .as_mut()
                    .ok_or(ProjectionError::MissingParent { ty })?;
                delivery.stage = Stage::Abandoned;
                delivery.stage_status = StageStatus::Done;
            }
            EventType::WorkflowCreated => {
                let data: WorkflowCreatedData = decode(event, ty)?;
                let workflow_id = subject_id(event, ty)?;
                let mut nodes = BTreeMap::new();
                for descriptor in &data.nodes {
                    nodes.insert(descriptor.node_id, descriptor.key.clone());
                    let (tier, exec) = role_profile(&descriptor.role);
                    self.nodes.insert(
                        descriptor.node_id,
                        NodeProjection {
                            id: descriptor.node_id,
                            workflow_id,
                            key: descriptor.key.clone(),
                            role: descriptor.role.clone(),
                            tier,
                            exec,
                            required: descriptor.required,
                            critical: descriptor.critical,
                            status: NodeStatus::Pending,
                            verdict: None,
                            superseded_by: None,
                        },
                    );
                }
                self.workflows.insert(
                    workflow_id,
                    WorkflowProjection {
                        id: workflow_id,
                        stage: data.stage,
                        template: data.template,
                        graph_rev: data.graph_rev,
                        status: WorkflowStatus::Running,
                        nodes,
                    },
                );
            }
            EventType::WorkflowRevised => {
                let data: WorkflowRevisedData = decode(event, ty)?;
                let workflow_id = subject_id(event, ty)?;
                let workflow = self
                    .workflows
                    .get_mut(&workflow_id)
                    .ok_or(ProjectionError::MissingParent { ty })?;
                workflow.graph_rev = data.graph_rev;
                for descriptor in &data.added {
                    workflow
                        .nodes
                        .insert(descriptor.node_id, descriptor.key.clone());
                    let (tier, exec) = role_profile(&descriptor.role);
                    self.nodes.insert(
                        descriptor.node_id,
                        NodeProjection {
                            id: descriptor.node_id,
                            workflow_id,
                            key: descriptor.key.clone(),
                            role: descriptor.role.clone(),
                            tier,
                            exec,
                            required: descriptor.required,
                            critical: descriptor.critical,
                            status: NodeStatus::Pending,
                            verdict: None,
                            superseded_by: None,
                        },
                    );
                }
            }
            EventType::WorkflowFinished => {
                let data: WorkflowFinishedData = decode(event, ty)?;
                let workflow_id = subject_id(event, ty)?;
                let workflow = self
                    .workflows
                    .get_mut(&workflow_id)
                    .ok_or(ProjectionError::MissingParent { ty })?;
                workflow.status = data.status;
            }
            EventType::NodeQueued
            | EventType::NodeStarted
            | EventType::NodeFinished
            | EventType::NodeSuperseded => {
                let data: NodeEventData = decode(event, ty)?;
                let node = self
                    .nodes
                    .get_mut(&data.node_id)
                    .ok_or(ProjectionError::MissingParent { ty })?;
                node.status = data.status;
                node.verdict = data.verdict;
                node.superseded_by = data.superseded_by;
            }
            EventType::NodeRetryRequested => {
                let data: NodeRetryRequestedData = decode(event, ty)?;
                let node = self
                    .nodes
                    .get_mut(&data.node_id)
                    .ok_or(ProjectionError::MissingParent { ty })?;
                node.status = NodeStatus::Queued;
            }
            EventType::AttemptStarted => {
                let data: AttemptStartedData = decode(event, ty)?;
                self.attempts.insert(
                    data.attempt_id,
                    AttemptProjection {
                        id: data.attempt_id,
                        node_id: data.node_id,
                        n: data.n,
                        status: None,
                        failure_class: None,
                    },
                );
            }
            EventType::AttemptFinished => {
                let data: AttemptFinishedData = decode(event, ty)?;
                let attempt_id = subject_id(event, ty)?;
                let attempt = self
                    .attempts
                    .get_mut(&attempt_id)
                    .ok_or(ProjectionError::MissingParent { ty })?;
                attempt.status = Some(data.status);
                attempt.failure_class = data.failure_class;
            }
            EventType::InboxRaised => {
                let data: InboxRaisedData = decode(event, ty)?;
                self.inbox.insert(data.item.id, data.item.status);
            }
            EventType::InboxResolved => {
                let data: InboxResolvedData = decode(event, ty)?;
                self.inbox.insert(data.inbox_id, InboxStatus::Resolved);
            }
            EventType::InboxDismissed => {
                let data: InboxDismissedData = decode(event, ty)?;
                self.inbox.insert(data.inbox_id, InboxStatus::Dismissed);
            }
            _ => {}
        }
        self.last_seq = Some(event.seq);
        Ok(())
    }
}
