//! sandbox.md §12.A：`validate_invariants` 的拒绝用例与渲染结果的属性测试。

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::time::Duration;

use gantry_core::entities::SandboxLimits;
use gantry_core::ids::{AttemptId, Id, SandboxId};
use gantry_core::ports::sandbox::TokenEnvSpec;
use gantry_sandbox::invariants::{validate_invariants, InvariantContext, InvariantViolation};
use gantry_sandbox::podman::{image_index, render_create_args, HARDENING_FLAGS};
use gantry_sandbox::spec::{
    ContainerSpec, ImageDigest, ImageRefError, LayoutError, LogPolicy, Mount, NetworkAttach,
    Runtime, StagingLayout, DST_HOME, DST_INPUTS, DST_JOB_JSON, DST_OUT, DST_TMP, DST_WORK,
    LABEL_JOB_ID, LABEL_MANAGED, LABEL_SANDBOX_ID,
};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const TOKEN: &str = "jt_secretcapability0001";

fn sandbox_id() -> SandboxId {
    Id::from_parts(1_700_000_000_000, 7).unwrap()
}

fn job_id() -> AttemptId {
    Id::from_parts(1_700_000_000_000, 9).unwrap()
}

fn layout() -> StagingLayout {
    StagingLayout::new("/var/lib/gantry/workspaces", sandbox_id()).unwrap()
}

fn ctx(layout: &StagingLayout) -> InvariantContext<'_> {
    InvariantContext {
        layout,
        job_token: Some(TOKEN),
    }
}

fn valid_spec(layout: &StagingLayout) -> ContainerSpec {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED.to_owned(), "1".to_owned());
    labels.insert(LABEL_JOB_ID.to_owned(), job_id().to_string());
    labels.insert(LABEL_SANDBOX_ID.to_owned(), sandbox_id().to_string());

    let mut env = BTreeMap::new();
    env.insert("GANTRY_JOB".to_owned(), "/gantry/job.json".to_owned());
    env.insert("GANTRY_OUT".to_owned(), "/gantry/out".to_owned());
    env.insert("HOME".to_owned(), "/gantry/home".to_owned());
    env.insert("TMPDIR".to_owned(), "/gantry/tmp".to_owned());
    env.insert(
        "ANTHROPIC_API_KEY".to_owned(),
        "gantry-egress-managed".to_owned(),
    );
    env.insert(
        "ANTHROPIC_BASE_URL".to_owned(),
        format!("http://egress.gantry.internal:8081/j/{TOKEN}/anthropic"),
    );
    env.insert(
        "HTTPS_PROXY".to_owned(),
        format!("http://job:{TOKEN}@egress.gantry.internal:3128"),
    );

    let mut model_base_url = BTreeMap::new();
    model_base_url.insert("anthropic".to_owned(), "ANTHROPIC_BASE_URL".to_owned());

    ContainerSpec {
        name: layout.container_name(),
        labels,
        image: ImageDigest::parse(&format!("ghcr.io/gantry/worker@{DIGEST}")).unwrap(),
        argv: vec!["gantry-worker".to_owned(), "run".to_owned()],
        env,
        token_env: TokenEnvSpec {
            model_base_url,
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
            add_hosts: vec![(
                "egress.gantry.internal".to_owned(),
                IpAddr::V4(Ipv4Addr::new(10, 88, 0, 1)),
            )],
        },
        runtime: Runtime::Runc,
        stop_grace: Duration::from_secs(10),
        log: LogPolicy {
            max_bytes: 16 * 1024 * 1024,
        },
    }
}

fn rejected(spec: &ContainerSpec, layout: &StagingLayout) -> InvariantViolation {
    let err = validate_invariants(spec, ctx(layout)).expect_err("spec must be rejected");
    // 拒绝时渲染也不产生任何参数。
    assert_eq!(render_create_args(spec, ctx(layout)), Err(err.clone()));
    err
}

