//! store M0 内存实现的最小一致性验收（store.md §12 的子集）。
#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use gantry_core::entities::Seq;
use gantry_core::entities::{
    Actor, ActorKind, ExpectedEffect, ForgeActionKind, GithubRef, OutboxStatus, Priority,
    SettingsResource, SettingsScope,
};
use gantry_core::events::{EventType, NewEvent, SchemaErrorCode, Subject};
use gantry_core::ids::{ActionId, DeliveryId, IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{
    AfterFailure, AppendOpts, ClaimOpts, EnqueuedBy, EventFilter, ModuleSpec, OutboxInsert, Store,
    StoreDeps, StoreError, TypePattern,
};
use serde_json::{json, Value};

const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");

const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";

fn cases() -> Value {
    serde_json::from_str(CASES).unwrap()
}

fn deps(start_ms: i64) -> StoreDeps {
    let clock = Arc::new(FakeClock::new(start_ms));
    let entropy = Arc::new(SeqEntropy::new(11));
    let ids = Arc::new(IdGen::new(clock.clone(), entropy));
    StoreDeps { clock, ids }
}

async fn store() -> Store {
    store_with_clock(1_700_000_000_000).await.0
}

async fn store_with_clock(start_ms: i64) -> (Store, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new(start_ms));
    let entropy = Arc::new(SeqEntropy::new(11));
    let ids = Arc::new(IdGen::new(clock.clone(), entropy));
    let store = Store::open_in_memory(
        StoreDeps {
            clock: clock.clone(),
            ids,
        },
        &[],
    )
    .await
    .unwrap();
    (store, clock)
}

fn project() -> ProjectId {
    PROJECT.parse().unwrap()
}

fn delivery() -> DeliveryId {
    DELIVERY.parse().unwrap()
}

fn event(ty: &str, subject: &str, data: Value) -> NewEvent {
    NewEvent {
        ty: ty.parse().unwrap(),
        subject: Subject::parse(subject).unwrap(),
        project_id: Some(project()),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    }
}

fn delivery_created() -> NewEvent {
    event(
        "delivery.created",
        DELIVERY,
        cases()["valid"]["delivery.created"].clone(),
    )
}

fn message_posted() -> NewEvent {
    event(
        "message.posted",
        "msg_01JBX2E3G5J7K9N1Q3S5T7W9YA",
        cases()["valid"]["message.posted"].clone(),
    )
}

fn create_pr_payload() -> Value {
    json!({
        "repo": "acme/widgets",
        "head": "gantry/dlv-12",
        "base": "main",
        "title": "fix",
        "body": "body",
        "draft": false,
        "closes": [],
        "expected_head_oid": "d7d018f867d6c5b78e82906386420d45a4d84486",
        "maintainer_can_modify": false
    })
}

fn settings_global() -> NewEvent {
    let mut event = event(
        "settings.updated",
        "settings",
        cases()["valid"]["settings.updated"].clone(),
    );
    event.project_id = None;
    // 示例夹具取自已有日志快照（base_rev 18236）；内存 store 从 rev 0 开始。
    event.data["base_rev"] = json!(0);
    event
}

#[tokio::test]
async fn append_assigns_seq_and_commit_is_visible() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let first = tx.append(delivery_created()).await.unwrap();
    let second = tx
        .append(event(
            "delivery.stage_changed",
            DELIVERY,
            json!({"from": "spec", "to": "plan", "stage_status": "running"}),
        ))
        .await
        .unwrap();

    let (first, second) = match (first, second) {
        (gantry_store::Appended::New(a), gantry_store::Appended::New(b)) => (a, b),
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(first.seq.as_u64(), 1);
    assert_eq!(second.seq.as_u64(), 2);
    assert!(first.ts <= second.ts);
    tx.commit().await.unwrap();

    assert_eq!(store.events().head().await.unwrap().as_u64(), 2);
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.head_seq().as_u64(), 2);
}

#[tokio::test]
async fn rollback_discards_every_change() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.advance_cursor("engine", Seq::new(1).unwrap())
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
    assert_eq!(store.cursors().load("engine").await.unwrap(), Seq::ZERO);
}

#[tokio::test]
async fn dedup_retry_has_no_side_effects() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let mut first = settings_global();
    first.dedup_key = Some("idem-1".to_owned());
    tx.append(first).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let mut again = settings_global();
    again.dedup_key = Some("idem-1".to_owned());
    let outcome = tx.append(again).await.unwrap();
    assert_eq!(
        outcome,
        gantry_store::Appended::Duplicate {
            existing_seq: Seq::new(1).unwrap()
        }
    );
    tx.commit().await.unwrap();

    assert_eq!(store.events().head().await.unwrap().as_u64(), 1);
    assert_eq!(
        store
            .snapshot()
            .await
            .unwrap()
            .settings_current(&SettingsScope::Global, SettingsResource::Levels)
            .rev
            .as_u64(),
        1
    );
}

