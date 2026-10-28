use std::sync::Arc;

use gantry_core::canon::sha256;
use gantry_core::ids::IdGen;
use gantry_core::ports::{RepoError, StoreError as PortStoreError};
use gantry_repo::cas::{CasKind, PatchCas};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Cas, CasKind as StoreCasKind, Store, StoreDeps};

fn deps() -> StoreDeps {
    let clock = Arc::new(FakeClock::new(1_800_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    StoreDeps { clock, ids }
}

fn rejected(error: RepoError, expected: &str) {
    match error {
        RepoError::Store(PortStoreError::Rejected { message }) => {
            assert!(message.contains(expected), "{message}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

async fn exercise(cas: &Cas) {
    let patch: &dyn PatchCas = cas;
    assert_eq!(patch.get_bytes(&sha256(b"missing"), 0).await.unwrap(), None);
    for (bytes, first, second, expected_kind) in [
        (
            &b"artifact first"[..],
            CasKind::Artifact,
            CasKind::Quarantine,
            StoreCasKind::Artifact,
        ),
        (
            &b"quarantine first"[..],
            CasKind::Quarantine,
            CasKind::Artifact,
            StoreCasKind::Quarantine,
        ),
    ] {
        let digest = patch.put(bytes, first).await.unwrap();
        assert_eq!(digest, sha256(bytes));
        assert_eq!(patch.put(bytes, first).await.unwrap(), digest);
        assert_eq!(patch.put(bytes, second).await.unwrap(), digest);
        assert_eq!(patch.put(bytes, CasKind::Artifact).await.unwrap(), digest);
        let stat = cas.stat(&digest).await.unwrap().unwrap();
        assert_eq!(stat.kind, expected_kind);
        assert!(stat.quarantined);
        assert_eq!(
            patch.get_bytes(&digest, bytes.len()).await.unwrap(),
            Some(bytes.to_vec())
        );
        rejected(
            patch.get_bytes(&digest, bytes.len() - 1).await.unwrap_err(),
            "exceeds",
        );
    }
    let too_large = vec![0; gantry_store::cas::DEFAULT_MAX_OBJECT_BYTES as usize + 1];
    rejected(
        patch.put(&too_large, CasKind::Artifact).await.unwrap_err(),
        "exceeds",
    );
    assert_eq!(patch.get_bytes(&sha256(&too_large), 0).await.unwrap(), None);
}

#[tokio::test]
async fn memory_store_adapter() {
    let store = Store::open_in_memory(deps(), &[]).await.unwrap();
    exercise(&store.cas()).await;
}

#[cfg(unix)]
mod disk {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf, u64, u64);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = std::env::temp_dir().canonicalize().unwrap();
            loop {
                let path = parent.join(format!(
                    "gantry-repo-store-cas-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => {
                        let meta = std::fs::symlink_metadata(&path).unwrap();
                        return Self(path, meta.dev(), meta.ino());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create test directory: {e}"),
                }
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
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

    #[tokio::test]
    async fn sqlite_adapter_reopen_and_errors() {
        let dir = TempDir::new();
        let cfg = || gantry_store::StoreConfig::new(&dir.0);
        let store = Store::open(cfg(), deps(), &[]).await.unwrap();
        exercise(&store.cas()).await;
        drop(store);

        let store = Store::open(cfg(), deps(), &[]).await.unwrap();
        let cas = store.cas();
        let patch: &dyn PatchCas = &cas;
        for (bytes, kind) in [
            (&b"artifact first"[..], StoreCasKind::Artifact),
            (&b"quarantine first"[..], StoreCasKind::Quarantine),
        ] {
            let digest = sha256(bytes);
            assert_eq!(
                patch.get_bytes(&digest, bytes.len()).await.unwrap(),
                Some(bytes.to_vec())
            );
            let stat = cas.stat(&digest).await.unwrap().unwrap();
            assert_eq!(stat.kind, kind);
            assert!(stat.quarantined);
        }

        let bytes = b"object to corrupt";
        let digest = patch.put(bytes, CasKind::Artifact).await.unwrap();
        let hex = digest.to_hex();
        let hex = hex.strip_prefix("sha256:").unwrap_or(&hex);
        let path = dir.0.join("cas/sha256").join(&hex[..2]).join(&hex[2..]);
        std::fs::write(&path, b"different bytes").unwrap();
        rejected(patch.get_bytes(&digest, 100).await.unwrap_err(), "corrupt");
        std::fs::remove_file(&path).unwrap();
        // 自引用符号链接在 Unix 上触发 ELOOP，不依赖目录读取行为。
        std::os::unix::fs::symlink(path.file_name().unwrap(), &path).unwrap();
        rejected(
            patch.get_bytes(&digest, usize::MAX).await.unwrap_err(),
            "io:",
        );
    }
}
