//! 读类路由：消息 #11、收件箱 #21 #22 #46。
//!
//! 收件箱使用读快照的 `seq`；消息使用下游返回的读取水位（api.md §3.6）。
//! 分页游标对客户端不透明：本模块把下游游标包成 `base64url`，解析失败、跨资源、
//! 方向不符一律 `400 invalid_cursor`（本文 §7.7）。
//!
//! 项目读路由（#3、#4、#10）使用 store 的 projects 投影（B2a–B2c）；交付单元、
//! 工作流、节点尝试（#5、#6、#17–#20）仍待对应的投影读入口。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::entities::{InboxKind, InboxStatus, OutboxStatus, Stage, StageStatus};
use gantry_core::ids::{ConversationId, DeliveryId, InboxId, ProjectId};
use gantry_core::ports::context::{PageOrder, PageQuery, StoreError as PortStoreError};
use gantry_core::ports::conversation::ConvError;
use gantry_store::{DeliveryQuery, DeliveryRow, InboxQuery, Order, Page, PageCursor, StoreError};
use http::StatusCode;
use serde_json::{json, Value};

use crate::consts::PAGE_DEFAULT_LIMIT;
use crate::problem::{Problem, ProblemCode};
use crate::query::Query;
use crate::server::{ApiCore, HandlerRequest, Reply};

fn invalid_cursor() -> Problem {
    Problem::new(ProblemCode::InvalidCursor, "cursor cannot be parsed")
}

fn unavailable() -> Problem {
    Problem::retry_after(ProblemCode::Unavailable, "store unavailable", 1)
}

fn store_error(error: StoreError) -> Problem {
    match error {
        StoreError::InvalidCursor => invalid_cursor(),
        _ => unavailable(),
    }
}

/// 客户端游标 = `base64url(<资源标签>|<下游游标>)`。
pub fn wrap_cursor(tag: &str, inner: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{tag}|{inner}"))
}

pub fn unwrap_cursor(tag: &str, cursor: &str) -> Result<String, Problem> {
    let raw = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| invalid_cursor())?;
    let text = String::from_utf8(raw).map_err(|_| invalid_cursor())?;
    let (got, inner) = text.split_once('|').ok_or_else(invalid_cursor)?;
    if got != tag || inner.is_empty() {
        return Err(invalid_cursor());
    }
    Ok(inner.to_owned())
}

fn order(query: &Query, default: Order) -> Order {
    match query.str("order") {
        Some("asc") => Order::Asc,
        Some("desc") => Order::Desc,
        _ => default,
    }
}

fn limit(query: &Query) -> u32 {
    query
        .int("limit")
        .map(|v| v as u32)
        .unwrap_or(PAGE_DEFAULT_LIMIT)
}

fn page_body(items: Vec<Value>, next: Option<String>) -> Value {
    json!({ "items": items, "next_cursor": next })
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<Value, Problem> {
    serde_json::to_value(value).map_err(|_| Problem::internal())
}

fn parse<T: std::str::FromStr>(raw: &str) -> Result<T, Problem> {
    raw.parse()
        .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "parameter is invalid"))
}

/// #11 `GET /v1/conversations/{conversation_id}/messages`。
pub async fn list_messages(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let id: ConversationId = parse(req.param("conversation_id"))?;
    let (direction, page_order) = match req.query.str("order") {
        Some("asc") => ("asc", PageOrder::Asc),
        _ => ("desc", PageOrder::Desc),
    };
    let tag = format!("v1:messages:{id}:{direction}");
    let cursor = req
        .query
        .str("cursor")
        .map(|cursor| {
            let inner = unwrap_cursor(&tag, cursor)?;
            // HTTP 分页按 ID 排序，不接受旧的内部事件序号游标。
            if inner.starts_with("seq:") {
                return Err(invalid_cursor());
            }
            Ok(inner)
        })
        .transpose()?;
    let page = core
        .services()
        .conversation
        .list_messages(
            &id,
            PageQuery {
                cursor,
                limit: limit(req.query),
                order: Some(page_order),
            },
        )
        .await
        .map_err(|error| match error {
            ConvError::NotFound => Problem::not_found(),
            ConvError::InvalidRequest { pointer, .. } if pointer == "/cursor" => invalid_cursor(),
            ConvError::Forbidden => {
                Problem::forbidden_role(gantry_core::entities::DeviceRole::Viewer)
            }
            ConvError::Store(PortStoreError::Unavailable { .. }) => unavailable(),
            _ => Problem::internal(),
        })?;
    let items = page.items.iter().map(to_json).collect::<Result<_, _>>()?;
    let next = page.next_cursor.map(|cursor| wrap_cursor(&tag, &cursor));
    Ok(Reply::json(
        StatusCode::OK,
        page_body(items, next),
        page.last_seq,
    ))
}