#[tokio::test]
async fn scan_pages_with_scanned_to_and_filters() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    for index in 0..5 {
        let mut event = delivery_created();
        event.data["title"] = json!(format!("delivery {index}"));
        event.data["level_cap"] = json!("Low");
        event.data["level_effective"] = json!("Low");
        // 每次 created 必须是新的交付单元；不同 subject 保持五个独立事件。
        let subject = format!("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZ{}", char::from(b'A' + index));
        event.subject = Subject::parse(&subject).unwrap();
        tx.append(event).await.unwrap();
    }
    tx.commit().await.unwrap();

    let log = store.events();
    let page = log
        .scan(Seq::ZERO, &EventFilter::default(), 2)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.scanned_to.as_u64(), 2);
    assert_eq!(page.head.as_u64(), 5);

    let page = log
        .scan(Seq::new(2).unwrap(), &EventFilter::default(), 2)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.scanned_to.as_u64(), 4);

    let page = log
        .scan(Seq::new(4).unwrap(), &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.scanned_to.as_u64(), 5);

    let filter = EventFilter {
        include: vec![TypePattern::Exact(EventType::DeliveryCreated)],
        ..EventFilter::default()
    };
    assert_eq!(
        log.scan(Seq::ZERO, &filter, 10).await.unwrap().events.len(),
        5
    );

    let filter = EventFilter {
        exclude: vec![TypePattern::Prefix("delivery.".to_owned())],
        ..EventFilter::default()
    };
    let page = log.scan(Seq::ZERO, &filter, 10).await.unwrap();
    assert!(page.events.is_empty());
    // 被过滤的事件也算已扫描：scanned_to 必须推进到 head。
    assert_eq!(page.scanned_to.as_u64(), 5);

    let error = log.check_cursor(Seq::new(6).unwrap()).await.unwrap_err();
    assert!(matches!(error, StoreError::CursorAhead { head } if head.as_u64() == 5));
}

#[tokio::test]
async fn global_events_match_project_filters() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(settings_global()).await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();

    let filter = EventFilter {
        project_ids: Some(BTreeSet::from([project()])),
        ..EventFilter::default()
    };
    let page = store.events().scan(Seq::ZERO, &filter, 10).await.unwrap();
    assert_eq!(page.events.len(), 2);

    let other: ProjectId = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZB".parse().unwrap();
    let filter = EventFilter {
        project_ids: Some(BTreeSet::from([other])),
        ..EventFilter::default()
    };
    let page = store.events().scan(Seq::ZERO, &filter, 10).await.unwrap();
    assert_eq!(page.events.len(), 1, "only the global settings event");
    assert_eq!(page.events[0].ty, EventType::SettingsUpdated);
}

#[tokio::test]
async fn timestamps_never_go_backwards() {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(3)) as Arc<dyn gantry_core::ids::Entropy>,
    ));
    let store = Store::open_in_memory(
        StoreDeps {
            clock: clock.clone(),
            ids,
        },
        &[],
    )
    .await
    .unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let first = tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();

    clock.set(gantry_core::time::Timestamp::from_millis(1_699_000_000_000));
    let mut tx = store.begin_write().await.unwrap();
    let second = tx.append(message_posted()).await.unwrap();
    tx.commit().await.unwrap();

    let (first, second) = match (first, second) {
        (gantry_store::Appended::New(a), gantry_store::Appended::New(b)) => (a, b),
        other => panic!("unexpected {other:?}"),
    };
    assert!(second.ts >= first.ts);
}

#[tokio::test]
async fn log_epoch_starts_at_one_and_bumps_on_restore() {
    let store = store().await;
    assert_eq!(store.log_identity().log_epoch, 1);
    let bumped = store.restore_epoch_bump();
    assert_eq!(bumped.log_epoch, 2);
    assert_eq!(store.log_identity().log_epoch, 2);
}

#[tokio::test]
async fn secret_values_are_rejected_before_the_log() {
    let store = store().await;
    let invalid = cases()["invalid"][0].clone();
    let mut event = settings_global();
    event.data = invalid["data"].clone();
    let mut tx = store.begin_write().await.unwrap();
    let error = tx.append(event).await.unwrap_err();
    match error {
        StoreError::InvalidEvent(error) => {
            assert_eq!(error.code, SchemaErrorCode::SecretValueForbidden);
        }
        other => panic!("unexpected {other:?}"),
    }
    tx.rollback().await.unwrap();
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
}

#[tokio::test]
async fn settings_projection_tracks_revs_and_config_rev() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let mut first = settings_global();
    first.data["value"] = json!({"high": {"budget": {"nodes": 30}}});
    tx.append(first).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let mut second = settings_global();
    second.data["base_rev"] = json!(1);
    second.data["value"] = json!({"high": {"budget": {"nodes": 35}}});
    tx.append(second).await.unwrap();
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    let current = snapshot.settings_current(&SettingsScope::Global, SettingsResource::Levels);
    assert_eq!(current.rev.as_u64(), 2);
    assert_eq!(current.value.unwrap()["high"]["budget"]["nodes"], json!(35));

    let at_one = snapshot
        .settings_at_rev(
            &SettingsScope::Global,
            SettingsResource::Levels,
            Seq::new(1).unwrap(),
        )
        .unwrap();
    assert_eq!(at_one.rev.as_u64(), 1);
    assert_eq!(at_one.value.unwrap()["high"]["budget"]["nodes"], json!(30));

    let as_of_one = snapshot.settings_as_of(
        &SettingsScope::Global,
        SettingsResource::Levels,
        Seq::new(1).unwrap(),
    );
    assert_eq!(as_of_one.rev.as_u64(), 1);

    assert_eq!(snapshot.config_rev(None).as_u64(), 2);
    assert_eq!(snapshot.config_rev(Some(&project())).as_u64(), 2);
}

