//! 加密备份与恢复（secrets.md §7.6，OQ-3 的提议方案）。
//!
//! 导出：持写锁读取每个使用方每个名字的当前版本（或墓碑），组成明文负载后整体用
//! `age`（X25519 接收方，可多个）加密为一个流。明文只在内存里（`Zeroizing`），不落盘。
//!
//! 负载格式（只存在于 age 密文内）：
//!
//! ```text
//! "GSECBAK1" ‖ u32be(len(manifest)) ‖ manifest.json ‖ 对每个 kind=secret 的条目依次：u32be(len) ‖ 值
//! ```
//!
//! 恢复：先把整个流解密并校验完毕（age 的 STREAM 认证覆盖截断与篡改），全部通过后才写
//! 任何文件；以**原版本号**、用本机主密钥派生的 DEK 重新加密写入。

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::str::FromStr;

use gantry_core::canon::Sha256Digest;
use gantry_core::ports::secrets::{CorruptReason, SecretsError};
use gantry_core::secret::{Consumer, SecretName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::fsx::FileKind;
use crate::namespace::Namespace;

const MAGIC: &[u8; 8] = b"GSECBAK1";
const FORMAT: u32 = 1;
/// 清单大小上限，防止恶意流让恢复方分配过多内存。
const MAX_MANIFEST: usize = 16 * 1024 * 1024;

/// 备份接收方公钥（age X25519，`age1…`）。公钥可信、可公开。
#[derive(Clone)]
pub struct BackupRecipient(age::x25519::Recipient);

impl BackupRecipient {
    pub fn parse(text: &str) -> Result<Self, SecretsError> {
        age::x25519::Recipient::from_str(text.trim())
            .map(Self)
            .map_err(|_| invalid("invalid backup recipient"))
    }
}

impl fmt::Debug for BackupRecipient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BackupRecipient({})", self.0)
    }
}

/// 恢复身份（age X25519 私钥，`AGE-SECRET-KEY-1…`）。最高敏感：离线保管，不在服务器上常驻；
/// 不可 `Clone`，`Debug` 不含值，本库不提供导出它的方法（生成离线私钥用 `age-keygen`）。
pub struct BackupIdentity(age::x25519::Identity);

impl BackupIdentity {
    pub fn parse(text: &str) -> Result<Self, SecretsError> {
        age::x25519::Identity::from_str(text.trim())
            .map(Self)
            .map_err(|_| invalid("invalid backup identity"))
    }

    /// 现场生成一个身份（测试与演练用）。
    pub fn generate() -> Self {
        Self(age::x25519::Identity::generate())
    }

    pub fn recipient(&self) -> BackupRecipient {
        BackupRecipient(self.0.to_public())
    }
}

impl fmt::Debug for BackupIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BackupIdentity(***)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupEntryKind {
    Secret,
    Tombstone,
}

/// 清单条目：只有名字、版本与种类，无值、无哈希、无长度。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupEntry {
    pub consumer: Consumer,
    pub name: SecretName,
    pub version: u64,
    pub kind: BackupEntryKind,
    /// 原文件的写入时间（毫秒），恢复时沿用。
    pub updated_at: i64,
}

/// 备份清单（与加密流内的清单相同），另附整个**密文**流的 SHA-256，供备份脚本核对传输完整性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupManifest {
    pub format: u32,
    pub instance_id: String,
    pub created_at: i64,
    pub entries: Vec<BackupEntry>,
    pub ciphertext_sha256: Sha256Digest,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireManifest {
    format: u32,
    instance_id: String,
    created_at: i64,
    entries: Vec<BackupEntry>,
}

/// 恢复模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportMode {
    /// 实例必须为空（任何使用方目录里都没有条目或暂存项）。
    Fresh,
    /// 只写备份版本号高于本地最大版本（含墓碑）的条目。
    Merge,
    /// 显式危险：删除本地同名的全部文件后按备份写入。
    Overwrite,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub written: usize,
    pub skipped: usize,
}

