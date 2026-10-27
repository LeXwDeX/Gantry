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
    assert_eq!(store.events().head().await.unwrap().as_u64(), 5);
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
            EventType::OutboxExecuting,
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
    assert_eq!(next.seq.as_u64(), 6);
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

// ---------------------------------------------------------------------------
// CAS 磁盘后端（store.md §7.7、§12.5）
// ---------------------------------------------------------------------------

fn object_file(dir: &Dir, digest: &gantry_core::canon::Sha256Digest) -> PathBuf {
    let hex = digest.to_hex();
    dir.0
        .join("cas")
        .join("sha256")
        .join(&hex[..2])
        .join(&hex[2..])
}

#[tokio::test]
async fn cas_objects_live_on_disk_and_survive_reopen() {
    use gantry_store::CasKind;
    let dir = Dir::new("cas-disk");
    let digest = {
        let store = open(&dir, 1_700_000_000_000).await.unwrap();
        let digest = store.cas().put(b"patch", CasKind::Artifact).await.unwrap();
        store
            .cas()
            .put(b"patch", CasKind::Quarantine)
            .await
            .unwrap();
        digest
    };
    let path = object_file(&dir, &digest);
    assert_eq!(std::fs::read(&path).unwrap(), b"patch");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }
    // 临时目录里不留东西。
    assert_eq!(std::fs::read_dir(dir.0.join("cas/tmp")).unwrap().count(), 0);

    let store = open(&dir, 1_700_000_100_000).await.unwrap();
    let stat = store.cas().stat(&digest).await.unwrap().unwrap();
    assert_eq!(stat.kind, CasKind::Artifact);
    assert!(stat.quarantined);
    assert_eq!(stat.created_at.as_millis(), 1_700_000_000_000);
    assert_eq!(
        store.cas().get_bytes(&digest, 16).await.unwrap().as_deref(),
        Some(&b"patch"[..])
    );
}

/// 文件先于行：崩溃后的"有文件无行"孤儿经 `verify` 补行；损坏对象被报告、读取被拒。
#[tokio::test]
async fn cas_verify_adds_rows_for_orphans_and_reports_corruption() {
    use gantry_store::CasKind;
    let dir = Dir::new("cas-verify");
    let store = open(&dir, 1_700_000_000_000).await.unwrap();
    let good = store.cas().put(b"good", CasKind::Progress).await.unwrap();

    // 模拟"rename 之后、插行之前"崩溃：只有文件。
    let orphan = gantry_core::canon::sha256(b"orphan");
    let path = object_file(&dir, &orphan);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"orphan").unwrap();
    assert!(store.cas().has(&orphan).await.unwrap());
    assert!(store.cas().stat(&orphan).await.unwrap().is_none());

    // 损坏：文件内容与摘要不符。
    let damaged = store
        .cas()
        .put(b"damaged", CasKind::Artifact)
        .await
        .unwrap();
    std::fs::write(object_file(&dir, &damaged), b"tampered").unwrap();

    let report = store.cas().verify().await.unwrap();
    assert_eq!(report.files, 3);
    assert_eq!(report.rows_added, 1);
    assert_eq!(report.corrupt, vec![damaged]);
    assert_eq!(
        store.cas().stat(&orphan).await.unwrap().unwrap().kind,
        CasKind::Artifact
    );
    assert!(store.cas().stat(&good).await.unwrap().is_some());
    assert!(matches!(
        store.cas().get_bytes(&damaged, 64).await.unwrap_err(),
        StoreError::Corrupt(_)
    ));
    // `open` 对小对象做同样的校验，不能绕过 verify_on_read。
    assert!(matches!(
        store.cas().open(&damaged).await.err().unwrap(),
        StoreError::Corrupt(_)
    ));
    // 再跑一次是幂等的。
    assert_eq!(store.cas().verify().await.unwrap().rows_added, 0);
}