#[tokio::test]
async fn outbox_enqueue_is_idempotent_and_rolls_back() {
    let store = store().await;
    let request = || OutboxInsert {
        project_id: project(),
        delivery_id: delivery(),
        kind: ForgeActionKind::CreatePr,
        target: "branch:gantry/dlv-12".to_owned(),
        intent: json!({"head": "gantry/dlv-12", "base": "main"}),
        payload: create_pr_payload(),
        requires_approval: None,
        depends_on: Vec::new(),
        priority: Priority::Normal,
        not_before: None,
        epoch: 0,
        coalesce_key: None,
        enqueued_by: EnqueuedBy {
            module: "engine".to_owned(),
            causation_id: None,
        },
    };

    let mut tx = store.begin_write().await.unwrap();
    let first = tx.outbox().enqueue(request()).await.unwrap();
    assert!(first.created);
    let second = tx.outbox().enqueue(request()).await.unwrap();
    assert!(!second.created);
    assert_eq!(first.action.id, second.action.id);
    tx.rollback().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert!(snapshot.outbox_action(&first.action.id).is_none());
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
}

#[tokio::test]
async fn outbox_claim_and_complete_update_the_row_and_log() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx
        .outbox()
        .enqueue(OutboxInsert {
            project_id: project(),
            delivery_id: delivery(),
            kind: ForgeActionKind::CreatePr,
            target: "branch:gantry/dlv-12".to_owned(),
            intent: json!({"head": "gantry/dlv-12"}),
            payload: create_pr_payload(),
            requires_approval: None,
            depends_on: Vec::new(),
            priority: Priority::High,
            not_before: None,
            epoch: 0,
            coalesce_key: None,
            enqueued_by: EnqueuedBy {
                module: "engine".to_owned(),
                causation_id: None,
            },
        })
        .await
        .unwrap();
    let claimed = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(!claimed.replay);
    assert_eq!(claimed.action.status, OutboxStatus::Executing);
    tx.outbox()
        .complete(
            &enqueued.action.id,
            GithubRef {
                bot: None,
                pr: None,
                marker: None,
                recorded_at: None,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    let action = snapshot.outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(action.status, OutboxStatus::Succeeded);
    assert_eq!(store.events().head().await.unwrap().as_u64(), 1);
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events[0].ty, EventType::OutboxSucceeded);
}

#[tokio::test]
async fn outbox_retry_stays_queued_and_terminal_fails() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx
        .outbox()
        .enqueue(OutboxInsert {
            project_id: project(),
            delivery_id: delivery(),
            kind: ForgeActionKind::CreateComment,
            target: "pr:12".to_owned(),
            intent: json!({"body": "hi"}),
            payload: json!({
                "repo": "acme/widgets",
                "target": {"type": "pr", "number": 12},
                "client_ref": "comment:1",
                "body": "hi"
            }),
            requires_approval: None,
            depends_on: Vec::new(),
            priority: Priority::Normal,
            not_before: None,
            epoch: 0,
            coalesce_key: None,
            enqueued_by: EnqueuedBy {
                module: "guardian".to_owned(),
                causation_id: None,
            },
        })
        .await
        .unwrap();
    tx.outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    tx.outbox()
        .fail(
            &enqueued.action.id,
            gantry_core::entities::OutboxError {
                code: "transient".to_owned(),
                message: "timeout".to_owned(),
            },
            AfterFailure::Retry {
                not_before: gantry_core::time::Timestamp::from_millis(1_700_000_000_000),
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    let action = snapshot.outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(action.status, OutboxStatus::Queued);
    assert!(action.not_before.is_some());
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);

    let mut tx = store.begin_write().await.unwrap();
    // Retry 回到 queued；再次认领是普通认领（replay = false），再走终态失败。
    let claimed = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(!claimed.replay);
    tx.outbox()
        .fail(
            &enqueued.action.id,
            gantry_core::entities::OutboxError {
                code: "permission_denied".to_owned(),
                message: "no access".to_owned(),
            },
            AfterFailure::Terminal,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    let action = snapshot.outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(action.status, OutboxStatus::Failed);
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].ty, EventType::OutboxFailed);
}

