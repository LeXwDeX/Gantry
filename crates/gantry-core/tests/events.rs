//! `core` 事件目录与 data 校验的 M0 验收（core §12.2「事件」）。
#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};

use gantry_core::entities::{Actor, ActorKind, Message, Seq};
use gantry_core::events::{
    contract_undefined_types, decode_event_data, event_type_by_name, validate_event_data, Event,
    EventType, InboxRaisedData, NewEvent, SchemaErrorCode, Subject, EVENT_TYPES,
};
use gantry_core::ids::{EventId, ProjectId};

const CASES: &str = include_str!("../fixtures/events/cases.json");
const API_EVENTS: &str = include_str!("../../../fixtures/api/events.json");

fn cases() -> Value {
    serde_json::from_str(CASES).unwrap()
}

fn event(ty: &str, data: Value) -> Event<Value> {
    let id: EventId = "evt_01JB7P6Q9T3X2N5R8K1V4W7YZN".parse().unwrap();
    Event {
        id,
        seq: Seq::new(1).unwrap(),
        ts: "2026-10-24T12:41:03.120Z".parse().unwrap(),
        ty: ty.parse().unwrap(),
        subject: Subject::settings(),
        project_id: None,
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data,
    }
}

#[test]
fn event_type_names_roundtrip_for_all_78() {
    assert_eq!(EventType::ALL.len(), 79);
    assert_eq!(EVENT_TYPES.len(), 79);
    for ty in EventType::ALL {
        let name = ty.as_str();
        assert_eq!(event_type_by_name(name), Some(ty), "catalog misses {name}");
        assert_eq!(name.parse::<EventType>().unwrap(), ty);
        let info = EVENT_TYPES
            .iter()
            .find(|info| info.name == name)
            .unwrap_or_else(|| panic!("EVENT_TYPES misses {name}"));
        assert_eq!(info.name, name);
    }
    assert_eq!(
        "delivery.paused".parse::<EventType>().unwrap(),
        EventType::DeliveryPaused
    );
    assert_eq!(
        "guardian.heal_requested".parse::<EventType>().unwrap(),
        EventType::GuardianHealRequested
    );
    assert!("forge.unknown".parse::<EventType>().is_err());
    assert!("messages.posted".parse::<EventType>().is_err());
}

#[test]
fn catalog_matches_data_model_52() {
    let expected = [
        "project.added",
        "project.updated",
        "settings.updated",
        "conversation.created",
        "delivery.created",
        "delivery.stage_changed",
        "delivery.level_changed",
        "delivery.abandoned",
        "delivery.paused",
        "delivery.resumed",
        "workflow.created",
        "workflow.revised",
        "workflow.finished",
        "node.queued",
        "node.started",
        "node.finished",
        "node.superseded",
        "node.retry_requested",
        "attempt.started",
        "attempt.finished",
        "attempt.progress",
        "patch.validated",
        "patch.rejected",
        "sandbox.created",
        "sandbox.destroyed",
        "worktree.created",
        "worktree.locked",
        "worktree.unlocked",
        "worktree.removed",
        "message.posted",
        "inbox.raised",
        "inbox.resolved",
        "inbox.dismissed",
        "guardian.triaged",
        "guardian.heal_requested",
        "guardian.escalated",
        "approval.requested",
        "approval.granted",
        "approval.consumed",
        "approval.invalidated",
        "approval.expired",
        "approval.denied",
        "outbox.enqueued",
        "outbox.executing",
        "outbox.succeeded",
        "outbox.failed",
        "outbox.cancelled",
        "forge.synced",
        "forge.drift_detected",
        "budget.charged",
        "budget.exhausted",
        "budget.limit_raised",
        "policy.denied",
        "device.paired",
        "device.updated",
        "device.revoked",
    ];
    let names: Vec<&str> = EVENT_TYPES.iter().map(|info| info.name).collect();
    for name in expected {
        assert!(names.contains(&name), "catalog misses fixed type {name}");
    }
    let forge_names: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| name.starts_with("forge."))
        .collect();
    assert_eq!(
        forge_names.len(),
        25,
        "23 forge kinds + synced/drift_detected"
    );
    assert_eq!(names.len(), 79);
}

