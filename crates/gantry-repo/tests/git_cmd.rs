//! repo.md §12.F：`GitCmd` 是唯一的 git 入口，固定环境与 `-c` 逐项出现在每条命令里。
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Fixture, TempDir};
use gantry_core::ports::{RepoError, RepoService};
use gantry_repo::git::{discover_git, GitCmd, GitEnv, FIXED_CONFIG};

/// sh 自己会补的变量；不来自 GitCmd。
const SHELL_VARS: &[&str] = &["PWD", "SHLVL", "_", "OLDPWD"];

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// 记录 argv / env 后转交真实 git 的假 git。
fn recording_git(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("git.log");
    let script = dir.join("fake-git");
    let real = discover_git().expect("git on PATH");
    write_script(
        &script,
        &format!(
            "#!/bin/sh\n{{ printf 'ARGV'; for a in \"$@\"; do printf '\\t%s' \"$a\"; done; printf '\\n'; env | sed 's/^/ENV\\t/'; printf 'END\\n'; }} >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    );
    (script, log)
}

struct Invocation {
    argv: Vec<String>,
    env: Vec<(String, String)>,
}

fn parse_log(log: &Path) -> Vec<Invocation> {
    let text = std::fs::read_to_string(log).unwrap();
    let mut out = Vec::new();
    let mut current: Option<Invocation> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("ARGV") {
            current = Some(Invocation {
                argv: rest.split('\t').skip(1).map(str::to_owned).collect(),
                env: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("ENV\t") {
            if let (Some(inv), Some((k, v))) = (current.as_mut(), rest.split_once('=')) {
                inv.env.push((k.to_owned(), v.to_owned()));
            }
        } else if line == "END" {
            out.extend(current.take());
        }
    }
    out
}

#[tokio::test]
async fn every_git_invocation_carries_fixed_env_and_config() {
    let scripts = TempDir::new("gitcmd-scripts");
    let (script, log) = recording_git(scripts.path());
    // 让服务端进程环境里带一个不该泄漏的变量。
    std::env::set_var("GANTRY_LEAK_CANARY", "leak");
    let fx = Fixture::with_config("gitcmd-record", |cfg| cfg.git_bin = Some(script.clone())).await;
    fx.seed().await;
    fx.repo.check_mirror(&fx.ctx(), &fx.project).await.unwrap();

    let invocations = parse_log(&log);
    assert!(invocations.len() >= 5, "expected several git calls");
    let home = fx.data_dir().join("repo-home");
    let expected_env: Vec<(String, String)> = [
        ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".to_owned()),
        ("HOME", home.display().to_string()),
        ("GIT_TERMINAL_PROMPT", "0".to_owned()),
        ("GIT_ATTR_NOSYSTEM", "1".to_owned()),
        ("GIT_OPTIONAL_LOCKS", "0".to_owned()),
        ("LC_ALL", "C".to_owned()),
        ("GIT_ALLOW_PROTOCOL", "file".to_owned()),
        ("PATH", "/usr/bin:/bin".to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for inv in &invocations {
        for entry in FIXED_CONFIG {
            let present = inv
                .argv
                .windows(2)
                .any(|pair| pair[0] == "-c" && pair[1] == *entry);
            assert!(present, "missing -c {entry} in {:?}", inv.argv);
        }
        let mut env: Vec<_> = inv
            .env
            .iter()
            .filter(|(k, _)| !SHELL_VARS.contains(&k.as_str()))
            .cloned()
            .collect();
        env.sort();
        let mut want = expected_env.clone();
        want.sort();
        assert_eq!(env, want, "env of {:?}", inv.argv);
    }
}

#[tokio::test]
async fn timeout_kills_the_process_group() {
    let scripts = TempDir::new("gitcmd-timeout");
    let marker = scripts.path().join("survived");
    let script = scripts.path().join("slow-git");
    write_script(
        &script,
        &format!(
            "#!/bin/sh\n(sleep 2; touch '{}') &\nsleep 30\n",
            marker.display()
        ),
    );
    let env = GitEnv {
        git_bin: script,
        home: scripts.path().to_path_buf(),
        timeout: Duration::from_millis(300),
        max_output: 1024,
    };
    let result = GitCmd::new("version", scripts.path())
        .arg("version")
        .run(&env)
        .await;
    assert!(matches!(result, Err(RepoError::Timeout { op: "version" })));
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(
        !marker.exists(),
        "children in the process group must be killed"
    );
}

#[tokio::test]
async fn stderr_tail_is_sanitized_untrusted_text() {
    let scripts = TempDir::new("gitcmd-stderr");
    let script = scripts.path().join("noisy-git");
    write_script(
        &script,
        "#!/bin/sh\nprintf 'bad \\033[31mred\\033[0m \\342\\200\\256evil\\n' >&2\nexit 3\n",
    );
    let env = GitEnv {
        git_bin: script,
        home: scripts.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        max_output: 1024,
    };
    let Err(RepoError::Git {
        op,
        status,
        stderr_tail,
    }) = GitCmd::new("probe", scripts.path())
        .arg("x")
        .run(&env)
        .await
    else {
        panic!("expected a git error");
    };
    assert_eq!(op, "probe");
    assert_eq!(status, Some(3));
    let text = stderr_tail.as_untrusted_str();
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{202e}'),
        "{text:?}"
    );
    assert!(text.contains("bad") && text.contains("evil"));
}

/// 源码扫描：`src/` 里只有 `git.rs` 构造子进程（clippy `disallowed_methods` 同时强制）。
#[test]
fn only_git_cmd_spawns_processes() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap().flatten() {
        let path = entry.path();
        let text = std::fs::read_to_string(&path).unwrap();
        for (no, line) in text.lines().enumerate() {
            if line.contains("Command::new") {
                hits.push(format!(
                    "{}:{}",
                    path.file_name().unwrap().to_string_lossy(),
                    no + 1
                ));
            }
        }
    }
    assert_eq!(hits.len(), 1, "unexpected process spawns: {hits:?}");
    assert!(hits[0].starts_with("git.rs:"), "{hits:?}");
}