#[tokio::test]
async fn empty_defined_events_are_rejected_but_not_undefined() {
    assert!(gantry_core::events::contract_undefined_types().is_empty());
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let error = tx
        .append(event(
            "forge.synced",
            "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
            json!({}),
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidEvent(_)));
    assert!(!error.is_contract_undefined());
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn unprojected_events_are_recorded_not_swallowed() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(event(
        "budget.exhausted",
        DELIVERY,
        json!({"dimension": "nodes", "limit": 40, "used": 40}),
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let unprojected = store.unprojected();
    assert_eq!(unprojected.len(), 1);
    assert_eq!(unprojected[0].ty, EventType::BudgetExhausted);
    assert_eq!(unprojected[0].reason, "no_projector");
}

#[tokio::test]
async fn module_specs_are_rejected_in_m0() {
    match Store::open_in_memory(deps(1_700_000_000_000), &[ModuleSpec { module: "forge" }]).await {
        Err(StoreError::Unsupported(_)) => {}
        Err(other) => panic!("unexpected {other}"),
        Ok(_) => panic!("module specs were accepted"),
    }
}

#[tokio::test]
async fn append_guard_checks_subject_seq() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    let error = tx
        .append_with(
            message_posted(),
            AppendOpts {
                expect_subject_seq: Some(Some(Seq::new(1).unwrap())),
            },
        )
        .await
        .unwrap_err();
    // message subject 没有事件：Some(Some(1)) 不符。
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    tx.append_with(
        message_posted(),
        AppendOpts {
            expect_subject_seq: Some(None),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

// ---------------------------------------------------------------------------
// 直接写与事务冲突（revision）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn direct_write_after_begin_conflicts_and_keeps_live_state() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    let bumped = store.restore_epoch_bump();
    assert_eq!(bumped.log_epoch, 2);

    let error = tx.commit().await.unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    // 直接写保留，旧事务的事件不落地。
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
    assert_eq!(store.log_identity().log_epoch, 2);

    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(store.events().head().await.unwrap().as_u64(), 1);
}

#[tokio::test]
async fn cursor_direct_write_conflicts_with_open_tx_but_survives() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    store
        .cursors()
        .advance("engine", Seq::new(1).unwrap())
        .await
        .unwrap();

    let error = tx.commit().await.unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
    assert_eq!(store.cursors().load("engine").await.unwrap().as_u64(), 1);

    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.advance_cursor("engine", Seq::new(1).unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(store.events().head().await.unwrap().as_u64(), 1);
    assert_eq!(store.cursors().load("engine").await.unwrap().as_u64(), 1);
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestCounter {
    value: u64,
}

#[tokio::test]
async fn module_state_direct_write_conflicts_and_failed_closure_does_not_write() {
    const KEY: &str = "test.counter";
    let store = store().await;
    store
        .register_module_state(KEY, TestCounter { value: 0 })
        .unwrap();
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 0 })
    );

    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    store
        .update_module_state(KEY, |counter: &mut TestCounter| counter.value += 1)
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 1 })
    );

    // 失败闭包不写回、不递增 revision：随后的事务仍可提交。
    let failed = store
        .try_update_module_state(KEY, |counter: &mut TestCounter| {
            counter.value = 99;
            Err::<(), &'static str>("reject")
        })
        .unwrap();
    assert_eq!(failed, Err("reject"));
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 1 })
    );
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(store.events().head().await.unwrap().as_u64(), 1);
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestFlag(bool);

#[tokio::test]
async fn re_registration_keeps_state_and_rejects_other_type() {
    const KEY: &str = "test.registration";
    let store = store().await;
    store
        .register_module_state(KEY, TestCounter { value: 0 })
        .unwrap();
    store
        .update_module_state(KEY, |counter: &mut TestCounter| counter.value = 1)
        .unwrap();

    // 同一 key + 同一类型再注册是幂等操作：`new_in` 重复建替身不能重置状态。
    store
        .register_module_state(KEY, TestCounter { value: 99 })
        .unwrap();
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 1 })
    );

    // 同一 key + 不同类型必须拒绝，读写两侧都不能把字节当另一类型解释。
    let error = store
        .register_module_state(KEY, TestFlag(true))
        .unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    let error = store.module_state::<TestFlag>(KEY).unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 1 })
    );
}

#[tokio::test]
async fn tx_module_state_is_isolated_until_commit() {
    const KEY: &str = "test.tx.counter";
    let store = store().await;
    store
        .register_module_state(KEY, TestCounter { value: 0 })
        .unwrap();

    let mut tx = store.begin_write().await.unwrap();
    tx.update_module_state(KEY, |counter: &mut TestCounter| counter.value += 1)
        .unwrap();
    assert_eq!(
        tx.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 1 })
    );
    // 事务内的修改在提交前不落到 live 状态。
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 0 })
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 0 })
    );

    let mut tx = store.begin_write().await.unwrap();
    tx.update_module_state(KEY, |counter: &mut TestCounter| counter.value += 2)
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 2 })
    );

    // 直接写在事务打开后落地：旧事务提交冲突，不覆盖直接写。
    let mut tx = store.begin_write().await.unwrap();
    tx.update_module_state(KEY, |counter: &mut TestCounter| counter.value = 7)
        .unwrap();
    store
        .update_module_state(KEY, |counter: &mut TestCounter| counter.value = 3)
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    assert_eq!(
        store.module_state::<TestCounter>(KEY).unwrap(),
        Some(TestCounter { value: 3 })
    );
}

#[tokio::test]
async fn write_tx_now_is_fixed_and_monotonic() {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(5)) as Arc<dyn gantry_core::ids::Entropy>,
    ));
    let store = Store::open_in_memory(
        StoreDeps {
            clock: clock.clone(),
            ids,
        },
        &[],
    )
    .await
    .unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let fixed = tx.now();
    clock.set(gantry_core::time::Timestamp::from_millis(1_700_000_500_000));
    assert_eq!(tx.now(), fixed, "same transaction must keep one timestamp");
    let appended = tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();
    match appended {
        gantry_store::Appended::New(event) => assert_eq!(event.ts, fixed),
        other => panic!("unexpected {other:?}"),
    }

    // 时钟回拨不改变下一事务的下界：仍不早于最后事件时间。
    clock.set(gantry_core::time::Timestamp::from_millis(1_699_000_000_000));
    let tx = store.begin_write().await.unwrap();
    assert!(tx.now() >= fixed);
}

// ---------------------------------------------------------------------------
// Outbox 终态、依赖级联与取消原因
// ---------------------------------------------------------------------------

