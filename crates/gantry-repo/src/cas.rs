//! 本模块用到的内容寻址存储（CAS）接缝。
//!
//! CAS 归 `store`（store.md §6、OQ-R9）。本模块仅使用 `get_bytes` / `put`；
//! `store::Cas` 实现此接缝，[`MemoryCas`] 供测试使用。

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use gantry_core::canon::{sha256, Sha256Digest};
use gantry_core::ports::{CasRef, RepoError};

/// store.md §6 的 `CasKind` 中本模块用到的两类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CasKind {
    /// 校验通过的补丁与校验记录。
    Artifact,
    /// 被拒补丁原文的隔离留存（repo.md §5、§8）。
    Quarantine,
}

#[async_trait]
pub trait PatchCas: Send + Sync {
    /// 读不超过 `max` 字节的对象；不存在 → `None`，超过上限 → 错误。
    async fn get_bytes(&self, digest: &CasRef, max: usize) -> Result<Option<Vec<u8>>, RepoError>;
    /// 写入并返回摘要；同一内容重复写入幂等，`Quarantine` 会把已有对象标为隔离。
    async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasRef, RepoError>;
}

#[async_trait]
impl PatchCas for gantry_store::Cas {
    async fn get_bytes(&self, digest: &CasRef, max: usize) -> Result<Option<Vec<u8>>, RepoError> {
        gantry_store::Cas::get_bytes(self, digest, max)
            .await
            .map_err(crate::store_error)
    }

    async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasRef, RepoError> {
        let kind = match kind {
            CasKind::Artifact => gantry_store::CasKind::Artifact,
            CasKind::Quarantine => gantry_store::CasKind::Quarantine,
        };
        gantry_store::Cas::put(self, bytes, kind)
            .await
            .map_err(crate::store_error)
    }
}

#[derive(Debug, Default)]
pub struct MemoryCas {
    objects: Mutex<BTreeMap<Sha256Digest, (Vec<u8>, bool)>>,
}

impl MemoryCas {
    pub fn new() -> Self {
        Self::default()
    }

    /// 对象是否在隔离区。
    pub fn is_quarantined(&self, digest: &CasRef) -> bool {
        self.lock()
            .get(digest)
            .is_some_and(|(_, quarantined)| *quarantined)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<Sha256Digest, (Vec<u8>, bool)>> {
        self.objects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl PatchCas for MemoryCas {
    async fn get_bytes(&self, digest: &CasRef, max: usize) -> Result<Option<Vec<u8>>, RepoError> {
        match self.lock().get(digest) {
            None => Ok(None),
            Some((bytes, _)) if bytes.len() > max => Err(RepoError::Io {
                message: format!("cas object exceeds {max} bytes"),
            }),
            Some((bytes, _)) => Ok(Some(bytes.clone())),
        }
    }

    async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasRef, RepoError> {
        let digest = sha256(bytes);
        let quarantine = kind == CasKind::Quarantine;
        let mut objects = self.lock();
        let entry = objects
            .entry(digest)
            .or_insert_with(|| (bytes.to_vec(), false));
        entry.1 |= quarantine;
        Ok(digest)
    }
}