#[tokio::test]
async fn cas_put_file_over_limit_leaves_nothing_behind() {
    use gantry_store::CasKind;
    let dir = Dir::new("cas-limit");
    let store = open(&dir, 1_700_000_000_000).await.unwrap();
    let source = dir.0.join("big.bin");
    let file = std::fs::File::create(&source).unwrap();
    file.set_len(gantry_store::cas::DEFAULT_MAX_OBJECT_BYTES + 1)
        .unwrap();
    let error = store
        .cas()
        .put_file(&source, CasKind::Artifact)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::CasTooLarge { .. }), "{error:?}");
    assert_eq!(std::fs::read_dir(dir.0.join("cas/tmp")).unwrap().count(), 0);
    assert_eq!(
        store
            .cas()
            .gc(&gantry_store::RetentionPolicy::default())
            .await
            .unwrap()
            .scanned,
        0
    );
}

/// 迁移 n → n+1（§12.5）：旧版库（无 `cas_objects`）打开时补齐，版本号前进。
#[tokio::test]
async fn older_schema_is_migrated_forward() {
    let dir = Dir::new("migrate");
    {
        let store = open(&dir, 1_700_000_000_000).await.unwrap();
        let mut tx = store.begin_write().await.unwrap();
        tx.append(delivery_created()).await.unwrap();
        tx.commit().await.unwrap();
        store
            .execute_raw_sql(
                "DROP TABLE cas_objects; UPDATE meta SET value = '1' WHERE key = 'schema_version';",
            )
            .unwrap();
    }
    let store = open(&dir, 1_700_000_100_000).await.unwrap();
    assert_eq!(store.events().head().await.unwrap().as_u64(), 1);
    let digest = store
        .cas()
        .put(b"after migration", gantry_store::CasKind::Artifact)
        .await
        .unwrap();
    drop(store);
    // 版本若没前进，再次打开会重跑迁移并因 `cas_objects` 已存在而失败。
    let store = open(&dir, 1_700_000_200_000).await.unwrap();
    assert!(store.cas().stat(&digest).await.unwrap().is_some());
}