fn comment_request(epoch: u32) -> OutboxInsert {
    OutboxInsert {
        project_id: project(),
        delivery_id: delivery(),
        kind: ForgeActionKind::CreateComment,
        target: "pr:12".to_owned(),
        intent: json!({"body": "hi"}),
        payload: json!({
            "repo": "acme/widgets",
            "target": {"type": "pr", "number": 12},
            "client_ref": "comment:1",
            "body": "hi"
        }),
        requires_approval: None,
        depends_on: Vec::new(),
        priority: Priority::Normal,
        not_before: None,
        epoch,
        coalesce_key: None,
        enqueued_by: EnqueuedBy {
            module: "guardian".to_owned(),
            causation_id: None,
        },
    }
}

fn terminal_error(code: &str) -> gantry_core::entities::OutboxError {
    gantry_core::entities::OutboxError {
        code: code.to_owned(),
        message: "terminal".to_owned(),
    }
}

fn empty_github_ref() -> GithubRef {
    GithubRef {
        bot: None,
        pr: None,
        marker: None,
        recorded_at: None,
    }
}

async fn complete_succeeded(store: &Store, request: OutboxInsert) -> ActionId {
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx.outbox().enqueue(request).await.unwrap();
    let claimed = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.action.id, enqueued.action.id);
    tx.outbox()
        .complete(&enqueued.action.id, empty_github_ref())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    enqueued.action.id
}

fn expected_effect() -> ExpectedEffect {
    ExpectedEffect {
        event: "forge.issue_comment".to_owned(),
        match_: BTreeMap::from([("issue".to_owned(), json!(12))]),
        matched_webhook_deliveries: Vec::new(),
    }
}

/// 非 `executing` 行的预写必须失败，且行与成功窗口统计都不变。
async fn assert_record_effects_rejected(
    store: &Store,
    id: &ActionId,
    since: gantry_core::time::Timestamp,
) {
    let mut tx = store.begin_write().await.unwrap();
    let before = tx.read().outbox_action(id).unwrap();
    let count_before = tx
        .outbox()
        .count_succeeded_since(&delivery(), since)
        .await
        .unwrap();
    let error = tx
        .outbox()
        .record_effects(id, vec![expected_effect()])
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    assert_eq!(tx.read().outbox_action(id).unwrap(), before);
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&delivery(), since)
            .await
            .unwrap(),
        count_before
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn outbox_record_effects_requires_executing() {
    const START_MS: i64 = 1_700_000_000_000;
    let (store, clock) = store_with_clock(START_MS).await;
    let claim = || ClaimOpts {
        lease: Duration::from_secs(60),
        exclude_deliveries: BTreeSet::new(),
    };

    // 每个动作单独事务驱动，保证 claim 只看到当前这一行。
    let succeeded = complete_succeeded(&store, comment_request(0)).await;

    let mut tx = store.begin_write().await.unwrap();
    let failed = tx.outbox().enqueue(comment_request(1)).await.unwrap();
    let claimed = tx.outbox().claim(claim()).await.unwrap().unwrap();
    assert_eq!(claimed.action.id, failed.action.id);
    tx.outbox()
        .fail(
            &failed.action.id,
            terminal_error("permission_denied"),
            AfterFailure::Terminal,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let cancelled = tx.outbox().enqueue(comment_request(2)).await.unwrap();
    tx.outbox()
        .cancel(&cancelled.action.id, gantry_store::CancelReason::Manual)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let executing = tx.outbox().enqueue(comment_request(3)).await.unwrap();
    let claimed = tx.outbox().claim(claim()).await.unwrap().unwrap();
    assert_eq!(claimed.action.id, executing.action.id);
    tx.commit().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let queued = tx.outbox().enqueue(comment_request(4)).await.unwrap();
    tx.commit().await.unwrap();

    // 失败调用发生在成功之后：若错误地刷新 Succeeded 行的 updated_at，
    // 该行会落入 since 之后的窗口，统计由 0 变 1。
    clock.advance_ms(1_000);
    let since = gantry_core::time::Timestamp::from_millis(START_MS + 1);
    for id in [
        succeeded,
        failed.action.id,
        cancelled.action.id,
        queued.action.id,
    ] {
        assert_record_effects_rejected(&store, &id, since).await;
    }

    // 回滚不保留预写效果。
    let mut tx = store.begin_write().await.unwrap();
    let before = tx.read().outbox_action(&executing.action.id).unwrap();
    assert_eq!(before.status, OutboxStatus::Executing);
    let effects = vec![expected_effect()];
    tx.outbox()
        .record_effects(&executing.action.id, effects.clone())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot.outbox_action(&executing.action.id).unwrap(),
        before
    );

    // 正常 Executing 仍可预写并随事务提交。
    let mut tx = store.begin_write().await.unwrap();
    tx.outbox()
        .record_effects(&executing.action.id, effects.clone())
        .await
        .unwrap();
    let after = tx.read().outbox_action(&executing.action.id).unwrap();
    assert_eq!(after.status, OutboxStatus::Executing);
    assert_eq!(after.expected_effects, effects);
    assert_eq!(after.updated_at, tx.now());
    assert!(after.updated_at > before.updated_at);
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&delivery(), since)
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.outbox_action(&executing.action.id).unwrap(), after);
}

