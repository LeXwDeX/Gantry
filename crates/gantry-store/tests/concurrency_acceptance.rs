//! store.md §12 并发验收的独立切片：分别运行持久层与内存替身。
#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::entities::{Actor, ActorKind, ForgeActionKind, OutboxStatus, Priority, Seq};
use gantry_core::events::{NewEvent, Subject};
use gantry_core::ids::{ActionId, DeliveryId, IdGen};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{Appended, ClaimOpts, EnqueuedBy, EventFilter, OutboxInsert, Store, StoreDeps};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Barrier};

const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const EXCLUDED: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC";
const WRITERS: usize = 16;
const EVENTS_PER_TX: usize = 3;

async fn open(sqlite: bool) -> Store {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let deps = StoreDeps {
        ids: Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(5)))),
        clock,
    };
    if sqlite {
        Store::open_temp(deps, &[]).await.unwrap()
    } else {
        Store::open_in_memory(deps, &[]).await.unwrap()
    }
}

fn message(text: &str, dedup_key: Option<String>) -> NewEvent {
    let cases: Value =
        serde_json::from_str(include_str!("../../gantry-core/fixtures/events/cases.json")).unwrap();
    let mut data = cases["valid"]["message.posted"].clone();
    data["text"] = json!(text);
    NewEvent {
        ty: "message.posted".parse().unwrap(),
        subject: Subject::parse("msg_01JBX2E3G5J7K9N1Q3S5T7W9YA").unwrap(),
        project_id: Some(PROJECT.parse().unwrap()),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key,
    }
}

fn request(delivery: DeliveryId, epoch: u32) -> OutboxInsert {
    OutboxInsert {
        project_id: PROJECT.parse().unwrap(),
        delivery_id: delivery,
        kind: ForgeActionKind::CreatePr,
        target: "pr:gantry/dlv-12->main".to_owned(),
        intent: json!({"base": "main", "head": "gantry/dlv-12", "title": "fix", "body": "body", "draft": false, "closes": []}),
        payload: json!({"repo": "acme/widgets", "head": "gantry/dlv-12", "base": "main", "title": "fix", "body": "body", "draft": false, "closes": [], "expected_head_oid": "d7d018f867d6c5b78e82906386420d45a4d84486", "maintainer_can_modify": false}),
        requires_approval: None,
        depends_on: Vec::new(),
        priority: Priority::Normal,
        not_before: None,
        epoch,
        coalesce_key: None,
        enqueued_by: EnqueuedBy {
            module: "engine".to_owned(),
            causation_id: None,
        },
    }
}

async fn dedup_contenders(store: Store) {
    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let mut tasks = Vec::new();
    for _ in 0..WRITERS {
        let store = store.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut tx = store.begin_write().await.unwrap();
            tokio::task::yield_now().await;
            let result = tx
                .append(message("same", Some("shared-key".into())))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            result
        }));
    }
    barrier.wait().await;
    let mut new_count = 0;
    let mut duplicate_count = 0;
    for task in tasks {
        match task.await.unwrap() {
            Appended::New(event) => {
                new_count += 1;
                assert_eq!(event.seq.as_u64(), 1);
            }
            Appended::Duplicate { existing_seq } => {
                duplicate_count += 1;
                assert_eq!(existing_seq.as_u64(), 1);
            }
        }
    }
    assert_eq!((new_count, duplicate_count), (1, WRITERS - 1));
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 200)
        .await
        .unwrap();
    assert_eq!(page.head.as_u64(), 1);
    assert_eq!(page.events.len(), 1);
}

async fn check_committed_prefix(store: &Store) -> u64 {
    let head = store.snapshot().await.unwrap().head_seq().as_u64();
    assert_eq!(head % EVENTS_PER_TX as u64, 0);
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 200)
        .await
        .unwrap();
    assert_eq!(page.head.as_u64(), head);
    assert_eq!(page.events.len(), head as usize);
    for (index, event) in page.events.iter().enumerate() {
        assert_eq!(event.seq.as_u64(), index as u64 + 1);
    }
    head
}

