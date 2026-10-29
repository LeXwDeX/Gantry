//! 项目事件投影与键集分页。

use std::collections::BTreeMap;

use gantry_core::entities::Project;
use gantry_core::events::ProjectEventData;
use gantry_core::ids::ProjectId;

use crate::{order_tag, Order, Page, PageCursor, Paged, StoreError};

pub(crate) fn from_event(data: ProjectEventData) -> Project {
    Project {
        id: data.id,
        repo: data.repo,
        installation_id: data.installation_id,
        bot_user_id: data.bot_user_id,
        default_branch: data.default_branch,
        allowed_merge_methods: data.allowed_merge_methods,
        merge_queue_enabled: data.merge_queue_enabled,
        created_at: data.created_at,
        updated_at: data.updated_at,
    }
}

pub(crate) fn list(
    rows: &BTreeMap<ProjectId, Project>,
    p: &Page,
) -> Result<Paged<Project>, StoreError> {
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
                (Some("projects"), Some(order), Some(key)) if order == order_tag(p.order) => Some(
                    key.parse::<ProjectId>()
                        .map_err(|_| StoreError::InvalidCursor)?,
                ),
                _ => return Err(StoreError::InvalidCursor),
            }
        }
    };
    let limit = p.limit as usize;
    let mut items: Vec<Project> = match p.order {
        Order::Asc => rows
            .values()
            .filter(|row| after.is_none_or(|id| row.id > id))
            .take(limit + 1)
            .cloned()
            .collect(),
        Order::Desc => rows
            .values()
            .rev()
            .filter(|row| after.is_none_or(|id| row.id < id))
            .take(limit + 1)
            .cloned()
            .collect(),
    };
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items
            .last()
            .map(|row| PageCursor::new(format!("projects:{}:{}", order_tag(p.order), row.id)))
    } else {
        None
    };
    Ok(Paged { items, next_cursor })
}
