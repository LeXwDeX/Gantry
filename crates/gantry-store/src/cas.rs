//! 内容寻址存储（CAS，store.md §6.4、§7.7）。
//!
//! 两个后端，接口与语义相同，共用 `tests/cas.rs`：
//! - 内存：`open_in_memory`（"CAS 放内存"，store.md §6.1）。
//! - 磁盘：`Store::open`。对象文件在 `<dir>/cas/sha256/<前两位>/<其余 62 位>`，元数据在
//!   SQLite 的 `cas_objects` 表。写入顺序是"临时文件 → fsync → rename → 插行"：崩溃后
//!   可能有"有文件无行"的孤儿，[`Cas::verify`] 补行；不会出现"有行无文件"。
//!
//! 语义：
//! - 对象按 SHA-256 摘要寻址，内容不可变；同内容重复 `put` 只存一份，返回同一摘要。
//! - 读取只接受 [`Sha256Digest`]：路径由摘要拼出，没有调用方可控的路径片段。
//! - 单个对象不超过 `max_object_bytes`；`get_bytes` 的对象超过调用方的 `max` 时拒绝。
//!   小于 `verify_on_read_below_bytes` 的对象读取时重算摘要，不符返回 `Corrupt`。
//! - `kind` 由首次写入决定；任何一次以 [`CasKind::Quarantine`] 写入都把对象标为
//!   隔离，且不会被撤销（被拒产物不能经"再以 Artifact 写一次"洗白）。
//! - `gc` 的默认策略是不删除（OQ-5）。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gantry_core::canon::{jcs, sha256, Sha256Digest};
use gantry_core::ids::Clock;
use gantry_core::time::Timestamp;
use gantry_core::worker::ResultBundle;
use sha2::{Digest, Sha256};

use crate::durable::Durable;
use crate::StoreError;

/// `store.cas.max_object_bytes` 的默认值（store.md §10）。
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// `store.cas.verify_on_read_below_bytes` 的默认值（store.md §10）。
pub const DEFAULT_VERIFY_ON_READ_BELOW_BYTES: u64 = 1024 * 1024;

/// 对象类别（store.md §6.4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CasKind {
    ResultBundle,
    Artifact,
    Progress,
    Quarantine,
}

impl CasKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CasKind::ResultBundle => "result_bundle",
            CasKind::Artifact => "artifact",
            CasKind::Progress => "progress",
            CasKind::Quarantine => "quarantine",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "result_bundle" => CasKind::ResultBundle,
            "artifact" => CasKind::Artifact,
            "progress" => CasKind::Progress,
            "quarantine" => CasKind::Quarantine,
            _ => return None,
        })
    }
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

/// [`Cas::verify`] 的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CasVerifyReport {
    /// 扫描到的对象文件数。
    pub files: u64,
    /// 为"有文件无行"的孤儿补上的行数。
    pub rows_added: u64,
    /// 内容与文件名摘要不符的对象。
    pub corrupt: Vec<Sha256Digest>,
}

/// `open` 返回的读取器。
pub struct CasReader {
    inner: ReaderInner,
}

enum ReaderInner {
    Memory(std::io::Cursor<Arc<[u8]>>),
    File(File),
}

impl Read for CasReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            ReaderInner::Memory(cursor) => cursor.read(buf),
            ReaderInner::File(file) => file.read(buf),
        }
    }
}

struct Object {
    bytes: Arc<[u8]>,
    stat: CasStat,
}

#[derive(Clone)]
enum Backend {
    Memory(Arc<Mutex<BTreeMap<Sha256Digest, Object>>>),
    Disk {
        root: PathBuf,
        durable: Arc<Durable>,
    },
}

/// 内容寻址存储句柄；克隆共享同一份对象。
#[derive(Clone)]
pub struct Cas {
    backend: Backend,
    clock: Arc<dyn Clock>,
    max_object_bytes: u64,
    verify_below: u64,
}

