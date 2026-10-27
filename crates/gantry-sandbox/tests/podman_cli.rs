//! `PodmanDriver` 的命令构造与输出解析（A 级）：用一个记录参数、按子命令回放输出的
//! 假 `podman` 脚本代替真实 Podman。真实运行时行为见 driver_conformance.rs 的
//! `[集成:podman]` 用例（需要 Linux）。

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{spec_for, TempRoot};
use gantry_sandbox::driver::{
    ContainerStatus, DriverCaps, DriverError, LabelSelector, SandboxDriver,
};
use gantry_sandbox::podman_driver::{OutReadMode, PodmanConfig, PodmanDriver};
use gantry_sandbox::spec::{ContainerId, ImageDigest, Runtime, LABEL_MANAGED};

const ID: &str = "4f1c0e2d9b8a7c6e5f4d3c2b1a0918273645546372819a0b1c2d3e4f5a6b7c8d";

struct FakePodman {
    root: TempRoot,
}

impl FakePodman {
    fn new() -> Self {
        let root = TempRoot::new();
        fs::create_dir_all(root.path().join("resp")).unwrap();
        fs::create_dir_all(root.path().join("ws")).unwrap();
        let script = format!(
            r#"#!/bin/sh
LOG='{log}'
RESP='{resp}'
{{ printf 'ARGS'; for a in "$@"; do printf ' [%s]' "$a"; done; printf '\n'; env | sort | sed 's/^/ENV /'; }} >> "$LOG"
sub="$3"
[ "$sub" = container ] && sub="$4"
[ "$sub" = image ] && sub="image-$4"
[ -f "$RESP/$sub.out" ] && cat "$RESP/$sub.out"
[ -f "$RESP/$sub.err" ] && cat "$RESP/$sub.err" >&2
code=0
[ -f "$RESP/$sub.code" ] && code=$(cat "$RESP/$sub.code")
exit $code
"#,
            log = root.path().join("log").display(),
            resp = root.path().join("resp").display(),
        );
        let bin = root.path().join("podman");
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn respond(&self, sub: &str, out: &[u8], err: &str, code: i32) {
        let resp = self.root.path().join("resp");
        fs::write(resp.join(format!("{sub}.out")), out).unwrap();
        fs::write(resp.join(format!("{sub}.err")), err).unwrap();
        fs::write(resp.join(format!("{sub}.code")), code.to_string()).unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.path().join("log")).unwrap_or_default()
    }

    fn workspaces(&self) -> PathBuf {
        self.root.path().join("ws")
    }

    fn driver(&self, out_read: OutReadMode) -> PodmanDriver {
        PodmanDriver::new(PodmanConfig {
            podman_bin: self.root.path().join("podman"),
            socket: PathBuf::from("/run/gantry-sbx/podman.sock"),
            workspaces_dir: self.workspaces(),
            runtime: Runtime::Runc,
            out_read,
            call_timeout: Duration::from_secs(20),
            max_output: 1 << 20,
            caps: DriverCaps {
                runtime: Runtime::Runc,
                oom_reporting: true,
                disk_quota: false,
                supports_pool: false,
                memory_limit: true,
                pids_limit: true,
                cpu_limit: false,
            },
        })
    }
}

fn args_lines(log: &str) -> Vec<&str> {
    log.lines().filter(|l| l.starts_with("ARGS")).collect()
}

fn inspect_json(status: &str, code: i32, oom: bool) -> Vec<u8> {
    format!(
        r#"[{{"Id":"{ID}","State":{{"Status":"{status}","ExitCode":{code},"OOMKilled":{oom}}},"Config":{{"Labels":{{"gantry.managed":"1"}}}}}}]"#
    )
    .into_bytes()
}

