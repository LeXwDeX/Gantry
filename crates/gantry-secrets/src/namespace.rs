//! 一个使用方命名空间内的读写实现。`SecretReader` 与 `ScopedSecrets` 都只持有一个
//! `Namespace`，也就只持有该使用方的 DEK。

use std::path::PathBuf;
use std::sync::Arc;

use gantry_core::ports::secrets::{
    Expect, PutOutcome, Secret, SecretMeta, SecretValue, SecretVersion, SecretsError, StagedSecret,
};
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::time::Timestamp;

use crate::envelope::{self, Binding, EnvelopeError, Kind, PREFIX_LEN, TAG_LEN};
use crate::fsx::{self, FileKind};
use crate::key::{Dek, KeyId};

/// 实例级共享参数（不含任何密钥）。
#[derive(Debug)]
pub(crate) struct Root {
    pub dir: PathBuf,
    #[allow(dead_code)] // 备份清单用（下一个切片）
    pub instance_id: String,
    pub key_id: KeyId,
    pub retain: usize,
    pub max_value: usize,
}

impl Root {
    fn file_limit(&self) -> usize {
        PREFIX_LEN + self.max_value + TAG_LEN
    }
}

/// 读取并发重试次数：读者不加锁，保留清理可能恰好删掉刚选中的文件。
const READ_RETRIES: usize = 16;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn version(v: u64) -> SecretVersion {
    SecretVersion::new(v).expect("on-disk versions start at 1")
}

/// 一个名字在目录里的文件快照。
#[derive(Default)]
pub(crate) struct Scan {
    /// 已提交版本与墓碑，按版本升序。
    pub files: Vec<(u64, FileKind)>,
    pub staged: Vec<u64>,
}

impl Scan {
    /// 最大版本号（含墓碑）；从未写过为 0。
    pub(crate) fn max(&self) -> u64 {
        self.files.last().map(|(v, _)| *v).unwrap_or(0)
    }

    /// 当前可见版本：最大版本是 `.sec` 才算存在。
    pub(crate) fn current(&self) -> Option<u64> {
        match self.files.last() {
            Some((v, FileKind::Sec)) => Some(*v),
            _ => None,
        }
    }

    pub(crate) fn has_sec(&self, v: u64) -> bool {
        self.files.contains(&(v, FileKind::Sec))
    }
}

fn expect_ok(expect: &Expect, current: Option<u64>) -> bool {
    match expect {
        Expect::Any => true,
        Expect::Absent => current.is_none(),
        Expect::Version(want) => current == Some(want.get()),
    }
}

pub(crate) struct Namespace {
    pub root: Arc<Root>,
    pub who: Consumer,
    pub dek: Arc<Dek>,
}

impl Namespace {
    pub(crate) fn dir(&self) -> PathBuf {
        self.root.dir.join(self.who.as_str())
    }

    fn path(&self, name: &SecretName, v: u64, kind: FileKind) -> PathBuf {
        self.dir().join(fsx::file_name(name, v, kind))
    }

    fn map_env(&self, path: PathBuf, error: EnvelopeError) -> SecretsError {
        let error = match error {
            EnvelopeError::Corrupt(reason) => SecretsError::Corrupt { path, reason },
            EnvelopeError::Unsupported(found) => SecretsError::UnsupportedFormat { found },
            EnvelopeError::KeyMismatch => SecretsError::KeyMismatch,
        };
        tracing::error!(consumer = self.who.as_str(), error = %error, "secret file rejected");
        error
    }

    pub(crate) fn scan(&self, name: &SecretName) -> Result<Scan, SecretsError> {
        let mut scan = Scan::default();
        for (entry, v, kind) in fsx::list_entries(&self.dir())? {
            if &entry != name {
                continue;
            }
            match kind {
                FileKind::Staged => scan.staged.push(v),
                _ => scan.files.push((v, kind)),
            }
        }
        scan.files.sort();
        scan.staged.sort();
        Ok(scan)
    }

