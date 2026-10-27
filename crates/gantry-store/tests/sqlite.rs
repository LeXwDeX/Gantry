//! SQLite 持久层专属用例（store.md §12.2、§12.5）。通用语义由其他用例文件在
//! `GANTRY_STORE_BACKEND=sqlite` 下对 SQLite 再跑一遍。
#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::entities::{
    Actor, ActorKind, ForgeActionKind, GithubRef, OutboxStatus, Priority, Seq,
};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{
    Appended, ClaimOpts, EnqueuedBy, EventFilter, ModuleSpec, OutboxInsert, Store, StoreConfig,
    StoreDeps, StoreError, SCHEMA_VERSION,
};
use serde_json::{json, Value};

const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const POLICY: ModuleSpec = ModuleSpec::new("policy", &["policy_challenges"]);

/// 测试数据目录；结束时删除。
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gantry-store-sqlite-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 每次"重启"用更晚的时钟：真实进程重启后墙钟前进，事件 ID 不会与旧的冲突。
fn deps(start_ms: i64) -> StoreDeps {
    let clock = Arc::new(FakeClock::new(start_ms));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(5))));
    StoreDeps { clock, ids }
}

async fn open(dir: &Dir, start_ms: i64) -> Result<Store, StoreError> {
    Store::open(StoreConfig::new(&dir.0), deps(start_ms), &[POLICY]).await
}

fn cases() -> Value {
    serde_json::from_str(CASES).unwrap()
}