#[tokio::test]
async fn create_renders_remote_call_with_clean_env() {
    let podman = FakePodman::new();
    podman.respond("create", format!("{ID}\n").as_bytes(), "", 0);
    let driver = podman.driver(OutReadMode::HostDirect);
    let root_path = podman.workspaces();
    let layout = common::layout(&root_path, 1);
    let spec = common::valid_spec(&layout, common::job_id(1));
    let id = driver.create(&spec).await.unwrap();
    assert_eq!(id, ContainerId::new(ID));
    let log = podman.log();
    let args = args_lines(&log);
    assert_eq!(args.len(), 1);
    let line = args[0];
    assert!(line.starts_with(
        "ARGS [--remote] [--url=unix:///run/gantry-sbx/podman.sock] [create] [--replace] [--name=gantry-sbx-"
    ));
    for flag in [
        "[--read-only]",
        "[--cap-drop=ALL]",
        "[--security-opt=no-new-privileges]",
        "[--runtime=runc]",
        "[--unsetenv-all]",
    ] {
        assert!(line.contains(flag), "missing {flag}");
    }
    // 子进程环境只有固定的三项（脚本自己的 PWD / SHLVL / _ 由 sh 生成）。
    let env: Vec<&str> = log
        .lines()
        .filter_map(|l| l.strip_prefix("ENV "))
        .filter(|l| !l.starts_with("PWD=") && !l.starts_with("SHLVL=") && !l.starts_with("_="))
        .collect();
    assert_eq!(
        env,
        vec!["HOME=/nonexistent", "LC_ALL=C", "PATH=/usr/bin:/bin"]
    );
}

#[tokio::test]
async fn invalid_spec_never_reaches_podman() {
    let podman = FakePodman::new();
    let driver = podman.driver(OutReadMode::HostDirect);
    let layout = common::layout(&podman.workspaces(), 2);
    let mut spec = common::valid_spec(&layout, common::job_id(2));
    spec.network.network = "host".into();
    assert!(matches!(
        driver.create(&spec).await,
        Err(DriverError::InvariantViolated(_))
    ));
    let mut spec = common::valid_spec(&layout, common::job_id(2));
    spec.runtime = Runtime::Runsc;
    assert!(driver.create(&spec).await.is_err());
    assert!(podman.log().is_empty());
}

#[tokio::test]
async fn create_rejects_unexpected_id_output() {
    let podman = FakePodman::new();
    podman.respond("create", b"--privileged\n", "", 0);
    let driver = podman.driver(OutReadMode::HostDirect);
    let layout = common::layout(&podman.workspaces(), 3);
    let spec = common::valid_spec(&layout, common::job_id(3));
    assert!(matches!(
        driver.create(&spec).await,
        Err(DriverError::Runtime { op: "create", .. })
    ));
}

#[tokio::test]
async fn inspect_parses_exit_facts_and_not_found() {
    let podman = FakePodman::new();
    let driver = podman.driver(OutReadMode::HostDirect);
    let id = ContainerId::new(ID);
    podman.respond("inspect", &inspect_json("exited", 137, true), "", 0);
    let state = driver.inspect(&id).await.unwrap();
    assert_eq!(state.status, ContainerStatus::Exited);
    let exit = state.exit.unwrap();
    assert_eq!(
        (exit.exit_code, exit.signal, exit.oom_killed),
        (Some(137), Some(9), true)
    );

    podman.respond("inspect", &inspect_json("running", 0, false), "", 0);
    let state = driver.inspect(&id).await.unwrap();
    assert_eq!((state.status, state.exit), (ContainerStatus::Running, None));

    podman.respond("inspect", b"[]", "Error: no such container 4f1c\n", 125);
    assert_eq!(driver.inspect(&id).await, Err(DriverError::NotFound));
}

