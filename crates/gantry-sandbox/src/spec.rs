//! 容器面的请求类型：`ContainerSpec` 与作业目录布局（sandbox.md §6.2、§7.2）。
//!
//! `ContainerSpec` 刻意没有 `privileged` / `cap_add` / `devices` / host 网络 /
//! host userns 等字段：加固项是驱动里的常量（§7.3），调用方无从放宽。

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use gantry_core::canon::Sha256Digest;
use gantry_core::entities::SandboxLimits;
use gantry_core::ids::SandboxId;
use gantry_core::ports::sandbox::TokenEnvSpec;

/// 容器在运行时里的 ID（Podman 返回的 64 位十六进制，或测试替身生成的值）。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerId(String);

impl ContainerId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 按摘要固定的镜像引用：`<name>@sha256:<64hex>`，或本地镜像 ID `sha256:<64hex>`。
///
/// 契约缺口：`ExecSpec.image_digest` 只是 `Sha256Digest`（OQ-S5 / OQ-W5），
/// 服务层用 [`ImageDigest::local_id`] 构造后者。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageDigest(String);

impl ImageDigest {
    pub fn parse(value: &str) -> Result<Self, ImageRefError> {
        let digest = match value.split_once('@') {
            Some((name, digest)) => {
                if !valid_image_name(name) {
                    return Err(ImageRefError::Name);
                }
                digest
            }
            None => value,
        };
        Sha256Digest::parse(digest).map_err(|_| ImageRefError::Digest)?;
        Ok(Self(value.to_owned()))
    }

    pub fn local_id(digest: Sha256Digest) -> Self {
        Self(digest.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageRefError {
    #[error("image name is not a plain registry/repository path")]
    Name,
    #[error("image must be pinned by sha256 digest")]
    Digest,
}

/// 镜像名只允许小写的 `registry[:port]/path`；不得以 `-` 开头，避免被当成参数。
fn valid_image_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'.' | b'_' | b'-' | b'/' | b':')
        })
}

/// OCI 运行时（`sandbox.runtime`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    Runsc,
    Runc,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runsc => "runsc",
            Self::Runc => "runc",
        }
    }
}

/// 挂载。bind 的源只能是作业目录下的四个路径（[`validate_invariants`](crate::invariants::validate_invariants) 断言）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mount {
    Bind { src: PathBuf, dst: String, ro: bool },
    Tmpfs { dst: String, size_mb: u64 },
}

/// 沙盒网段与到 Egress 的路由（§7.4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkAttach {
    /// 预先建好的 Podman 网络名（沙盒网段）。
    pub network: String,
    /// `--add-host` 固定的名称（例如 Egress 的沙盒侧地址）。
    pub add_hosts: Vec<(String, IpAddr)>,
}

/// 容器日志策略（`--log-driver=k8s-file --log-opt max-size=…`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogPolicy {
    pub max_bytes: u64,
}

/// 驱动的创建请求（sandbox.md §6.2）。
#[derive(Clone, Debug, PartialEq)]
pub struct ContainerSpec {
    pub name: String,
    /// 必含 `gantry.managed=1`、`gantry.job_id`、`gantry.sandbox_id`。
    pub labels: BTreeMap<String, String>,
    pub image: ImageDigest,
    pub argv: Vec<String>,
    /// 已填好 `token_env` 变量值的完整环境变量表。
    pub env: BTreeMap<String, String>,
    pub token_env: TokenEnvSpec,
    pub mounts: Vec<Mount>,
    pub limits: SandboxLimits,
    pub network: NetworkAttach,
    pub runtime: Runtime,
    pub stop_grace: Duration,
    pub log: LogPolicy,
}

/// 容器内的固定挂载点。
pub const DST_JOB_JSON: &str = "/gantry/job.json";
pub const DST_INPUTS: &str = "/gantry/inputs";
pub const DST_WORK: &str = "/gantry/work";
pub const DST_OUT: &str = "/gantry/out";
pub const DST_HOME: &str = "/gantry/home";
pub const DST_TMP: &str = "/gantry/tmp";

/// 必带的标签名（worker-protocol §3.5）。
pub const LABEL_MANAGED: &str = "gantry.managed";
pub const LABEL_JOB_ID: &str = "gantry.job_id";
pub const LABEL_SANDBOX_ID: &str = "gantry.sandbox_id";

/// 容器名前缀：`gantry-sbx-<sbx_id>`。
pub const NAME_PREFIX: &str = "gantry-sbx-";

/// 一个沙盒的作业目录 `<workspaces_dir>/<sbx_id>/`（§7.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagingLayout {
    workspaces_dir: PathBuf,
    sandbox_id: SandboxId,
}

impl StagingLayout {
    /// `workspaces_dir` 必须是绝对、已规范化的路径，且不含 Podman 卷参数的分隔符。
    pub fn new(
        workspaces_dir: impl Into<PathBuf>,
        sandbox_id: SandboxId,
    ) -> Result<Self, LayoutError> {
        let workspaces_dir = workspaces_dir.into();
        if !workspaces_dir.is_absolute() {
            return Err(LayoutError::NotAbsolute);
        }
        if workspaces_dir
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(LayoutError::NotNormalized);
        }
        let Some(text) = workspaces_dir.to_str() else {
            return Err(LayoutError::NotUtf8);
        };
        if text.contains([':', ',', '\n', '\0']) {
            return Err(LayoutError::ForbiddenChar);
        }
        Ok(Self {
            workspaces_dir,
            sandbox_id,
        })
    }

    pub fn workspaces_dir(&self) -> &Path {
        &self.workspaces_dir
    }

    pub fn sandbox_id(&self) -> SandboxId {
        self.sandbox_id
    }

    pub fn dir(&self) -> PathBuf {
        self.workspaces_dir.join(self.sandbox_id.to_string())
    }

    pub fn meta_json(&self) -> PathBuf {
        self.dir().join("meta.json")
    }

    pub fn progress_spool(&self) -> PathBuf {
        self.dir().join("progress.spool.jsonl")
    }

    pub fn job_json(&self) -> PathBuf {
        self.dir().join("job.json")
    }

    pub fn inputs(&self) -> PathBuf {
        self.dir().join("inputs")
    }

    pub fn work(&self) -> PathBuf {
        self.dir().join("work")
    }

    pub fn out(&self) -> PathBuf {
        self.dir().join("out")
    }

    /// 四个可挂载路径与各自唯一允许的挂载点。
    pub fn bind_targets(&self) -> [(PathBuf, &'static str); 4] {
        [
            (self.job_json(), DST_JOB_JSON),
            (self.inputs(), DST_INPUTS),
            (self.work(), DST_WORK),
            (self.out(), DST_OUT),
        ]
    }

    pub fn container_name(&self) -> String {
        format!("{NAME_PREFIX}{}", self.sandbox_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("workspaces_dir must be absolute")]
    NotAbsolute,
    #[error("workspaces_dir must not contain . or .. components")]
    NotNormalized,
    #[error("workspaces_dir must be valid UTF-8")]
    NotUtf8,
    #[error("workspaces_dir must not contain ':', ',', newline or NUL")]
    ForbiddenChar,
}
