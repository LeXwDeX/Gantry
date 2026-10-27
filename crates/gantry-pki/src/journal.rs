//! 吊销日志 `revocations.journal`（pki.md §7.6，P-67、P-80）。
//!
//! 事件日志之外、只追加的 JSON Lines 文件：每次吊销在事务提交**之前**向本地文件追加一行并
//! `fsync`，再同步复制到备份目标（同样追加并 `fsync`）；任一步失败 → 整个吊销失败。
//! 文件从不改写；事务回滚也不删除已写的行（重放时照样吊销，偏向安全）。
//!
//! 恢复后重放：本地文件与备份副本取并集、按行去重，只重放 `revoked_at` 晚于备份时刻的条目。
//! 坏行 → 整体报错（宁可不启动，也不漏掉吊销）。

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use gantry_core::canon::Sha256Digest;
use gantry_core::entities::Actor;
use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use serde::{Deserialize, Serialize};

use crate::keys::CertSerial;

/// 一条吊销记录。字段顺序即落盘顺序。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub v: u8,
    pub device_id: DeviceId,
    pub cert_serials: Vec<CertSerial>,
    pub approval_pubkey_fpr: Option<Sha256Digest>,
    pub revoked_at: Timestamp,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub log_id: String,
    pub log_epoch: u64,
}

pub const JOURNAL_VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal append failed for {path:?}: {source}")]
    Append { path: PathBuf, source: io::Error },
    #[error("journal read failed for {path:?}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("journal {path:?} line {line} is malformed: {reason}")]
    BadLine {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("journal entry cannot be encoded: {0}")]
    Encode(String),
}

/// 写入端：本地文件 + 备份目标副本。
#[derive(Clone, Debug)]
pub struct RevocationJournal {
    local: PathBuf,
    backup: PathBuf,
}

impl RevocationJournal {
    /// `local` = `pki.revocations_journal`；`backup` = `deploy` 的 `backup.journal_target`。
    pub fn new(local: impl Into<PathBuf>, backup: impl Into<PathBuf>) -> Self {
        Self {
            local: local.into(),
            backup: backup.into(),
        }
    }

    pub fn local_path(&self) -> &Path {
        &self.local
    }

    pub fn backup_path(&self) -> &Path {
        &self.backup
    }

    /// 追加一行并 `fsync`，先本地后备份目标。返回 `Ok` 才允许提交吊销事务。
    pub fn append(&self, entry: &JournalEntry) -> Result<(), JournalError> {
        let mut line = serde_json::to_string(entry)
            .map_err(|error| JournalError::Encode(error.to_string()))?;
        line.push('\n');
        append_line(&self.local, line.as_bytes())?;
        append_line(&self.backup, line.as_bytes())?;
        Ok(())
    }

    /// 读取两份文件的并集（按行内容去重，保持首次出现的顺序：先本地、后备份）。
    /// 文件不存在视为空。任何坏行 → 错误。
    pub fn read_all(&self) -> Result<Vec<JournalEntry>, JournalError> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for path in [&self.local, &self.backup] {
            for (line, entry) in read_file(path)? {
                if seen.insert(line) {
                    out.push(entry);
                }
            }
        }
        Ok(out)
    }

    /// 需要重放的条目：`revoked_at` 晚于备份时刻（更早的已在备份里生效，P-80）。
    pub fn entries_after(&self, backup_at: Timestamp) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .read_all()?
            .into_iter()
            .filter(|entry| entry.revoked_at > backup_at)
            .collect())
    }
}

fn append_line(path: &Path, bytes: &[u8]) -> Result<(), JournalError> {
    let wrap = |source| JournalError::Append {
        path: path.to_path_buf(),
        source,
    };
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(wrap)?;
    file.write_all(bytes).map_err(wrap)?;
    file.sync_all().map_err(wrap)?;
    Ok(())
}

fn read_file(path: &Path) -> Result<Vec<(String, JournalEntry)>, JournalError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(JournalError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let mut out = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|source| JournalError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let bad = |reason: String| JournalError::BadLine {
            path: path.to_path_buf(),
            line: index + 1,
            reason,
        };
        let entry: JournalEntry =
            serde_json::from_str(&line).map_err(|error| bad(error.to_string()))?;
        if entry.v != JOURNAL_VERSION {
            return Err(bad(format!("unsupported version {}", entry.v)));
        }
        if entry.cert_serials.is_empty() {
            return Err(bad("cert_serials is empty".into()));
        }
        out.push((line, entry));
    }
    Ok(out)
}
