//! 在镜像仓上运行 git（forge.md §7.10）。
//!
//! 令牌只经环境变量与 askpass 注入：**不进** remote URL、`.git/config`、日志、命令行参数（I1）。
//! 子进程环境清空后只放回 `PATH`；`HOME` 指向空目录、`GIT_CONFIG_NOSYSTEM=1`，不读系统与
//! 用户配置；每条命令都带 `core.hooksPath=/dev/null`，不执行镜像仓里的 hooks（I8）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use gantry_core::secret::SecretString;
use tokio::process::Command;

/// askpass 辅助程序：用户名固定 `x-access-token`（需核实，forge.md §7.10），密码取环境变量。
const ASKPASS: &str = "#!/bin/sh\ncase \"$1\" in\n  Username*) printf '%s\\n' x-access-token ;;\n  *) printf '%s\\n' \"$GANTRY_GIT_TOKEN\" ;;\nesac\n";

#[derive(Debug)]
pub(crate) struct GitOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GitError {
    #[error("git timed out")]
    Timeout,
    #[error("spawn git: {0}")]
    Spawn(String),
}

pub(crate) struct GitRunner {
    root: PathBuf,
    askpass: PathBuf,
    home: PathBuf,
    timeout: Duration,
}

impl GitRunner {
    pub fn new(timeout: Duration) -> std::io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let root =
            std::env::temp_dir().join(format!("gantry-forge-git-{}-{nonce:x}", std::process::id()));
        let home = root.join("home");
        std::fs::create_dir_all(&home)?;
        let askpass = root.join("askpass.sh");
        std::fs::write(&askpass, ASKPASS)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            root,
            askpass,
            home,
            timeout,
        })
    }

    pub async fn run(
        &self,
        git_dir: &Path,
        args: &[&str],
        token: Option<&SecretString>,
    ) -> Result<GitOutput, GitError> {
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ASKPASS", &self.askpass)
            .arg("--git-dir")
            .arg(git_dir)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(token) = token {
            command.env("GANTRY_GIT_TOKEN", token.expose_secret());
        }
        let child = command
            .spawn()
            .map_err(|error| GitError::Spawn(error.kind().to_string()))?;
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| GitError::Timeout)?
            .map_err(|error| GitError::Spawn(error.kind().to_string()))?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

impl Drop for GitRunner {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `git ls-remote` 一行的解析：`<sha>\t<ref>`。
pub(crate) fn ls_remote_sha(stdout: &str, full_ref: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let (sha, name) = line.split_once('\t')?;
        (name.trim() == full_ref).then(|| sha.trim().to_owned())
    })
}
