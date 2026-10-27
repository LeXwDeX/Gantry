#![cfg(feature = "testing")]

//! CAS（store.md §6.4、§7.7、§12.5）。只经 `Store::cas()` 的公开接口；磁盘后端落地后
//! 对同一组用例再跑一遍。"有文件无行经 verify 补行"属于磁盘后端，随 SQLite 切片补。

use std::io::Read;
use std::sync::Arc;

use gantry_core::canon::{sha256, Sha256Digest};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::worker::ResultBundle;
use gantry_store::{CasKind, RetentionPolicy, Store, StoreDeps, StoreError};
use serde_json::Value;

const NOW_MS: i64 = 1_700_000_000_000;
const BUNDLE: &str = include_str!("../../gantry-core/fixtures/protocol/worker/result_bundle.json");

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW_MS));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(3)),
    ));
    gantry_store::testing::open_for_tests(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

#[tokio::test]
async fn put_is_content_addressed_and_deduplicated() {
    let store = store().await;
    let cas = store.cas();
    let a = cas.put(b"hello", CasKind::Artifact).await.unwrap();
    let b = cas.put(b"hello", CasKind::Artifact).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(a, sha256(b"hello"));
    assert!(cas.has(&a).await.unwrap());
    assert_eq!(
        cas.gc(&RetentionPolicy::default()).await.unwrap().scanned,
        1
    );

    let stat = cas.stat(&a).await.unwrap().unwrap();
    assert_eq!(stat.bytes, 5);
    assert_eq!(stat.kind, CasKind::Artifact);
    assert!(!stat.quarantined);
    assert_eq!(
        cas.get_bytes(&a, 5).await.unwrap().as_deref(),
        Some(&b"hello"[..])
    );

    let mut reader = cas.open(&a).await.unwrap().unwrap();
    let mut read = Vec::new();
    reader.read_to_end(&mut read).unwrap();
    assert_eq!(read, b"hello");
}

#[tokio::test]
async fn missing_object_reads_none() {
    let store = store().await;
    let cas = store.cas();
    let digest = sha256(b"never stored");
    assert!(!cas.has(&digest).await.unwrap());
    assert!(cas.get_bytes(&digest, 1024).await.unwrap().is_none());
    assert!(cas.stat(&digest).await.unwrap().is_none());
    assert!(cas.open(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn store_handles_share_one_cas() {
    let store = store().await;
    let digest = store.cas().put(b"shared", CasKind::Progress).await.unwrap();
    let clone = store.clone();
    assert!(clone.cas().has(&digest).await.unwrap());
}

#[tokio::test]
async fn concurrent_put_of_same_content_is_safe() {
    let store = store().await;
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let cas = store.cas();
        tasks.push(tokio::spawn(async move {
            cas.put(b"same bytes", CasKind::Artifact).await.unwrap()
        }));
    }
    let mut digests = Vec::new();
    for task in tasks {
        digests.push(task.await.unwrap());
    }
    assert!(digests.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(
        store.cas().gc(&RetentionPolicy::default()).await.unwrap(),
        gantry_store::GcReport {
            scanned: 1,
            deleted: 0
        }
    );
}

#[tokio::test]
async fn get_bytes_over_max_is_rejected() {
    let store = store().await;
    let cas = store.cas();
    let digest = cas.put(&[7u8; 100], CasKind::Artifact).await.unwrap();
    let error = cas.get_bytes(&digest, 99).await.unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::CasTooLarge {
                bytes: 100,
                max: 99
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        cas.get_bytes(&digest, 100).await.unwrap().unwrap().len(),
        100
    );
}

#[tokio::test]
async fn object_over_max_object_bytes_is_rejected() {
    let store = store().await;
    let cas = store.cas();
    let big = vec![0u8; gantry_store::cas::DEFAULT_MAX_OBJECT_BYTES as usize + 1];
    let error = cas.put(&big, CasKind::Artifact).await.unwrap_err();
    assert!(matches!(error, StoreError::CasTooLarge { .. }), "{error:?}");
    assert!(!cas.has(&sha256(&big)).await.unwrap());
}

#[test]
fn traversal_strings_are_not_digests() {
    for text in [
        "../../etc/passwd",
        "sha256:../../etc/passwd",
        "sha256:/abs",
        "sha256:5e8c9902207afaeb7120430c585a445f21e92932081d64bc99f80e4925bcb00",
        "sha256:5e8c9902207afaeb7120430c585a445f21e92932081d64bc99f80e4925bcb00/",
    ] {
        assert!(Sha256Digest::parse(text).is_err(), "{text}");
    }
}

#[tokio::test]
async fn quarantine_is_sticky() {
    let store = store().await;
    let cas = store.cas();
    let digest = cas.put(b"rejected patch", CasKind::Artifact).await.unwrap();
    cas.put(b"rejected patch", CasKind::Quarantine)
        .await
        .unwrap();
    cas.put(b"rejected patch", CasKind::Artifact).await.unwrap();
    let stat = cas.stat(&digest).await.unwrap().unwrap();
    assert!(stat.quarantined);
    // kind 由首次写入决定。
    assert_eq!(stat.kind, CasKind::Artifact);

    let fresh = cas
        .put(b"quarantined first", CasKind::Quarantine)
        .await
        .unwrap();
    let stat = cas.stat(&fresh).await.unwrap().unwrap();
    assert_eq!(stat.kind, CasKind::Quarantine);
    assert!(stat.quarantined);
}

#[tokio::test]
async fn put_file_streams_from_disk() {
    let store = store().await;
    let dir = std::env::temp_dir().join(format!("gantry-store-cas-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("changes.patch");
    std::fs::write(&path, b"diff --git a/x b/x\n").unwrap();
    let digest = store
        .cas()
        .put_file(&path, CasKind::Artifact)
        .await
        .unwrap();
    assert_eq!(digest, sha256(b"diff --git a/x b/x\n"));
    std::fs::remove_dir_all(&dir).unwrap();

    let error = store
        .cas()
        .put_file(&dir.join("missing"), CasKind::Artifact)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Io(_)), "{error:?}");
}

/// 键序不同但内容相同的清单得到同一摘要（worker-protocol §3.5）。
#[tokio::test]
async fn put_bundle_is_key_order_independent() {
    let store = store().await;
    let original: Value = serde_json::from_str(BUNDLE).unwrap();
    let reversed: Value = reverse_keys(&original);
    assert_ne!(
        serde_json::to_string(&original).unwrap(),
        serde_json::to_string(&reversed).unwrap()
    );
    let a: ResultBundle = serde_json::from_value(original).unwrap();
    let b: ResultBundle = serde_json::from_value(reversed).unwrap();
    let cas = store.cas();
    let da = cas.put_bundle(&a).await.unwrap();
    let db = cas.put_bundle(&b).await.unwrap();
    assert_eq!(da, db);
    assert_eq!(
        cas.stat(&da).await.unwrap().unwrap().kind,
        CasKind::ResultBundle
    );

    // 存入的是 JCS 规范化字节，可原样解回同一清单。
    let bytes = cas.get_bytes(&da, 1 << 20).await.unwrap().unwrap();
    let back: ResultBundle = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, a);
}

fn reverse_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, item) in map.iter().rev() {
                out.insert(key.clone(), reverse_keys(item));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(reverse_keys).collect()),
        other => other.clone(),
    }
}
