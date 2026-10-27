//! 尚无契约端口的依赖：CAS 与结构化输出 schema 注册表（契约缺口 1、6，见 #95 评论）。
//!
//! 这两个接口只在 sandbox 内部使用，不进 `core::ports`：store 交付 `Store::cas()`、
//! core 交付 schema 注册表之后，换成正式实现即可。内存实现只供测试。

use std::collections::BTreeMap;
use std::sync::Mutex;

use gantry_core::canon::{sha256, Sha256Digest};
use gantry_core::ports::sandbox::CasObject;

/// `CasKind`（store.md §6.4 [提议]）里本模块用到的两类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CasKind {
    Artifact,
    Progress,
}

pub trait ArtifactStore: Send + Sync {
    /// 写入不透明字节，返回服务端重算的摘要与大小。
    fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasObject, String>;
    /// 读出 `repo` 返回的 `diff.patch` 等对象。
    fn get(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, String>;
}

pub trait OutputSchemas: Send + Sync {
    /// 按 `output.structured_schema.id` 取 schema 字节。
    fn get(&self, id: &str) -> Option<Vec<u8>>;
}

/// 内存 CAS。
#[derive(Default)]
pub struct MemoryArtifacts {
    objects: Mutex<BTreeMap<Sha256Digest, (CasKind, Vec<u8>)>>,
}

impl MemoryArtifacts {
    pub fn insert(&self, bytes: &[u8]) -> Sha256Digest {
        let digest = sha256(bytes);
        self.objects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(digest, (CasKind::Artifact, bytes.to_vec()));
        digest
    }

    pub fn len(&self) -> usize {
        self.objects.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ArtifactStore for MemoryArtifacts {
    fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasObject, String> {
        let digest = sha256(bytes);
        self.objects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(digest)
            .or_insert_with(|| (kind, bytes.to_vec()));
        Ok(CasObject {
            sha256: digest,
            bytes: bytes.len() as u64,
        })
    }

    fn get(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(digest)
            .map(|(_, bytes)| bytes.clone()))
    }
}

/// 固定的 schema 表。
#[derive(Default)]
pub struct StaticSchemas(pub BTreeMap<String, Vec<u8>>);

impl OutputSchemas for StaticSchemas {
    fn get(&self, id: &str) -> Option<Vec<u8>> {
        self.0.get(id).cloned()
    }
}