    fn check_size(&self, value: &SecretValue) -> Result<(), SecretsError> {
        if value.expose().len() > self.root.max_value {
            return Err(SecretsError::TooLarge {
                limit: self.root.max_value,
            });
        }
        Ok(())
    }

    fn seal(&self, name: &SecretName, v: u64, kind: Kind, at: i64, plaintext: &[u8]) -> Vec<u8> {
        envelope::seal(
            &self.dek,
            self.root.key_id,
            kind,
            at,
            &Binding {
                consumer: self.who,
                name,
                version: v,
            },
            plaintext,
        )
    }

    /// 读一个已提交版本；文件不存在返回 `Ok(None)`。
    fn read_sec(&self, name: &SecretName, v: u64) -> Result<Option<(Secret, i64)>, SecretsError> {
        let path = self.path(name, v, FileKind::Sec);
        let Some(data) = fsx::read_file(&path, self.root.file_limit())? else {
            return Ok(None);
        };
        let binding = Binding {
            consumer: self.who,
            name,
            version: v,
        };
        let (mut plain, created_at) =
            envelope::open(&self.dek, self.root.key_id, Kind::Secret, &binding, &data)
                .map_err(|error| self.map_env(path, error))?;
        let value = SecretValue::from_bytes(std::mem::take(&mut *plain));
        Ok(Some((
            Secret {
                value,
                version: version(v),
            },
            created_at,
        )))
    }

    /// 只读头部取 `created_at`（不解密）。用于元数据。
    fn header_time(
        &self,
        name: &SecretName,
        v: u64,
        kind: FileKind,
    ) -> Result<Option<i64>, SecretsError> {
        let path = self.path(name, v, kind);
        let Some(data) = fsx::read_file(&path, self.root.file_limit())? else {
            return Ok(None);
        };
        let (_, created_at) = envelope::peek(&data).map_err(|error| self.map_env(path, error))?;
        Ok(Some(created_at))
    }

    fn meta(&self, name: &SecretName, v: u64, created_at: i64) -> SecretMeta {
        SecretMeta {
            consumer: self.who,
            name: name.clone(),
            version: version(v),
            updated_at: Timestamp::from_millis(created_at),
        }
    }

    // ---- 读 ----------------------------------------------------------------

    pub(crate) fn get(&self, name: &SecretName) -> Result<Option<Secret>, SecretsError> {
        for _ in 0..READ_RETRIES {
            let Some(v) = self.scan(name)?.current() else {
                return Ok(None);
            };
            if let Some((secret, _)) = self.read_sec(name, v)? {
                return Ok(Some(secret));
            }
        }
        Err(SecretsError::Io(std::io::Error::other(
            "secret kept changing while reading",
        )))
    }

    pub(crate) fn version(&self, name: &SecretName) -> Result<Option<SecretVersion>, SecretsError> {
        Ok(self.scan(name)?.current().map(version))
    }

    pub(crate) fn get_version(
        &self,
        name: &SecretName,
        want: SecretVersion,
    ) -> Result<Option<Secret>, SecretsError> {
        let scan = self.scan(name)?;
        if scan.current().is_none() || !scan.has_sec(want.get()) {
            return Ok(None);
        }
        Ok(self.read_sec(name, want.get())?.map(|(secret, _)| secret))
    }

    // ---- 写 ----------------------------------------------------------------

    fn conflict(expect: Expect, current: Option<u64>) -> SecretsError {
        SecretsError::Conflict {
            expected: expect,
            actual: current.map(version),
        }
    }