#[test]
fn valid_spec_renders_hardening_flags_before_image() {
    let layout = layout();
    let spec = valid_spec(&layout);
    let args = render_create_args(&spec, ctx(&layout)).unwrap();
    assert_eq!(args[0], "create");
    let image = image_index(&args).unwrap();
    assert_eq!(args[image], spec.image.as_str());
    assert_eq!(&args[image + 1..], spec.argv.as_slice());
    let options = &args[1..image];
    for flag in HARDENING_FLAGS {
        assert!(options.iter().any(|arg| arg == flag), "missing {flag}");
    }
    for expected in [
        "--runtime=runc",
        "--memory=4096m",
        "--memory-swap=4096m",
        "--pids-limit=512",
        "--cpus=2",
        "--network=gantry-sbx",
        "--add-host=egress.gantry.internal:10.88.0.1",
        "--stop-timeout=10",
        "--log-opt=max-size=16777216",
        "--label=gantry.managed=1",
        "--volume=/var/lib/gantry/workspaces/sbx_01HF7YAT00000000000000000Z/inputs:/gantry/inputs:ro,nosuid,nodev,noexec",
        "--volume=/var/lib/gantry/workspaces/sbx_01HF7YAT00000000000000000Z/work:/gantry/work:rw,nosuid,nodev",
        "--volume=/var/lib/gantry/workspaces/sbx_01HF7YAT00000000000000000Z/out:/gantry/out:rw,nosuid,nodev,noexec",
        "--tmpfs=/gantry/home:rw,nosuid,nodev,size=256m,uid=1000,gid=1000,mode=0700",
    ] {
        let expected = expected.replace("sbx_01HF7YAT00000000000000000Z", &sandbox_id().to_string());
        assert!(options.contains(&expected), "missing {expected}: {options:?}");
    }
}

#[test]
fn read_only_workspace_renders_ro_work_mount() {
    let layout = layout();
    let mut spec = valid_spec(&layout);
    for mount in &mut spec.mounts {
        if let Mount::Bind { dst, ro, .. } = mount {
            if dst == DST_WORK {
                *ro = true;
            }
        }
    }
    let args = render_create_args(&spec, ctx(&layout)).unwrap();
    let work = format!(
        "--volume={}:/gantry/work:ro,nosuid,nodev",
        layout.work().display()
    );
    assert!(args.contains(&work));
}

#[test]
fn rejects_host_and_mode_networks() {
    let layout = layout();
    for network in [
        "host",
        "none",
        "bridge",
        "pasta",
        "slirp4netns",
        "container:abc",
        "ns:/proc/1/ns/net",
        "",
        "Host",
        "-x",
    ] {
        let mut spec = valid_spec(&layout);
        spec.network.network = network.to_owned();
        assert_eq!(
            rejected(&spec, &layout),
            InvariantViolation::Network,
            "{network}"
        );
    }
    let mut spec = valid_spec(&layout);
    spec.network.add_hosts = vec![("egress".to_owned(), IpAddr::V6(Ipv6Addr::UNSPECIFIED))];
    assert_eq!(rejected(&spec, &layout), InvariantViolation::AddHost);
    let mut spec = valid_spec(&layout);
    spec.network.add_hosts = vec![("bad host:1".to_owned(), IpAddr::V4(Ipv4Addr::LOCALHOST))];
    assert_eq!(rejected(&spec, &layout), InvariantViolation::AddHost);
}

#[test]
fn rejects_writable_inputs_and_read_only_out() {
    let layout = layout();
    for (target, expected) in [
        (DST_INPUTS, InvariantViolation::MountWritable(DST_INPUTS)),
        (
            DST_JOB_JSON,
            InvariantViolation::MountWritable(DST_JOB_JSON),
        ),
        (DST_OUT, InvariantViolation::MountReadOnly(DST_OUT)),
    ] {
        let mut spec = valid_spec(&layout);
        for mount in &mut spec.mounts {
            if let Mount::Bind { dst, ro, .. } = mount {
                if dst == target {
                    *ro = !*ro;
                }
            }
        }
        assert_eq!(rejected(&spec, &layout), expected);
    }
}