fn invalid(msg: &str) -> SecretsError {
    SecretsError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        msg.to_owned(),
    ))
}

fn corrupt(reason: CorruptReason) -> SecretsError {
    SecretsError::Corrupt {
        path: PathBuf::from("<backup>"),
        reason,
    }
}

struct HashingWriter<'a> {
    inner: &'a mut dyn Write,
    hasher: Sha256,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn export(
    namespaces: &[Namespace],
    instance_id: &str,
    to: &[BackupRecipient],
    out: &mut dyn Write,
) -> Result<BackupManifest, SecretsError> {
    if to.is_empty() {
        return Err(invalid("at least one backup recipient is required"));
    }
    let Some(first) = namespaces.first() else {
        return Err(invalid("instance hosts no consumers"));
    };
    let _lock = crate::fsx::lock(&first.root.dir)?;
    let mut entries = Vec::new();
    let mut values: Vec<Zeroizing<Vec<u8>>> = Vec::new();
    for ns in namespaces {
        for (name, v, kind, plain, at) in ns.export_current()? {
            let kind = match kind {
                FileKind::Tomb => BackupEntryKind::Tombstone,
                _ => BackupEntryKind::Secret,
            };
            entries.push(BackupEntry {
                consumer: ns.who,
                name,
                version: v,
                kind,
                updated_at: at,
            });
            if let Some(plain) = plain {
                values.push(plain);
            }
        }
    }
    let created_at = crate::namespace::now_ms();
    let wire = WireManifest {
        format: FORMAT,
        instance_id: instance_id.to_owned(),
        created_at,
        entries,
    };
    let manifest = serde_json::to_vec(&wire).map_err(std::io::Error::other)?;

    let recipients = to.iter().map(|r| &r.0 as &dyn age::Recipient);
    let encryptor = age::Encryptor::with_recipients(recipients)
        .map_err(|_| invalid("cannot build backup encryptor"))?;
    let mut hashing = HashingWriter {
        inner: out,
        hasher: Sha256::new(),
    };
    {
        let mut w = encryptor.wrap_output(&mut hashing)?;
        w.write_all(MAGIC)?;
        w.write_all(&(manifest.len() as u32).to_be_bytes())?;
        w.write_all(&manifest)?;
        for value in &values {
            w.write_all(&(value.len() as u32).to_be_bytes())?;
            w.write_all(value)?;
        }
        w.finish()?;
    }
    hashing.flush()?;
    let digest: [u8; 32] = hashing.hasher.finalize().into();
    tracing::info!(
        entries = wire.entries.len(),
        op = "export_backup",
        result = "ok"
    );
    Ok(BackupManifest {
        format: wire.format,
        instance_id: wire.instance_id,
        created_at: wire.created_at,
        entries: wire.entries,
        ciphertext_sha256: Sha256Digest::from_bytes(digest),
    })
}

/// 清单与每个条目对应的值（墓碑为 `None`）。
type Decrypted = (WireManifest, Vec<Option<Zeroizing<Vec<u8>>>>);

