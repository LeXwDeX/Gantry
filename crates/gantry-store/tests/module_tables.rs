//! 模块私有表（store.md §7.10、§12.3）：同事务提交 / 回滚、模块隔离、前缀扫描。
#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind};
use gantry_core::events::{NewEvent, Subject};
use gantry_core::ids::IdGen;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{ModuleSpec, Store, StoreDeps, StoreError, MODULE_SCAN_MAX};
use serde::{Deserialize, Serialize};
use serde_json::json;

const POLICY: ModuleSpec = ModuleSpec::new("policy", &["policy_challenges", "policy_consumed"]);
const FORGE: ModuleSpec = ModuleSpec::new("forge", &["forge_webhook_inbox"]);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Challenge {
    nonce: String,
    used: bool,
}

fn deps() -> StoreDeps {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(3))));
    StoreDeps { clock, ids }
}

async fn store() -> Store {
    gantry_store::testing::open_for_tests(deps(), &[POLICY, FORGE])
        .await
        .unwrap()
}

const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");

fn delivery_created() -> NewEvent {
    let cases: serde_json::Value = serde_json::from_str(CASES).unwrap();
    NewEvent {
        ty: "delivery.created".parse().unwrap(),
        subject: Subject::parse("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB").unwrap(),
        project_id: Some("prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap()),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        dedup_key: None,
        data: cases["valid"]["delivery.created"].clone(),
    }
}

fn challenge(nonce: &str) -> Challenge {
    Challenge {
        nonce: nonce.to_owned(),
        used: false,
    }
}

#[tokio::test]
async fn module_rows_commit_with_events() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    let mut conn = tx.module_conn("policy");
    assert!(conn
        .insert("policy_challenges", "apr_1/dev_1/n1", &challenge("n1"))
        .unwrap());
    // 事务内能读到自己的写入。
    assert_eq!(
        conn.get::<Challenge>("policy_challenges", "apr_1/dev_1/n1")
            .unwrap(),
        Some(challenge("n1"))
    );
    // 提交前，快照看不到。
    let before = store.snapshot().await.unwrap();
    assert_eq!(
        before
            .module_conn("policy")
            .get::<Challenge>("policy_challenges", "apr_1/dev_1/n1")
            .unwrap(),
        None
    );
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.head_seq().as_u64(), 1);
    assert_eq!(
        snapshot
            .module_conn("policy")
            .get::<Challenge>("policy_challenges", "apr_1/dev_1/n1")
            .unwrap(),
        Some(challenge("n1"))
    );
    // 旧快照不受后续提交影响。
    assert_eq!(
        before
            .module_conn("policy")
            .get::<Challenge>("policy_challenges", "apr_1/dev_1/n1")
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn module_rows_roll_back_with_events() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(delivery_created()).await.unwrap();
    tx.module_conn("policy")
        .put("policy_consumed", "apr_1", &json!({"consumed_by": "act_x"}))
        .unwrap();
    tx.rollback().await.unwrap();

    // 丢弃（Drop）未提交的事务同样回滚。
    {
        let mut tx = store.begin_write().await.unwrap();
        tx.module_conn("policy")
            .put("policy_consumed", "apr_2", &json!({"consumed_by": "act_y"}))
            .unwrap();
    }

    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.head_seq().as_u64(), 0);
    let reader = snapshot.module_conn("policy");
    for key in ["apr_1", "apr_2"] {
        assert_eq!(
            reader
                .get::<serde_json::Value>("policy_consumed", key)
                .unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn insert_does_not_overwrite_put_and_delete_report_previous() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let mut conn = tx.module_conn("policy");
    assert!(conn
        .insert("policy_challenges", "k", &challenge("a"))
        .unwrap());
    assert!(!conn
        .insert("policy_challenges", "k", &challenge("b"))
        .unwrap());
    assert_eq!(
        conn.get::<Challenge>("policy_challenges", "k").unwrap(),
        Some(challenge("a"))
    );
    let used = Challenge {
        used: true,
        ..challenge("a")
    };
    assert!(conn.put("policy_challenges", "k", &used).unwrap());
    assert!(!conn.put("policy_challenges", "k2", &used).unwrap());
    assert_eq!(
        conn.get::<Challenge>("policy_challenges", "k").unwrap(),
        Some(used)
    );
    assert!(conn.delete("policy_challenges", "k").unwrap());
    assert!(!conn.delete("policy_challenges", "k").unwrap());
    assert_eq!(
        conn.get::<Challenge>("policy_challenges", "k").unwrap(),
        None
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn modules_cannot_touch_each_others_tables() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let mut conn = tx.module_conn("policy");
    for table in ["forge_webhook_inbox", "policy_unknown", "events"] {
        assert!(matches!(
            conn.put(table, "k", &1),
            Err(StoreError::PreconditionFailed(_))
        ));
        assert!(matches!(
            conn.get::<u32>(table, "k"),
            Err(StoreError::PreconditionFailed(_))
        ));
    }
    // 未注册的模块什么表都访问不了。
    assert!(matches!(
        tx.module_conn("pki").put("pki_devices", "k", &1),
        Err(StoreError::PreconditionFailed(_))
    ));
    tx.commit().await.unwrap();

    let snapshot = store.snapshot().await.unwrap();
    assert!(matches!(
        snapshot
            .module_conn("forge")
            .get::<u32>("policy_challenges", "k"),
        Err(StoreError::PreconditionFailed(_))
    ));
}

#[tokio::test]
async fn scan_is_ordered_prefix_bounded_and_keyset_paged() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let mut conn = tx.module_conn("policy");
    for key in [
        "apr_1/dev_2/c",
        "apr_1/dev_1/b",
        "apr_1/dev_1/a",
        "apr_10/dev_1/z",
        "apr_0/dev_1/y",
    ] {
        conn.put("policy_challenges", key, &challenge(key)).unwrap();
    }
    let keys = |rows: Vec<(String, Challenge)>| -> Vec<String> {
        rows.into_iter().map(|(key, _)| key).collect()
    };
    assert_eq!(
        keys(conn.scan("policy_challenges", "apr_1/", None, 10).unwrap()),
        ["apr_1/dev_1/a", "apr_1/dev_1/b", "apr_1/dev_2/c"]
    );
    assert_eq!(
        keys(
            conn.scan("policy_challenges", "apr_1/dev_1/", None, 10)
                .unwrap()
        ),
        ["apr_1/dev_1/a", "apr_1/dev_1/b"]
    );
    // 键集分页：after 之后（不含）。
    assert_eq!(
        keys(conn.scan("policy_challenges", "apr_1/", None, 2).unwrap()),
        ["apr_1/dev_1/a", "apr_1/dev_1/b"]
    );
    assert_eq!(
        keys(
            conn.scan("policy_challenges", "apr_1/", Some("apr_1/dev_1/b"), 2)
                .unwrap()
        ),
        ["apr_1/dev_2/c"]
    );
    // after 早于前缀时从前缀起扫。
    assert_eq!(
        keys(
            conn.scan("policy_challenges", "apr_1/", Some("a"), 1)
                .unwrap()
        ),
        ["apr_1/dev_1/a"]
    );
    // 空前缀 = 全表，按字节序。
    assert_eq!(
        keys(conn.scan("policy_challenges", "", None, 10).unwrap()),
        [
            "apr_0/dev_1/y",
            "apr_1/dev_1/a",
            "apr_1/dev_1/b",
            "apr_1/dev_2/c",
            "apr_10/dev_1/z"
        ]
    );
    for limit in [0, MODULE_SCAN_MAX + 1] {
        assert!(matches!(
            conn.scan::<Challenge>("policy_challenges", "", None, limit),
            Err(StoreError::PreconditionFailed(_))
        ));
    }
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn keys_and_values_are_validated() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    let mut conn = tx.module_conn("policy");
    for key in ["", "a\0b"] {
        assert!(matches!(
            conn.put("policy_challenges", key, &1),
            Err(StoreError::PreconditionFailed(_))
        ));
    }
    conn.put("policy_challenges", "k", &json!({"nonce": 7}))
        .unwrap();
    assert!(matches!(
        conn.get::<Challenge>("policy_challenges", "k"),
        Err(StoreError::ModuleValue(_))
    ));
}

#[tokio::test]
async fn invalid_specs_fail_open() {
    for spec in [
        ModuleSpec::new("policy", &["challenges"]),
        ModuleSpec::new("core", &["core_x"]),
    ] {
        assert!(matches!(
            gantry_store::testing::open_for_tests(deps(), &[spec]).await,
            Err(StoreError::InvalidModuleSpec(_))
        ));
    }
}