#[test]
fn every_valid_fixture_passes_validation() {
    let cases = cases();
    let valid = cases["valid"].as_object().unwrap();
    let typed: Vec<EventType> = EventType::ALL
        .into_iter()
        .filter(|ty| {
            validate_event_data(ty, &json!({})).map_err(|e| e.code)
                != Err(SchemaErrorCode::ContractUndefined)
        })
        .collect();
    assert_eq!(
        valid.len(),
        typed.len(),
        "one positive fixture per typed event"
    );
    for ty in typed {
        let data = valid
            .get(ty.as_str())
            .unwrap_or_else(|| panic!("missing positive fixture for {ty}"));
        validate_event_data(&ty, data).unwrap_or_else(|error| panic!("{ty} rejected: {error:?}"));
    }
}

/// 用户压低上限只写 `kind = cap` 下降；`effective` 下降一律拒绝（engine.md §11，#48）。
#[test]
fn level_changed_cap_may_fall_but_effective_only_rises() {
    let ty = EventType::DeliveryLevelChanged;
    let cap_down = json!({"from": "High", "to": "Low", "kind": "cap", "reason": "user"});
    validate_event_data(&ty, &cap_down).unwrap();
    let effective_down =
        json!({"from": "High", "to": "Low", "kind": "effective", "reason": "user"});
    let error = validate_event_data(&ty, &effective_down).unwrap_err();
    assert!(error.pointer.contains("/to"), "{error:?}");
}

