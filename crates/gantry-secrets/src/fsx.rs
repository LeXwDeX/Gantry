//! 文件系统辅助：权限检查、写锁、不覆盖的原子写入、文件名解析。
//!
//! 所有打开文件的操作都带 `O_NOFOLLOW`；目录本身在 `open` 时检查过不是符号链接。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::OsRng;
use gantry_core::ports::secrets::SecretsError;
use gantry_core::secret::SecretName;

pub(crate) const META_FILE: &str = "secrets.meta";
pub(crate) const LOCK_FILE: &str = ".lock";
pub(crate) const TMP_PREFIX: &str = ".tmp.";
pub(crate) const DIR_MODE: u32 = 0o700;
pub(crate) const FILE_MODE: u32 = 0o600;

fn nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

pub(crate) fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// 检查一个路径：不是符号链接、属主 = `uid`、组与其他用户无任何权限。
pub(crate) fn check_entry(path: &Path, uid: u32, want_dir: bool) -> Result<(), SecretsError> {
    let meta = fs::symlink_metadata(path)?;
    let mode = meta.mode() & 0o7777;
    let ok_type = if want_dir {
        meta.file_type().is_dir()
    } else {
        meta.file_type().is_file()
    };
    if !ok_type || meta.uid() != uid || mode & 0o077 != 0 {
        return Err(SecretsError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// 检查整个实例目录（根目录、根下文件、各使用方目录及其中文件）。不修复任何东西。
pub(crate) fn check_tree(dir: &Path, uid: u32) -> Result<(), SecretsError> {
    check_entry(dir, uid, true)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            check_entry(&path, uid, true)?;
            for inner in fs::read_dir(&path)? {
                let inner = inner?;
                let inner_path = inner.path();
                check_entry(&inner_path, uid, false)?;
            }
        } else {
            check_entry(&path, uid, false)?;
        }
    }
    Ok(())
}

pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(DIR_MODE).create(path)?;
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(DIR_MODE))
}

pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 测试用崩溃注入：环境变量 `GANTRY_SECRETS_CRASH` 等于点名时 SIGKILL 自身。
#[cfg(feature = "fault-injection")]
pub(crate) fn fault(point: &str) {
    if std::env::var("GANTRY_SECRETS_CRASH").ok().as_deref() == Some(point) {
        let _ =
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
        loop {
            std::thread::park();
        }
    }
}

#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn fault(_point: &str) {}

/// 在 `dir` 内写一个新文件：`.tmp.<随机>`（0600）→ fsync → 硬链接到目标名（目标已存在则失败，
/// 版本号永不覆盖）→ 删除临时名 → fsync 目录。
pub(crate) fn write_new(dir: &Path, target: &str, data: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!("{TMP_PREFIX}{}", random_hex(8)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .custom_flags(nofollow())
        .open(&tmp)?;
    let written = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    fault("after_tmp");
    let linked = fs::hard_link(&tmp, dir.join(target));
    if linked.is_ok() {
        fault("after_link");
    }
    let _ = fs::remove_file(&tmp);
    linked?;
    sync_dir(dir)
}

/// 把已存在的文件以不覆盖的方式改名（硬链接 + 删除原名）。
pub(crate) fn promote(dir: &Path, from: &str, to: &str) -> io::Result<()> {
    fault("commit_before_link");
    fs::hard_link(dir.join(from), dir.join(to))?;
    fault("commit_after_link");
    fs::remove_file(dir.join(from))?;
    sync_dir(dir)
}

/// 读整个文件（不跟随符号链接）；文件不存在返回 `Ok(None)`；超过 `limit` 字节视为损坏。
pub(crate) fn read_file(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, SecretsError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let len = file.metadata()?.len();
    if len > limit as u64 {
        return Err(SecretsError::Corrupt {
            path: path.to_path_buf(),
            reason: gantry_core::ports::secrets::CorruptReason::HeaderInvalid,
        });
    }
    let mut buf = Vec::with_capacity(len as usize);
    file.take(limit as u64 + 1).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

/// 写操作互斥锁：`.lock` 上的 `flock(LOCK_EX)`，句柄丢弃即释放。
pub(crate) struct WriteLock(#[allow(dead_code)] File);

pub(crate) fn lock(dir: &Path) -> Result<WriteLock, SecretsError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nofollow())
        .open(dir.join(LOCK_FILE))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).map_err(io::Error::from)?;
    Ok(WriteLock(file))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FileKind {
    Sec,
    Staged,
    Tomb,
}

impl FileKind {
    fn ext(self) -> &'static str {
        match self {
            Self::Sec => "sec",
            Self::Staged => "staged",
            Self::Tomb => "tomb",
        }
    }
}

pub(crate) fn file_name(name: &SecretName, version: u64, kind: FileKind) -> String {
    format!("{}.v{version}.{}", name.as_str(), kind.ext())
}

/// 从右侧解析 `<name>.v<N>.<sec|staged|tomb>`；名字里允许 `.`。
pub(crate) fn parse_file_name(file: &str) -> Option<(SecretName, u64, FileKind)> {
    let (rest, ext) = file.rsplit_once('.')?;
    let kind = match ext {
        "sec" => FileKind::Sec,
        "staged" => FileKind::Staged,
        "tomb" => FileKind::Tomb,
        _ => return None,
    };
    let (name, digits) = rest.rsplit_once(".v")?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let version: u64 = digits.parse().ok()?;
    let name = SecretName::parse(name).ok()?;
    Some((name, version, kind))
}

/// 列出一个使用方目录下的全部条目文件（忽略临时文件与无法解析的名字）。
pub(crate) fn list_entries(dir: &Path) -> Result<Vec<(SecretName, u64, FileKind)>, SecretsError> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(parsed) = entry.file_name().to_str().and_then(parse_file_name) {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// 清理崩溃遗留的临时文件。
pub(crate) fn remove_tmp_files(dir: &Path) -> Result<Vec<PathBuf>, SecretsError> {
    let mut removed = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let is_tmp = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(TMP_PREFIX));
        if is_tmp {
            fs::remove_file(entry.path())?;
            removed.push(entry.path());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_from_the_right() {
        let (name, version, kind) = parse_file_name("github.app.key.v12.sec").unwrap();
        assert_eq!(name.as_str(), "github.app.key");
        assert_eq!(version, 12);
        assert_eq!(kind, FileKind::Sec);
        let (name, version, kind) = parse_file_name("a.v1.v3.tomb").unwrap();
        assert_eq!((name.as_str(), version, kind), ("a.v1", 3, FileKind::Tomb));
        assert!(parse_file_name("a.v0.sec").is_none());
        assert!(parse_file_name("a.v01.sec").is_none());
        assert!(parse_file_name("a.v1.txt").is_none());
        assert!(parse_file_name(".tmp.abcd").is_none());
        assert!(parse_file_name(".v1.sec").is_none());
    }

    #[test]
    fn owner_mismatch_is_insecure() {
        let dir = std::env::temp_dir().join(format!("gantry-secrets-owner-{}", random_hex(6)));
        create_dir(&dir).unwrap();
        assert!(check_entry(&dir, euid(), true).is_ok());
        let other = euid().wrapping_add(1);
        assert!(matches!(
            check_entry(&dir, other, true),
            Err(SecretsError::InsecurePermissions { .. })
        ));
        fs::remove_dir(&dir).unwrap();
    }
}
