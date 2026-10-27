//! 方式 A：主机直读 `out/`（sandbox.md §7.8）。
//!
//! 以 `O_DIRECTORY | O_NOFOLLOW` 打开的目录 fd 为根：先 `fstatat(AT_SYMLINK_NOFOLLOW)`
//! 判型，只对普通文件以 `O_NOFOLLOW | O_NONBLOCK` 相对该 fd 打开，再对打开的 fd
//! `fstat` 复核（同一 inode、仍是普通文件、`nlink == 1`），最多读"上限 + 1"字节。
//! 条目名只有一级且不含 `/`，`O_NOFOLLOW` 已足以保证不越出目录；Linux 上
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` 是同一语义的加强版，留给 Podman
//! 驱动在 B 级环境里核实后启用。

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{pre_check, Collector, IntakePlan, IntakeResult, Reject, MAX_ENTRIES};

#[derive(Debug, thiserror::Error)]
pub enum HostReadError {
    #[error("out/ cannot be opened as a directory without following links: {0}")]
    OpenDir(io::Error),
    #[error("out/ cannot be listed: {0}")]
    List(io::Error),
}

/// 读取已停止容器的 `out/`。目录本身打不开（不存在、是符号链接）是 `Collect` 失败；
/// 目录内任何异常条目只进 `unexpected`。
pub fn read_out_dir(out: &Path, plan: &IntakePlan) -> Result<IntakeResult, HostReadError> {
    let dir = open_dir(out).map_err(HostReadError::OpenDir)?;
    let names = list_names(&dir).map_err(HostReadError::List)?;
    let mut collector = Collector::default();
    for (index, name) in names.iter().enumerate() {
        if index >= MAX_ENTRIES {
            collector.note("<remaining entries>", Reject::TooManyEntries);
            break;
        }
        let artifact = match pre_check(name, plan) {
            Ok(artifact) => artifact,
            Err(reason) => {
                // 白名单外的条目也报告类型，便于诊断（不打开、不跟随）。
                let reason = match (reason, stat_type(&dir, name)) {
                    (Reject::NotWhitelisted, Some(kind)) if kind != Reject::NotWhitelisted => kind,
                    _ => reason,
                };
                collector.reject(name, reason);
                continue;
            }
        };
        match read_regular(&dir, name, plan.limit(artifact)) {
            Ok(bytes) => collector.offer(artifact, bytes, plan),
            Err(reason) => collector.reject(name, reason),
        }
    }
    Ok(collector.finish())
}

fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: c_path 是有效的 NUL 结尾字符串；返回值按 POSIX 约定检查。
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd 刚由 open 返回且未被其他对象持有。
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// 枚举目录 fd 下的名字（不含 `.` 与 `..`），按字节序排序以保证结果确定。
fn list_names(dir: &OwnedFd) -> io::Result<Vec<Vec<u8>>> {
    // fdopendir 接管 fd 的所有权，所以先 dup 一份。
    // SAFETY: dir 是有效 fd。
    let dup = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: dup 是有效的目录 fd，交给 fdopendir 后由 closedir 关闭。
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: fdopendir 失败时 fd 仍归调用方。
        unsafe { libc::close(dup) };
        return Err(err);
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: stream 非空且只在本函数内使用。
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: readdir 返回的 d_name 是 NUL 结尾字符串，在下一次 readdir 前有效。
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
        if names.len() > MAX_ENTRIES {
            break;
        }
    }
    // SAFETY: stream 由 fdopendir 返回，只关闭一次。
    unsafe { libc::closedir(stream) };
    names.sort();
    Ok(names)
}

fn fstatat_nofollow(dir: &OwnedFd, name: &[u8]) -> io::Result<libc::stat> {
    let c_name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL"))?;
    // SAFETY: stat 结构由内核填充；c_name 有效。
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn kind_of(mode: libc::mode_t) -> Reject {
    match mode & libc::S_IFMT {
        libc::S_IFREG => Reject::NotWhitelisted,
        libc::S_IFLNK => Reject::Symlink,
        libc::S_IFDIR => Reject::Directory,
        libc::S_IFIFO => Reject::Fifo,
        libc::S_IFCHR | libc::S_IFBLK => Reject::Device,
        libc::S_IFSOCK => Reject::Socket,
        _ => Reject::OtherType,
    }
}

fn stat_type(dir: &OwnedFd, name: &[u8]) -> Option<Reject> {
    fstatat_nofollow(dir, name)
        .ok()
        .map(|st| kind_of(st.st_mode))
}

fn read_regular(dir: &OwnedFd, name: &[u8], limit: u64) -> Result<Vec<u8>, Reject> {
    let before = fstatat_nofollow(dir, name).map_err(|_| Reject::Changed)?;
    match kind_of(before.st_mode) {
        Reject::NotWhitelisted => {}
        other => return Err(other),
    }
    #[allow(clippy::unnecessary_cast)]
    let links = before.st_nlink as u64;
    if links > 1 {
        return Err(Reject::Hardlink);
    }
    let c_name = CString::new(name).map_err(|_| Reject::InvalidName)?;
    // SAFETY: c_name 有效；O_NONBLOCK 保证即便被换成 FIFO 也不会阻塞。
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(Reject::Changed);
    }
    // SAFETY: fd 刚由 openat 返回。
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let after = file.metadata().map_err(|_| Reject::Changed)?;
    // st_ino / st_dev 的整数类型随平台不同（macOS 的 st_dev 是 i32）。
    #[allow(clippy::unnecessary_cast)]
    let same_inode = after.ino() == before.st_ino as u64 && after.dev() == before.st_dev as u64;
    if !after.file_type().is_file() || !same_inode {
        return Err(Reject::Changed);
    }
    if after.nlink() > 1 {
        return Err(Reject::Hardlink);
    }
    // 不信 stat 的大小：读到 limit + 1 字节即判超限。
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| Reject::Changed)?;
    if bytes.len() as u64 > limit {
        return Err(Reject::TooLarge);
    }
    Ok(bytes)
}