#[test]
fn undefined_events_are_rejected_with_schema_id() {
    let cases = cases();
    let expected: Vec<String> = cases["contract_undefined"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    let mut actual: Vec<String> = contract_undefined_types()
        .into_iter()
        .map(|ty| ty.as_str().to_owned())
        .collect();
    actual.sort();
    let mut expected_sorted = expected.clone();
    expected_sorted.sort();
    assert_eq!(actual, expected_sorted);

    for name in expected {
        let ty: EventType = name.parse().unwrap();
        let error = validate_event_data(&ty, &json!({})).unwrap_err();
        assert_eq!(error.code, SchemaErrorCode::ContractUndefined, "{name}");
        let expected_schema = format!("schemas/events/{name}.json");
        assert_eq!(
            error.schema.as_deref(),
            Some(expected_schema.as_str()),
            "{name}"
        );
        // 未定义事件即使带上任意内容也不能返回成功。
        let error = validate_event_data(&ty, &json!({"anything": 1})).unwrap_err();
        assert_eq!(error.code, SchemaErrorCode::ContractUndefined, "{name}");
    }
}

#[test]
fn invalid_fixtures_report_json_pointer() {
    let cases = cases();
    for case in cases["invalid"].as_array().unwrap() {
        let ty: EventType = case["type"].as_str().unwrap().parse().unwrap();
        let data = &case["data"];
        let expected = case["pointer_contains"].as_str().unwrap();
        let error = match validate_event_data(&ty, data) {
            Ok(()) => panic!("{ty} unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(
            error.pointer.contains(expected),
            "{ty}: pointer {:?} does not contain {expected}",
            error.pointer
        );
    }
}

#[test]
fn settings_secret_value_is_fail_closed() {
    let data = json!({
        "scope": "global",
        "resource": "models",
        "schema_version": 1,
        "base_rev": 1,
        "value": {
            "models": {
                "providers": {
                    "anthropic": {
                        "credential": { "secret_ref": "model_key", "value": "sk-live" }
                    }
                }
            }
        },
        "relaxes": [],
        "secret_changes": []
    });
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::SecretValueForbidden);
    assert!(error.pointer.contains("credential"), "{:?}", error.pointer);

    // secret_changes 的元素拒绝 value 与哈希字段（deny_unknown_fields）。
    for extra in [("value", json!("sk")), ("hash", json!("abc"))] {
        let data = json!({
            "scope": "global",
            "resource": "github",
            "schema_version": 1,
            "base_rev": 1,
            "value": {},
            "relaxes": [],
            "secret_changes": [
                { "ref": "app_key", "op": "rotate", "version": 2, extra.0: extra.1 }
            ]
        });
        let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
        assert_eq!(error.code, SchemaErrorCode::Shape);
        assert!(
            error.pointer.contains("/secret_changes"),
            "{:?}",
            error.pointer
        );
    }

    // resource 文档里的任意未知键（包括名为 value 的业务键）都不符合 Patch 形状。
    let data = json!({
        "scope": "global",
        "resource": "levels",
        "schema_version": 1,
        "base_rev": 1,
        "value": { "value": 1 },
        "relaxes": [],
        "secret_changes": []
    });
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert!(error.message.contains("unknown field `value`"), "{error:?}");
}

#[test]
fn api_fixture_events_deserialize_and_validate() {
    let fixture: Value = serde_json::from_str(API_EVENTS).unwrap();
    let events = fixture["events"].as_array().unwrap();
    assert_eq!(events.len(), 7);
    for raw in events {
        let event: Event<Value> = serde_json::from_value(raw.clone()).unwrap();
        event
            .validate_data()
            .unwrap_or_else(|error| panic!("{} rejected: {error:?}", event.ty));
        // 同一值反复序列化稳定。
        let again: Event<Value> =
            serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        assert_eq!(event, again);

        // settings 信封的一致性。
        if event.ty == EventType::SettingsUpdated {
            let new = NewEvent {
                ty: event.ty,
                subject: event.subject.clone(),
                project_id: event.project_id,
                actor: event.actor.clone(),
                causation_id: None,
                correlation_id: None,
                data: event.data.clone(),
                dedup_key: None,
            };
            new.validate().unwrap();
        }
    }
}

#[test]
fn inbox_raised_accepts_only_wrapped_shape() {
    let cases = cases();
    let wrapped = cases["valid"]["inbox.raised"].clone();
    assert!(
        wrapped.get("item").is_some(),
        "fixture uses the wrapped shape"
    );
    let wrapped_event = event("inbox.raised", wrapped);
    let decoded: InboxRaisedData = decode_event_data(&wrapped_event).unwrap();
    assert_eq!(
        decoded.item().id.to_string(),
        "inb_01JBX2F5H7K9M1P3R5T7V9X0ZC"
    );
    assert!(decoded.supersedes.is_empty());

    let flat = serde_json::to_value(decoded.item()).unwrap();
    let flat_event = event("inbox.raised", flat);
    let error = decode_event_data::<InboxRaisedData>(&flat_event).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
}

#[test]
fn event_seq_must_be_a_safe_integer() {
    let raw = json!({
        "id": "evt_01JB7P6Q9T3X2N5R8K1V4W7YZN",
        "seq": 9_007_199_254_740_992u64,
        "ts": "2026-10-24T12:41:03.120Z",
        "type": "delivery.stage_changed",
        "subject": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "actor": {"kind": "engine", "id": "engine"},
        "causation_id": null,
        "correlation_id": null,
        "data": {"from": "implement", "to": "propose", "stage_status": "running"}
    });
    assert!(serde_json::from_value::<Event<Value>>(raw).is_err());

    let ok = json!({
        "id": "evt_01JB7P6Q9T3X2N5R8K1V4W7YZN",
        "seq": 9_007_199_254_740_991u64,
        "ts": "2026-10-24T12:41:03.120Z",
        "type": "delivery.stage_changed",
        "subject": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "actor": {"kind": "engine", "id": "engine"},
        "causation_id": null,
        "correlation_id": null,
        "data": {"from": "implement", "to": "propose", "stage_status": "running"}
    });
    let event: Event<Value> = serde_json::from_value(ok).unwrap();
    assert_eq!(event.seq.as_u64(), Seq::MAX);
}

#[test]
fn subjects_accept_settings_and_prefixed_ulids() {
    assert_eq!(Subject::parse("settings").unwrap(), Subject::settings());
    for value in [
        "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "evt_01JB7P6Q9T3X2N5R8K1V4W7YZN",
        "act_01JBA1F3Q9TX2N7B5D8H0M4RVC",
    ] {
        assert!(Subject::parse(value).is_ok(), "{value}");
    }
    for value in [
        "",
        "Settings",
        "xyz_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "prj_01jb7k2m9q4x8v3n5r6t1w0yza",
        "prj_01JB7K2M9Q4X8V3N5R6T1W0YZ",
        "prj_8JB7K2M9Q4X8V3N5R6T1W0YZA",
    ] {
        assert!(Subject::parse(value).is_err(), "{value}");
    }
    let subject: Subject = serde_json::from_value(json!("settings")).unwrap();
    assert_eq!(subject.as_str(), "settings");
    assert_eq!(subject.kind(), gantry_core::events::SubjectKind::Settings);
    assert_eq!(
        Subject::parse("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB")
            .unwrap()
            .kind(),
        gantry_core::events::SubjectKind::Delivery
    );
}

#[test]
fn decode_event_data_checks_the_event_type() {
    let cases = cases();
    let stage = event(
        "delivery.stage_changed",
        cases["valid"]["delivery.stage_changed"].clone(),
    );
    let data: gantry_core::events::DeliveryStageChangedData = decode_event_data(&stage).unwrap();
    assert_eq!(data.to, gantry_core::entities::Stage::Review);
    assert!(decode_event_data::<Message>(&stage).is_err());

    let started = event("node.started", cases["valid"]["node.started"].clone());
    let node: gantry_core::events::NodeEventData = decode_event_data(&started).unwrap();
    assert_eq!(node.status, gantry_core::entities::NodeStatus::Running);
}

#[test]
fn new_event_checks_settings_subject_consistency() {
    let cases = cases();
    let data = cases["valid"]["settings.updated"].clone();
    let project: ProjectId = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap();
    let actor = Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM");

    NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::settings(),
        project_id: None,
        actor: actor.clone(),
        causation_id: None,
        correlation_id: None,
        data: data.clone(),
        dedup_key: None,
    }
    .validate()
    .unwrap();

    NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::settings(),
        project_id: Some(project),
        actor: actor.clone(),
        causation_id: None,
        correlation_id: None,
        data: data.clone(),
        dedup_key: None,
    }
    .validate()
    .unwrap_err();

    // data.scope 不能与信封不一致：global data 配 project subject 必须拒绝。
    NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::parse("prj_01JB7K2M9Q4X8V3N5R6T1W0YZA").unwrap(),
        project_id: Some(project),
        actor: actor.clone(),
        causation_id: None,
        correlation_id: None,
        data: data.clone(),
        dedup_key: None,
    }
    .validate()
    .unwrap_err();

    let mut project_data = data;
    project_data["scope"] = json!("project");
    NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::parse("prj_01JB7K2M9Q4X8V3N5R6T1W0YZA").unwrap(),
        project_id: Some(project),
        actor: actor.clone(),
        causation_id: None,
        correlation_id: None,
        data: project_data,
        dedup_key: None,
    }
    .validate()
    .unwrap();

    NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::parse("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB").unwrap(),
        project_id: None,
        actor,
        causation_id: None,
        correlation_id: None,
        data: cases["valid"]["settings.updated"].clone(),
        dedup_key: None,
    }
    .validate()
    .unwrap_err();
}