/// 解密并完整校验备份流，返回清单与每个条目对应的值（墓碑为 `None`）。不写任何文件。
fn decrypt(id: &BackupIdentity, input: &mut dyn Read) -> Result<Decrypted, SecretsError> {
    let decryptor =
        age::Decryptor::new(input).map_err(|_| corrupt(CorruptReason::HeaderInvalid))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&id.0 as &dyn age::Identity))
        .map_err(|error| match error {
            age::DecryptError::NoMatchingKeys => SecretsError::KeyMismatch,
            _ => corrupt(CorruptReason::AeadFailed),
        })?;
    let mut payload = Zeroizing::new(Vec::new());
    reader
        .read_to_end(&mut payload)
        .map_err(|_| corrupt(CorruptReason::AeadFailed))?;

    let mut cursor: &[u8] = &payload;
    let mut take = |n: usize| -> Result<&[u8], SecretsError> {
        if cursor.len() < n {
            return Err(corrupt(CorruptReason::HeaderInvalid));
        }
        let (head, rest) = cursor.split_at(n);
        cursor = rest;
        Ok(head)
    };
    if take(MAGIC.len())? != MAGIC {
        return Err(corrupt(CorruptReason::HeaderInvalid));
    }
    let len = u32::from_be_bytes(take(4)?.try_into().expect("4 bytes")) as usize;
    if len > MAX_MANIFEST {
        return Err(corrupt(CorruptReason::HeaderInvalid));
    }
    let manifest: WireManifest =
        serde_json::from_slice(take(len)?).map_err(|_| corrupt(CorruptReason::HeaderInvalid))?;
    if manifest.format != FORMAT {
        return Err(SecretsError::UnsupportedFormat {
            found: u8::try_from(manifest.format).unwrap_or(u8::MAX),
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut values = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        if entry.version == 0 || !seen.insert((entry.consumer, entry.name.clone())) {
            return Err(corrupt(CorruptReason::HeaderInvalid));
        }
        match entry.kind {
            BackupEntryKind::Tombstone => values.push(None),
            BackupEntryKind::Secret => {
                let n = u32::from_be_bytes(take(4)?.try_into().expect("4 bytes")) as usize;
                values.push(Some(Zeroizing::new(take(n)?.to_vec())));
            }
        }
    }
    if !cursor.is_empty() {
        return Err(corrupt(CorruptReason::HeaderInvalid));
    }
    Ok((manifest, values))
}

pub(crate) fn import(
    namespaces: &[Namespace],
    id: &BackupIdentity,
    input: &mut dyn Read,
    mode: ImportMode,
) -> Result<ImportReport, SecretsError> {
    let (manifest, values) = decrypt(id, input)?;
    let by_consumer: BTreeMap<Consumer, &Namespace> =
        namespaces.iter().map(|ns| (ns.who, ns)).collect();
    // 全部校验通过之后才写：使用方必须由本实例承载，值不超过上限。
    for (entry, value) in manifest.entries.iter().zip(&values) {
        let ns = by_consumer
            .get(&entry.consumer)
            .ok_or_else(|| SecretsError::Denied {
                who: entry.consumer,
                name: entry.name.clone(),
            })?;
        if let Some(value) = value {
            if value.is_empty() {
                return Err(SecretsError::EmptyValue);
            }
            if value.len() > ns.root.max_value {
                return Err(SecretsError::TooLarge {
                    limit: ns.root.max_value,
                });
            }
        }
    }
    let Some(first) = namespaces.first() else {
        return Err(invalid("instance hosts no consumers"));
    };
    let _lock = crate::fsx::lock(&first.root.dir)?;
    if mode == ImportMode::Fresh {
        for ns in namespaces {
            if !crate::fsx::list_entries(&ns.dir())?.is_empty() {
                return Err(invalid("import_backup(Fresh) requires an empty instance"));
            }
        }
    }
    let mut report = ImportReport::default();
    for (entry, value) in manifest.entries.iter().zip(values) {
        let ns = by_consumer[&entry.consumer];
        let local_max = ns.scan(&entry.name)?.max();
        match mode {
            ImportMode::Merge if local_max >= entry.version => {
                report.skipped += 1;
                continue;
            }
            ImportMode::Overwrite => {
                if local_max > 0 {
                    tracing::warn!(
                        consumer = entry.consumer.as_str(),
                        name = entry.name.as_str(),
                        op = "import_backup",
                        mode = "overwrite",
                        result = "replacing"
                    );
                }
                ns.remove_all(&entry.name)?;
            }
            _ => {}
        }
        let kind = match entry.kind {
            BackupEntryKind::Secret => FileKind::Sec,
            BackupEntryKind::Tombstone => FileKind::Tomb,
        };
        let plain = value.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);
        ns.write_restored(&entry.name, entry.version, kind, entry.updated_at, plain)?;
        report.written += 1;
    }
    tracing::info!(
        written = report.written,
        skipped = report.skipped,
        op = "import_backup",
        result = "ok"
    );
    Ok(report)
}
