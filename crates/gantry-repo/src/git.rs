//! `GitCmd`：本模块唯一的 git 子进程入口（repo.md §7.2）。
//!
//! 固定环境与 `-c` 项，不继承服务端进程的环境变量；参数以数组传递，从不拼
//! shell 字符串；每次调用有墙钟超时与输出上限，超时杀掉整个进程组。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use gantry_core::ports::RepoError;
use gantry_core::text::{normalize_untrusted, NormalizeLimits, UntrustedText};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 每条命令都带的 `-c` 固定项（repo.md §7.2）。
pub const FIXED_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.protectNTFS=true",
    "core.protectHFS=true",
    "core.autocrlf=false",
    "core.attributesFile=/dev/null",
    "apply.whitespace=nowarn",
];

/// stderr 只保留尾部，按不可信文本规范化。
const STDERR_TAIL: usize = 4 * 1024;
/// 单次调用写出文件的上限（`RLIMIT_FSIZE`）。
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_OPEN_FILES: u64 = 1024;

/// 所有 git 调用共享的运行参数。
#[derive(Clone, Debug)]
pub struct GitEnv {
    /// git 可执行文件的绝对路径（启动时解析一次；测试可换成记录 argv / env 的假 git）。
    pub git_bin: PathBuf,
    /// 空的 `HOME`，确保读不到任何用户级配置。
    pub home: PathBuf,
    pub timeout: Duration,
    /// stdout 上限；超出视为失败。
    pub max_output: usize,
}

impl GitEnv {
    /// 固定环境变量（不含调用方显式追加的身份变量）。
    pub fn fixed_env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("HOME", self.home.clone().into_os_string()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("GIT_ATTR_NOSYSTEM", "1".into()),
            ("GIT_OPTIONAL_LOCKS", "0".into()),
            ("LC_ALL", "C".into()),
            ("GIT_ALLOW_PROTOCOL", "file".into()),
            ("PATH", "/usr/bin:/bin".into()),
        ]
    }
}

/// 允许调用方追加的环境变量：只有提交身份与日期，以及临时 index。
const ALLOWED_EXTRA_ENV: &[&str] = &[
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
    "GIT_INDEX_FILE",
];

#[derive(Debug)]
pub struct GitOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr_tail: UntrustedText,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_str(&self) -> Result<&str, RepoError> {
        std::str::from_utf8(&self.stdout).map_err(|_| RepoError::Io {
            message: "git output is not UTF-8".into(),
        })
    }
}

