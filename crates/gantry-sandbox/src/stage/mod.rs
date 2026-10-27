//! staging：把可信 JobSpec 与不可信材料写进作业目录（sandbox.md §7.2，worker-protocol §2.1、§4.4）。
//!
//! 只写 `job.json` 与 `inputs/`；`work/` 由 `repo.prepare_workspace` 填充，`out/` 保持为空。
//! 写完后全部输入文件 `0444`、目录 `0555`，最后原子创建 `inputs/READY`。
//!
//! 契约缺口（见 #95 评论）：
//! - `output.schema.json` 应取自 core 的内置 schema 注册表，core 目前没有，调用方传入字节，
//!   这里只校验摘要与 `job.json` 的声明一致；
//! - `job.json` 应用 `schemas/worker/job.schema.json` 校验，运行时没有 JSON Schema 校验器；
//!   这里用「类型化序列化 → 严格解析 → 反序列化后逐字段相等」加 `JobSpec::validate` 代替；
//! - `upstream/` 的文件要从 CAS 按 `UpstreamInput.files` 取出，取决于 ResultBundle 清单形状
//!   与 CAS 端口，调用方传入已取出的字节。

pub mod materials;
pub mod secrets;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use gantry_core::canon::{jcs, sha256, Sha256Digest};
use gantry_core::ids::NodeId;
use gantry_core::json_strict::{parse_json_strict, JsonLimits};
use gantry_core::ports::sandbox::RawMaterial;
use gantry_core::settings::resource::MaterialIo;
use gantry_core::worker::JobSpec;
use serde::Serialize;

use crate::spec::StagingLayout;
use materials::{pack_materials, NonceSource, PackedMaterial};

/// `output.schema.json` 在容器里的路径。
pub const OUTPUT_SCHEMA_PATH: &str = "/gantry/inputs/output.schema.json";
/// review / verify 用的差异文件。
pub const DIFF_PATCH_NAME: &str = "diff.patch";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StageError {
    /// 引擎侧缺陷：不重试（→ `SandboxError::InvalidSpec`）。
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
    /// 宿主文件操作失败（→ `Infra{WorkspacePrep}`）。
    #[error("staging I/O failed: {0}")]
    Io(String),
}

fn io_err(context: &str) -> impl Fn(std::io::Error) -> StageError + '_ {
    move |err| StageError::Io(format!("{context}: {err}"))
}

/// 一个上游节点的已取出文件（`inputs/upstream/<node_id>/<name>`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamFiles {
    pub node_id: NodeId,
    pub files: Vec<(String, Vec<u8>)>,
}

/// staging 的全部输入。
pub struct StageRequest<'a> {
    /// JobSpec 的可信部分；`materials` 与 `isolation.nonce` 由本步骤填写。
    pub job: &'a JobSpec,
    pub raw_materials: &'a [RawMaterial],
    pub materials_io: &'a MaterialIo,
    /// 结构化输出 schema 的字节（契约缺口：应来自 core 的内置注册表）。
    pub output_schema: &'a [u8],
    /// `repo.prepare_workspace` 返回的差异（review / verify）。
    pub diff_patch: Option<&'a [u8]>,
    pub upstream: &'a [UpstreamFiles],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: Sha256Digest,
    pub bytes: u64,
}

/// `inputs/MANIFEST.json`（worker-protocol §2.1）：用于发现挂载错误，不是安全机制。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub job_sha256: Sha256Digest,
    pub files: Vec<ManifestFile>,
}

/// staging 的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct Staged {
    pub job: JobSpec,
    pub job_bytes: Vec<u8>,
    pub job_sha256: Sha256Digest,
    pub manifest: Manifest,
    pub materials: Vec<PackedMaterial>,
}

/// 只接受单层、无路径分隔、不以点开头的文件名。
fn safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 规范化序列化 JobSpec，并确认它能被严格解析回同一个值（代替 JSON Schema 校验，见模块说明）。
pub fn canonical_job_bytes(job: &JobSpec) -> Result<Vec<u8>, StageError> {
    job.validate()
        .map_err(|err| StageError::InvalidSpec(format!("job.json: {err}")))?;
    let value = serde_json::to_value(job)
        .map_err(|err| StageError::InvalidSpec(format!("job.json serialization: {err}")))?;
    let bytes = jcs(&value)
        .map_err(|err| StageError::InvalidSpec(format!("job.json canonical form: {err}")))?;
    let reparsed = parse_json_strict(&bytes, JsonLimits::default())
        .map_err(|err| StageError::InvalidSpec(format!("job.json strict parse: {err}")))?;
    let round: JobSpec = serde_json::from_value(reparsed).map_err(|err| {
        StageError::InvalidSpec(format!("job.json does not match JobSpec: {err}"))
    })?;
    if &round != job {
        return Err(StageError::InvalidSpec(
            "job.json does not round-trip".into(),
        ));
    }
    Ok(bytes)
}

/// 写临时文件后 `rename`，再 `fsync` 父目录之外的文件本身（目录 fsync 留给调用方的相位持久化）。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StageError> {
    let tmp = path.with_extension("gantry-tmp");
    {
        let mut file = fs::File::create(&tmp).map_err(io_err("create temp file"))?;
        file.write_all(bytes).map_err(io_err("write temp file"))?;
        file.sync_all().map_err(io_err("fsync temp file"))?;
    }
    fs::rename(&tmp, path).map_err(io_err("rename temp file"))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), StageError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_err("chmod"))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), StageError> {
    Ok(())
}