#[tokio::test]
async fn outbox_terminal_fail_requires_executing() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx.outbox().enqueue(comment_request(0)).await.unwrap();
    let error = tx
        .outbox()
        .fail(
            &enqueued.action.id,
            terminal_error("permission_denied"),
            AfterFailure::Terminal,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::PreconditionFailed(_)));
    tx.commit().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    tx.outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    tx.outbox()
        .fail(
            &enqueued.action.id,
            terminal_error("permission_denied"),
            AfterFailure::Terminal,
        )
        .await
        .unwrap();
    let again = tx
        .outbox()
        .fail(
            &enqueued.action.id,
            terminal_error("permission_denied"),
            AfterFailure::Terminal,
        )
        .await
        .unwrap_err();
    assert!(matches!(again, StoreError::PreconditionFailed(_)));
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot.outbox_action(&enqueued.action.id).unwrap().status,
        OutboxStatus::Failed
    );
}

#[tokio::test]
async fn outbox_failure_projection_requires_executing() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx.outbox().enqueue(comment_request(0)).await.unwrap();
    let subject = gantry_core::events::Subject::parse(&enqueued.action.id.to_string()).unwrap();

    let failed = tx
        .append(NewEvent {
            ty: EventType::OutboxFailed,
            subject: subject.clone(),
            project_id: Some(project()),
            actor: Actor::new(ActorKind::Forge, "forge"),
            causation_id: None,
            correlation_id: None,
            data: json!({"code": "permission_denied", "message": "no access"}),
            dedup_key: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(failed, StoreError::PreconditionFailed(_)));
    let succeeded = tx
        .append(NewEvent {
            ty: EventType::OutboxSucceeded,
            subject,
            project_id: Some(project()),
            actor: Actor::new(ActorKind::Forge, "forge"),
            causation_id: None,
            correlation_id: None,
            data: json!({"github_ref": {"bot": null, "pr": null, "marker": null, "recorded_at": null}}),
            dedup_key: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(succeeded, StoreError::PreconditionFailed(_)));
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot.outbox_action(&enqueued.action.id).unwrap().status,
        OutboxStatus::Queued
    );
}

#[tokio::test]
async fn outbox_claim_cascades_dependency_failures() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    // 多层链 A -> B -> C -> D；依赖必须先存在，所以只能按上游到下游入队。
    // 实现仍用不动点循环，避免依赖链中间节点先失败时只取消一层。
    let mut a_request = comment_request(0);
    a_request.priority = Priority::High;
    let a = tx.outbox().enqueue(a_request).await.unwrap();
    let mut b_request = comment_request(1);
    b_request.depends_on = vec![a.action.id];
    let b = tx.outbox().enqueue(b_request).await.unwrap();
    let mut c_request = comment_request(2);
    c_request.depends_on = vec![b.action.id];
    let c = tx.outbox().enqueue(c_request).await.unwrap();
    let mut d_request = comment_request(3);
    d_request.depends_on = vec![c.action.id];
    let d = tx.outbox().enqueue(d_request).await.unwrap();
    let claimed = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.action.id, a.action.id);
    tx.outbox()
        .fail(
            &a.action.id,
            terminal_error("permission_denied"),
            AfterFailure::Terminal,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin_write().await.unwrap();
    let none = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap();
    assert!(none.is_none(), "all dependents must be cancelled");
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    for id in [b.action.id, c.action.id, d.action.id] {
        let action = snapshot.outbox_action(&id).unwrap();
        assert_eq!(action.status, OutboxStatus::Cancelled);
        assert_eq!(action.last_error.unwrap().code, "dependency_failed");
    }
}

#[tokio::test]
async fn outbox_cancel_reason_codes_are_snake_case() {
    let store = store().await;
    let cases = [
        (gantry_store::CancelReason::Superseded, "superseded"),
        (
            gantry_store::CancelReason::DependencyFailed,
            "dependency_failed",
        ),
        (
            gantry_store::CancelReason::ApprovalInvalid,
            "approval_invalid",
        ),
        (gantry_store::CancelReason::Manual, "manual"),
    ];
    let mut tx = store.begin_write().await.unwrap();
    for (index, (reason, expected)) in cases.into_iter().enumerate() {
        let enqueued = tx
            .outbox()
            .enqueue(comment_request(index as u32))
            .await
            .unwrap();
        tx.outbox()
            .cancel(&enqueued.action.id, reason)
            .await
            .unwrap();
        let action = tx.read().outbox_action(&enqueued.action.id).unwrap();
        assert_eq!(action.last_error.unwrap().code, expected);
    }
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn outbox_renew_lease_requires_executing_and_future() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx.outbox().enqueue(comment_request(0)).await.unwrap();
    let now = tx.now();
    let queued = tx
        .outbox()
        .renew_lease(
            &enqueued.action.id,
            gantry_core::time::Timestamp::from_millis(now.as_millis() + 60_000),
        )
        .await
        .unwrap_err();
    assert!(matches!(queued, StoreError::PreconditionFailed(_)));

    tx.outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    let now = tx.now();
    let past = tx
        .outbox()
        .renew_lease(
            &enqueued.action.id,
            gantry_core::time::Timestamp::from_millis(now.as_millis() - 1),
        )
        .await
        .unwrap_err();
    assert!(matches!(past, StoreError::PreconditionFailed(_)));

    let until = gantry_core::time::Timestamp::from_millis(now.as_millis() + 120_000);
    tx.outbox()
        .renew_lease(&enqueued.action.id, until)
        .await
        .unwrap();
    let action = tx.read().outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(action.status, OutboxStatus::Executing);
    assert_eq!(action.lease_until, Some(until));
    assert_eq!(action.updated_at, now);
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .outbox_action(&enqueued.action.id)
            .unwrap()
            .lease_until,
        Some(until)
    );
}

