//! sandbox 本地 CAS 接缝与 store 两种后端的适配。
use std::sync::Arc;

use gantry_core::canon::sha256;
use gantry_core::ids::IdGen;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_sandbox::service::seams::{ArtifactStore, CasKind};
use gantry_store::{Store, StoreDeps};

fn deps() -> StoreDeps {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(3))));
    StoreDeps { clock, ids }
}

async fn check_backend(store: &Store) {
    let cas = store.cas();
    for (bytes, kind, expected) in [
        (
            &b"artifact bytes"[..],
            CasKind::Artifact,
            gantry_store::CasKind::Artifact,
        ),
        (
            &b"progress bytes"[..],
            CasKind::Progress,
            gantry_store::CasKind::Progress,
        ),
    ] {
        let object = ArtifactStore::put(&cas, bytes, kind).await.unwrap();
        assert_eq!(object.sha256, sha256(bytes));
        assert_eq!(object.bytes, bytes.len() as u64);
        assert_eq!(ArtifactStore::put(&cas, bytes, kind).await.unwrap(), object);
        assert_eq!(
            ArtifactStore::get(&cas, &object.sha256)
                .await
                .unwrap()
                .unwrap(),
            bytes
        );
        assert_eq!(
            store
                .cas()
                .stat(&object.sha256)
                .await
                .unwrap()
                .unwrap()
                .kind,
            expected
        );
    }
    assert!(ArtifactStore::get(&cas, &sha256(b"missing"))
        .await
        .unwrap()
        .is_none());
    let oversized = vec![0; gantry_store::cas::DEFAULT_MAX_OBJECT_BYTES as usize + 1];
    let error = ArtifactStore::put(&cas, &oversized, CasKind::Artifact)
        .await
        .unwrap_err();
    assert!(error.contains("exceeds"), "{error}");
    assert!(!cas.has(&sha256(&oversized)).await.unwrap());
}

#[tokio::test]
async fn memory_adapter() {
    let store = Store::open_in_memory(deps(), &[]).await.unwrap();
    check_backend(&store).await;
}

#[tokio::test]
async fn sqlite_adapter() {
    let store = Store::open_temp(deps(), &[]).await.unwrap();
    check_backend(&store).await;
    store.execute_raw_sql("CREATE TRIGGER fail_cas BEFORE INSERT ON cas_objects BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;").unwrap();
    let err = ArtifactStore::put(&store.cas(), b"write error", CasKind::Artifact)
        .await
        .unwrap_err();
    assert!(err.contains("injected write failure"), "{err}");
}

#[cfg(unix)]
struct OwnedDir(std::path::PathBuf, u64, u64);

#[cfg(unix)]
impl OwnedDir {
    fn new() -> Self {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().canonicalize().unwrap();
        loop {
            let path = parent.join(format!(
                "gantry-sandbox-cas-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let meta = std::fs::symlink_metadata(&path).unwrap();
                    return Self(path, meta.dev(), meta.ino());
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => panic!("create test directory: {err}"),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for OwnedDir {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;

        if let Ok(meta) = std::fs::symlink_metadata(&self.0) {
            if meta.is_dir()
                && !meta.file_type().is_symlink()
                && (meta.dev(), meta.ino()) == (self.1, self.2)
            {
                std::fs::remove_dir_all(&self.0).expect("remove owned test directory");
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_read_errors_propagate() {
    let dir = OwnedDir::new();
    let store = Store::open(gantry_store::StoreConfig::new(&dir.0), deps(), &[])
        .await
        .unwrap();
    let cas = store.cas();
    let object = ArtifactStore::put(&cas, b"original", CasKind::Artifact)
        .await
        .unwrap();
    let hex = object.sha256.to_hex();
    let hex = hex.strip_prefix("sha256:").unwrap_or(&hex);
    let path = dir.0.join("cas/sha256").join(&hex[..2]).join(&hex[2..]);
    std::fs::write(&path, b"corrupt").unwrap();
    let error = ArtifactStore::get(&cas, &object.sha256).await.unwrap_err();
    assert!(error.contains("corrupt"), "{error}");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(gantry_store::cas::DEFAULT_MAX_OBJECT_BYTES + 1)
        .unwrap();
    let error = ArtifactStore::get(&cas, &object.sha256).await.unwrap_err();
    assert!(error.contains("exceeds"), "{error}");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(path.file_name().unwrap(), &path).unwrap();
    let error = ArtifactStore::get(&cas, &object.sha256).await.unwrap_err();
    assert!(error.contains("io:"), "{error}");
    drop(cas);
    drop(store);
}
