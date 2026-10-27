//! 测试共用：Store / Notify 构造与上报请求构造器。

#![allow(dead_code)]

use std::sync::Arc;

use gantry_core::entities::{
    Actor, ActorKind, BudgetDimension, DecisionKind, InboxKind, InboxOption, InboxPayload,
    MissingAspect, ReasonCode, Ref, SecurityFinding, Severity,
};
use gantry_core::events::{EventType, Subject};
use gantry_core::ids::{ApprovalId, DeliveryId, Id, IdGen, ProjectId, Ulid};
use gantry_core::ports::{ActorCtx, ClientProfile, DedupKey, RaiseRequest, SafeSummary};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::text::UntrustedText;
use gantry_notify::Notify;
use gantry_store::{EventFilter, Store, StoreDeps, TypePattern};
use serde_json::Value;

pub const NOW_MS: i64 = 1_700_000_000_000;

pub struct Harness {
    pub store: Store,
    pub notify: Notify,
    pub clock: Arc<FakeClock>,
}

pub fn harness() -> Harness {
    let clock = Arc::new(FakeClock::new(NOW_MS));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(0x907))));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    })
    .unwrap();
    let notify = Notify::new(store.clone(), ids);
    Harness {
        store,
        notify,
        clock,
    }
}

pub fn project(n: u128) -> ProjectId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

pub fn delivery(n: u128) -> DeliveryId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

pub fn approval(n: u128) -> ApprovalId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

pub fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "engine"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(NOW_MS as u64, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

pub fn device_ctx(profile: ClientProfile, n: u128) -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Device, format!("dev_{n}")),
        device: Some(Id::from_parts(NOW_MS as u64, n).unwrap()),
        client_profile: Some(profile),
        ..ctx()
    }
}

pub fn opt(id: &str) -> InboxOption {
    InboxOption::new(id, format!("选项 {id}"), false)
}

fn reason(code: &str) -> ReasonCode {
    ReasonCode::parse(code).unwrap()
}

pub fn request(kind: InboxKind, payload: InboxPayload, key: &str) -> RaiseRequest {
    RaiseRequest {
        project_id: Some(project(100)),
        delivery_id: Some(delivery(200)),
        kind,
        severity: Severity::Warn,
        summary: SafeSummary::new("需要你处理"),
        payload,
        dedup_key: DedupKey::new(key),
        supersedes: Vec::new(),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
    }
}

pub fn approval_needed(n: u128) -> RaiseRequest {
    request(
        InboxKind::ApprovalNeeded,
        InboxPayload::ApprovalNeeded {
            approval_id: approval(n),
        },
        &format!("approval:{}", approval(n)),
    )
}

pub fn decision(kind: DecisionKind, ids: &[&str], key: &str) -> RaiseRequest {
    request(
        InboxKind::DecisionNeeded,
        InboxPayload::DecisionNeeded {
            kind,
            reason_code: reason("gate_reject"),
            options: ids.iter().map(|id| opt(id)).collect(),
            node_id: None,
            patch: None,
        },
        key,
    )
}

pub fn gate_reject(key: &str) -> RaiseRequest {
    decision(DecisionKind::GateReject, &["replan", "abandon"], key)
}

pub fn escalation(code: &str, ids: Option<&[&str]>, key: &str) -> RaiseRequest {
    request(
        InboxKind::Escalation,
        InboxPayload::Escalation {
            reason_code: reason(code),
            node_id: None,
            attempt_id: None,
            target_level: None,
            action_id: None,
            evidence: Vec::new(),
            options: ids.map(|ids| ids.iter().map(|id| opt(id)).collect()),
        },
        key,
    )
}

/// `escalation(outbox_failed:not_mergeable)`，带失败动作的 `action_id`（#67）。
pub fn outbox_failed(ids: Option<&[&str]>, key: &str) -> RaiseRequest {
    let mut req = escalation("outbox_failed:not_mergeable", ids, key);
    if let InboxPayload::Escalation { action_id, .. } = &mut req.payload {
        *action_id = Some(Id::from_parts(NOW_MS as u64, 0xac7).unwrap());
    }
    req
}

pub fn level_cap(key: &str) -> RaiseRequest {
    escalation(
        "level_cap_exceeded",
        Some(&["upgrade", "narrow", "abandon"]),
        key,
    )
}

pub fn spec_needed(key: &str) -> RaiseRequest {
    request(
        InboxKind::SpecNeeded,
        InboxPayload::SpecNeeded {
            missing: vec![MissingAspect {
                aspect: "scope".into(),
                question: "范围是什么？".into(),
            }],
        },
        key,
    )
}

pub fn budget(ids: &[&str], key: &str) -> RaiseRequest {
    request(
        InboxKind::BudgetExhausted,
        InboxPayload::BudgetExhausted {
            dimension: BudgetDimension::Nodes,
            limit: 10,
            used: 10,
            options: ids.iter().map(|id| opt(id)).collect(),
        },
        key,
    )
}

pub fn security(key: &str) -> RaiseRequest {
    request(
        InboxKind::SecurityFinding,
        InboxPayload::SecurityFinding {
            source: Ref::Delivery { id: delivery(200) },
            finding: SecurityFinding {
                severity: Severity::Blocker,
                category: "secret".into(),
                title: UntrustedText::new("token in diff".into(), "test".into(), false),
            },
            attempt_id: None,
        },
        key,
    )
}

/// 一个合法请求覆盖全部六个 kind。
pub fn one_of_each(prefix: &str) -> Vec<RaiseRequest> {
    vec![
        {
            let mut req = approval_needed(1);
            req.dedup_key = DedupKey::new(format!("{prefix}:approval"));
            req
        },
        gate_reject(&format!("{prefix}:decision")),
        spec_needed(&format!("{prefix}:spec")),
        budget(&["abandon"], &format!("{prefix}:budget")),
        level_cap(&format!("{prefix}:esc")),
        security(&format!("{prefix}:sec")),
    ]
}

/// 事件日志里所有 `inbox.*` 事件（已提交）。
pub async fn inbox_events(store: &Store) -> Vec<gantry_core::events::Event<Value>> {
    let filter = EventFilter {
        include: vec![TypePattern::parse("inbox.*").unwrap()],
        ..EventFilter::default()
    };
    let mut cursor = gantry_core::entities::Seq::ZERO;
    let mut events = Vec::new();
    loop {
        let page = store.events().scan(cursor, &filter, 200).await.unwrap();
        events.extend(page.events);
        if page.scanned_to >= page.head {
            return events;
        }
        cursor = page.scanned_to;
    }
}

pub async fn events_of(store: &Store, ty: EventType) -> Vec<gantry_core::events::Event<Value>> {
    inbox_events(store)
        .await
        .into_iter()
        .filter(|event| event.ty == ty)
        .collect()
}

pub fn subject(id: &gantry_core::ids::InboxId) -> Subject {
    Subject::parse(&id.to_string()).unwrap()
}