/// 一次 git 调用。
pub struct GitCmd {
    op: &'static str,
    cwd: PathBuf,
    config: Vec<String>,
    args: Vec<OsString>,
    env: Vec<(&'static str, OsString)>,
    stdin: Option<Vec<u8>>,
}

impl GitCmd {
    /// `cwd` 只能是镜像仓、`scratch/<job_id>/` 或 `prepare_workspace` 刚建好的 `dest`
    /// （写入期间，沙盒尚未接触它）。
    pub fn new(op: &'static str, cwd: impl Into<PathBuf>) -> Self {
        Self {
            op,
            cwd: cwd.into(),
            config: Vec::new(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
        }
    }

    /// 只作用于这一条命令的额外 `-c`（例如 `uploadpack.allowAnySHA1InWant`）。
    pub fn config(mut self, entry: impl Into<String>) -> Self {
        self.config.push(entry.into());
        self
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    /// 只接受 [`ALLOWED_EXTRA_ENV`] 里的变量；其他名字是编程错误。
    pub fn env(mut self, key: &'static str, value: impl Into<OsString>) -> Self {
        assert!(
            ALLOWED_EXTRA_ENV.contains(&key),
            "GitCmd does not accept environment variable {key}"
        );
        self.env.push((key, value.into()));
        self
    }

    pub fn stdin(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
        self
    }

    pub fn op(&self) -> &'static str {
        self.op
    }

    /// 运行并要求退出码为 0。
    pub async fn run(self, env: &GitEnv) -> Result<GitOutput, RepoError> {
        let op = self.op;
        let output = self.output(env).await?;
        if !output.success() {
            return Err(RepoError::Git {
                op,
                status: output.status,
                stderr_tail: output.stderr_tail,
            });
        }
        Ok(output)
    }

    /// 运行并返回输出，非零退出码不视为错误（由调用方解释）。
    pub async fn output(self, env: &GitEnv) -> Result<GitOutput, RepoError> {
        let op = self.op;
        let mut command = self.build(env);
        let mut child = command.spawn().map_err(|error| RepoError::Io {
            message: format!("spawn git {op}: {error}"),
        })?;
        let pid = child.id();
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let input = self.stdin;
        let max_output = env.max_output;

        let work = async move {
            let feed = async {
                if let (Some(mut pipe), Some(bytes)) = (stdin.take(), input) {
                    // 子进程提前退出时写管道会失败；以退出码为准。
                    let _ = pipe.write_all(&bytes).await;
                    let _ = pipe.shutdown().await;
                }
                drop(stdin);
            };
            let read_out = read_limited(&mut stdout, max_output);
            let read_err = read_tail(&mut stderr, STDERR_TAIL);
            let ((), out, err) = tokio::join!(feed, read_out, read_err);
            let status = child.wait().await.map_err(|error| RepoError::Io {
                message: format!("wait git {op}: {error}"),
            })?;
            Ok::<_, RepoError>((status, out?, err?))
        };

        match tokio::time::timeout(env.timeout, work).await {
            Ok(result) => {
                let (status, stdout, stderr) = result?;
                let (stdout, overflow) = stdout;
                if overflow {
                    return Err(RepoError::Io {
                        message: format!("git {op} output exceeded {max_output} bytes"),
                    });
                }
                Ok(GitOutput {
                    status: status.code(),
                    stdout,
                    stderr_tail: untrusted_tail(&stderr, op),
                })
            }
            Err(_) => {
                kill_group(pid);
                Err(RepoError::Timeout { op })
            }
        }
    }

    fn build(&self, env: &GitEnv) -> tokio::process::Command {
        #[allow(clippy::disallowed_methods)] // 唯一入口
        let mut command = tokio::process::Command::new(&env.git_bin);
        command.env_clear();
        for (key, value) in env.fixed_env() {
            command.env(key, value);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        for entry in FIXED_CONFIG
            .iter()
            .map(|entry| (*entry).to_owned())
            .chain(self.config.iter().cloned())
        {
            command.arg("-c").arg(entry);
        }
        command.args(&self.args);
        command.current_dir(&self.cwd);
        command.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);
        configure_limits(&mut command);
        command
    }
}

#[cfg(unix)]
fn configure_limits(command: &mut tokio::process::Command) {
    command.process_group(0);
    // SAFETY: pre_exec 闭包只调用 async-signal-safe 的 setrlimit。
    unsafe {
        command.pre_exec(|| {
            let fsize = libc::rlimit {
                rlim_cur: MAX_FILE_BYTES as libc::rlim_t,
                rlim_max: MAX_FILE_BYTES as libc::rlim_t,
            };
            let nofile = libc::rlimit {
                rlim_cur: MAX_OPEN_FILES as libc::rlim_t,
                rlim_max: MAX_OPEN_FILES as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &fsize) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_limits(_command: &mut tokio::process::Command) {
    let _ = (MAX_FILE_BYTES, MAX_OPEN_FILES);
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: 向自己建立的进程组发 SIGKILL；进程组已不存在时调用无害。
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<(Vec<u8>, bool), RepoError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut overflow = false;
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|error| RepoError::Io {
                message: format!("read git stdout: {error}"),
            })?;
        if n == 0 {
            break;
        }
        if buf.len() + n > max {
            overflow = true;
            continue; // 继续读空管道，避免子进程阻塞
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok((buf, overflow))
}

async fn read_tail<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    keep: usize,
) -> Result<Vec<u8>, RepoError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|error| RepoError::Io {
                message: format!("read git stderr: {error}"),
            })?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > keep * 2 {
            buf.drain(..buf.len() - keep);
        }
    }
    if buf.len() > keep {
        buf.drain(..buf.len() - keep);
    }
    Ok(buf)
}

fn untrusted_tail(bytes: &[u8], op: &str) -> UntrustedText {
    let normalized = normalize_untrusted(bytes, NormalizeLimits::default());
    UntrustedText::new(
        normalized.text,
        format!("git:{op}:stderr"),
        normalized.stats.truncated,
    )
}

/// 在 `PATH` 里找 git，返回绝对路径。
pub fn discover_git() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