#[test]
fn rejects_host_paths_and_socket_mounts() {
    let layout = layout();
    let other = StagingLayout::new(
        "/var/lib/gantry/workspaces",
        Id::from_parts(1_700_000_000_000, 8).unwrap(),
    )
    .unwrap();
    for src in [
        PathBuf::from("/run/gantry-sbx/podman.sock"),
        PathBuf::from("/run/user/1000/podman/podman.sock"),
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from("/run/secrets"),
        PathBuf::from("/"),
        PathBuf::from("/var/lib/gantry"),
        layout.dir(),
        layout.dir().join("meta.json"),
        layout.inputs().join(".."),
        layout.dir().join("work/../inputs"),
        other.work(),
    ] {
        let mut spec = valid_spec(&layout);
        spec.mounts.push(Mount::Bind {
            src: src.clone(),
            dst: "/gantry/extra".to_owned(),
            ro: true,
        });
        assert_eq!(
            rejected(&spec, &layout),
            InvariantViolation::MountSource,
            "{src:?}"
        );
    }
    // 合法源挂到别的目标也不行。
    let mut spec = valid_spec(&layout);
    spec.mounts[1] = Mount::Bind {
        src: layout.inputs(),
        dst: "/etc".to_owned(),
        ro: true,
    };
    assert!(matches!(
        rejected(&spec, &layout),
        InvariantViolation::MountDestination(_)
    ));
}

#[test]
fn rejects_missing_duplicate_or_foreign_tmpfs() {
    let layout = layout();
    let mut spec = valid_spec(&layout);
    spec.mounts
        .retain(|m| !matches!(m, Mount::Bind { dst, .. } if dst == DST_OUT));
    assert_eq!(
        rejected(&spec, &layout),
        InvariantViolation::MountSet(DST_OUT)
    );

    let mut spec = valid_spec(&layout);
    spec.mounts.push(Mount::Bind {
        src: layout.work(),
        dst: DST_WORK.to_owned(),
        ro: true,
    });
    assert_eq!(
        rejected(&spec, &layout),
        InvariantViolation::MountSet(DST_WORK)
    );

    for (dst, size) in [("/etc", 16), ("/gantry/out", 16), (DST_HOME, 0)] {
        let mut spec = valid_spec(&layout);
        spec.mounts
            .retain(|m| !matches!(m, Mount::Tmpfs { dst: d, .. } if d == DST_HOME));
        spec.mounts.push(Mount::Tmpfs {
            dst: dst.to_owned(),
            size_mb: size,
        });
        assert_eq!(rejected(&spec, &layout), InvariantViolation::Tmpfs, "{dst}");
    }
}

#[test]
fn rejects_secret_like_env_names() {
    let layout = layout();
    for name in [
        "GITHUB_TOKEN",
        "gh_token",
        "JOB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "DB_PASSWORD",
        "SSH_PRIVATE_KEY",
        "1BAD",
        "BAD-NAME",
        "",
    ] {
        let mut spec = valid_spec(&layout);
        spec.env.insert(name.to_owned(), "x".to_owned());
        assert_eq!(
            rejected(&spec, &layout),
            InvariantViolation::EnvName(name.to_owned()),
            "{name}"
        );
    }
    let mut spec = valid_spec(&layout);
    spec.env
        .insert("ANTHROPIC_API_KEY".to_owned(), "sk-ant-real".to_owned());
    assert_eq!(
        rejected(&spec, &layout),
        InvariantViolation::Placeholder("ANTHROPIC_API_KEY".to_owned())
    );
}

#[test]
fn job_token_only_in_token_env_values() {
    let layout = layout();
    let mut spec = valid_spec(&layout);
    spec.env
        .insert("GANTRY_DEBUG".to_owned(), format!("leak {TOKEN}"));
    assert_eq!(
        rejected(&spec, &layout),
        InvariantViolation::TokenLeak("GANTRY_DEBUG".to_owned())
    );

    let mut spec = valid_spec(&layout);
    spec.env.remove("HTTPS_PROXY");
    assert_eq!(
        rejected(&spec, &layout),
        InvariantViolation::TokenEnvMissing("HTTPS_PROXY".to_owned())
    );
}

#[test]
fn rejects_unpinned_images() {
    for image in [
        "ghcr.io/gantry/worker:latest",
        "ghcr.io/gantry/worker",
        "ghcr.io/gantry/worker@sha256:abc",
        "ghcr.io/gantry/worker@sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        "--privileged@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Ghcr.io/x@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "",
    ] {
        assert!(ImageDigest::parse(image).is_err(), "{image}");
    }
    assert_eq!(
        ImageDigest::parse("worker:latest"),
        Err(ImageRefError::Digest)
    );
    assert!(ImageDigest::parse(DIGEST).is_ok());
}

