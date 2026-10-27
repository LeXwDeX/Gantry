//! 内容寻址存储（CAS，store.md §6.4、§7.7）。
//!
//! 本切片落地 `Cas` 的接口与语义，后端是内存（`open_in_memory` 的"CAS 放内存"，
//! store.md §6.1）。磁盘后端（`cas/sha256/ab/…` 文件 + `cas_objects` 行）随 SQLite
//! 切片落地，复用同一组一致性测试。
//!
//! 语义：
//! - 对象按 SHA-256 摘要寻址，内容不可变；同内容重复 `put` 只存一份，返回同一摘要。
//! - 读取只接受 [`Sha256Digest`]：没有调用方可控的路径片段。
//! - 单个对象不超过 `max_object_bytes`；`get_bytes` 的对象超过调用方的 `max` 时拒绝。
//! - `kind` 由首次写入决定；任何一次以 [`CasKind::Quarantine`] 写入都把对象标为
//!   隔离，且不会被撤销（被拒产物不能经"再以 Artifact 写一次"洗白）。
//! - `gc` 的默认策略是不删除（OQ-5）。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use gantry_core::canon::{jcs, sha256, Sha256Digest};
use gantry_core::ids::Clock;
use gantry_core::time::Timestamp;
use gantry_core::worker::ResultBundle;

use crate::StoreError;

/// `store.cas.max_object_bytes` 的默认值（store.md §10）。
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// 对象类别（store.md §6.4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CasKind {
    ResultBundle,
    Artifact,
    Progress,
    Quarantine,
}

/// `stat` 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CasStat {
    pub bytes: u64,
    pub kind: CasKind,
    pub created_at: Timestamp,
    pub quarantined: bool,
}

/// 保留策略。默认不删除任何对象（OQ-5）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetentionPolicy {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub scanned: u64,
    pub deleted: u64,
}

/// `open` 返回的读取器。
pub struct CasReader {
    inner: std::io::Cursor<Arc<[u8]>>,
}

impl Read for CasReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

struct Object {
    bytes: Arc<[u8]>,
    stat: CasStat,
}

/// 内容寻址存储句柄；克隆共享同一份对象。
#[derive(Clone)]
pub struct Cas {
    objects: Arc<Mutex<BTreeMap<Sha256Digest, Object>>>,
    clock: Arc<dyn Clock>,
    max_object_bytes: u64,
}

impl Cas {
    pub(crate) fn in_memory(clock: Arc<dyn Clock>, max_object_bytes: u64) -> Self {
        Self {
            objects: Arc::new(Mutex::new(BTreeMap::new())),
            clock,
            max_object_bytes,
        }
    }

    pub async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<Sha256Digest, StoreError> {
        self.check_size(bytes.len() as u64)?;
        let digest = sha256(bytes);
        let mut objects = self.objects.lock().map_err(|_| StoreError::Poisoned)?;
        match objects.get_mut(&digest) {
            Some(existing) => {
                if kind == CasKind::Quarantine {
                    existing.stat.quarantined = true;
                }
            }
            None => {
                objects.insert(
                    digest,
                    Object {
                        bytes: Arc::from(bytes),
                        stat: CasStat {
                            bytes: bytes.len() as u64,
                            kind,
                            created_at: self.clock.now(),
                            quarantined: kind == CasKind::Quarantine,
                        },
                    },
                );
            }
        }
        Ok(digest)
    }

    /// 流式读入文件；超过 `max_object_bytes` 时在读到上限后即停止并拒绝。
    pub async fn put_file(&self, path: &Path, kind: CasKind) -> Result<Sha256Digest, StoreError> {
        let file = std::fs::File::open(path).map_err(io_error)?;
        let mut bytes = Vec::new();
        file.take(self.max_object_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        self.put(&bytes, kind).await
    }

    pub async fn open(&self, digest: &Sha256Digest) -> Result<Option<CasReader>, StoreError> {
        let objects = self.objects.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(objects.get(digest).map(|object| CasReader {
            inner: std::io::Cursor::new(object.bytes.clone()),
        }))
    }

    /// 读取不超过 `max` 字节的对象；不存在 → `None`，超过 → `CasTooLarge`。
    pub async fn get_bytes(
        &self,
        digest: &Sha256Digest,
        max: usize,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let objects = self.objects.lock().map_err(|_| StoreError::Poisoned)?;
        let Some(object) = objects.get(digest) else {
            return Ok(None);
        };
        if object.bytes.len() > max {
            return Err(StoreError::CasTooLarge {
                bytes: object.bytes.len() as u64,
                max: max as u64,
            });
        }
        // verify_on_read：内存后端不会损坏，但磁盘后端会；保持同一个检查点。
        if sha256(&object.bytes) != *digest {
            return Err(StoreError::Corrupt(format!(
                "cas object {} does not match its digest",
                digest.to_hex()
            )));
        }
        Ok(Some(object.bytes.to_vec()))
    }

    pub async fn has(&self, digest: &Sha256Digest) -> Result<bool, StoreError> {
        let objects = self.objects.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(objects.contains_key(digest))
    }

    pub async fn stat(&self, digest: &Sha256Digest) -> Result<Option<CasStat>, StoreError> {
        let objects = self.objects.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(objects.get(digest).map(|object| object.stat.clone()))
    }

    /// 结果包清单：JCS 规范化后存入，键序不同的同一清单得到同一摘要
    /// （worker-protocol §3.5）。
    pub async fn put_bundle(&self, bundle: &ResultBundle) -> Result<Sha256Digest, StoreError> {
        let value = serde_json::to_value(bundle)
            .map_err(|error| StoreError::Corrupt(format!("result bundle: {error}")))?;
        let bytes =
            jcs(&value).map_err(|error| StoreError::Corrupt(format!("result bundle: {error}")))?;
        self.put(&bytes, CasKind::ResultBundle).await
    }

    /// 默认策略不删除（OQ-5）；只报告扫描数。
    pub async fn gc(&self, _policy: &RetentionPolicy) -> Result<GcReport, StoreError> {
        let objects = self.objects.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(GcReport {
            scanned: objects.len() as u64,
            deleted: 0,
        })
    }

    fn check_size(&self, bytes: u64) -> Result<(), StoreError> {
        if bytes > self.max_object_bytes {
            return Err(StoreError::CasTooLarge {
                bytes,
                max: self.max_object_bytes,
            });
        }
        Ok(())
    }
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Io(error.to_string())
}
