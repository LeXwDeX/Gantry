//! 各集成测试共用的构造器。
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gantry_core::entities::SandboxLimits;
use gantry_core::ids::{AttemptId, Id, SandboxId};
use gantry_core::ports::sandbox::TokenEnvSpec;
use gantry_core::worker::PatchMode;
use gantry_sandbox::driver::{LogChunk, LogCursor, SandboxDriver};
use gantry_sandbox::intake::IntakePlan;
use gantry_sandbox::spec::{
    ContainerId, ContainerSpec, ImageDigest, LogPolicy, Mount, NetworkAttach, Runtime,
    StagingLayout, DST_HOME, DST_INPUTS, DST_JOB_JSON, DST_OUT, DST_TMP, DST_WORK, LABEL_JOB_ID,
    LABEL_MANAGED, LABEL_SANDBOX_ID,
};

pub const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

pub fn sandbox_id(n: u128) -> SandboxId {
    Id::from_parts(1_700_000_000_000, n).unwrap()
}

pub fn job_id(n: u128) -> AttemptId {
    Id::from_parts(1_700_000_000_000, 1_000 + n).unwrap()
}

/// 每个用例一个独立的临时目录（不依赖 tempfile crate）。
pub struct TempRoot(PathBuf);

impl TempRoot {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gantry-sandbox-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        // macOS 的临时目录经 /var → /private/var 符号链接；用规范路径，避免作业目录判断偏差。
        Self(path.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn layout(root: &Path, n: u128) -> StagingLayout {
    StagingLayout::new(root, sandbox_id(n)).unwrap()
}

/// 通过参数闸的最小合法规格。
pub fn valid_spec(layout: &StagingLayout, job: AttemptId) -> ContainerSpec {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED.to_owned(), "1".to_owned());
    labels.insert(LABEL_JOB_ID.to_owned(), job.to_string());
    labels.insert(LABEL_SANDBOX_ID.to_owned(), layout.sandbox_id().to_string());
    let mut env = BTreeMap::new();
    env.insert("HOME".to_owned(), "/gantry/home".to_owned());
    env.insert(
        "HTTPS_PROXY".to_owned(),
        "http://job:t@egress:3128".to_owned(),
    );
    ContainerSpec {
        name: layout.container_name(),
        labels,
        image: ImageDigest::parse(&format!("ghcr.io/gantry/worker@{DIGEST}")).unwrap(),
        argv: vec!["gantry-worker".to_owned(), "run".to_owned()],
        env,
        token_env: TokenEnvSpec {
            model_base_url: BTreeMap::new(),
            proxy: vec!["HTTPS_PROXY".to_owned()],
        },
        mounts: vec![
            Mount::Bind {
                src: layout.job_json(),
                dst: DST_JOB_JSON.to_owned(),
                ro: true,
            },
            Mount::Bind {
                src: layout.inputs(),
                dst: DST_INPUTS.to_owned(),
                ro: true,
            },
            Mount::Bind {
                src: layout.work(),
                dst: DST_WORK.to_owned(),
                ro: false,
            },
            Mount::Bind {
                src: layout.out(),
                dst: DST_OUT.to_owned(),
                ro: false,
            },
            Mount::Tmpfs {
                dst: DST_HOME.to_owned(),
                size_mb: 256,
            },
            Mount::Tmpfs {
                dst: DST_TMP.to_owned(),
                size_mb: 512,
            },
        ],
        limits: SandboxLimits {
            cpu_cores: 2.0,
            mem_mb: 4096,
            disk_mb: 10240,
            pids: 512,
            wall_time_s: 3600,
        },
        network: NetworkAttach {
            network: "gantry-sbx".to_owned(),
            add_hosts: vec![("egress".to_owned(), IpAddr::V4(Ipv4Addr::new(10, 88, 0, 1)))],
        },
        runtime: Runtime::Runc,
        stop_grace: Duration::from_secs(10),
        log: LogPolicy {
            max_bytes: 16 * 1024 * 1024,
        },
    }
}

pub fn spec_for(root: &TempRoot, n: u128) -> (ContainerSpec, StagingLayout) {
    let layout = layout(root.path(), n);
    (valid_spec(&layout, job_id(n)), layout)
}

/// §12.A 第 3 条列出的违规（特权 / 设备 / cap_add 在类型上无从表达，见属性测试）。
pub fn violations() -> Vec<fn(&mut ContainerSpec)> {
    vec![
        |s| s.network.network = "host".into(),
        |s| {
            for m in &mut s.mounts {
                if let Mount::Bind { dst, ro, .. } = m {
                    if dst == DST_INPUTS {
                        *ro = false;
                    }
                }
            }
        },
        |s| {
            s.mounts.push(Mount::Bind {
                src: "/etc".into(),
                dst: "/host".into(),
                ro: true,
            })
        },
        |s| {
            s.mounts.push(Mount::Bind {
                src: "/run/gantry-sbx/podman.sock".into(),
                dst: "/run/podman.sock".into(),
                ro: false,
            })
        },
        |s| {
            s.env.insert("GITHUB_TOKEN".into(), "ghp_x".into());
        },
    ]
}

pub fn plan() -> IntakePlan {
    IntakePlan {
        result_max_bytes: 1 << 20,
        report_max_bytes: 1 << 18,
        proposals_max_bytes: 1 << 18,
        patch_max_bytes: 1 << 20,
        patch: PatchMode::Optional,
        total_max_bytes: 12 << 20,
    }
}

pub async fn collect_logs<D: SandboxDriver + ?Sized>(
    driver: &D,
    id: &ContainerId,
    since: Option<LogCursor>,
) -> Vec<LogChunk> {
    let mut stream = driver.logs(id, since);
    let mut out = Vec::new();
    while let Some(item) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        out.push(item.unwrap());
    }
    out
}