#[tokio::test]
async fn stop_kill_remove_and_image_exists_commands() {
    let podman = FakePodman::new();
    let driver = podman.driver(OutReadMode::HostDirect);
    let id = ContainerId::new(ID);
    podman.respond("inspect", &inspect_json("exited", 143, false), "", 0);
    driver.stop(&id, Duration::from_secs(10)).await.unwrap();
    driver.remove(&id).await.unwrap();
    podman.respond(
        "kill",
        b"",
        "Error: can only kill running containers\n",
        125,
    );
    driver.kill(&id).await.unwrap();
    podman.respond("image-exists", b"", "", 1);
    assert!(!driver
        .image_present(&ImageDigest::parse(common::DIGEST).unwrap())
        .await
        .unwrap());
    let log = podman.log();
    let args = args_lines(&log);
    assert!(args
        .iter()
        .any(|l| l.ends_with(&format!("[stop] [--time=10] [{ID}]"))));
    assert!(args
        .iter()
        .any(|l| l.ends_with(&format!("[rm] [--force] [--ignore] [{ID}]"))));
    assert!(args
        .iter()
        .any(|l| l.ends_with(&format!("[kill] [--signal=KILL] [{ID}]"))));
    assert!(args
        .iter()
        .any(|l| l.ends_with(&format!("[image] [exists] [{}]", common::DIGEST))));

    // stop 返回后容器必须已 exited。
    podman.respond("inspect", &inspect_json("running", 0, false), "", 0);
    assert!(driver.stop(&id, Duration::from_secs(1)).await.is_err());
}

#[tokio::test]
async fn list_filters_by_label_again_after_podman() {
    let podman = FakePodman::new();
    let driver = podman.driver(OutReadMode::HostDirect);
    let other = "b".repeat(64);
    podman.respond(
        "ps",
        format!(
            r#"[{{"Id":"{ID}","Names":["gantry-sbx-a"],"Labels":{{"gantry.managed":"1"}},"State":"running"}},
                {{"Id":"{other}","Names":["intruder"],"Labels":{{"gantry.managed":"0"}},"State":"exited"}}]"#
        )
        .as_bytes(),
        "",
        0,
    );
    let mut selector = LabelSelector::default();
    selector.0.insert(LABEL_MANAGED.into(), "1".into());
    let listed = driver.list(&selector).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, ContainerId::new(ID));
    assert_eq!(listed[0].status, ContainerStatus::Running);
    assert!(podman
        .log()
        .contains("[ps] [--all] [--format=json] [--filter=label=gantry.managed=1]"));
}

fn tar_with(name: &str, data: &[u8]) -> Vec<u8> {
    // 复用 intake 测试里的同一种手写头。
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000644\0");
    h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut out = h.to_vec();
    out.extend_from_slice(data);
    out.extend(std::iter::repeat_n(
        0u8,
        (512 - data.len() % 512) % 512 + 1024,
    ));
    out
}

#[tokio::test]
async fn read_out_archive_mode_and_refusal_while_running() {
    let podman = FakePodman::new();
    let driver = podman.driver(OutReadMode::Archive);
    let id = ContainerId::new(ID);
    let layout = common::layout(&podman.workspaces(), 4);
    podman.respond("inspect", &inspect_json("running", 0, false), "", 0);
    assert_eq!(
        driver.read_out(&id, &layout, &common::plan()).await,
        Err(DriverError::StillRunning)
    );
    assert!(!podman.log().contains("[cp]"));

    podman.respond("inspect", &inspect_json("exited", 0, false), "", 0);
    podman.respond("cp", &tar_with("out/result.json", b"{}"), "", 0);
    let result = driver
        .read_out(&id, &layout, &common::plan())
        .await
        .unwrap();
    assert_eq!(result.files.len(), 1);
    assert!(podman
        .log()
        .contains(&format!("[cp] [{ID}:/gantry/out] [-]")));
}

#[tokio::test]
async fn prepare_and_purge_staging_are_idempotent() {
    let podman = FakePodman::new();
    let driver = podman.driver(OutReadMode::HostDirect);
    let layout = common::layout(&podman.workspaces(), 5);
    driver.prepare_dirs(&layout).await.unwrap();
    driver.prepare_dirs(&layout).await.unwrap();
    assert!(layout.out().is_dir() && layout.inputs().is_dir() && layout.work().is_dir());
    driver.purge_staging(&layout).await.unwrap();
    driver.purge_staging(&layout).await.unwrap();
    assert!(!Path::new(&layout.dir()).exists());
    let _ = spec_for;
}
