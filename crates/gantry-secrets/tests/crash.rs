//! 崩溃注入（secrets.md §12.2）：子进程在 `.tmp` 写完、硬链接前后、`commit` 前后各被
//! `SIGKILL` 一次；重开后 `.sec` 都是完整文件，`staged()` 报告遗留暂存项，
//! `secrets` 不自动提交或丢弃。需要 `fault-injection` feature。

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{ctx, name, sentinel, TempDir};
use gantry_core::ports::secrets::{Expect, SecretSink, SecretSource, SecretValue, SecretVersion};
use gantry_core::secret::Consumer;
use gantry_secrets::{KeyFile, SecretAdminOps, Secrets, SecretsConfig};

const ENV_DIR: &str = "GANTRY_SECRETS_TEST_DIR";
const ENV_KEY: &str = "GANTRY_SECRETS_TEST_KEY";
const ENV_OP: &str = "GANTRY_SECRETS_TEST_OP";

fn config(dir: &Path, key: &Path) -> SecretsConfig {
    SecretsConfig::new(
        dir,
        vec![Consumer::Forge],
        Box::new(KeyFile {
            path: key.to_path_buf(),
        }),
    )
}

fn val(s: &str) -> SecretValue {
    SecretValue::from_bytes(s.as_bytes().to_vec())
}

/// 子进程入口：父进程设置环境变量后以 `--exact crash_child` 运行本测试二进制。
#[test]
fn crash_child() {
    let (Some(dir), Some(key), Some(op)) = (
        std::env::var_os(ENV_DIR),
        std::env::var_os(ENV_KEY),
        std::env::var(ENV_OP).ok(),
    ) else {
        return;
    };
    let secrets = Secrets::open(config(Path::new(&dir), Path::new(&key))).unwrap();
    let s = secrets.scoped(Consumer::Forge).unwrap();
    match op.as_str() {
        "put" => {
            s.put(&name("k"), val("second-value"), Expect::Any, &ctx())
                .unwrap();
        }
        "commit" => {
            let staged = s.staged().unwrap().pop().expect("parent staged one");
            s.commit(&staged, &ctx()).unwrap();
        }
        other => panic!("unknown op {other}"),
    }
}

struct Fixture {
    _tmp: TempDir,
    dir: PathBuf,
    key: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new("crash");
    let key = tmp.path().join("master.key");
    let seed = sentinel();
    let bytes: Vec<u8> = (0..32)
        .map(|i| seed.as_bytes()[i % seed.len()] ^ (i as u8).wrapping_mul(29))
        .collect();
    std::fs::write(&key, bytes).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o400)).unwrap();
    let dir = tmp.path().join("inst");
    Secrets::init(config(&dir, &key)).unwrap();
    Fixture {
        _tmp: tmp,
        dir,
        key,
    }
}

fn run_child(fx: &Fixture, op: &str, point: &str) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
        .env(ENV_DIR, &fx.dir)
        .env(ENV_KEY, &fx.key)
        .env(ENV_OP, op)
        .env("GANTRY_SECRETS_CRASH", point)
        .status()
        .unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "child must die by SIGKILL at {point}"
    );
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn v(n: u64) -> SecretVersion {
    SecretVersion::new(n).unwrap()
}

fn check_put_crash(point: &str, expect_new: bool) {
    let fx = fixture();
    {
        let secrets = Secrets::open(config(&fx.dir, &fx.key)).unwrap();
        secrets
            .scoped(Consumer::Forge)
            .unwrap()
            .put(&name("k"), val("first-value"), Expect::Any, &ctx())
            .unwrap();
    }
    run_child(&fx, "put", point);
    let forge_dir = fx.dir.join("forge");
    assert!(
        names_in(&forge_dir).iter().any(|n| n.starts_with(".tmp.")),
        "{point}: the crash should leave a temp file behind"
    );
    let secrets = Secrets::open(config(&fx.dir, &fx.key)).unwrap();
    assert!(
        !names_in(&forge_dir).iter().any(|n| n.starts_with(".tmp.")),
        "open cleans temp files"
    );
    let s = secrets.reader(Consumer::Forge).unwrap();
    let got = s.get(&name("k")).unwrap().unwrap();
    if expect_new {
        assert_eq!(
            (got.value.expose(), got.version),
            (&b"second-value"[..], v(2))
        );
    } else {
        assert_eq!(
            (got.value.expose(), got.version),
            (&b"first-value"[..], v(1))
        );
    }
    assert!(secrets.admin().staged_all().unwrap().is_empty());
}

#[test]
fn crash_after_tmp_write_keeps_previous_version() {
    check_put_crash("after_tmp", false);
}

#[test]
fn crash_after_link_keeps_complete_new_version() {
    check_put_crash("after_link", true);
}

fn check_commit_crash(point: &str, expect_committed: bool) {
    let fx = fixture();
    let staged = {
        let secrets = Secrets::open(config(&fx.dir, &fx.key)).unwrap();
        let s = secrets.scoped(Consumer::Forge).unwrap();
        s.put(&name("k"), val("first-value"), Expect::Any, &ctx())
            .unwrap();
        s.stage(&name("k"), val("staged-value"), Expect::Any, &ctx())
            .unwrap()
    };
    run_child(&fx, "commit", point);
    let secrets = Secrets::open(config(&fx.dir, &fx.key)).unwrap();
    let s = secrets.scoped(Consumer::Forge).unwrap();
    let got = s.get(&name("k")).unwrap().unwrap();
    if expect_committed {
        // 硬链接已完成：提交生效，open 清掉同版本的多余暂存名。
        assert_eq!(
            (got.value.expose(), got.version),
            (&b"staged-value"[..], v(2))
        );
        assert!(secrets.admin().staged_all().unwrap().is_empty());
    } else {
        // 尚未提交：旧值仍可见，暂存项原样报告，不自动提交或丢弃。
        assert_eq!(
            (got.value.expose(), got.version),
            (&b"first-value"[..], v(1))
        );
        assert_eq!(secrets.admin().staged_all().unwrap(), vec![staged.clone()]);
        assert_eq!(s.staged().unwrap(), vec![staged.clone()]);
        // 调用方对账后可以提交。
        s.commit(&staged, &ctx()).unwrap();
        assert_eq!(
            s.get(&name("k")).unwrap().unwrap().value.expose(),
            b"staged-value"
        );
    }
}

#[test]
fn crash_before_commit_link_leaves_staged_entry() {
    check_commit_crash("commit_before_link", false);
}

#[test]
fn crash_after_commit_link_completes_commit() {
    check_commit_crash("commit_after_link", true);
}