/// 崩溃留下的 `cas/tmp` 半成品在下次打开时清掉，不会与新临时文件撞名（§9）。
#[tokio::test]
async fn cas_tmp_leftovers_are_cleared_on_open() {
    let dir = Dir::new("cas-tmp");
    drop(open(&dir, 1_700_000_000_000).await.unwrap());
    let tmp = dir.0.join("cas/tmp");
    std::fs::write(tmp.join(format!("{}-0", std::process::id())), b"half").unwrap();
    std::fs::write(tmp.join("stray"), b"half").unwrap();

    let store = open(&dir, 1_700_000_100_000).await.unwrap();
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    store
        .cas()
        .put(b"fresh", gantry_store::CasKind::Artifact)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 备份、恢复与 verify（store.md §7.8、§12.5）
// ---------------------------------------------------------------------------

async fn append_delivery_and_messages(store: &Store, from: u32, to: u32) {
    let mut tx = store.begin_write().await.unwrap();
    for n in from..to {
        tx.append(message_posted(&format!("msg-{n}")))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn backup_restores_to_a_committed_prefix_with_bumped_epoch() {
    use gantry_store::{CasKind, VerifyLevel};
    let dir = Dir::new("backup-src");
    let restored = Dir::new("backup-dst");
    let backups = Dir::new("backup-files");
    let store = open(&dir, 1_700_000_000_000).await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.commit().await.unwrap();
    append_delivery_and_messages(&store, 0, 5).await;
    store
        .cas()
        .put(b"bundle", CasKind::ResultBundle)
        .await
        .unwrap();

    // 备份期间写者继续写：副本是某次提交之后的完整前缀。
    let writer = {
        let store = store.clone();
        tokio::spawn(async move { append_delivery_and_messages(&store, 5, 25).await })
    };
    let report = store.backup_to(&backups.0).await.unwrap();
    writer.await.unwrap();
    assert!(report.head_seq.as_u64() >= 6);
    assert_eq!(report.events, report.head_seq.as_u64());
    assert_eq!(report.cas_objects, 1);
    assert_eq!(report.log_epoch, 1);
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&report.manifest).unwrap()).unwrap();
    assert_eq!(manifest["head_seq"], json!(report.head_seq.as_u64()));
    assert_eq!(manifest["log_id"], json!(report.log_id));
    drop(store);

    let identity = Store::restore_from(&report.db, &restored.0).unwrap();
    assert_eq!(identity.log_id, report.log_id);
    assert_eq!(identity.log_epoch, 2);
    let back = open(&restored, 1_700_000_100_000).await.unwrap();
    assert_eq!(back.log_identity(), identity);
    assert_eq!(back.events().head().await.unwrap(), report.head_seq);
    assert!(back
        .snapshot()
        .await
        .unwrap()
        .delivery(&DELIVERY.parse().unwrap())
        .is_some());
    let full = back.verify(VerifyLevel::Full).await.unwrap();
    assert!(full.is_ok(), "{:?}", full.problems);

    // 恢复到原目录（原库 epoch 1，副本 epoch 1）同样 +1；目录有实例在跑时拒绝。
    let running = open(&dir, 1_700_000_200_000).await.unwrap();
    assert!(matches!(
        Store::restore_from(&report.db, &dir.0),
        Err(StoreError::AlreadyRunning)
    ));
    drop(running);
    let again = Store::restore_from(&report.db, &dir.0).unwrap();
    assert_eq!(again.log_epoch, 2);
    let reopened = open(&dir, 1_700_000_300_000).await.unwrap();
    assert_eq!(reopened.events().head().await.unwrap(), report.head_seq);
}

#[tokio::test]
async fn corrupt_backup_is_refused() {
    let backups = Dir::new("backup-bad");
    let target = Dir::new("backup-bad-dst");
    std::fs::create_dir_all(&backups.0).unwrap();
    let bogus = backups.0.join("gantry-1.db");
    std::fs::write(&bogus, b"not a database").unwrap();
    let error = Store::restore_from(&bogus, &target.0).unwrap_err();
    assert!(
        matches!(error, StoreError::Corrupt(_) | StoreError::Sqlite(_)),
        "{error:?}"
    );
    assert!(!target.0.join("gantry.db").exists());
}

#[tokio::test]
async fn verify_reports_divergence_between_disk_and_memory() {
    use gantry_store::VerifyLevel;
    let dir = Dir::new("verify");
    let store = open(&dir, 1_700_000_000_000).await.unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.module_conn("policy")
        .put("policy_challenges", "a/b/c", &json!({"nonce": "n1"}))
        .unwrap();
    tx.commit().await.unwrap();

    assert!(store.verify(VerifyLevel::Quick).await.unwrap().is_ok());
    let full = store.verify(VerifyLevel::Full).await.unwrap();
    assert!(full.is_ok(), "{:?}", full.problems);
    assert!(full.cas.is_some());

    // 绕过 store 改磁盘：Quick 发现 projected_seq 不符，Full 发现模块表不一致。
    store
        .execute_raw_sql(
            "UPDATE meta SET value = '7' WHERE key = 'projected_seq';
             UPDATE policy_challenges SET value = '{\"nonce\":\"forged\"}' WHERE key = 'a/b/c';",
        )
        .unwrap();
    let quick = store.verify(VerifyLevel::Quick).await.unwrap();
    assert!(
        quick.problems.iter().any(|p| p.contains("projected_seq")),
        "{:?}",
        quick.problems
    );
    let full = store.verify(VerifyLevel::Full).await;
    // projected_seq 不符时重载本身就会报 Corrupt；修回后 Full 报出模块表差异。
    assert!(matches!(full, Err(StoreError::Corrupt(_))), "{full:?}");
    store
        .execute_raw_sql("UPDATE meta SET value = '1' WHERE key = 'projected_seq';")
        .unwrap();
    let full = store.verify(VerifyLevel::Full).await.unwrap();
    assert_eq!(
        full.problems,
        vec!["module tables differs from a rebuild".to_owned()]
    );
}