#[test]
fn rejects_wrong_identity_limits_and_timers() {
    let layout = layout();
    let mut spec = valid_spec(&layout);
    spec.name = "gantry-sbx-other".to_owned();
    assert!(matches!(
        rejected(&spec, &layout),
        InvariantViolation::Name { .. }
    ));

    for label in [LABEL_MANAGED, LABEL_JOB_ID, LABEL_SANDBOX_ID] {
        let mut spec = valid_spec(&layout);
        spec.labels.insert(label.to_owned(), "wrong".to_owned());
        assert_eq!(rejected(&spec, &layout), InvariantViolation::Label(label));
    }
    let mut spec = valid_spec(&layout);
    spec.labels.insert("x".to_owned(), "a\nb".to_owned());
    assert_eq!(rejected(&spec, &layout), InvariantViolation::LabelChars);

    let mut spec = valid_spec(&layout);
    spec.argv.clear();
    assert_eq!(rejected(&spec, &layout), InvariantViolation::Argv);

    for (field, mutate) in [
        (
            "cpu_cores",
            (|l: &mut SandboxLimits| l.cpu_cores = 0.0) as fn(&mut SandboxLimits),
        ),
        ("cpu_cores", |l| l.cpu_cores = f64::NAN),
        ("mem_mb", |l| l.mem_mb = 0),
        ("disk_mb", |l| l.disk_mb = 0),
        ("pids", |l| l.pids = 0),
        ("wall_time_s", |l| l.wall_time_s = 0),
    ] {
        let mut spec = valid_spec(&layout);
        mutate(&mut spec.limits);
        assert_eq!(rejected(&spec, &layout), InvariantViolation::Limit(field));
    }

    for grace in [
        Duration::ZERO,
        Duration::from_millis(1500),
        Duration::from_secs(601),
    ] {
        let mut spec = valid_spec(&layout);
        spec.stop_grace = grace;
        assert_eq!(rejected(&spec, &layout), InvariantViolation::StopGrace);
    }
}

#[test]
fn layout_rejects_paths_that_break_volume_syntax() {
    let id = sandbox_id();
    assert_eq!(
        StagingLayout::new("relative/ws", id),
        Err(LayoutError::NotAbsolute)
    );
    assert_eq!(
        StagingLayout::new("/var/../etc", id),
        Err(LayoutError::NotNormalized)
    );
    assert_eq!(
        StagingLayout::new("/var/ws:/etc", id),
        Err(LayoutError::ForbiddenChar)
    );
    assert_eq!(
        StagingLayout::new("/var/ws,rw", id),
        Err(LayoutError::ForbiddenChar)
    );
}

// ---------------------------------------------------------------------------
// 属性测试：随机（含恶意取值）的 ContainerSpec，只要通过 validate_invariants，
// 渲染出的选项段永远满足 §12.A 第 4 条。
// ---------------------------------------------------------------------------

/// 固定种子的 xorshift64*，保证失败可复现，不引入额外依赖。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const EVIL: &[&str] = &[
    "--privileged",
    "--network=host",
    "--pid=host",
    "--ipc=host",
    "--userns=host",
    "--cap-add=ALL",
    "--device=/dev/kvm",
    "--security-opt=seccomp=unconfined",
    "--security-opt=apparmor=unconfined",
    "--volume=/:/host",
    "--volume=/run/gantry-sbx/podman.sock:/s",
    "x\n--privileged",
    "a b",
    "host",
    "",
    "ok",
];

