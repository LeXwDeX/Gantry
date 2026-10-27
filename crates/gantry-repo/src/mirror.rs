//! 裸镜像仓（R1）：布局、创建、受控访问、只读查询与完整性检查。
//!
//! 布局（repo.md §7.1）：`<data_dir>/mirrors/<owner>/<repo>.git/`，`hooks/` 为空，
//! `config` 不含 url / include / 凭据；`gantry.lock` 是 flock 锁文件（共享 = 读 /
//! fetch / push / 准备工作区 / 校验，独占 = gc、重建）；`gantry-project` 记录归属项目，
//! 供重启时重建"项目 → 镜像仓"的映射。

use std::fs::File;
use std::path::{Path, PathBuf};

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::ids::ProjectId;
use gantry_core::ports::{CommitInfo, MirrorGuard, MirrorHealth, MirrorInfo, RepoError};

use crate::git::{GitCmd, GitEnv};

/// 上游分支快照的 refspec；不拉 `refs/pull/*`（repo.md §7.1）。
pub const FETCH_REFSPEC: &str = "+refs/heads/*:refs/mirror/heads/*";
pub const LOCK_FILE: &str = "gantry.lock";
pub const PROJECT_FILE: &str = "gantry-project";

/// 新建镜像仓时写入的配置（repo.md §7.2 末段）。
pub const MIRROR_CONFIG: &[(&str, &str)] = &[
    ("transfer.fsckObjects", "true"),
    ("fetch.fsckObjects", "true"),
    ("receive.fsckObjects", "true"),
    ("gc.auto", "0"),
    ("gc.pruneExpire", "2.weeks"),
];

#[derive(Clone, Debug)]
pub struct MirrorEntry {
    pub slug: RepoSlug,
    pub git_dir: PathBuf,
    pub health: MirrorHealth,
}

impl MirrorEntry {
    pub fn info(&self) -> MirrorInfo {
        MirrorInfo {
            git_dir: self.git_dir.clone(),
            fetch_refspecs: vec![FETCH_REFSPEC.to_owned()],
        }
    }

    pub fn guard(&self, lock: MirrorLock) -> MirrorGuard {
        MirrorGuard::new(self.git_dir.clone(), vec![FETCH_REFSPEC.to_owned()], lock)
    }
}

/// `<data_dir>/mirrors/<owner>/<repo>.git`。
pub fn mirror_path(mirrors_dir: &Path, slug: &RepoSlug) -> Result<PathBuf, RepoError> {
    let (owner, repo) = slug
        .as_str()
        .split_once('/')
        .ok_or_else(|| io_error("repo slug must be owner/repo"))?;
    for part in [owner, repo] {
        // RepoSlug 允许 `.`；`.` / `..` 组件会逃出 mirrors 目录。
        if part == "." || part == ".." {
            return Err(io_error("repo slug component must not be . or .."));
        }
    }
    Ok(mirrors_dir.join(owner).join(format!("{repo}.git")))
}

/// 读取 `gantry-project`：第一行项目 ID，第二行 slug。
pub fn read_project_file(git_dir: &Path) -> Option<(ProjectId, RepoSlug)> {
    let text = std::fs::read_to_string(git_dir.join(PROJECT_FILE)).ok()?;
    let mut lines = text.lines();
    let project = lines.next()?.parse().ok()?;
    let slug = RepoSlug::parse(lines.next()?).ok()?;
    Some((project, slug))
}

