//! 交付单元投影的过滤与键集分页。

use std::collections::BTreeMap;

use gantry_core::entities::{Stage, StageStatus};
use gantry_core::ids::{DeliveryId, ProjectId};

use crate::projection::DeliveryRow;
use crate::{order_tag, Order, Page, PageCursor, Paged, StoreError};

/// `GET /v1/deliveries` 的过滤条件（api.md §4.4）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveryQuery {
    pub project_id: Option<ProjectId>,
    /// 任一阶段命中即保留；空 = 全部。
    pub stages: Vec<Stage>,
    pub stage_status: Option<StageStatus>,
    /// `true` 时排除终态（`closed` / `abandoned`）。
    pub active: bool,
}

pub(crate) fn list(
    rows: &BTreeMap<DeliveryId, DeliveryRow>,
    q: &DeliveryQuery,
    p: &Page,
) -> Result<Paged<DeliveryRow>, StoreError> {
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
                (Some("deliveries"), Some(order), Some(key)) if order == order_tag(p.order) => {
                    Some(
                        key.parse::<DeliveryId>()
                            .map_err(|_| StoreError::InvalidCursor)?,
                    )
                }
                _ => return Err(StoreError::InvalidCursor),
            }
        }
    };
    let keep = |row: &DeliveryRow| {
        q.project_id.is_none_or(|id| row.project_id == id)
            && (q.stages.is_empty() || q.stages.contains(&row.stage))
            && q.stage_status
                .is_none_or(|status| row.stage_status == status)
            && (!q.active || !row.stage.is_terminal())
    };
    let limit = p.limit as usize;
    let mut items: Vec<DeliveryRow> = match p.order {
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
            .map(|row| PageCursor::new(format!("deliveries:{}:{}", order_tag(p.order), row.id)))
    } else {
        None
    };
    Ok(Paged { items, next_cursor })
}