    pub(crate) fn put(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
    ) -> Result<PutOutcome, SecretsError> {
        self.check_size(&value)?;
        let _lock = fsx::lock(&self.root.dir)?;
        let scan = self.scan(name)?;
        let current = scan.current();
        if !expect_ok(&expect, current) {
            return Err(Self::conflict(expect, current));
        }
        let next = scan.max() + 1;
        let at = now_ms();
        let data = self.seal(name, next, Kind::Secret, at, value.expose());
        drop(value);
        fsx::write_new(
            &self.dir(),
            &fsx::file_name(name, next, FileKind::Sec),
            &data,
        )?;
        self.retain(name);
        tracing::info!(
            consumer = self.who.as_str(),
            name = name.as_str(),
            version = next,
            op = "put",
            result = "ok"
        );
        Ok(PutOutcome {
            meta: self.meta(name, next, at),
            previous: current.map(version),
        })
    }

    pub(crate) fn stage(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
    ) -> Result<StagedSecret, SecretsError> {
        self.check_size(&value)?;
        let _lock = fsx::lock(&self.root.dir)?;
        let scan = self.scan(name)?;
        if !scan.staged.is_empty() {
            return Err(SecretsError::AlreadyStaged);
        }
        let current = scan.current();
        if !expect_ok(&expect, current) {
            return Err(Self::conflict(expect, current));
        }
        let next = scan.max() + 1;
        let data = self.seal(name, next, Kind::Secret, now_ms(), value.expose());
        drop(value);
        fsx::write_new(
            &self.dir(),
            &fsx::file_name(name, next, FileKind::Staged),
            &data,
        )?;
        tracing::info!(
            consumer = self.who.as_str(),
            name = name.as_str(),
            version = next,
            op = "stage",
            result = "ok"
        );
        Ok(StagedSecret {
            consumer: self.who,
            name: name.clone(),
            version: version(next),
        })
    }

    fn check_owner(&self, staged: &StagedSecret) -> Result<(), SecretsError> {
        if staged.consumer != self.who {
            tracing::warn!(
                consumer = self.who.as_str(),
                name = staged.name.as_str(),
                op = "staged",
                result = "denied"
            );
            return Err(SecretsError::Denied {
                who: self.who,
                name: staged.name.clone(),
            });
        }
        Ok(())
    }

    pub(crate) fn commit(&self, staged: &StagedSecret) -> Result<SecretMeta, SecretsError> {
        self.check_owner(staged)?;
        let _lock = fsx::lock(&self.root.dir)?;
        let v = staged.version.get();
        let scan = self.scan(&staged.name)?;
        if !scan.staged.contains(&v) {
            return Err(SecretsError::NotStaged);
        }
        if scan.max() >= v {
            return Err(SecretsError::Conflict {
                expected: Expect::Version(staged.version),
                actual: scan.current().map(version),
            });
        }
        let created_at = self
            .header_time(&staged.name, v, FileKind::Staged)?
            .ok_or(SecretsError::NotStaged)?;
        fsx::promote(
            &self.dir(),
            &fsx::file_name(&staged.name, v, FileKind::Staged),
            &fsx::file_name(&staged.name, v, FileKind::Sec),
        )?;
        self.retain(&staged.name);
        tracing::info!(
            consumer = self.who.as_str(),
            name = staged.name.as_str(),
            version = v,
            op = "commit",
            result = "ok"
        );
        Ok(self.meta(&staged.name, v, created_at))
    }