/// 扫描 `mirrors/*/*.git`，返回带 `gantry-project` 的镜像仓。
pub fn scan(mirrors_dir: &Path) -> Vec<(ProjectId, MirrorEntry)> {
    let mut found = Vec::new();
    let Ok(owners) = std::fs::read_dir(mirrors_dir) else {
        return found;
    };
    for owner in owners.flatten() {
        let Ok(repos) = std::fs::read_dir(owner.path()) else {
            continue;
        };
        for repo in repos.flatten() {
            // 只认 `<repo>.git`；重建时改名保留的 `<repo>.git.broken-*` 不是活的镜像仓。
            if !repo.file_name().to_string_lossy().ends_with(".git") {
                continue;
            }
            let git_dir = repo.path();
            if let Some((project, slug)) = read_project_file(&git_dir) {
                found.push((
                    project,
                    MirrorEntry {
                        slug,
                        git_dir,
                        health: MirrorHealth::Healthy,
                    },
                ));
            }
        }
    }
    found
}

/// 在临时目录里建好裸仓，成功后原子 rename 到 `dest`。
pub async fn create(
    env: &GitEnv,
    tmp_root: &Path,
    template: &Path,
    project: &ProjectId,
    slug: &RepoSlug,
    dest: &Path,
) -> Result<(), RepoError> {
    let tmp = tmp_root.join(format!("mirror-{project}"));
    remove_dir_if_exists(&tmp)?;
    std::fs::create_dir_all(template).map_err(io)?;
    GitCmd::new("init", tmp_root)
        .arg("init")
        .arg("--bare")
        .arg("--quiet")
        .arg(format!("--template={}", template.display()))
        .arg(&tmp)
        .run(env)
        .await?;
    for (key, value) in MIRROR_CONFIG {
        GitCmd::new("config", &tmp)
            .args(["config", key, value])
            .run(env)
            .await?;
    }
    std::fs::create_dir_all(tmp.join("hooks")).map_err(io)?;
    let _ = std::fs::remove_file(tmp.join("description"));
    File::create(tmp.join(LOCK_FILE)).map_err(io)?;
    std::fs::write(tmp.join(PROJECT_FILE), format!("{project}\n{slug}\n")).map_err(io)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    std::fs::rename(&tmp, dest).map_err(io)?;
    Ok(())
}

/// `git cat-file`：提交对象的 parent、tree、trailer。不存在或不是提交 → `None`。
pub async fn commit_info(
    env: &GitEnv,
    git_dir: &Path,
    oid: &GitOid,
) -> Result<Option<CommitInfo>, RepoError> {
    let kind = GitCmd::new("cat-file", git_dir)
        .args(["cat-file", "-t"])
        .arg(oid.to_string())
        .output(env)
        .await?;
    if !kind.success() || kind.stdout_str()?.trim() != "commit" {
        return Ok(None);
    }
    let raw = GitCmd::new("cat-file", git_dir)
        .args(["cat-file", "commit"])
        .arg(oid.to_string())
        .run(env)
        .await?;
    let (header, message) = split_commit(&raw.stdout);
    let mut parents = Vec::new();
    let mut tree = None;
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("tree ") {
            tree = GitOid::parse(value.trim()).ok();
        } else if let Some(value) = line.strip_prefix("parent ") {
            parents.push(GitOid::parse(value.trim()).map_err(|_| io_error("bad parent oid"))?);
        }
    }
    let tree = tree.ok_or_else(|| io_error("commit without tree"))?;
    let parsed = GitCmd::new("interpret-trailers", git_dir)
        .args(["interpret-trailers", "--parse"])
        .stdin(message)
        .run(env)
        .await?;
    let trailers = String::from_utf8_lossy(&parsed.stdout)
        .lines()
        .filter_map(|line| line.split_once(": "))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    Ok(Some(CommitInfo {
        parents,
        tree,
        trailers,
    }))
}

fn split_commit(raw: &[u8]) -> (String, Vec<u8>) {
    let text = raw;
    match text.windows(2).position(|pair| pair == b"\n\n") {
        Some(at) => (
            String::from_utf8_lossy(&text[..at]).into_owned(),
            text[at + 2..].to_vec(),
        ),
        None => (String::from_utf8_lossy(text).into_owned(), Vec::new()),
    }
}

