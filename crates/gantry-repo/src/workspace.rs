//! 准备沙盒工作区（R4，repo.md §7.3；后置条件以 worker-protocol §2.4 为准）。
//!
//! `dest` 是 sandbox 建好的空目录。本模块只往里写，并在交还之前做自检；
//! 交还之后绝不再读它（沙盒会往里写东西，I8）。对象经 `file://` 传输从镜像仓
//! 复制过来，不用 hardlink / alternates / `--shared`，工作区与镜像仓不共享 inode。

use std::path::{Path, PathBuf};
use std::time::Instant;

use gantry_core::canon::GitOid;
use gantry_core::ports::{PrepareWorkspace, PreparedWorkspace, RepoError, WorkspaceStats};

use crate::git::{GitCmd, GitEnv};
use crate::mirror::{io, io_error};

/// 自检失败时用的 `op`。
const SELF_CHECK: &str = "workspace-self-check";

pub async fn prepare(
    env: &GitEnv,
    mirror: &Path,
    template: &Path,
    req: &PrepareWorkspace,
) -> Result<PreparedWorkspace, RepoError> {
    let started = Instant::now();
    if req.want_diff {
        // diff.patch 与候选 head 工作区属于 M2（repo.md §1.1）。
        return Err(io_error("want_diff (diff.patch) is an M2 capability"));
    }
    ensure_empty_dir(&req.dest)?;
    match populate(env, mirror, template, req).await {
        Ok((tree, stats)) => Ok(PreparedWorkspace {
            work_dir: req.dest.clone(),
            head: req.base_sha,
            tree,
            diff_patch: None,
            stats: WorkspaceStats {
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..stats
            },
        }),
        Err(error) => {
            clear_dir(&req.dest)?;
            Err(error)
        }
    }
}

async fn populate(
    env: &GitEnv,
    mirror: &Path,
    template: &Path,
    req: &PrepareWorkspace,
) -> Result<(GitOid, WorkspaceStats), RepoError> {
    let dest = &req.dest;
    GitCmd::new("workspace-init", dest)
        .args(["init", "--quiet"])
        .arg(format!("--template={}", template.display()))
        .arg(".")
        .run(env)
        .await?;
    // 仅在这一条命令上允许按 SHA 取对象；file:// 走 upload-pack 打包传输，即复制。
    GitCmd::new("workspace-fetch", dest)
        .config("uploadpack.allowAnySHA1InWant=true")
        .args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
        ])
        .arg(format!("file://{}", mirror.display()))
        .arg(req.base_sha.to_string())
        .run(env)
        .await?;
    GitCmd::new("workspace-checkout", dest)
        .args(["checkout", "--quiet", "--detach", "--no-recurse-submodules"])
        .arg(req.base_sha.to_string())
        .run(env)
        .await?;
    scrub(dest)?;
    self_check(env, dest, &req.base_sha).await?;
    let stats = measure(dest)?;
    if stats.bytes > req.max_bytes {
        return Err(RepoError::WorkspaceTooLarge {
            bytes: stats.bytes,
            max: req.max_bytes,
        });
    }
    let tree = GitCmd::new("rev-parse", dest)
        .args(["rev-parse", "--verify", "HEAD^{tree}"])
        .run(env)
        .await?;
    let tree = GitOid::parse(tree.stdout_str()?.trim()).map_err(|_| io_error("bad tree oid"))?;
    Ok((tree, stats))
}

/// 去掉 remote、hooks 与无用的元数据；`.git/config` 只留 `git init` 写入的 `core` 项。
fn scrub(dest: &Path) -> Result<(), RepoError> {
    let git_dir = dest.join(".git");
    for name in ["FETCH_HEAD", "ORIG_HEAD", "description", "packed-refs"] {
        let _ = std::fs::remove_file(git_dir.join(name));
    }
    let hooks = git_dir.join("hooks");
    crate::mirror::remove_dir_if_exists(&hooks)?;
    std::fs::create_dir_all(&hooks).map_err(io)?;
    crate::mirror::remove_dir_if_exists(&git_dir.join("refs/remotes"))?;
    let _ = std::fs::remove_file(git_dir.join("info/attributes"));
    Ok(())
}

