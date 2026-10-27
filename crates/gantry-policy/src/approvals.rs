//! 审批的读写：事件是事实源，审批行由 `approval.*` 事件折叠得到（policy.md §7.1）。
//!
//! M0 的 `store` 还没有 `approvals` 投影（store.md OQ-2），这里按 subject 读回该审批的
//! 全部事件再折叠。状态转移一律"读当前状态 → `machines::approval::check` → 带
//! `expect_subject_seq` 守卫追加"，守卫等价于比较并交换：别的写者先改了这个审批，
//! 追加就会失败，不会产生两次转移。

use gantry_core::canon::SubjectDigest;
use gantry_core::entities::{Actor, ActorKind, Approval, ApprovalStatus, Seq};
use gantry_core::events::{
    ApprovalConsumedData, ApprovalGrantedData, ApprovalInvalidatedData, ApprovalRequestedData,
    Event, EventType, NewEvent, Subject,
};
use gantry_core::ids::{ApprovalId, EventId};
use gantry_core::machines::approval as machine;
use gantry_core::ports::StoreError as PortStoreError;
use gantry_core::time::Timestamp;
use gantry_store::{AppendOpts, EventFilter, EventLog, Store, TypePattern, WriteTx};
use serde_json::Value;

/// 折叠出的审批与它最后一条事件的序号（CAS 守卫用）。
#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub approval: Approval,
    pub last_seq: Seq,
}

impl Record {
    pub fn is_live(&self) -> bool {
        matches!(
            self.approval.status,
            ApprovalStatus::Pending | ApprovalStatus::Granted
        )
    }

    pub fn is_due(&self, now: Timestamp) -> bool {
        self.is_live() && now >= self.approval.expires_at
    }
}

pub(crate) fn subject(id: &ApprovalId) -> Subject {
    Subject::parse(&id.to_string()).expect("approval ids are valid subjects")
}

pub(crate) fn policy_actor() -> Actor {
    Actor::new(ActorKind::System, "policy")
}

pub(crate) fn port_error(error: gantry_store::StoreError) -> PortStoreError {
    use gantry_store::StoreError as E;
    match error {
        E::NotFound => PortStoreError::NotFound,
        E::Poisoned => PortStoreError::Unavailable { retry_after: None },
        E::PreconditionFailed(_) => PortStoreError::Conflict {
            code: "precondition_failed",
        },
        E::Unsupported(message) => PortStoreError::Rejected {
            message: message.to_owned(),
        },
        other => PortStoreError::InvalidState {
            message: other.to_string(),
        },
    }
}

/// 读一个审批；不存在返回 `None`。
pub(crate) async fn load(
    log: &EventLog,
    id: &ApprovalId,
) -> Result<Option<Record>, PortStoreError> {
    let events = log.by_subject(&subject(id)).await.map_err(port_error)?;
    Ok(fold(*id, &events))
}

/// 读全部审批（按创建顺序）。M0 的内存存储没有投影，规模由测试决定。
pub(crate) async fn load_all(log: &EventLog) -> Result<Vec<Record>, PortStoreError> {
    let filter = EventFilter {
        project_ids: None,
        include: vec![TypePattern::Exact(EventType::ApprovalRequested)],
        exclude: Vec::new(),
    };
    let mut after = Seq::ZERO;
    let mut records = Vec::new();
    loop {
        let page = log.scan(after, &filter, 200).await.map_err(port_error)?;
        for event in &page.events {
            let id: ApprovalId =
                event
                    .subject
                    .as_str()
                    .parse()
                    .map_err(|_| PortStoreError::InvalidState {
                        message: "approval.requested subject is not an approval id".into(),
                    })?;
            if let Some(record) = load(log, &id).await? {
                records.push(record);
            }
        }
        if page.scanned_to >= page.head || page.events.is_empty() {
            return Ok(records);
        }
        after = page.scanned_to;
    }
}

