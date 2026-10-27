//! 读类路由：消息 #11、收件箱 #21 #22 #46。
//!
//! 收件箱使用读快照的 `seq`；消息使用下游返回的读取水位（api.md §3.6）。
//! 分页游标对客户端不透明：本模块把下游游标包成 `base64url`，解析失败、跨资源、
//! 方向不符一律 `400 invalid_cursor`（本文 §7.7）。
//!
//! 项目、交付单元、工作流、节点尝试（#3–#6、#17–#20）要等 `store` 的完整
//! `ProjectionRead`（store.md §6.2），当前 M0 内存实现只有收件箱与交付单元行；
//! 项目对话列表 #10 仍需项目存在性的读取支持。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::entities::{InboxKind, InboxStatus};
use gantry_core::ids::{ConversationId, InboxId, ProjectId};
use gantry_core::ports::context::{PageOrder, PageQuery, StoreError as PortStoreError};
use gantry_core::ports::conversation::ConvError;
use gantry_store::{InboxQuery, Order, Page, PageCursor, StoreError};
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