/// 后置条件（repo.md §7.3 步 7），失败返回 `Git { op: "workspace-self-check" }`。
async fn self_check(env: &GitEnv, dest: &Path, base: &GitOid) -> Result<(), RepoError> {
    let status = GitCmd::new("status", dest)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .run(env)
        .await?;
    if !status.stdout.is_empty() {
        return Err(self_check_failed("worktree is not clean"));
    }
    let symbolic = GitCmd::new("symbolic-ref", dest)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output(env)
        .await?;
    if symbolic.success() {
        return Err(self_check_failed("HEAD is not detached"));
    }
    let head = GitCmd::new("rev-parse", dest)
        .args(["rev-parse", "--verify", "HEAD"])
        .run(env)
        .await?;
    if head.stdout_str()?.trim() != base.to_string() {
        return Err(self_check_failed("HEAD is not base_sha"));
    }
    let refs = GitCmd::new("for-each-ref", dest)
        .args(["for-each-ref", "--format=%(refname)"])
        .run(env)
        .await?;
    if !refs.stdout.is_empty() {
        return Err(self_check_failed("workspace carries refs"));
    }
    let git_dir = dest.join(".git");
    if git_dir.join("objects/info/alternates").exists() {
        return Err(self_check_failed("alternates present"));
    }
    let config = std::fs::read_to_string(git_dir.join("config")).map_err(io)?;
    for line in config.lines() {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with("[remote")
            || lower.starts_with("[include")
            || lower.starts_with("[credential")
            || lower.starts_with("url")
        {
            return Err(self_check_failed(
                "config carries remote / include / credential",
            ));
        }
    }
    if dir_has_files(&git_dir.join("hooks"))? {
        return Err(self_check_failed("hooks present"));
    }
    check_single_links(&git_dir)?;
    GitCmd::new("fsck", dest)
        .args([
            "fsck",
            "--connectivity-only",
            "--no-progress",
            "--no-dangling",
        ])
        .run(env)
        .await?;
    Ok(())
}

fn self_check_failed(reason: &str) -> RepoError {
    RepoError::Git {
        op: SELF_CHECK,
        status: None,
        stderr_tail: gantry_core::text::UntrustedText::new(
            reason.to_owned(),
            "repo:workspace-self-check".into(),
            false,
        ),
    }
}

/// `.git` 下所有普通文件 `st_nlink == 1`（没有指向镜像仓的硬链接）。
fn check_single_links(dir: &Path) -> Result<(), RepoError> {
    for path in walk(dir)? {
        let meta = std::fs::symlink_metadata(&path).map_err(io)?;
        if meta.is_file() && nlink(&meta) != 1 {
            return Err(self_check_failed("hardlinked file under .git"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn nlink(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn nlink(_meta: &std::fs::Metadata) -> u64 {
    1
}

fn measure(dest: &Path) -> Result<WorkspaceStats, RepoError> {
    let mut stats = WorkspaceStats::default();
    for path in walk(dest)? {
        let meta = std::fs::symlink_metadata(&path).map_err(io)?;
        if meta.is_file() || meta.file_type().is_symlink() {
            stats.bytes += meta.len();
            stats.files += 1;
        }
    }
    Ok(stats)
}

/// 不跟随符号链接的递归遍历。
fn walk(root: &Path) -> Result<Vec<PathBuf>, RepoError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(io)? {
            let entry = entry.map_err(io)?;
            let kind = entry.file_type().map_err(io)?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    Ok(out)
}

fn dir_has_files(dir: &Path) -> Result<bool, RepoError> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(error)),
    }
}

fn ensure_empty_dir(dest: &Path) -> Result<(), RepoError> {
    let meta = std::fs::symlink_metadata(dest).map_err(io)?;
    if !meta.is_dir() {
        return Err(io_error("workspace dest must be a directory"));
    }
    if dir_has_files(dest)? {
        return Err(io_error("workspace dest must be empty"));
    }
    Ok(())
}

/// 失败时清空 `dest` 的内容，保留目录本身（它归 sandbox）。
fn clear_dir(dest: &Path) -> Result<(), RepoError> {
    for entry in std::fs::read_dir(dest).map_err(io)? {
        let entry = entry.map_err(io)?;
        let path = entry.path();
        if entry.file_type().map_err(io)?.is_dir() {
            std::fs::remove_dir_all(&path).map_err(io)?;
        } else {
            std::fs::remove_file(&path).map_err(io)?;
        }
    }
    Ok(())
}