impl Cas {
    pub(crate) fn in_memory(clock: Arc<dyn Clock>, max_object_bytes: u64) -> Self {
        Self {
            backend: Backend::Memory(Arc::new(Mutex::new(BTreeMap::new()))),
            clock,
            max_object_bytes,
            verify_below: DEFAULT_VERIFY_ON_READ_BELOW_BYTES,
        }
    }

    pub(crate) fn on_disk(
        root: PathBuf,
        durable: Arc<Durable>,
        clock: Arc<dyn Clock>,
        max_object_bytes: u64,
    ) -> Result<Self, StoreError> {
        create_dir(&root.join("sha256"))?;
        create_dir(&root.join("tmp"))?;
        // 调用方已持有数据目录的排他锁：`tmp/` 里的都是上次崩溃留下的半成品（§9），清掉。
        for leftover in read_dir_sorted(&root.join("tmp"))? {
            std::fs::remove_file(&leftover).map_err(io_error)?;
        }
        Ok(Self {
            backend: Backend::Disk { root, durable },
            clock,
            max_object_bytes,
            verify_below: DEFAULT_VERIFY_ON_READ_BELOW_BYTES,
        })
    }

    pub async fn put(&self, bytes: &[u8], kind: CasKind) -> Result<Sha256Digest, StoreError> {
        self.check_size(bytes.len() as u64)?;
        let digest = sha256(bytes);
        match &self.backend {
            Backend::Memory(objects) => {
                let mut objects = objects.lock().map_err(|_| StoreError::Poisoned)?;
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
                                stat: self.new_stat(bytes.len() as u64, kind),
                            },
                        );
                    }
                }
            }
            Backend::Disk { root, durable } => {
                let target = object_path(root, &digest);
                if !target.exists() {
                    let (tmp, mut file) = temp_file(root)?;
                    let written = file
                        .write_all(bytes)
                        .and_then(|()| file.sync_all())
                        .map_err(io_error);
                    drop(file);
                    if let Err(error) = written {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(error);
                    }
                    publish(&tmp, &target)?;
                }
                durable.cas_record(&digest, &self.new_stat(bytes.len() as u64, kind))?;
            }
        }
        Ok(digest)
    }

    /// 流式读入文件：边读边算摘要；超过 `max_object_bytes` 时立即停止并拒绝。
    pub async fn put_file(&self, path: &Path, kind: CasKind) -> Result<Sha256Digest, StoreError> {
        let mut source = File::open(path).map_err(io_error)?;
        match &self.backend {
            Backend::Memory(_) => {
                let mut bytes = Vec::new();
                (&mut source)
                    .take(self.max_object_bytes.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(io_error)?;
                self.put(&bytes, kind).await
            }
            Backend::Disk { root, durable } => {
                let (tmp, mut file) = temp_file(root)?;
                let streamed = stream_into(&mut source, &mut file, self.max_object_bytes)
                    .and_then(|done| file.sync_all().map(|()| done).map_err(io_error));
                drop(file);
                let (digest, len) = match streamed {
                    Ok(done) => done,
                    Err(error) => {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(error);
                    }
                };
                let target = object_path(root, &digest);
                if target.exists() {
                    let _ = std::fs::remove_file(&tmp);
                } else {
                    publish(&tmp, &target)?;
                }
                durable.cas_record(&digest, &self.new_stat(len, kind))?;
                Ok(digest)
            }
        }
    }

    pub async fn open(&self, digest: &Sha256Digest) -> Result<Option<CasReader>, StoreError> {
        match &self.backend {
            Backend::Memory(objects) => {
                let objects = objects.lock().map_err(|_| StoreError::Poisoned)?;
                Ok(objects.get(digest).map(|object| CasReader {
                    inner: ReaderInner::Memory(std::io::Cursor::new(object.bytes.clone())),
                }))
            }
            Backend::Disk { root, .. } => {
                let file = match File::open(object_path(root, digest)) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(io_error(error)),
                };
                let len = file.metadata().map_err(io_error)?.len();
                if len >= self.verify_below {
                    return Ok(Some(CasReader {
                        inner: ReaderInner::File(file),
                    }));
                }
                // 小对象与 `get_bytes` 一样先校验摘要，读取接口不能绕过 verify_on_read。
                let mut bytes = Vec::with_capacity(len as usize);
                (&file).read_to_end(&mut bytes).map_err(io_error)?;
                if sha256(&bytes) != *digest {
                    return Err(StoreError::Corrupt(format!(
                        "cas object {} does not match its digest",
                        digest.to_hex()
                    )));
                }
                Ok(Some(CasReader {
                    inner: ReaderInner::Memory(std::io::Cursor::new(Arc::from(bytes))),
                }))
            }
        }
    }

    /// 读取不超过 `max` 字节的对象；不存在 → `None`，超过 → `CasTooLarge`。
    pub async fn get_bytes(
        &self,
        digest: &Sha256Digest,
        max: usize,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let bytes: Vec<u8> = match &self.backend {
            Backend::Memory(objects) => {
                let objects = objects.lock().map_err(|_| StoreError::Poisoned)?;
                let Some(object) = objects.get(digest) else {
                    return Ok(None);
                };
                check_max(object.bytes.len() as u64, max)?;
                object.bytes.to_vec()
            }
            Backend::Disk { root, .. } => {
                let path = object_path(root, digest);
                let len = match std::fs::metadata(&path) {
                    Ok(meta) => meta.len(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(io_error(error)),
                };
                check_max(len, max)?;
                std::fs::read(&path).map_err(io_error)?
            }
        };
        if (bytes.len() as u64) < self.verify_below && sha256(&bytes) != *digest {
            return Err(StoreError::Corrupt(format!(
                "cas object {} does not match its digest",
                digest.to_hex()
            )));
        }
        Ok(Some(bytes))
    }

    pub async fn has(&self, digest: &Sha256Digest) -> Result<bool, StoreError> {
        match &self.backend {
            Backend::Memory(objects) => {
                let objects = objects.lock().map_err(|_| StoreError::Poisoned)?;
                Ok(objects.contains_key(digest))
            }
            Backend::Disk { root, .. } => Ok(object_path(root, digest).exists()),
        }
    }

    /// 元数据。磁盘后端读 `cas_objects` 行；"有文件无行"的孤儿在 `verify` 补行前返回 `None`。
    pub async fn stat(&self, digest: &Sha256Digest) -> Result<Option<CasStat>, StoreError> {
        match &self.backend {
            Backend::Memory(objects) => {
                let objects = objects.lock().map_err(|_| StoreError::Poisoned)?;
                Ok(objects.get(digest).map(|object| object.stat.clone()))
            }
            Backend::Disk { durable, .. } => durable.cas_stat(digest),
        }
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

    /// 默认策略不删除（OQ-5）；只报告对象数。
    pub async fn gc(&self, _policy: &RetentionPolicy) -> Result<GcReport, StoreError> {
        let scanned = match &self.backend {
            Backend::Memory(objects) => {
                objects.lock().map_err(|_| StoreError::Poisoned)?.len() as u64
            }
            Backend::Disk { durable, .. } => durable.cas_count()?,
        };
        Ok(GcReport {
            scanned,
            deleted: 0,
        })
    }

    /// 完整性检查（store.md §7.7、§7.8）：遍历对象文件，重算摘要，为"有文件无行"的孤儿补行
    /// （类别记为 `Artifact`，创建时间取当前时钟）。不删除也不修复损坏对象，只报告。
    /// 内存后端没有孤儿，只重算摘要。
    pub async fn verify(&self) -> Result<CasVerifyReport, StoreError> {
        let mut report = CasVerifyReport::default();
        match &self.backend {
            Backend::Memory(objects) => {
                let objects = objects.lock().map_err(|_| StoreError::Poisoned)?;
                for (digest, object) in objects.iter() {
                    report.files += 1;
                    if sha256(&object.bytes) != *digest {
                        report.corrupt.push(*digest);
                    }
                }
            }
            Backend::Disk { root, durable } => {
                for prefix in read_dir_sorted(&root.join("sha256"))? {
                    for file in read_dir_sorted(&prefix)? {
                        let name = format!(
                            "sha256:{}{}",
                            prefix.file_name().unwrap_or_default().to_string_lossy(),
                            file.file_name().unwrap_or_default().to_string_lossy()
                        );
                        // 名字不是合法摘要的文件不是 store 写的，跳过不碰。
                        let Ok(digest) = Sha256Digest::parse(&name) else {
                            continue;
                        };
                        report.files += 1;
                        let mut source = File::open(&file).map_err(io_error)?;
                        let (actual, len) =
                            stream_into(&mut source, &mut std::io::sink(), u64::MAX)?;
                        if actual != digest {
                            report.corrupt.push(digest);
                            continue;
                        }
                        if durable.cas_stat(&digest)?.is_none() {
                            durable.cas_record(&digest, &self.new_stat(len, CasKind::Artifact))?;
                            report.rows_added += 1;
                        }
                    }
                }
            }
        }
        Ok(report)
    }

    fn new_stat(&self, bytes: u64, kind: CasKind) -> CasStat {
        CasStat {
            bytes,
            kind,
            created_at: self.clock.now(),
            quarantined: kind == CasKind::Quarantine,
        }
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

fn check_max(len: u64, max: usize) -> Result<(), StoreError> {
    if len > max as u64 {
        return Err(StoreError::CasTooLarge {
            bytes: len,
            max: max as u64,
        });
    }
    Ok(())
}

/// `<root>/sha256/<前两位>/<其余 62 位>`：完全由摘要决定。
fn object_path(root: &Path, digest: &Sha256Digest) -> PathBuf {
    let hex = digest.to_hex();
    let hex = hex.strip_prefix("sha256:").unwrap_or(&hex);
    root.join("sha256").join(&hex[..2]).join(&hex[2..])
}

fn temp_file(root: &Path) -> Result<(PathBuf, File), StoreError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let path = root.join("tmp").join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            // 名字被占用（例如同进程号的残留）就换下一个计数。
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
}

/// 把已 fsync 的临时文件改名到目标位置；目标已存在（并发写入同内容）时丢弃临时文件。
///
/// 返回前同步目标所在目录（新建分片目录时连同其上级），让 rename 产生的目录项在
/// 插入 `cas_objects` 行之前就已持久化：掉电后不会出现"有行无文件"（§7.7）。
fn publish(tmp: &Path, target: &Path) -> Result<(), StoreError> {
    let parent = target
        .parent()
        .ok_or_else(|| StoreError::Io("cas object path has no parent".to_owned()))?;
    let new_shard = !parent.exists();
    create_dir(parent)?;
    if new_shard {
        if let Some(shards) = parent.parent() {
            sync_dir(shards)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o640)).map_err(io_error)?;
    }
    if target.exists() {
        let _ = std::fs::remove_file(tmp);
        return Ok(());
    }
    std::fs::rename(tmp, target).map_err(io_error)?;
    sync_dir(parent)
}

/// 目录 fsync。只在 unix 上有意义；其他平台的文件系统不支持对目录句柄 fsync。
fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(io_error)?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// 边读边算摘要并写入 `sink`；超过 `max` 字节时返回 `CasTooLarge`。
fn stream_into(
    source: &mut impl Read,
    sink: &mut impl Write,
    max: u64,
) -> Result<(Sha256Digest, u64), StoreError> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = source.read(&mut buf).map_err(io_error)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            return Err(StoreError::CasTooLarge { bytes: total, max });
        }
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n]).map_err(io_error)?;
    }
    sink.flush().map_err(io_error)?;
    Ok((Sha256Digest::from_bytes(hasher.finalize().into()), total))
}

fn create_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750)).map_err(io_error)?;
    }
    Ok(())
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(io_error)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(io_error)?;
    entries.sort();
    Ok(entries)
}

fn io_error(error: std::io::Error) -> StoreError {
    if error.raw_os_error() == Some(28) {
        return StoreError::DiskFull;
    }
    StoreError::Io(error.to_string())
}
