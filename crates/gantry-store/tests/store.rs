//! store M0 内存实现的最小一致性验收（store.md §12 的子集）。
#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::entities::Seq;
use gantry_core::entities::{
    Actor, ActorKind, ForgeActionKind, GithubRef, OutboxStatus, Priority, SettingsResource,
    SettingsScope,
};
use gantry_core::events::{EventType, NewEvent, SchemaErrorCode, Subject};
use gantry_core::ids::{DeliveryId, IdGen, ProjectId};
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
    Store::open_in_memory(deps(1_700_000_000_000), &[])
        .await
        .unwrap()
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
async fn contract_undefined_events_cannot_be_appended() {
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
    assert!(
        matches!(error, StoreError::InvalidEvent(ref error) if error.code == SchemaErrorCode::ContractUndefined)
    );
    assert!(error.is_contract_undefined());
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
