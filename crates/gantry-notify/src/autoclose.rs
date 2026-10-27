//! 自动关闭（notify.md §7.5 的 R1、R2）。
//!
//! 按持久游标读事件日志，把已经失效的 open 项置为 `resolved`：
//!
//! - R1：`approval.granted` / `denied` / `invalidated` / `expired` / `consumed`
//!   → 关闭 `payload.approval_id` 匹配的 `approval_needed`；
//! - R2：`delivery.abandoned`，或 `delivery.stage_changed` 且 `to ∈ {closed, abandoned}`
//!   → 关闭该交付单元的全部 open 项。
//!
//! 只关闭，不创建、不通知。每页事件的关闭与游标推进在同一个写事务里提交，
//! 崩溃重放不会重复关闭；`resolve` 本身对终态项幂等，换一个游标从头重放也
//! 不会产生新事件。R3（`supersedes`）在 `raise` 里完成，R4 是调用方显式 `resolve`。

use gantry_core::entities::{Actor, ActorKind, ApprovalStatus, Seq, Stage};
use gantry_core::events::{
    decode_event_data, DeliveryStageChangedData, Event, EventType, SchemaError,
};
use gantry_core::ids::{ApprovalId, DeliveryId};
use gantry_core::ports::{ActorCtx, InboxPort, Resolution, ResolveError, ResolveTarget};
use gantry_store::{EventFilter, StoreError, TypePattern};
use serde_json::Value;

use crate::Notify;

/// 默认游标名（`store` 的 `Cursors` 按名字区分消费者）。
pub const AUTOCLOSE_CONSUMER: &str = "notify.autoclose";

const TRIGGERS: &[&str] = &[
    "approval.granted",
    "approval.denied",
    "approval.invalidated",
    "approval.expired",
    "approval.consumed",
    "delivery.abandoned",
    "delivery.stage_changed",
];
const SCAN_PAGE: u32 = 200;

#[derive(Debug, thiserror::Error)]
pub enum AutoCloseError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error("trigger event {seq:?} is malformed: {source}")]
    Malformed { seq: Seq, source: SchemaError },
}

/// 自动关闭订阅者。`gantryd` 在收到新事件时调用 [`AutoClose::run_once`]。
#[derive(Clone, Debug)]
pub struct AutoClose {
    notify: Notify,
    consumer: String,
}

impl AutoClose {
    pub fn new(notify: Notify) -> Self {
        Self::with_consumer(notify, AUTOCLOSE_CONSUMER)
    }

    /// 指定游标名；同一事件日志用两个游标各跑一遍即"重放"。
    pub fn with_consumer(notify: Notify, consumer: impl Into<String>) -> Self {
        Self {
            notify,
            consumer: consumer.into(),
        }
    }

    /// 处理游标之后到当前 head 的全部触发事件；返回本轮关闭的上报项数。
    pub async fn run_once(&self) -> Result<usize, AutoCloseError> {
        let store = self.notify.store().clone();
        let filter = EventFilter {
            include: TRIGGERS
                .iter()
                .map(|ty| TypePattern::parse(ty))
                .collect::<Result<_, _>>()?,
            ..EventFilter::default()
        };
        let mut cursor = store.cursors().load(&self.consumer).await?;
        let mut closed = 0;
        loop {
            let page = store.events().scan(cursor, &filter, SCAN_PAGE).await?;
            if page.events.is_empty() && page.scanned_to <= cursor {
                return Ok(closed);
            }
            let mut tx = store.begin_write().await?;
            for event in &page.events {
                let Some((target, how)) = rule(event)? else {
                    continue;
                };
                let ctx = self.ctx(event);
                closed += self
                    .notify
                    .resolve(Some(&mut tx), target, how, &ctx)
                    .await?
                    .len();
            }
            if page.scanned_to > cursor {
                tx.advance_cursor(&self.consumer, page.scanned_to).await?;
            }
            tx.commit().await?;
            cursor = page.scanned_to;
            if page.scanned_to >= page.head {
                return Ok(closed);
            }
        }
    }

    fn ctx(&self, event: &Event<Value>) -> ActorCtx {
        ActorCtx {
            // §7.3：自动关闭的 actor 为 {kind: system, id: "notify"}。
            actor: Actor::new(ActorKind::System, "notify"),
            device: None,
            role: None,
            client_profile: None,
            request_id: self.notify.request_id(),
            idempotency_key: None,
            dedup_key: None,
            causation_id: Some(event.id),
        }
    }
}

/// 触发事件 → (关闭谁, 用什么 `Resolution`)；不触发返回 `None`。
fn rule(event: &Event<Value>) -> Result<Option<(ResolveTarget, Resolution)>, AutoCloseError> {
    let approval_status = match event.ty {
        EventType::ApprovalGranted => Some(ApprovalStatus::Granted),
        EventType::ApprovalDenied => Some(ApprovalStatus::Denied),
        EventType::ApprovalInvalidated => Some(ApprovalStatus::Invalidated),
        EventType::ApprovalExpired => Some(ApprovalStatus::Expired),
        EventType::ApprovalConsumed => Some(ApprovalStatus::Consumed),
        _ => None,
    };
    if let Some(status) = approval_status {
        let approval_id: ApprovalId = parse_subject(event)?;
        return Ok(Some((
            ResolveTarget::Approval(approval_id),
            Resolution::ApprovalSettled {
                approval_id,
                status,
            },
        )));
    }
    let how = match event.ty {
        EventType::DeliveryAbandoned => Resolution::DeliveryAbandoned,
        EventType::DeliveryStageChanged => {
            let data: DeliveryStageChangedData =
                decode_event_data(event).map_err(|source| AutoCloseError::Malformed {
                    seq: event.seq,
                    source,
                })?;
            match data.to {
                Stage::Closed => Resolution::DeliveryClosed,
                Stage::Abandoned => Resolution::DeliveryAbandoned,
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    let delivery_id: DeliveryId = parse_subject(event)?;
    Ok(Some((
        ResolveTarget::Delivery {
            id: delivery_id,
            kinds: None,
        },
        how,
    )))
}

fn parse_subject<K: gantry_core::ids::IdKind>(
    event: &Event<Value>,
) -> Result<gantry_core::ids::Id<K>, AutoCloseError> {
    event
        .subject
        .as_str()
        .parse()
        .map_err(|_| AutoCloseError::Malformed {
            seq: event.seq,
            source: SchemaError::consistency("/subject", "subject is not the expected id kind"),
        })
}