/// #5 `GET /v1/deliveries`（api.md §4.4）。
pub async fn list_deliveries(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    const TAG: &str = "deliveries";
    let q = req.query;
    let mut page = Page {
        limit: limit(q),
        cursor: None,
        order: order(q, Order::Desc),
    };
    if let Some(cursor) = q.str("cursor") {
        page.cursor = Some(PageCursor::new(unwrap_cursor(TAG, cursor)?));
    }
    let query = DeliveryQuery {
        project_id: q.str("project_id").map(parse).transpose()?,
        stages: q
            .list("stage")
            .map(|items| {
                items
                    .iter()
                    .map(|s| stage(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default(),
        stage_status: q.str("stage_status").map(stage_status).transpose()?,
        active: q.bool("active").unwrap_or(false),
    };
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    let seq = snapshot.head_seq();
    let paged = snapshot
        .deliveries(&query, &page)
        .await
        .map_err(store_error)?;
    let mut out = Vec::with_capacity(paged.items.len());
    for row in &paged.items {
        out.push(delivery_json(&mut snapshot, row).await?);
    }
    let next = paged.next_cursor.map(|c| wrap_cursor(TAG, c.as_str()));
    Ok(Reply::json(StatusCode::OK, page_body(out, next), seq))
}

/// #6 `GET /v1/deliveries/{delivery_id}`。
pub async fn get_delivery(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let id: DeliveryId = parse(req.param("delivery_id"))?;
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    let seq = snapshot.head_seq();
    let row = snapshot.delivery(&id).ok_or_else(Problem::not_found)?;
    Ok(Reply::json(
        StatusCode::OK,
        delivery_json(&mut snapshot, &row).await?,
        seq,
    ))
}

fn stage(raw: &str) -> Result<Stage, Problem> {
    serde_json::from_value(json!(raw))
        .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "parameter is invalid"))
}

fn stage_status(raw: &str) -> Result<StageStatus, Problem> {
    serde_json::from_value(json!(raw))
        .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "parameter is invalid"))
}

/// `Delivery` 表示：`pr` 取 `create_pr` 成功动作的 GitHub 引用，再从 store 的 PR 快照补全（api.md §4.4）。
async fn delivery_json(
    snapshot: &mut gantry_store::ReadSnapshot,
    row: &DeliveryRow,
) -> Result<Value, Problem> {
    let project = snapshot
        .project(&row.project_id)
        .await
        .map_err(store_error)?;
    let pr_ref = snapshot.outbox_list().into_iter().find_map(|action| {
        (action.delivery_id == row.id
            && action.kind == gantry_core::entities::ForgeActionKind::CreatePr
            && action.status == OutboxStatus::Succeeded)
            .then(|| action.github_ref.as_ref()?.pr.clone())
            .flatten()
    });
    let pr = match (project.as_ref(), pr_ref) {
        (Some(project), Some(pr)) => snapshot
            .pr_snapshot(&project.repo, pr.number)
            .await
            .map_err(store_error)?,
        _ => None,
    };
    Ok(json!({
        "id": row.id,
        "project_id": row.project_id,
        "title": row.title,
        "level_cap": row.level_cap,
        "level_effective": row.level_effective,
        "stage": row.stage,
        "stage_status": row.stage_status,
        "issues": row.issues,
        "pr": pr,
        "worktree_id": row.worktree_id,
        "parent_id": null,
        "depends_on": [],
        "origin": row.origin,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    }))
}

/// #3 `GET /v1/projects`。
pub async fn list_projects(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    const TAG: &str = "projects";
    let q = req.query;
    let mut page = Page {
        limit: limit(q),
        cursor: None,
        order: order(q, Order::Desc),
    };
    if let Some(cursor) = q.str("cursor") {
        page.cursor = Some(PageCursor::new(unwrap_cursor(TAG, cursor)?));
    }
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    let seq = snapshot.head_seq();
    let paged = snapshot.projects(&page).await.map_err(store_error)?;
    let items = paged.items.iter().map(to_json).collect::<Result<_, _>>()?;
    let next = paged.next_cursor.map(|c| wrap_cursor(TAG, c.as_str()));
    Ok(Reply::json(StatusCode::OK, page_body(items, next), seq))
}