#[test]
fn forge_kind_must_match_event_type() {
    let cases = cases();
    let data = cases["valid"]["forge.pr_synchronized"].clone();
    validate_event_data(
        &EventType::Forge(gantry_core::entities::ForgeEventKind::PrSynchronized),
        &data,
    )
    .unwrap();
    let error = validate_event_data(
        &EventType::Forge(gantry_core::entities::ForgeEventKind::PrMerged),
        &data,
    )
    .unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Consistency);
    assert_eq!(error.pointer, "/kind");
}

#[test]
fn undefined_events_are_never_projected_as_success() {
    // 这条是 store 侧 fail-closed 的契约前提：未定义事件永远不能通过校验。
    for ty in contract_undefined_types() {
        assert!(validate_event_data(&ty, &json!({})).is_err(), "{ty}");
    }
}

/// #122：人工锁不绑定交付单元，`delivery_id: null` 合法；engine / guardian 锁照常绑定。
#[test]
fn worktree_locked_delivery_binding() {
    let data = |holder: &str, delivery_id: Value| {
        json!({
            "worktree_id": "wtr_01JBA1D9Q4MT7X2C6E8H0K3PRV",
            "holder": holder,
            "delivery_id": delivery_id,
            "lease_until": "2026-10-24T12:45:00.000Z",
        })
    };
    let delivery = json!("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB");
    let ty = EventType::WorktreeLocked;
    assert!(validate_event_data(&ty, &data("human", Value::Null)).is_ok());
    assert!(validate_event_data(&ty, &data("engine", delivery.clone())).is_ok());
    assert!(validate_event_data(&ty, &data("guardian", delivery.clone())).is_ok());
    assert!(validate_event_data(&ty, &data("engine", Value::Null)).is_err());
    assert!(validate_event_data(&ty, &data("human", delivery)).is_err());
}