/// 自底向上把 `inputs/` 下的文件设为 `0444`、目录设为 `0555`。
fn seal(dir: &Path) -> Result<(), StageError> {
    for entry in fs::read_dir(dir).map_err(io_err("list inputs"))? {
        let entry = entry.map_err(io_err("list inputs"))?;
        let kind = entry.file_type().map_err(io_err("stat inputs"))?;
        if kind.is_dir() {
            seal(&entry.path())?;
        } else {
            set_mode(&entry.path(), 0o444)?;
        }
    }
    set_mode(dir, 0o555)
}

/// 销毁前恢复目录的写权限，让 `remove_dir_all` 能删掉封好的 `inputs/`。
/// 不跟随符号链接；出错时尽力而为（删除失败由调用方报告）。
pub fn unseal(dir: &Path) {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let _ = set_mode(dir, 0o755);
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                unseal(&entry.path());
            }
        }
    }
}

/// 执行 staging。`prepare_dirs` 之后、`start` 之前调用；`out/` 必须为空。
pub fn stage(
    layout: &StagingLayout,
    request: &StageRequest<'_>,
    rng: &mut dyn NonceSource,
) -> Result<Staged, StageError> {
    let job = request.job;
    let schema_digest = sha256(request.output_schema);
    if schema_digest != job.output.structured_schema.sha256 {
        return Err(StageError::InvalidSpec(
            "output schema bytes do not match job.output.structured_schema.sha256".into(),
        ));
    }
    if job.output.structured_schema.path != OUTPUT_SCHEMA_PATH {
        return Err(StageError::InvalidSpec(format!(
            "job.output.structured_schema.path must be {OUTPUT_SCHEMA_PATH}"
        )));
    }

    let packed = pack_materials(
        request.raw_materials,
        request.materials_io,
        &job.isolation,
        rng,
    )?;
    let mut final_job = job.clone();
    final_job.isolation.nonce = packed.nonce.clone();
    final_job.materials = packed.items.iter().map(|item| item.meta.clone()).collect();
    let job_bytes = canonical_job_bytes(&final_job)?;
    secrets::assert_no_secret_like_values(&job_bytes)?;
    let job_sha256 = sha256(&job_bytes);

    let out_empty = fs::read_dir(layout.out())
        .map_err(io_err("list out/"))?
        .next()
        .is_none();
    if !out_empty {
        return Err(StageError::Io("out/ is not empty before start".into()));
    }

    let inputs = layout.inputs();
    let mut written: Vec<(PathBuf, String, Vec<u8>)> = Vec::new();
    let materials_dir = inputs.join("materials");
    fs::create_dir_all(&materials_dir).map_err(io_err("create materials/"))?;
    for item in &packed.items {
        written.push((
            materials_dir.join(&item.file_name),
            item.meta.path.clone(),
            item.wrapped.clone(),
        ));
    }
    if !request.upstream.is_empty() {
        let upstream_dir = inputs.join("upstream");
        for node in request.upstream {
            let node_dir = upstream_dir.join(node.node_id.to_string());
            fs::create_dir_all(&node_dir).map_err(io_err("create upstream/"))?;
            for (name, bytes) in &node.files {
                if !safe_file_name(name) {
                    return Err(StageError::InvalidSpec(format!(
                        "upstream file name {name:?} is not a plain file name"
                    )));
                }
                written.push((
                    node_dir.join(name),
                    format!("/gantry/inputs/upstream/{}/{name}", node.node_id),
                    bytes.clone(),
                ));
            }
        }
    }
    if let Some(diff) = request.diff_patch {
        written.push((
            inputs.join(DIFF_PATCH_NAME),
            format!("/gantry/inputs/{DIFF_PATCH_NAME}"),
            diff.to_vec(),
        ));
    }
    written.push((
        inputs.join("output.schema.json"),
        OUTPUT_SCHEMA_PATH.to_owned(),
        request.output_schema.to_vec(),
    ));

    let mut files = Vec::with_capacity(written.len());
    for (host_path, container_path, bytes) in &written {
        write_atomic(host_path, bytes)?;
        files.push(ManifestFile {
            path: container_path.clone(),
            sha256: sha256(bytes),
            bytes: bytes.len() as u64,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest { job_sha256, files };
    let manifest_value = serde_json::to_value(&manifest)
        .map_err(|err| StageError::Io(format!("manifest serialization: {err}")))?;
    let manifest_bytes = jcs(&manifest_value)
        .map_err(|err| StageError::Io(format!("manifest canonical form: {err}")))?;
    write_atomic(&inputs.join("MANIFEST.json"), &manifest_bytes)?;
    write_atomic(&layout.job_json(), &job_bytes)?;
    set_mode(&layout.job_json(), 0o444)?;

    // READY 最后出现：worker 见到它才读 job.json（§7.2 第 7 步）。
    write_atomic(&inputs.join("READY"), b"")?;
    seal(&inputs)?;

    Ok(Staged {
        job: final_job,
        job_bytes,
        job_sha256,
        manifest,
        materials: packed.items,
    })
}