/// #4 `GET /v1/projects/{project_id}`。
pub async fn get_project(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let id: ProjectId = parse(req.param("project_id"))?;
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    let seq = snapshot.head_seq();
    let project = snapshot
        .project(&id)
        .await
        .map_err(store_error)?
        .ok_or_else(Problem::not_found)?;
    Ok(Reply::json(StatusCode::OK, to_json(&project)?, seq))
}

/// #10 `GET /v1/projects/{project_id}/conversations`。
pub async fn list_conversations(
    core: &ApiCore,
    req: &HandlerRequest<'_>,
) -> Result<Reply, Problem> {
    let project: ProjectId = parse(req.param("project_id"))?;
    // 项目不存在是 404，由 API 边界按 store 投影判定，不依赖下游实现。
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    if snapshot
        .project(&project)
        .await
        .map_err(store_error)?
        .is_none()
    {
        return Err(Problem::not_found());
    }
    let tag = format!("v1:conversations:{project}");
    let cursor = req
        .query
        .str("cursor")
        .map(|cursor| unwrap_cursor(&tag, cursor))
        .transpose()?;
    let page_order = match req.query.str("order") {
        Some("asc") => PageOrder::Asc,
        _ => PageOrder::Desc,
    };
    let page = core
        .services()
        .conversation
        .list_conversations(
            &project,
            PageQuery {
                cursor,
                limit: limit(req.query),
                order: Some(page_order),
            },
        )
        .await
        .map_err(|error| match error {
            ConvError::NotFound => Problem::not_found(),
            ConvError::InvalidRequest { pointer, .. } if pointer == "/cursor" => invalid_cursor(),
            ConvError::Forbidden => {
                Problem::forbidden_role(gantry_core::entities::DeviceRole::Viewer)
            }
            ConvError::Store(PortStoreError::Unavailable { .. }) => unavailable(),
            _ => Problem::internal(),
        })?;
    let items = page.items.iter().map(to_json).collect::<Result<_, _>>()?;
    let next = page.next_cursor.map(|cursor| wrap_cursor(&tag, &cursor));
    Ok(Reply::json(
        StatusCode::OK,
        page_body(items, next),
        page.last_seq,
    ))
}

/// #21 `GET /v1/inbox`。
pub async fn list_inbox(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    const TAG: &str = "inbox";
    let q = req.query;
    let mut page = Page {
        limit: limit(q),
        cursor: None,
        order: order(q, Order::Desc),
    };
    if let Some(cursor) = q.str("cursor") {
        page.cursor = Some(PageCursor::new(unwrap_cursor(TAG, cursor)?));
    }
    let filter = InboxQuery {
        project_id: q.str("project_id").map(parse::<ProjectId>).transpose()?,
        status: q
            .str("status")
            .map(|s| serde_json::from_value::<InboxStatus>(json!(s)))
            .transpose()
            .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "status is invalid"))?,
        kind: q
            .str("kind")
            .map(|s| serde_json::from_value::<InboxKind>(json!(s)))
            .transpose()
            .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "kind is invalid"))?,
    };
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    let seq = snapshot.head_seq();
    let paged = snapshot.inbox(&filter, &page).await.map_err(store_error)?;
    let items = paged.items.iter().map(to_json).collect::<Result<_, _>>()?;
    let next = paged.next_cursor.map(|c| wrap_cursor(TAG, c.as_str()));
    Ok(Reply::json(StatusCode::OK, page_body(items, next), seq))
}

/// #22 `GET /v1/inbox/{inbox_id}`。
pub async fn get_inbox(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let id: InboxId = parse(req.param("inbox_id"))?;
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    let seq = snapshot.head_seq();
    let item = snapshot
        .inbox_item(&id)
        .await
        .map_err(store_error)?
        .ok_or_else(Problem::not_found)?;
    Ok(Reply::json(StatusCode::OK, to_json(&item)?, seq))
}

/// #46 `GET /v1/inbox/summary`：只数 `open`，按项目分组（全局项为 `null`，P-63）。
pub async fn inbox_summary(core: &ApiCore, _req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let mut snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(store_error)?;
    let seq = snapshot.head_seq();
    let rows = snapshot.inbox_summary().await.map_err(store_error)?;
    let total: u64 = rows.iter().map(|r| u64::from(r.open)).sum();
    let by_project = rows
        .iter()
        .map(|r| {
            Ok(json!({
                "project_id": r.project_id,
                "open": r.open,
                "max_severity": to_json(&r.max_severity)?,
            }))
        })
        .collect::<Result<Vec<_>, Problem>>()?;
    let body = json!({ "total_open": total, "by_project": by_project });
    Ok(Reply::json(StatusCode::OK, body, seq))
}