fn random_spec(rng: &mut Rng, layout: &StagingLayout) -> ContainerSpec {
    let mut spec = valid_spec(layout);
    for _ in 0..rng.below(4) {
        match rng.below(10) {
            0 => {
                let key = if rng.chance(50) {
                    "X_LABEL"
                } else {
                    *rng.pick(EVIL)
                };
                spec.labels
                    .insert(key.to_owned(), (*rng.pick(EVIL)).to_owned());
            }
            1 => {
                let names = [
                    "GANTRY_X",
                    "GITHUB_TOKEN",
                    "LD_PRELOAD",
                    "PATH",
                    "--privileged",
                    "A=B",
                ];
                spec.env
                    .insert((*rng.pick(&names)).to_owned(), (*rng.pick(EVIL)).to_owned());
            }
            2 => spec.argv.push((*rng.pick(EVIL)).to_owned()),
            3 => {
                spec.network.network =
                    (*rng.pick(&["host", "gantry-sbx", "--privileged", "container:x", "sbx-2"]))
                        .to_owned()
            }
            4 => {
                let srcs = [
                    PathBuf::from("/"),
                    PathBuf::from("/run/gantry-sbx/podman.sock"),
                    layout.work(),
                    layout.out(),
                    layout.inputs(),
                    PathBuf::from("/var/run/docker.sock"),
                ];
                let dsts = [
                    DST_WORK,
                    DST_OUT,
                    DST_INPUTS,
                    "/host",
                    "/var/run/docker.sock",
                ];
                spec.mounts.push(Mount::Bind {
                    src: rng.pick(&srcs).clone(),
                    dst: (*rng.pick(&dsts)).to_owned(),
                    ro: rng.chance(50),
                });
            }
            5 => {
                let index = rng.below(spec.mounts.len());
                if let Mount::Bind { ro, .. } = &mut spec.mounts[index] {
                    *ro = !*ro;
                }
            }
            6 => {
                spec.mounts.push(Mount::Tmpfs {
                    dst: (*rng.pick(&["/etc", DST_HOME, "/dev", DST_TMP])).to_owned(),
                    size_mb: rng.below(3) as u64,
                });
            }
            7 => {
                let images = [format!("ghcr.io/x@{DIGEST}"), DIGEST.to_owned()];
                spec.image = ImageDigest::parse(rng.pick(&images)).unwrap();
            }
            8 => {
                spec.runtime = if rng.chance(50) {
                    Runtime::Runsc
                } else {
                    Runtime::Runc
                };
                spec.limits.pids = rng.below(3) as u32;
            }
            _ => {
                spec.network.add_hosts.push((
                    (*rng.pick(&["egress", "--privileged", "a:b"])).to_owned(),
                    IpAddr::V4(Ipv4Addr::new(10, 0, 0, rng.below(3) as u8)),
                ));
            }
        }
    }
    spec
}

const FORBIDDEN_SUBSTRINGS: &[&str] = &[
    "privileged",
    "=host",
    "cap-add",
    "--device",
    "unconfined",
    ".sock",
    "--userns",
    "--pid=",
    "--env-host",
    "--secret",
];

#[test]
fn property_rendered_options_never_relax_hardening() {
    let layout = layout();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut accepted = 0;
    let mut rejected_count = 0;
    for _ in 0..20_000 {
        let spec = random_spec(&mut rng, &layout);
        let Ok(args) = render_create_args(&spec, ctx(&layout)) else {
            rejected_count += 1;
            continue;
        };
        accepted += 1;
        let image = image_index(&args).expect("image present");
        assert_eq!(args[image], spec.image.as_str());
        let options = &args[1..image];
        for arg in options {
            assert!(arg.starts_with("--"), "non-flag in option segment: {arg:?}");
            // 调用方可填的值都在 --label= / --env= 里，渲染为单个参数；
            // 其余选项不得含任何放宽项。
            if arg.starts_with("--label=") || arg.starts_with("--env=") {
                continue;
            }
            for bad in FORBIDDEN_SUBSTRINGS {
                assert!(!arg.contains(bad), "forbidden {bad} in {arg:?}");
            }
            if let Some(volume) = arg.strip_prefix("--volume=") {
                let src = volume.split(':').next().unwrap();
                assert!(
                    layout
                        .bind_targets()
                        .iter()
                        .any(|(path, _)| path.to_str() == Some(src)),
                    "volume source outside the job dir: {src}"
                );
            }
        }
        for required in [
            "--read-only",
            "--security-opt=no-new-privileges",
            "--cap-drop=ALL",
            "--user=1000:1000",
            "--unsetenv-all",
        ] {
            assert!(
                options.iter().any(|arg| arg == required),
                "missing {required}"
            );
        }
        let inputs = options
            .iter()
            .find(|arg| arg.contains(":/gantry/inputs:"))
            .unwrap();
        assert!(inputs.ends_with(":ro,nosuid,nodev,noexec"));
        assert_eq!(
            options
                .iter()
                .filter(|arg| arg.starts_with("--network="))
                .count(),
            1
        );
    }
    // 两个分支都要有足够多的样本，否则这个属性测试什么都没证明。
    assert!(accepted > 1_000, "accepted {accepted}");
    assert!(rejected_count > 1_000, "rejected {rejected_count}");
}