    pub(crate) fn abort(&self, staged: &StagedSecret) -> Result<(), SecretsError> {
        self.check_owner(staged)?;
        let _lock = fsx::lock(&self.root.dir)?;
        let path = self.path(&staged.name, staged.version.get(), FileKind::Staged);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SecretsError::NotStaged)
            }
            Err(error) => return Err(error.into()),
        }
        fsx::sync_dir(&self.dir())?;
        tracing::info!(
            consumer = self.who.as_str(),
            name = staged.name.as_str(),
            version = staged.version.get(),
            op = "abort",
            result = "ok"
        );
        Ok(())
    }

    pub(crate) fn staged(&self) -> Result<Vec<StagedSecret>, SecretsError> {
        let mut out: Vec<StagedSecret> = fsx::list_entries(&self.dir())?
            .into_iter()
            .filter(|(_, _, kind)| *kind == FileKind::Staged)
            .map(|(name, v, _)| StagedSecret {
                consumer: self.who,
                name,
                version: version(v),
            })
            .collect();
        out.sort_by(|a, b| (&a.name, a.version).cmp(&(&b.name, b.version)));
        Ok(out)
    }

    pub(crate) fn delete(
        &self,
        name: &SecretName,
        expect: Expect,
    ) -> Result<Option<SecretMeta>, SecretsError> {
        let _lock = fsx::lock(&self.root.dir)?;
        let scan = self.scan(name)?;
        let current = scan.current();
        if !expect_ok(&expect, current) {
            if scan.files.is_empty() {
                return Err(SecretsError::NotFound);
            }
            return Err(Self::conflict(expect, current));
        }
        let Some(cur) = current else {
            return Ok(None);
        };
        let previous_time = self.header_time(name, cur, FileKind::Sec)?.unwrap_or(0);
        let next = scan.max() + 1;
        let data = self.seal(name, next, Kind::Tombstone, now_ms(), &[]);
        fsx::write_new(
            &self.dir(),
            &fsx::file_name(name, next, FileKind::Tomb),
            &data,
        )?;
        self.retain(name);
        tracing::info!(
            consumer = self.who.as_str(),
            name = name.as_str(),
            version = next,
            op = "delete",
            result = "ok"
        );
        Ok(Some(self.meta(name, cur, previous_time)))
    }

    pub(crate) fn list(&self) -> Result<Vec<SecretMeta>, SecretsError> {
        let mut latest: std::collections::BTreeMap<SecretName, (u64, FileKind)> =
            std::collections::BTreeMap::new();
        for (name, v, kind) in fsx::list_entries(&self.dir())? {
            if kind == FileKind::Staged {
                continue;
            }
            let slot = latest.entry(name).or_insert((v, kind));
            if v > slot.0 {
                *slot = (v, kind);
            }
        }
        let mut out = Vec::new();
        for (name, (v, kind)) in latest {
            if kind != FileKind::Sec {
                continue;
            }
            if let Some(created_at) = self.header_time(&name, v, FileKind::Sec)? {
                out.push(self.meta(&name, v, created_at));
            }
        }
        Ok(out)
    }

    pub(crate) fn check_refs(&self, refs: &[SecretName]) -> Result<Vec<SecretName>, SecretsError> {
        let mut missing = Vec::new();
        for name in refs {
            if self.scan(name)?.current().is_none() {
                missing.push(name.clone());
            }
        }
        Ok(missing)
    }

    /// 保留清理（持锁调用）：保留最大版本的文件，以及最近一个墓碑之后最新的
    /// `retain` 个 `.sec`；其余 `.sec` 与旧墓碑删除。尽力而为，失败只记 `warn`。
    fn retain(&self, name: &SecretName) {
        let Ok(scan) = self.scan(name) else {
            tracing::warn!(
                consumer = self.who.as_str(),
                name = name.as_str(),
                op = "retain",
                result = "scan_failed"
            );
            return;
        };
        let max = scan.max();
        let last_tomb = scan
            .files
            .iter()
            .filter(|(_, kind)| *kind == FileKind::Tomb)
            .map(|(v, _)| *v)
            .max()
            .unwrap_or(0);
        let keep_secs: Vec<u64> = scan
            .files
            .iter()
            .rev()
            .filter(|(v, kind)| *kind == FileKind::Sec && *v > last_tomb)
            .take(self.root.retain)
            .map(|(v, _)| *v)
            .collect();
        for (v, kind) in &scan.files {
            if *v == max || (*kind == FileKind::Sec && keep_secs.contains(v)) {
                continue;
            }
            if let Err(error) = std::fs::remove_file(self.path(name, *v, *kind)) {
                tracing::warn!(consumer = self.who.as_str(), name = name.as_str(), version = *v, op = "retain", error = %error.kind(), result = "remove_failed");
            }
        }
    }
}