#[tokio::test]
async fn outbox_requeue_requires_executing_and_refunds_only_when_asked() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let enqueued = tx.outbox().enqueue(comment_request(0)).await.unwrap();
    let now = tx.now();
    let queued = tx
        .outbox()
        .requeue(&enqueued.action.id, now, true)
        .await
        .unwrap_err();
    assert!(matches!(queued, StoreError::PreconditionFailed(_)));

    tx.outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    let claimed = tx.read().outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(claimed.attempts, 1);

    tx.outbox()
        .requeue(&enqueued.action.id, now, false)
        .await
        .unwrap();
    let kept = tx.read().outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(kept.status, OutboxStatus::Queued);
    assert_eq!(kept.attempts, 1);
    assert_eq!(kept.not_before, Some(now));
    assert_eq!(kept.lease_until, None);

    tx.outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    let reclaimed = tx.read().outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(reclaimed.attempts, 2);

    tx.outbox()
        .requeue(&enqueued.action.id, now, true)
        .await
        .unwrap();
    let refunded = tx.read().outbox_action(&enqueued.action.id).unwrap();
    assert_eq!(refunded.status, OutboxStatus::Queued);
    assert_eq!(refunded.attempts, 1);
    assert_eq!(refunded.lease_until, None);

    // 退掉本次认领后再认领应重新计为一次普通尝试，而不是 replay。
    let reclaimed = tx
        .outbox()
        .claim(ClaimOpts {
            lease: Duration::from_secs(60),
            exclude_deliveries: BTreeSet::new(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(!reclaimed.replay);
    assert_eq!(reclaimed.action.attempts, 2);
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .outbox_action(&enqueued.action.id)
            .unwrap()
            .attempts,
        2
    );
}

#[tokio::test]
async fn outbox_cancel_blocked_returns_all_cascaded_ids() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    // A -> B -> C -> D；A 手工取消后，cancel_blocked 必须沿链取消其余三层。
    let a = tx.outbox().enqueue(comment_request(0)).await.unwrap();
    let mut b_request = comment_request(1);
    b_request.depends_on = vec![a.action.id];
    let b = tx.outbox().enqueue(b_request).await.unwrap();
    let mut c_request = comment_request(2);
    c_request.depends_on = vec![b.action.id];
    let c = tx.outbox().enqueue(c_request).await.unwrap();
    let mut d_request = comment_request(3);
    d_request.depends_on = vec![c.action.id];
    let d = tx.outbox().enqueue(d_request).await.unwrap();

    tx.outbox()
        .cancel(&a.action.id, gantry_store::CancelReason::Manual)
        .await
        .unwrap();
    let cancelled = tx.outbox().cancel_blocked().await.unwrap();
    assert_eq!(
        cancelled.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([b.action.id, c.action.id, d.action.id])
    );
    assert!(tx.outbox().cancel_blocked().await.unwrap().is_empty());

    let root = tx.read().outbox_action(&a.action.id).unwrap();
    assert_eq!(root.status, OutboxStatus::Cancelled);
    assert_eq!(root.last_error.unwrap().code, "manual");
    for id in [b.action.id, c.action.id, d.action.id] {
        let action = tx.read().outbox_action(&id).unwrap();
        assert_eq!(action.status, OutboxStatus::Cancelled);
        assert_eq!(action.last_error.unwrap().code, "dependency_failed");
    }
    tx.commit().await.unwrap();

    // outbox.cancelled 的 data 未定义：级联只改表，不写事件。
    assert_eq!(store.events().head().await.unwrap(), Seq::ZERO);
}

#[tokio::test]
async fn outbox_count_succeeded_since_filters_delivery_and_includes_boundary() {
    const START_MS: i64 = 1_700_000_000_000;
    let (store, clock) = store_with_clock(START_MS).await;

    complete_succeeded(&store, comment_request(0)).await;
    clock.advance_ms(1_000);
    complete_succeeded(&store, comment_request(1)).await;
    clock.advance_ms(1_000);
    let other: DeliveryId = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC".parse().unwrap();
    let mut other_request = comment_request(0);
    other_request.delivery_id = other;
    complete_succeeded(&store, other_request).await;

    // 未成功的行不计入。
    let mut tx = store.begin_write().await.unwrap();
    tx.outbox().enqueue(comment_request(2)).await.unwrap();

    let start = gantry_core::time::Timestamp::from_millis(START_MS);
    let after_start = gantry_core::time::Timestamp::from_millis(START_MS + 1);
    let second = gantry_core::time::Timestamp::from_millis(START_MS + 1_000);
    let third = gantry_core::time::Timestamp::from_millis(START_MS + 2_000);
    let after_third = gantry_core::time::Timestamp::from_millis(START_MS + 2_001);

    // since 为闭区间：成功时间等于 since 也要计入。
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&delivery(), start)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&delivery(), after_start)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&delivery(), second)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&delivery(), third)
            .await
            .unwrap(),
        0
    );
    // 另一个交付单元的同一窗口独立计数。
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&other, start)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&other, third)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        tx.outbox()
            .count_succeeded_since(&other, after_third)
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();
}

// ---------------------------------------------------------------------------
// 单写者等待：异步让出执行线程，同步入口共用同一队列
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ReleaseMode {
    Drop,
    Commit,
    Rollback,
}

