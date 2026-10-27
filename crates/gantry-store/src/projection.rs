//! 交付单元行的折叠规则：`delivery.*` 事件只在这里折叠（M1 收尾评审 A1）。
//!
//! store 的追加前置校验与引擎的 `DeliveryState` 都调用 [`DeliveryRow::created`] /
//! [`DeliveryRow::apply`]，不再各写一份派生规则（#48 即两份规则漂移所致）。
//! 函数是纯的：不读时钟、不做 IO，同一事件序列在任何调用方得到同一行。

use gantry_core::entities::{DeliveryOrigin, IssueRef, Seq, Stage, StageStatus};
use gantry_core::events::{
    DeliveryAbandonedData, DeliveryBudget, DeliveryCreatedData, DeliveryLevelChangedData,
    DeliveryPausedData, DeliveryResumedData, DeliveryStageChangedData, Event, EventType,
    LevelChangeKind, SchemaError,
};
use gantry_core::ids::{DeliveryId, ProjectId};
use gantry_core::level::Level;
use gantry_core::machines::delivery::AbandonCause;
use gantry_core::time::Timestamp;
use serde_json::Value;

/// 最近一次进入当前阶段的位置（engine.md §7.14.4 `stage_entered`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageEntered {
    pub seq: Seq,
    pub ts: Timestamp,
    pub reason: Option<String>,
}

/// 交付单元的投影行：只含 `delivery.*` 事件能确定的字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryRow {
    pub id: DeliveryId,
    pub project_id: ProjectId,
    pub title: String,
    pub origin: DeliveryOrigin,
    pub level_cap: Level,
    pub level_effective: Level,
    pub stage: Stage,
    pub stage_status: StageStatus,
    /// 暂停前的值，resume 时恢复（P-56）。
    pub status_before_pause: Option<StageStatus>,
    pub issues: Vec<IssueRef>,
    /// 创建时固化（P-57）。
    pub config_rev: Seq,
    pub budget: DeliveryBudget,
    pub stage_entered: StageEntered,
    pub abandon_cause: Option<AbandonCause>,
}

#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error("event data does not match its contract: {0}")]
    Data(SchemaError),
    #[error("{0}")]
    Precondition(String),
}

impl DeliveryRow {
    /// 由 `delivery.created` 建行；交付单元从 `spec` 开始（data-model §4.1）。
    pub fn created(event: &Event<Value>) -> Result<Self, FoldError> {
        if event.ty != EventType::DeliveryCreated {
            return Err(FoldError::Precondition(format!(
                "{} cannot create a delivery row",
                event.ty.as_str()
            )));
        }
        let data: DeliveryCreatedData = decode(event)?;
        Ok(Self {
            id: delivery_id(event)?,
            project_id: event.project_id.ok_or_else(|| {
                FoldError::Data(SchemaError::consistency(
                    "/project_id",
                    "delivery.created requires project_id",
                ))
            })?,
            title: data.title,
            origin: data.origin,
            level_cap: data.level_cap,
            level_effective: data.level_effective,
            stage: Stage::Spec,
            stage_status: StageStatus::Running,
            status_before_pause: None,
            issues: data.issues,
            config_rev: data.config_rev,
            budget: data.budget,
            stage_entered: StageEntered {
                seq: event.seq,
                ts: event.ts,
                reason: None,
            },
            abandon_cause: None,
        })
    }

    /// 是否由本函数折叠：`delivery.created` 之外的 `delivery.*`。
    pub fn folds(ty: EventType) -> bool {
        matches!(
            ty,
            EventType::DeliveryStageChanged
                | EventType::DeliveryLevelChanged
                | EventType::DeliveryPaused
                | EventType::DeliveryResumed
                | EventType::DeliveryAbandoned
        )
    }

    /// 折叠一条本交付单元的 `delivery.*` 事件；其他类型原样返回 `Ok`。
    /// 前置条件不满足（与当前行不符）时返回错误，行保持不变。
    pub fn apply(&mut self, event: &Event<Value>) -> Result<(), FoldError> {
        if !Self::folds(event.ty) {
            return Ok(());
        }
        let id = delivery_id(event)?;
        if id != self.id {
            return Err(FoldError::Precondition(format!(
                "event for {id} applied to delivery {}",
                self.id
            )));
        }
        match event.ty {
            EventType::DeliveryStageChanged => {
                let data: DeliveryStageChangedData = decode(event)?;
                if self.stage != data.from {
                    return Err(FoldError::Precondition(format!(
                        "delivery {id} stage is {}, event says from {}",
                        self.stage, data.from
                    )));
                }
                self.stage = data.to;
                self.stage_status = data.stage_status;
                self.stage_entered = StageEntered {
                    seq: event.seq,
                    ts: event.ts,
                    reason: data.reason,
                };
            }
            EventType::DeliveryLevelChanged => {
                let data: DeliveryLevelChangedData = decode(event)?;
                let current = match data.kind {
                    LevelChangeKind::Cap => self.level_cap,
                    LevelChangeKind::Effective => self.level_effective,
                };
                if current != data.from {
                    return Err(FoldError::Precondition(format!(
                        "delivery {id} level does not match event.from"
                    )));
                }
                match data.kind {
                    // 压低上限时 effective 派生为 min(原值, 新上限)，不另写事件（engine.md §11，#48）。
                    LevelChangeKind::Cap => {
                        self.level_cap = data.to;
                        self.level_effective = self.level_effective.min(data.to);
                    }
                    LevelChangeKind::Effective => self.level_effective = data.to,
                }
            }
            EventType::DeliveryPaused => {
                let data: DeliveryPausedData = decode(event)?;
                if self.stage_status != data.status_before {
                    return Err(FoldError::Precondition(
                        "delivery stage_status does not match event".to_owned(),
                    ));
                }
                self.status_before_pause = Some(data.status_before);
                self.stage_status = StageStatus::Paused;
            }
            EventType::DeliveryResumed => {
                let data: DeliveryResumedData = decode(event)?;
                self.stage_status = data.status_after;
                self.status_before_pause = None;
            }
            EventType::DeliveryAbandoned => {
                let data: DeliveryAbandonedData = decode(event)?;
                self.stage = Stage::Abandoned;
                self.stage_status = StageStatus::Done;
                self.abandon_cause = Some(abandon_cause(data.reason.as_deref()));
                self.stage_entered = StageEntered {
                    seq: event.seq,
                    ts: event.ts,
                    reason: data.reason,
                };
            }
            _ => {}
        }
        Ok(())
    }
}

/// `delivery.abandoned.reason` → 放弃原因（engine.md §7.14.4）。
pub fn abandon_cause(reason: Option<&str>) -> AbandonCause {
    match reason {
        Some("pr_closed") => AbandonCause::PrClosedUnmerged,
        Some("issue_not_planned") => AbandonCause::IssueNotPlanned,
        _ => AbandonCause::UserAbandoned,
    }
}

fn decode<T: gantry_core::events::EventData>(event: &Event<Value>) -> Result<T, FoldError> {
    gantry_core::events::decode_event_data(event).map_err(FoldError::Data)
}

fn delivery_id(event: &Event<Value>) -> Result<DeliveryId, FoldError> {
    let text = event.subject.as_str();
    if !text.starts_with("dlv_") {
        return Err(FoldError::Precondition(format!(
            "subject {text} is not a dlv id"
        )));
    }
    text.parse()
        .map_err(|_| FoldError::Precondition(format!("invalid subject {text}")))
}
