//! 启动校验（A1；worker.md §7.2）：读 `job.json`、按适配层核对、比对 MANIFEST。
//!
//! 任一失败都是 `BAD_JOB`（退出码 10）。错误信息只写原因，不含文件内容。
//! MANIFEST 只用于发现挂载错误或串用，**不是**安全机制（worker-protocol §2.1）。

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use gantry_core::canon::{sha256, Sha256Digest};
use gantry_core::json_strict::{parse_json_strict, JsonLimits};
use gantry_core::ports::worker::{AdapterError, WorkerAdapter};
use gantry_core::worker::{JobSpec, WorkerProtocol};
use serde::Deserialize;
use serde_json::Value;

use crate::layout;

/// 沙盒内的目录。生产环境取固定布局；测试替身经 `GANTRY_*` 指向临时目录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub job: PathBuf,
    pub inputs: PathBuf,
    pub work: PathBuf,
    pub out: PathBuf,
}

impl Paths {
    pub fn sandbox() -> Self {
        Self {
            job: layout::JOB_JSON.into(),
            inputs: layout::INPUTS.into(),
            work: layout::WORK.into(),
            out: layout::OUT.into(),
        }
    }

    /// 把 MANIFEST 里的容器内绝对路径（必须在 `/gantry/inputs/` 下）映射到本地目录。
    fn resolve_input(&self, container_path: &str) -> Option<PathBuf> {
        let rest = container_path
            .strip_prefix(layout::INPUTS)?
            .strip_prefix('/')?;
        let rel = Path::new(rest);
        if rest.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            return None;
        }
        Some(self.inputs.join(rel))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BadJob {
    #[error("job.json is missing")]
    Missing,
    #[error("job.json cannot be read: {0}")]
    Unreadable(String),
    #[error("job.json is not valid UTF-8")]
    NotUtf8,
    #[error("job.json is not valid JSON ({0})")]
    NotJson(String),
    #[error("unsupported protocol")]
    UnsupportedProtocol,
    #[error("execution.agent does not match adapter {expected}")]
    AgentMismatch { expected: &'static str },
    #[error("job.json does not match JobSpec: {0}")]
    InvalidSpec(String),
    #[error("execution.agent_options rejected: {0}")]
    AgentOptions(AdapterError),
    #[error("MANIFEST: {0}")]
    Manifest(String),
}

/// 通过 A1 的 JobSpec 与原始字节的哈希（`result.json.job_sha256`）。
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedJob {
    pub job: JobSpec,
    pub job_sha256: Sha256Digest,
}

pub fn read_job(paths: &Paths, adapter: &dyn WorkerAdapter) -> Result<LoadedJob, BadJob> {
    let bytes = match fs::read(&paths.job) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(BadJob::Missing),
        Err(err) => return Err(BadJob::Unreadable(err.kind().to_string())),
    };
    load_job(&bytes, adapter)
}

/// A1 的 JobSpec 部分。顶层与多数对象的未知字段被忽略；`constraints.*`、`output.*`
/// 的未知字段与 `agent_options` 的未知键拒绝（W8）。
pub fn load_job(bytes: &[u8], adapter: &dyn WorkerAdapter) -> Result<LoadedJob, BadJob> {
    if std::str::from_utf8(bytes).is_err() {
        return Err(BadJob::NotUtf8);
    }
    let value = parse_json_strict(bytes, JsonLimits::default())
        .map_err(|err| BadJob::NotJson(redact(&err.to_string())))?;
    // 先单独看 protocol 与 agent，给出比整体反序列化更准确的原因。
    match value.get("protocol").and_then(Value::as_str) {
        Some(WorkerProtocol::VALUE) => {}
        _ => return Err(BadJob::UnsupportedProtocol),
    }
    let expected = adapter.agent_id();
    if value.pointer("/execution/agent").and_then(Value::as_str) != Some(expected) {
        return Err(BadJob::AgentMismatch { expected });
    }
    let job: JobSpec = serde_json::from_value(value)
        .map_err(|err| BadJob::InvalidSpec(redact(&err.to_string())))?;
    job.validate()
        .map_err(|err| BadJob::InvalidSpec(err.to_string()))?;
    if !adapter.protocol_versions().contains(&job.protocol) {
        return Err(BadJob::UnsupportedProtocol);
    }
    let options = Value::Object(job.execution.agent_options.clone().into_iter().collect());
    adapter
        .validate_agent_options(&options)
        .map_err(BadJob::AgentOptions)?;
    Ok(LoadedJob {
        job,
        job_sha256: sha256(bytes),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    job_sha256: Sha256Digest,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    sha256: Sha256Digest,
    bytes: u64,
}

/// A1 的 MANIFEST 部分：`job_sha256`、逐个文件的 `sha256` / `bytes`、
/// `output.schema.json` 的哈希等于 `output.structured_schema.sha256`。
pub fn verify_manifest(paths: &Paths, loaded: &LoadedJob) -> Result<(), BadJob> {
    let manifest_path = paths.inputs.join("MANIFEST.json");
    let bytes = fs::read(&manifest_path)
        .map_err(|err| BadJob::Manifest(format!("cannot read MANIFEST.json: {}", err.kind())))?;
    let value = parse_json_strict(&bytes, JsonLimits::default()).map_err(|err| {
        BadJob::Manifest(format!("not valid JSON ({})", redact(&err.to_string())))
    })?;
    let manifest: Manifest = serde_json::from_value(value).map_err(|err| {
        BadJob::Manifest(format!("unexpected shape: {}", redact(&err.to_string())))
    })?;
    if manifest.job_sha256 != loaded.job_sha256 {
        return Err(BadJob::Manifest(
            "job_sha256 does not match job.json".into(),
        ));
    }
    let schema = &loaded.job.output.structured_schema;
    let mut schema_listed = false;
    for file in &manifest.files {
        let local = paths
            .resolve_input(&file.path)
            .ok_or_else(|| BadJob::Manifest(format!("path outside inputs: {}", file.path)))?;
        let content = fs::read(&local)
            .map_err(|err| BadJob::Manifest(format!("{}: {}", file.path, err.kind())))?;
        if content.len() as u64 != file.bytes || sha256(&content) != file.sha256 {
            return Err(BadJob::Manifest(format!("{} does not match", file.path)));
        }
        if file.path == schema.path {
            schema_listed = true;
            if file.sha256 != schema.sha256 {
                return Err(BadJob::Manifest(
                    "output.schema.json does not match output.structured_schema.sha256".into(),
                ));
            }
        }
    }
    if !schema_listed {
        return Err(BadJob::Manifest(format!("{} is not listed", schema.path)));
    }
    Ok(())
}

/// 解析器的错误信息会用反引号回显取值；stderr 只写原因，不写文件内容（worker.md §7.2）。
fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for (i, part) in message.split('`').enumerate() {
        out.push_str(if i % 2 == 1 { "…" } else { part });
    }
    out
}