/// 两个独立 Tokio 任务竞争写令牌：持有者先拿到并按 `mode` 释放，等待者随后完成。
///
/// 等待者在同一轮 poll 中先发信号再进入 `begin_write`；收到信号时它已经注册或
/// 已完成。若 `begin_write` 同步阻塞，单线程运行时的持有者无法继续执行。
async fn contention_round(mode: ReleaseMode) {
    let store = store().await;
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let holder_store = store.clone();
    let holder = tokio::spawn(async move {
        let tx = holder_store.begin_write().await.unwrap();
        held_tx.send(()).unwrap();
        release_rx.await.unwrap();
        match mode {
            ReleaseMode::Drop => drop(tx),
            ReleaseMode::Commit => {
                tx.commit().await.unwrap();
            }
            ReleaseMode::Rollback => {
                tx.rollback().await.unwrap();
            }
        }
    });
    held_rx.await.unwrap();

    let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
    let waiter_store = store.clone();
    let waiter = tokio::spawn(async move {
        waiting_tx.send(()).unwrap();
        let mut tx = waiter_store.begin_write().await.unwrap();
        tx.append(delivery_created()).await.unwrap();
        tx.rollback().await.unwrap();
    });
    waiting_rx.await.unwrap();

    release_tx.send(()).unwrap();
    holder.await.unwrap();
    waiter.await.unwrap();
}

/// 取消队首等待者不能吞掉令牌；它后面的等待者必须继续前进。
async fn cancelled_waiter_propagates_the_token() {
    let store = store().await;
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let holder_store = store.clone();
    let holder = tokio::spawn(async move {
        let tx = holder_store.begin_write().await.unwrap();
        held_tx.send(()).unwrap();
        release_rx.await.unwrap();
        drop(tx);
    });
    held_rx.await.unwrap();

    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let cancelled_store = store.clone();
    let cancelled = tokio::spawn(async move {
        cancelled_tx.send(()).unwrap();
        let tx = cancelled_store.begin_write().await.unwrap();
        drop(tx);
    });
    cancelled_rx.await.unwrap();

    let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
    let waiter_store = store.clone();
    let waiter = tokio::spawn(async move {
        waiting_tx.send(()).unwrap();
        let tx = waiter_store.begin_write().await.unwrap();
        drop(tx);
    });
    waiting_rx.await.unwrap();

    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());

    release_tx.send(()).unwrap();
    holder.await.unwrap();
    waiter.await.unwrap();

    let tx = store.begin_write().await.unwrap();
    drop(tx);
}

/// 子进程入口：只在父测试的看门狗下运行，避免旧实现的同步阻塞挂死测试进程。
#[test]
#[ignore = "child process: driven by write_gate_async_contention_completes watchdog"]
fn write_gate_async_contention_child() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for mode in [
            ReleaseMode::Drop,
            ReleaseMode::Commit,
            ReleaseMode::Rollback,
        ] {
            contention_round(mode).await;
        }
        cancelled_waiter_propagates_the_token().await;
    });
}

/// 外部进程看门狗：单线程运行时里两个独立任务竞争写令牌必须有界完成。
#[test]
fn write_gate_async_contention_completes() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "write_gate_async_contention_child", "--ignored"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stdout = String::new();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                pipe.read_to_string(&mut stdout).unwrap();
            }
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_string(&mut stderr).unwrap();
            }
            assert!(
                status.success(),
                "write gate child failed: {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert!(
                stdout.contains("1 passed"),
                "write gate child did not run the regression test:\n{stdout}"
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "write gate child did not finish within {CHILD_TIMEOUT:?}; \
                 async begin_write must not block the runtime thread"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 同步入口与异步入口串行化在同一把写者权威上。
#[tokio::test]
async fn sync_and_async_entries_share_one_gate() {
    let store = store().await;
    let tx = store.begin_write().await.unwrap();

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let sync_store = store.clone();
    let thread = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let tx = sync_store.begin_write_sync().unwrap();
        drop(tx);
        done_tx.send(()).unwrap();
    });
    started_rx.recv().unwrap();

    // 异步持有者仍在事务中：同步入口不能提前完成。
    assert!(
        done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "sync writer acquired while the async writer held the token"
    );

    drop(tx);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("sync writer was not woken after the async writer released");
    thread.join().unwrap();
}

/// 压低上限只写 `kind = cap`；投影派生 `level_effective = min(原值, 新上限)`（engine.md §11，#48）。
#[tokio::test]
async fn lowering_cap_clamps_effective_in_projection() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap(); // cap High, effective Medium
    for (from, to) in [("High", "Low"), ("Low", "High")] {
        tx.append(event(
            "delivery.level_changed",
            DELIVERY,
            json!({"from": from, "to": to, "kind": "cap", "reason": "user"}),
        ))
        .await
        .unwrap();
    }
    // 原值 Medium 已被派生为 Low：以 Medium 为 from 的 effective 事件不再匹配。
    let stale = tx
        .append(event(
            "delivery.level_changed",
            DELIVERY,
            json!({"from": "Medium", "to": "High", "kind": "effective", "reason": "escalation:auth_paths"}),
        ))
        .await;
    assert!(
        matches!(stale, Err(gantry_store::StoreError::PreconditionFailed(_))),
        "{stale:?}"
    );
    tx.append(event(
        "delivery.level_changed",
        DELIVERY,
        json!({"from": "Low", "to": "High", "kind": "effective", "reason": "escalation:auth_paths"}),
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
}