fn fold(id: ApprovalId, events: &[Event<Value>]) -> Option<Record> {
    let mut record: Option<Record> = None;
    for event in events {
        if event.ty == EventType::ApprovalRequested {
            let data: ApprovalRequestedData = serde_json::from_value(event.data.clone()).ok()?;
            record = Some(Record {
                approval: Approval {
                    id,
                    delivery_id: event
                        .correlation_id
                        .as_deref()
                        .and_then(|value| value.parse().ok()),
                    action: data.action,
                    target: data.target,
                    subject_digest: data.subject_digest,
                    device_id: None,
                    signature: None,
                    status: ApprovalStatus::Pending,
                    expires_at: data.expires_at,
                    consumed_by: None,
                    created_at: event.ts,
                    updated_at: event.ts,
                },
                last_seq: event.seq,
            });
            continue;
        }
        let Some(current) = record.as_mut() else {
            continue;
        };
        let approval = &mut current.approval;
        match event.ty {
            EventType::ApprovalGranted => {
                if let Ok(data) = serde_json::from_value::<ApprovalGrantedData>(event.data.clone())
                {
                    approval.device_id = Some(data.device_id);
                }
                approval.status = ApprovalStatus::Granted;
            }
            EventType::ApprovalConsumed => {
                if let Ok(data) = serde_json::from_value::<ApprovalConsumedData>(event.data.clone())
                {
                    approval.consumed_by = Some(data.consumed_by);
                }
                approval.status = ApprovalStatus::Consumed;
            }
            EventType::ApprovalInvalidated => approval.status = ApprovalStatus::Invalidated,
            EventType::ApprovalExpired => approval.status = ApprovalStatus::Expired,
            EventType::ApprovalDenied => approval.status = ApprovalStatus::Denied,
            _ => continue,
        }
        approval.updated_at = event.ts;
        current.last_seq = event.seq;
    }
    record
}

/// 一次状态转移：校验转移表，带 subject 守卫追加事件，返回转移后的记录。
pub(crate) async fn transition(
    tx: &mut WriteTx,
    record: &Record,
    to: ApprovalStatus,
    data: Value,
    actor: Actor,
    causation_id: Option<EventId>,
) -> Result<Record, PortStoreError> {
    machine::check(record.approval.status, to).map_err(|error| PortStoreError::InvalidState {
        message: error.to_string(),
    })?;
    let ty = match to {
        ApprovalStatus::Granted => EventType::ApprovalGranted,
        ApprovalStatus::Consumed => EventType::ApprovalConsumed,
        ApprovalStatus::Invalidated => EventType::ApprovalInvalidated,
        ApprovalStatus::Expired => EventType::ApprovalExpired,
        ApprovalStatus::Denied => EventType::ApprovalDenied,
        ApprovalStatus::Pending => unreachable!("check rejects transitions into pending"),
    };
    let event = NewEvent {
        ty,
        subject: subject(&record.approval.id),
        project_id: None,
        actor,
        causation_id,
        correlation_id: record.approval.delivery_id.map(|id| id.to_string()),
        data,
        dedup_key: None,
    };
    let appended = tx
        .append_with(
            event,
            AppendOpts {
                expect_subject_seq: Some(Some(record.last_seq)),
            },
        )
        .await
        .map_err(port_error)?;
    let gantry_store::Appended::New(event) = appended else {
        unreachable!("approval transitions carry no dedup_key");
    };
    let mut next = record.clone();
    next.approval.status = to;
    next.approval.updated_at = event.ts;
    next.last_seq = event.seq;
    if let Ok(data) = serde_json::from_value::<ApprovalConsumedData>(event.data) {
        next.approval.consumed_by = Some(data.consumed_by);
    }
    Ok(next)
}

pub(crate) fn consumed_data(id: ApprovalId, consumed_by: String, subject: SubjectDigest) -> Value {
    serde_json::to_value(ApprovalConsumedData {
        approval_id: id,
        consumed_by,
        subject_digest: subject,
    })
    .expect("approval.consumed data serializes")
}

pub(crate) fn invalidated_data(data: ApprovalInvalidatedData) -> Value {
    serde_json::to_value(data).expect("approval.invalidated data serializes")
}

pub(crate) fn expired_data() -> Value {
    serde_json::json!({})
}

/// 惰性过期（policy.md §7.5）：到期仍为 pending / granted 的审批先转 `expired` 并提交。
/// 返回最新记录；没有到期时不写。
pub(crate) async fn expire_if_due(
    store: &Store,
    record: Record,
    now: Timestamp,
) -> Result<Record, PortStoreError> {
    if !record.is_due(now) {
        return Ok(record);
    }
    let mut tx = store.begin_write().await.map_err(port_error)?;
    let next = transition(
        &mut tx,
        &record,
        ApprovalStatus::Expired,
        expired_data(),
        policy_actor(),
        None,
    )
    .await;
    match next {
        Ok(next) => {
            tx.commit().await.map_err(port_error)?;
            Ok(next)
        }
        // 别的写者先转移了：重读，以最新状态为准。
        Err(PortStoreError::Conflict { .. }) => {
            drop(tx);
            let log = store.events();
            let latest = load(&log, &record.approval.id)
                .await?
                .ok_or(PortStoreError::NotFound)?;
            Ok(latest)
        }
        Err(error) => Err(error),
    }
}
