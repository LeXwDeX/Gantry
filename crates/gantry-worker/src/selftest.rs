//! 镜像自检 `gantry-worker selftest`（K6；worker.md §7.8、§12.J）。
//!
//! 在镜像构建的最后一步与 CI 中运行：非 root、全文件系统无 setuid / setgid、没有 `gh`、
//! 系统级 git 凭据助手为空、`safe.directory` 已设置、`claude` 版本等于钉死的版本、
//! `git` / `rg` / `jq` 都在 `PATH` 里。系统访问经 [`Probe`]，便于测试。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::claude_cli::PINNED_CLAUDE_VERSION;

/// 必须在 `PATH` 里的工具。
pub const REQUIRED_TOOLS: [&str; 4] = ["git", "rg", "jq", "claude"];
/// 不得出现在 `PATH` 里的工具。
pub const FORBIDDEN_TOOLS: [&str; 1] = ["gh"];
/// 扫描 setuid / setgid 时跳过的伪文件系统。
const SKIP_DIRS: [&str; 3] = ["/proc", "/sys", "/dev"];

/// 自检需要的系统访问。
pub trait Probe {
    fn uid(&self) -> u32;
    /// 在 `PATH` 里找可执行文件。
    fn which(&self, program: &str) -> Option<PathBuf>;
    /// 运行命令，成功时返回 stdout（失败或无输出时为空串）。
    fn output(&self, program: &str, args: &[&str]) -> Option<String>;
    /// 扫描根文件系统上带 setuid / setgid 位的普通文件。
    fn setid_scan(&self) -> SetidScan;
}

/// setuid / setgid 扫描结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetidScan {
    pub found: Vec<PathBuf>,
    /// 当前用户能进入但无法列出或读取元数据的路径：里面可能有 setid 文件，按"无法核实"处理。
    pub unverifiable: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

pub fn run_checks(probe: &dyn Probe) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut push = |name, ok, detail: String| checks.push(Check { name, ok, detail });

    let uid = probe.uid();
    push("non_root", uid != 0, format!("uid={uid}"));

    let scan = probe.setid_scan();
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .take(5)
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    push(
        "no_setid_files",
        scan.found.is_empty() && scan.unverifiable.is_empty(),
        format!(
            "found: [{}]; unverifiable: [{}]",
            list(&scan.found),
            list(&scan.unverifiable)
        ),
    );

    for tool in FORBIDDEN_TOOLS {
        let found = probe.which(tool);
        push(
            "no_gh",
            found.is_none(),
            found.map(|p| p.display().to_string()).unwrap_or_default(),
        );
    }

    let helpers = probe
        .output(
            "git",
            &["config", "--system", "--get-all", "credential.helper"],
        )
        .unwrap_or_default();
    let helpers: Vec<&str> = helpers.lines().filter(|l| !l.trim().is_empty()).collect();
    push(
        "git_credential_helper_empty",
        helpers.is_empty(),
        helpers.join(", "),
    );

    let safe = probe
        .output(
            "git",
            &["config", "--system", "--get-all", "safe.directory"],
        )
        .unwrap_or_default();
    push(
        "git_safe_directory",
        safe.lines().any(|l| !l.trim().is_empty()),
        safe.trim().to_owned(),
    );

    for tool in REQUIRED_TOOLS {
        let found = probe.which(tool);
        push(
            "tool_on_path",
            found.is_some(),
            format!(
                "{tool}: {}",
                found
                    .map(|p| p.display().to_string())
                    .unwrap_or("missing".into())
            ),
        );
    }

    let version = probe
        .output("claude", &["--version"])
        .and_then(|out| out.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default();
    push(
        "claude_version_pinned",
        version == PINNED_CLAUDE_VERSION,
        format!("found {version:?}, pinned {PINNED_CLAUDE_VERSION}"),
    );
    checks
}

/// 真实系统。
pub struct SystemProbe {
    pub root: PathBuf,
}

impl Default for SystemProbe {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }
}

impl Probe for SystemProbe {
    fn uid(&self) -> u32 {
        #[cfg(unix)]
        {
            unsafe { libc::getuid() }
        }
        #[cfg(not(unix))]
        {
            u32::MAX
        }
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| is_executable(candidate))
    }

    fn output(&self, program: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(program)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn setid_scan(&self) -> SetidScan {
        scan_setid(&self.root)
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// 递归扫描 `root` 所在的文件系统（同 `find -xdev`：不跨挂载点、不跟随符号链接，
/// 跳过伪文件系统）。挂载进来的工作区等目录不是镜像内容。
///
/// 列不出内容的目录：当前用户不能进入（无 `x`）时，里面的文件它执行不到，安全跳过；
/// 能进入却列不出（如 `0711`）时记为无法核实，自检据此失败，而不是当成"没有"。
pub fn scan_setid(root: &Path) -> SetidScan {
    let mut scan = SetidScan::default();
    match fs::symlink_metadata(root) {
        Ok(meta) => scan_dir(root, device_of(&meta), &mut scan),
        Err(_) => scan.unverifiable.push(root.to_path_buf()),
    }
    scan
}

fn scan_dir(dir: &Path, device: u64, scan: &mut SetidScan) {
    if SKIP_DIRS.iter().any(|skip| dir == Path::new(skip)) {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            if searchable(dir) {
                scan.unverifiable.push(dir.to_path_buf());
            }
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            scan.unverifiable.push(dir.to_path_buf());
            continue;
        };
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                scan.unverifiable.push(path);
                continue;
            }
        };
        if device_of(&meta) != device {
            continue;
        }
        if meta.is_dir() {
            scan_dir(&path, device, scan);
        } else if meta.is_file() && has_setid(&meta) {
            scan.found.push(path);
        }
    }
}

/// 当前用户能否进入目录（`access(X_OK)`）。
fn searchable(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return true;
        };
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        true
    }
}

fn device_of(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.dev()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn has_setid(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o6000 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}
