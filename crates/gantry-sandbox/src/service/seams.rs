//! sandbox 内部的 CAS 与结构化输出 schema 接缝。
//!
//! 这两个接口只在 sandbox 内部使用，不进 `core::ports`。CAS 可使用 `Store::cas()`；
//! 生产 schema 适配器使用 core 内置查询；内存实现供测试注入。

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use gantry_core::canon::{sha256, Sha256Digest};
use gantry_core::ports::sandbox::CasObject;

/// `CasKind`（store.md §6.4 [提议]）里本模块用到的两类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CasKind {
    Artifact,
    Progress,
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// 写入不透明字节，返回服务端重算的摘要与大小。
    async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasObject, String>;
    /// 读出 `repo` 返回的 `diff.patch` 等对象。
    async fn get(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, String>;
}

#[async_trait]
impl ArtifactStore for gantry_store::Cas {
    async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasObject, String> {
        let kind = match kind {
            CasKind::Artifact => gantry_store::CasKind::Artifact,
            CasKind::Progress => gantry_store::CasKind::Progress,
        };
        let sha256 = gantry_store::Cas::put(self, bytes, kind)
            .await
            .map_err(|err| err.to_string())?;
        Ok(CasObject {
            sha256,
            bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, String> {
        // Store 当前创建的 CAS 均使用这一上限；可配置时需重新对齐（OQ-S10）。
        gantry_store::Cas::get_bytes(
            self,
            digest,
            gantry_store::cas::DEFAULT_MAX_OBJECT_BYTES as usize,
        )
        .await
        .map_err(|err| err.to_string())
    }
}

pub trait OutputSchemas: Send + Sync {
    /// 按 `output.structured_schema.id` 取 schema 字节。
    fn get(&self, id: &str) -> Option<Vec<u8>>;
}

/// 委托 core 内置查询的生产 schema 适配器。
#[derive(Default)]
pub struct BuiltinSchemas;

impl OutputSchemas for BuiltinSchemas {
    fn get(&self, id: &str) -> Option<Vec<u8>> {
        gantry_core::worker::output_schema(id).map(<[u8]>::to_vec)
    }
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

#[async_trait]
impl ArtifactStore for MemoryArtifacts {
    async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<CasObject, String> {
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

    async fn get(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(digest)
            .map(|(_, bytes)| bytes.clone()))
    }
}

/// 测试注入用的固定 schema 表。
#[derive(Default)]
pub struct StaticSchemas(pub BTreeMap<String, Vec<u8>>);

impl OutputSchemas for StaticSchemas {
    fn get(&self, id: &str) -> Option<Vec<u8>> {
        self.0.get(id).cloned()
    }
}