pub async fn has_commit(env: &GitEnv, git_dir: &Path, oid: &GitOid) -> Result<bool, RepoError> {
    let out = GitCmd::new("cat-file", git_dir)
        .args(["cat-file", "-e"])
        .arg(format!("{oid}^{{commit}}"))
        .output(env)
        .await?;
    Ok(out.success())
}

/// 完整 ref 必须精确存在；其他修订按既有 Git 语义解析。
pub async fn resolve(env: &GitEnv, git_dir: &Path, rev: &str) -> Result<Option<GitOid>, RepoError> {
    if rev.is_empty() || rev.starts_with('-') {
        return Ok(None);
    }
    let oid;
    let rev = if rev.starts_with("refs/") {
        if rev.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Ok(None);
        }
        // rev-parse 对不存在的 ref 可能按 git describe 解析；先按字面精确查找。
        let out = GitCmd::new("show-ref", git_dir)
            .args(["show-ref", "--verify", "--hash", "--"])
            .arg(rev)
            .output(env)
            .await?;
        if !out.success() {
            return Ok(None);
        }
        let Some(found) = GitOid::parse(out.stdout_str()?.trim()).ok() else {
            return Ok(None);
        };
        oid = found.to_string();
        oid.as_str()
    } else {
        if rev.chars().any(|c| c.is_whitespace() || c == '\0') {
            return Ok(None);
        }
        rev
    };
    let out = GitCmd::new("rev-parse", git_dir)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{rev}^{{commit}}"))
        .output(env)
        .await?;
    if !out.success() {
        return Ok(None);
    }
    Ok(GitOid::parse(out.stdout_str()?.trim()).ok())
}

/// 完整性检查：`git fsck --connectivity-only`。
pub async fn check(env: &GitEnv, git_dir: &Path) -> Result<MirrorHealth, RepoError> {
    if !git_dir.join(PROJECT_FILE).is_file() {
        return Ok(MirrorHealth::NeedsRebuild {
            reason: "mirror layout is incomplete".into(),
        });
    }
    let out = GitCmd::new("fsck", git_dir)
        .args([
            "fsck",
            "--connectivity-only",
            "--no-progress",
            "--no-dangling",
        ])
        .output(env)
        .await?;
    if out.success() {
        Ok(MirrorHealth::Healthy)
    } else {
        Ok(MirrorHealth::NeedsRebuild {
            reason: format!("git fsck exited with {:?}", out.status),
        })
    }
}

// ---------------------------------------------------------------------------
// flock
// ---------------------------------------------------------------------------

/// 镜像仓的 flock 守卫；Drop 即释放。
#[derive(Debug)]
pub struct MirrorLock {
    _file: File,
}

pub async fn lock(git_dir: &Path, exclusive: bool) -> Result<MirrorLock, RepoError> {
    let path = git_dir.join(LOCK_FILE);
    tokio::task::spawn_blocking(move || lock_blocking(&path, exclusive))
        .await
        .map_err(|error| io_error(&format!("lock task failed: {error}")))?
}

fn lock_blocking(path: &Path, exclusive: bool) -> Result<MirrorLock, RepoError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io)?;
    flock(&file, exclusive)?;
    Ok(MirrorLock { _file: file })
}

#[cfg(unix)]
fn flock(file: &File, exclusive: bool) -> Result<(), RepoError> {
    use std::os::unix::io::AsRawFd;
    let op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    loop {
        // SAFETY: fd 在 `file` 存活期间有效。
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(io(error));
        }
    }
}

#[cfg(not(unix))]
fn flock(_file: &File, _exclusive: bool) -> Result<(), RepoError> {
    Ok(())
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

pub fn io(error: std::io::Error) -> RepoError {
    RepoError::Io {
        message: error.to_string(),
    }
}

pub fn io_error(message: &str) -> RepoError {
    RepoError::Io {
        message: message.to_owned(),
    }
}

pub fn remove_dir_if_exists(path: &Path) -> Result<(), RepoError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(error)),
    }
}
