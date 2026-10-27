//! Podman 的 `create` 参数渲染（sandbox.md §7.3）。
//!
//! 加固项是本文件里的常量，不来自 [`ContainerSpec`]；每个调用方可填的值都以
//! `--flag=value` 的单个参数出现，值里再怎么写 `--privileged` 也只是那个参数的值。
//! 渲染前先跑 [`validate_invariants`]，不通过就不产生任何参数。

use crate::invariants::{validate_invariants, InvariantContext, InvariantViolation};
use crate::spec::{ContainerSpec, Mount, DST_INPUTS, DST_JOB_JSON, DST_OUT, DST_WORK};

/// 容器内的非 root 用户（§7.3「用户」）。
pub const CONTAINER_USER: &str = "1000:1000";

/// 不随规格变化的加固参数，按 §7.3 的表逐项列出。
pub const HARDENING_FLAGS: &[&str] = &[
    "--pull=never",
    "--init",
    "--read-only",
    "--read-only-tmpfs=false",
    "--user=1000:1000",
    "--cap-drop=ALL",
    "--security-opt=no-new-privileges",
    "--ipc=private",
    "--uts=private",
    "--cgroupns=private",
    "--shm-size=64m",
    "--dns=none",
    "--sysctl=net.ipv6.conf.all.disable_ipv6=1",
    "--ulimit=nofile=4096:4096",
    "--ulimit=core=0",
    "--unsetenv-all",
    "--log-driver=k8s-file",
    "--stop-signal=SIGTERM",
    "--restart=no",
    "--workdir=/gantry/work",
];

/// 渲染 `podman create` 的参数（不含 `podman` 本身与 `--remote` / `--url`）。
///
/// 结构：`create`、选项、镜像、容器命令。镜像之前的每一项都是选项。
pub fn render_create_args(
    spec: &ContainerSpec,
    ctx: InvariantContext<'_>,
) -> Result<Vec<String>, InvariantViolation> {
    validate_invariants(spec, ctx)?;
    let mut args = vec!["create".to_owned(), format!("--name={}", spec.name)];
    args.push(format!("--runtime={}", spec.runtime.as_str()));
    args.extend(HARDENING_FLAGS.iter().map(|flag| (*flag).to_owned()));

    let limits = &spec.limits;
    args.push(format!("--cpus={}", limits.cpu_cores));
    args.push(format!("--memory={}m", limits.mem_mb));
    args.push(format!("--memory-swap={}m", limits.mem_mb));
    args.push(format!("--pids-limit={}", limits.pids));

    args.push(format!("--network={}", spec.network.network));
    for (host, ip) in &spec.network.add_hosts {
        args.push(format!("--add-host={host}:{ip}"));
    }

    for mount in &spec.mounts {
        args.push(render_mount(mount));
    }

    for (name, value) in &spec.env {
        args.push(format!("--env={name}={value}"));
    }

    args.push(format!("--log-opt=max-size={}", spec.log.max_bytes));
    args.push(format!("--stop-timeout={}", spec.stop_grace.as_secs()));
    for (key, value) in &spec.labels {
        args.push(format!("--label={key}={value}"));
    }

    args.push(spec.image.as_str().to_owned());
    args.extend(spec.argv.iter().cloned());
    Ok(args)
}

fn render_mount(mount: &Mount) -> String {
    match mount {
        Mount::Bind { src, dst, ro } => {
            let opts = match dst.as_str() {
                DST_JOB_JSON | DST_INPUTS => "ro,nosuid,nodev,noexec",
                DST_OUT => "rw,nosuid,nodev,noexec",
                DST_WORK if *ro => "ro,nosuid,nodev",
                _ => "rw,nosuid,nodev",
            };
            // validate_invariants 已保证 src 是作业目录下的 UTF-8 路径且不含 ':' ','。
            format!("--volume={}:{dst}:{opts}", src.display())
        }
        Mount::Tmpfs { dst, size_mb } => {
            format!("--tmpfs={dst}:rw,nosuid,nodev,size={size_mb}m,uid=1000,gid=1000,mode=0700")
        }
    }
}

/// 渲染结果里镜像所在的位置（第一个不以 `--` 开头的参数，跳过开头的 `create`）。
pub fn image_index(args: &[String]) -> Option<usize> {
    args.iter()
        .enumerate()
        .skip(1)
        .find(|(_, arg)| !arg.starts_with("--"))
        .map(|(index, _)| index)
}
