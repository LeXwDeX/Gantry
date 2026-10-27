//! `changes.patch` 的生成（A5；worker-protocol §5.3；worker.md §7.5）。
//!
//! 适配层对工作区里的 `.git` 也不信任：git 命令一律在**干净的临时 `GIT_DIR`** 上运行——
//! 它只经 `objects/info/alternates` 借用工作区的对象库，配置、hooks、`info/`、index 都是
//! 适配层自己的。所以工作区 `.git/config` 里的 `include.path`、`filter.*`、`diff.external`、
//! `core.fsmonitor`、`core.hooksPath`，`.git/hooks/` 里的脚本，以及 `.gitattributes` 引用的
//! 外部驱动都不会被执行（驱动只能在配置里定义）。最坏结果只是一份无效补丁，服务端会拒绝。
//!
//! 比较的是"以 `base_sha` 为起点、`add -A` 之后的 index"与 `base_sha` 的树，所以 agent
//! 自己 `commit` / `reset` / 改写历史都不影响结果。

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use gantry_core::worker::{JobSpec, PatchArtifact, PatchMode};

use crate::artifacts::{write_atomic, ArtifactError, CHANGES_PATCH};
use crate::events::WarnCode;

/// 每条 git 命令的墙钟上限 **[建议值，待调优]**。
pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("git {0} failed: {1}")]
    Git(&'static str, String),
    #[error("git {0} timed out")]
    Timeout(&'static str),
    #[error("cannot prepare a clean git dir: {0}")]
    Setup(io::Error),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
}

/// git 调用；测试里用记录调用的替身。
pub trait Git {
    fn run(&mut self, step: &'static str, args: &[&str]) -> Result<Vec<u8>, PatchError>;
}

/// 在干净 `GIT_DIR` 上运行系统 `git`。
pub struct CleanGit {
    git_dir: PathBuf,
    work: PathBuf,
    index: PathBuf,
    home: PathBuf,
    timeout: Duration,
}

impl CleanGit {
    /// 在 `scratch` 下建干净的 `GIT_DIR`，对象库借用 `<work>/.git/objects`。
    pub fn prepare(work: &Path, scratch: &Path, timeout: Duration) -> Result<Self, PatchError> {
        let objects = work.join(".git").join("objects");
        if !objects.is_dir() {
            return Err(PatchError::Setup(io::Error::new(
                io::ErrorKind::NotFound,
                "workspace has no .git/objects directory",
            )));
        }
        let git_dir = scratch.join("gantry-git");
        let home = scratch.join("gantry-git-home");
        for dir in [
            git_dir.join("objects").join("info"),
            git_dir.join("refs").join("heads"),
            home.clone(),
        ] {
            fs::create_dir_all(dir).map_err(PatchError::Setup)?;
        }
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/gantry\n").map_err(PatchError::Setup)?;
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfilemode = true\n\
             \tautocrlf = false\n\tsafecrlf = false\n\tfsmonitor = false\n\thooksPath = /dev/null\n",
        )
        .map_err(PatchError::Setup)?;
        let objects = objects.canonicalize().map_err(PatchError::Setup)?;
        fs::write(
            git_dir.join("objects").join("info").join("alternates"),
            format!("{}\n", objects.display()),
        )
        .map_err(PatchError::Setup)?;
        Ok(Self {
            index: git_dir.join("index"),
            git_dir,
            work: work.to_path_buf(),
            home,
            timeout,
        })
    }
}

impl Git for CleanGit {
    fn run(&mut self, step: &'static str, args: &[&str]) -> Result<Vec<u8>, PatchError> {
        let mut child = Command::new("git")
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| PatchError::Git(step, err.kind().to_string()))?;
        let mut stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let out = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let err = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.take(4096).read_to_end(&mut buf);
            buf
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| PatchError::Git(step, e.to_string()))?
            {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PatchError::Timeout(step));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let stdout = out.join().unwrap_or_default();
        let stderr = err.join().unwrap_or_default();
        if !status.success() {
            // stderr 只取首行，作诊断；不含文件内容。
            let first = String::from_utf8_lossy(&stderr)
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            return Err(PatchError::Git(step, first));
        }
        Ok(stdout)
    }
}

/// 补丁生成的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchOutcome {
    /// 无改动或 `output.patch = forbidden` 时为 `None`（不产生文件）。
    pub artifact: Option<PatchArtifact>,
    pub warnings: Vec<WarnCode>,
}

/// 生成并写出 `changes.patch`。必须在 agent 及其全部子进程终止之后调用。
pub fn package_patch(
    job: &JobSpec,
    git: &mut dyn Git,
    out: &Path,
) -> Result<PatchOutcome, PatchError> {
    if job.output.patch == PatchMode::Forbidden {
        // 只读角色：不运行任何写 index 的 git 命令。
        return Ok(PatchOutcome {
            artifact: None,
            warnings: Vec::new(),
        });
    }
    let base = job.workspace.base_sha.to_string();
    let base = base.as_str();
    git.run("read-tree", &["read-tree", base])?;
    git.run("add", &["add", "-A", "--", "."])?;
    let patch = git.run(
        "diff",
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--full-index",
            "-U3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
        ],
    )?;
    if patch.is_empty() {
        return Ok(PatchOutcome {
            artifact: None,
            warnings: Vec::new(),
        });
    }
    let raw = git.run(
        "diff-raw",
        &[
            "diff",
            "--cached",
            "--raw",
            "--no-renames",
            "--no-abbrev",
            "-z",
            base,
        ],
    )?;
    let numstat = git.run(
        "diff-numstat",
        &["diff", "--cached", "--numstat", "--no-renames", "-z", base],
    )?;
    let files_changed = raw_entries(&raw).len() as u32;
    let mut warnings = Vec::new();
    let modes = raw_entries(&raw);
    if numstat_has_binary(&numstat) {
        warnings.push(WarnCode::BinaryDetected);
    }
    if modes.iter().any(|(a, b)| a == "120000" || b == "120000") {
        warnings.push(WarnCode::SymlinkDetected);
    }
    if modes.iter().any(|(a, b)| a == "160000" || b == "160000") {
        warnings.push(WarnCode::GitlinkDetected);
    }
    if let Some(limits) = &job.constraints.patch {
        if patch.len() as u64 > limits.max_bytes || files_changed > limits.max_files {
            warnings.push(WarnCode::PatchTooLarge);
        }
    }
    let reference = write_atomic(out, CHANGES_PATCH, &patch)?;
    Ok(PatchOutcome {
        artifact: Some(PatchArtifact {
            path: reference.path,
            sha256: reference.sha256,
            bytes: reference.bytes,
            base_sha: job.workspace.base_sha,
            files_changed,
        }),
        warnings,
    })
}

/// `--raw -z` 的每个条目：`:<old_mode> <new_mode> <old> <new> <status>\0<path>\0`。
fn raw_entries(raw: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut fields = raw.split(|b| *b == 0);
    while let Some(head) = fields.next() {
        let Some(head) = head.strip_prefix(b":") else {
            continue;
        };
        let _path = fields.next();
        let text = String::from_utf8_lossy(head);
        let mut parts = text.split(' ');
        if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
            out.push((a.to_owned(), b.to_owned()));
        }
    }
    out
}

/// `--numstat -z` 中二进制文件的计数是 `-\t-`。
fn numstat_has_binary(numstat: &[u8]) -> bool {
    numstat
        .split(|b| *b == 0)
        .any(|entry| entry.starts_with(b"-\t-\t"))
}