fn event(ty: &str, subject: &str, data: Value) -> NewEvent {
    NewEvent {
        ty: ty.parse().unwrap(),
        subject: Subject::parse(subject).unwrap(),
        project_id: Some(PROJECT.parse::<ProjectId>().unwrap()),
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

fn message_posted(dedup: &str) -> NewEvent {
    let mut ev = event(
        "message.posted",
        "msg_01JBX2E3G5J7K9N1Q3S5T7W9YA",
        cases()["valid"]["message.posted"].clone(),
    );
    ev.dedup_key = Some(dedup.to_owned());
    ev
}

fn enqueue_request() -> OutboxInsert {
    OutboxInsert {
        project_id: PROJECT.parse().unwrap(),
        delivery_id: DELIVERY.parse().unwrap(),
        kind: ForgeActionKind::CreatePr,
        target: "pr:gantry/dlv-12->main".to_owned(),
        intent: json!({
            "base": "main",
            "head": "gantry/dlv-12",
            "title": "fix",
            "body": "body",
            "draft": false,
            "closes": []
        }),
        payload: json!({
            "repo": "acme/widgets",
            "head": "gantry/dlv-12",
            "base": "main",
            "title": "fix",
            "body": "body",
            "draft": false,
            "closes": [],
            "expected_head_oid": "d7d018f867d6c5b78e82906386420d45a4d84486",
            "maintainer_can_modify": false
        }),
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
    }
}

#[tokio::test]
async fn committed_state_survives_reopen() {
    let dir = Dir::new("reopen");
    let (identity, action_id) = {
        let store = open(&dir, 1_700_000_000_000).await.unwrap();
        let mut tx = store.begin_write().await.unwrap();
        tx.append(delivery_created()).await.unwrap();
        tx.append(message_posted("msg-1")).await.unwrap();
        tx.advance_cursor("conversation", Seq::new(2).unwrap())
            .await
            .unwrap();
        tx.module_conn("policy")
            .put("policy_challenges", "a/b/c", &json!({"nonce": "n1"}))
            .unwrap();
        tx.module_conn("policy")
            .put("policy_challenges", "a/b/d", &json!({"nonce": "n2"}))
            .unwrap();
        let enqueued = tx.outbox().enqueue(enqueue_request()).await.unwrap();
        tx.commit().await.unwrap();

        // 第二个事务：认领并完成动作、删一行模块表、直接推进另一个游标。
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
        tx.module_conn("policy")
            .delete("policy_challenges", "a/b/d")
            .unwrap();
        tx.commit().await.unwrap();
        store
            .cursors()
            .advance("notify", Seq::new(1).unwrap())
            .await
            .unwrap();
        (store.log_identity(), enqueued.action.id)
    };

    let store = open(&dir, 1_700_000_100_000).await.unwrap();
    assert_eq!(store.log_identity().log_id, identity.log_id);
    assert_eq!(store.log_identity().log_epoch, 1);
    assert_eq!(store.events().head().await.unwrap().as_u64(), 4);
    let page = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 10)
        .await
        .unwrap();
    let types: Vec<EventType> = page.events.iter().map(|e| e.ty).collect();
    assert_eq!(
        types,
        vec![
            EventType::DeliveryCreated,
            EventType::MessagePosted,
            EventType::OutboxEnqueued,
            EventType::OutboxSucceeded
        ]
    );

    // 投影由重放得到；Outbox 行、游标、模块表从持久层载入。
    let snap = store.snapshot().await.unwrap();
    assert!(snap.delivery(&DELIVERY.parse().unwrap()).is_some());
    assert_eq!(
        snap.outbox_action(&action_id).unwrap().status,
        OutboxStatus::Succeeded
    );
    let reader = snap.module_conn("policy");
    assert_eq!(
        reader.get::<Value>("policy_challenges", "a/b/c").unwrap(),
        Some(json!({"nonce": "n1"}))
    );
    assert_eq!(
        reader.get::<Value>("policy_challenges", "a/b/d").unwrap(),
        None
    );
    assert_eq!(
        store.cursors().load("conversation").await.unwrap().as_u64(),
        2
    );
    assert_eq!(store.cursors().load("notify").await.unwrap().as_u64(), 1);

    // dedup_key 跨重启仍生效；seq 接着往后分配。
    let mut tx = store.begin_write().await.unwrap();
    assert!(matches!(
        tx.append(message_posted("msg-1")).await.unwrap(),
        Appended::Duplicate { existing_seq } if existing_seq.as_u64() == 2
    ));
    let Appended::New(next) = tx.append(message_posted("msg-2")).await.unwrap() else {
        panic!("expected a new event");
    };
    assert_eq!(next.seq.as_u64(), 5);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn rolled_back_and_uncommitted_writes_are_not_persisted() {
    let dir = Dir::new("rollback");
    {
        let store = open(&dir, 1_700_000_000_000).await.unwrap();
        let mut tx = store.begin_write().await.unwrap();
        tx.append(delivery_created()).await.unwrap();
        tx.rollback().await.unwrap();

        // 提交前"崩溃"：事务被丢弃。
        let mut tx = store.begin_write().await.unwrap();
        tx.append(delivery_created()).await.unwrap();
        tx.advance_cursor("conversation", Seq::new(1).unwrap())
            .await
            .unwrap();
        drop(tx);
    }
    let store = open(&dir, 1_700_000_100_000).await.unwrap();
    assert_eq!(store.events().head().await.unwrap().as_u64(), 0);
    assert_eq!(
        store.cursors().load("conversation").await.unwrap(),
        Seq::ZERO
    );
}

#[tokio::test]
async fn second_open_of_same_directory_is_rejected() {
    let dir = Dir::new("lock");
    let first = open(&dir, 1_700_000_000_000).await.unwrap();
    let second = open(&dir, 1_700_000_000_000).await;
    assert!(
        matches!(second, Err(StoreError::AlreadyRunning)),
        "{:?}",
        second.err()
    );
    drop(first);
    // 第一个实例关闭后可以再打开。
    open(&dir, 1_700_000_100_000).await.unwrap();
}

#[tokio::test]
async fn newer_schema_is_rejected() {
    let dir = Dir::new("schema");
    {
        let store = open(&dir, 1_700_000_000_000).await.unwrap();
        store
            .execute_raw_sql(&format!(
                "UPDATE meta SET value = '{}' WHERE key = 'schema_version'",
                SCHEMA_VERSION + 1
            ))
            .unwrap();
    }
    let error = open(&dir, 1_700_000_100_000).await.err().unwrap();
    assert!(
        matches!(error, StoreError::SchemaTooNew { found, supported }
            if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION),
        "{error:?}"
    );
}

#[tokio::test]
async fn events_table_is_append_only() {
    let dir = Dir::new("append-only");
    let store = open(&dir, 1_700_000_000_000).await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();

    for sql in [
        "UPDATE events SET type = 'x' WHERE seq = 1",
        "DELETE FROM events WHERE seq = 1",
    ] {
        let error = store.execute_raw_sql(sql).unwrap_err();
        assert!(
            error.to_string().contains("append-only"),
            "{sql}: {error:?}"
        );
    }
}

#[tokio::test]
async fn log_epoch_bump_is_persisted() {
    let dir = Dir::new("epoch");
    {
        let store = open(&dir, 1_700_000_000_000).await.unwrap();
        assert_eq!(store.restore_epoch_bump().log_epoch, 2);
    }
    let store = open(&dir, 1_700_000_100_000).await.unwrap();
    assert_eq!(store.log_identity().log_epoch, 2);
}

#[tokio::test]
async fn open_temp_removes_its_directory() {
    let store = Store::open_temp(deps(1_700_000_000_000), &[])
        .await
        .unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();
    drop(store);
}