async fn transactions_and_reader(store: Store) {
    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let (observations, mut pending) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
    let reader_store = store.clone();
    let reader = tokio::spawn(async move {
        let mut count = 0;
        let mut observed_nonempty = false;
        // 写者在提交前等待确认；通道关闭时所有写者已提交或回滚。
        while let Some(ack) = pending.recv().await {
            observed_nonempty |= check_committed_prefix(&reader_store).await > 0;
            count += 1;
            ack.send(()).unwrap();
        }
        check_committed_prefix(&reader_store).await;
        (count, observed_nonempty)
    });
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let store = store.clone();
        let barrier = barrier.clone();
        let observations = observations.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut tx = store.begin_write().await.unwrap();
            for slot in 0..EVENTS_PER_TX {
                let result = tx
                    .append(message(&format!("{writer}:{slot}"), None))
                    .await
                    .unwrap();
                assert!(matches!(result, Appended::New(_)));
                if slot == 0 {
                    let (ack, observed) = oneshot::channel();
                    observations.send(ack).unwrap();
                    tokio::task::yield_now().await;
                    observed.await.unwrap();
                }
            }
            if writer % 4 == 0 {
                tx.rollback().await.unwrap();
            } else {
                tx.commit().await.unwrap();
            }
        }));
    }
    drop(observations);
    barrier.wait().await;
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(reader.await.unwrap(), (WRITERS, true));
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 200)
        .await
        .unwrap();
    assert_eq!(page.events.len(), (WRITERS - WRITERS / 4) * EVENTS_PER_TX);
    let mut seen = BTreeSet::new();
    for chunk in page.events.chunks_exact(EVENTS_PER_TX) {
        let text = chunk[0].data["text"].as_str().unwrap();
        let writer: usize = text.split(':').next().unwrap().parse().unwrap();
        assert!(writer < WRITERS && writer % 4 != 0 && seen.insert(writer));
        for (slot, event) in chunk.iter().enumerate() {
            assert_eq!(event.data["text"], format!("{writer}:{slot}"));
        }
    }
    assert_eq!(
        seen,
        (0..WRITERS).filter(|writer| writer % 4 != 0).collect()
    );
}

async fn claim_contenders(store: Store) {
    const READY: u32 = 8;
    const CLAIMANTS: usize = 12;
    let excluded: DeliveryId = EXCLUDED.parse().unwrap();
    let mut ready = BTreeSet::<ActionId>::new();
    let mut excluded_ids = BTreeSet::<ActionId>::new();
    let mut tx = store.begin_write().await.unwrap();
    for epoch in 0..READY {
        let enqueued = tx
            .outbox()
            .enqueue(request(DELIVERY.parse().unwrap(), epoch))
            .await
            .unwrap();
        assert!(enqueued.created);
        ready.insert(enqueued.action.id);
    }
    for epoch in 0..2 {
        let enqueued = tx.outbox().enqueue(request(excluded, epoch)).await.unwrap();
        assert!(enqueued.created);
        excluded_ids.insert(enqueued.action.id);
    }
    tx.commit().await.unwrap();
    assert_eq!(ready.len(), READY as usize);
    assert_eq!(excluded_ids.len(), 2);

    let barrier = Arc::new(Barrier::new(CLAIMANTS + 1));
    let mut tasks = Vec::new();
    for _ in 0..CLAIMANTS {
        let store = store.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut tx = store.begin_write().await.unwrap();
            tokio::task::yield_now().await;
            let claim = tx
                .outbox()
                .claim(ClaimOpts {
                    lease: Duration::from_secs(60),
                    exclude_deliveries: BTreeSet::from([excluded]),
                })
                .await
                .unwrap();
            tx.commit().await.unwrap();
            claim
        }));
    }
    barrier.wait().await;
    let mut claimed = BTreeSet::new();
    let mut empty = 0;
    for task in tasks {
        if let Some(result) = task.await.unwrap() {
            assert!(!result.replay);
            assert_eq!(result.action.status, OutboxStatus::Executing);
            assert_eq!(result.action.attempts, 1);
            assert!(result.action.lease_until.unwrap().as_millis() > 1_700_000_000_000);
            assert!(claimed.insert(result.action.id), "row claimed twice");
        } else {
            empty += 1;
        }
    }
    assert_eq!(claimed, ready);
    assert_eq!(empty, CLAIMANTS - READY as usize);
    let snapshot = store.snapshot().await.unwrap();
    for id in &ready {
        let action = snapshot.outbox_action(id).unwrap();
        assert_eq!(action.status, OutboxStatus::Executing);
        assert_eq!(action.attempts, 1);
    }
    for id in &excluded_ids {
        let action = snapshot.outbox_action(id).unwrap();
        assert_eq!(action.status, OutboxStatus::Queued);
        assert_eq!(action.attempts, 0);
        assert!(action.lease_until.is_none());
    }
}

#[tokio::test]
async fn concurrent_dedup_on_both_backends() {
    for sqlite in [true, false] {
        tokio::time::timeout(
            Duration::from_secs(30),
            dedup_contenders(open(sqlite).await),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn concurrent_transactions_and_reader_on_both_backends() {
    for sqlite in [true, false] {
        tokio::time::timeout(
            Duration::from_secs(30),
            transactions_and_reader(open(sqlite).await),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn concurrent_outbox_claims_on_both_backends() {
    for sqlite in [true, false] {
        tokio::time::timeout(
            Duration::from_secs(30),
            claim_contenders(open(sqlite).await),
        )
        .await
        .unwrap();
    }
}
