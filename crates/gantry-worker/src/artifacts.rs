//! `out/` 产物的写入（worker-protocol §5.1–§5.4；worker.md §7.5）。
//!
//! 每个文件都先写 `<name>.tmp`、`fsync`、再 `rename`；`result.json` 必须最后写。

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use gantry_core::canon::sha256;
use gantry_core::worker::{ArtifactRef, Proposals, ResultJson};

pub const RESULT_JSON: &str = "result.json";
pub const PROPOSALS_JSON: &str = "proposals.json";
pub const REPORT_MD: &str = "report.md";
pub const CHANGES_PATCH: &str = "changes.patch";

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("{0} is not valid: {1}")]
    Invalid(&'static str, String),
    #[error("cannot write {0}: {1}")]
    Io(&'static str, io::Error),
}

/// 原子写 `out/<name>`，返回协议里的引用（`path` 相对 `out/`，worker-protocol §5.2）。
pub fn write_atomic(
    out: &Path,
    name: &'static str,
    bytes: &[u8],
) -> Result<ArtifactRef, ArtifactError> {
    write_tmp(out, name, bytes)?;
    commit_tmp(out, name)?;
    Ok(ArtifactRef {
        path: name.to_owned(),
        sha256: sha256(bytes),
        bytes: bytes.len() as u64,
    })
}

/// 原子写的第一步：写 `<name>.tmp` 并 `fsync`。此后崩溃，`out/` 里只有 `.tmp`。
pub fn write_tmp(out: &Path, name: &'static str, bytes: &[u8]) -> Result<(), ArtifactError> {
    let io_err = |err| ArtifactError::Io(name, err);
    let mut file = File::create(out.join(format!("{name}.tmp"))).map_err(io_err)?;
    file.write_all(bytes).map_err(io_err)?;
    file.sync_all().map_err(io_err)
}

/// 原子写的第二步：`rename` 为正式文件名，并让目录项落盘（否则崩溃后 rename 可能丢失）。
pub fn commit_tmp(out: &Path, name: &'static str) -> Result<(), ArtifactError> {
    let io_err = |err| ArtifactError::Io(name, err);
    fs::rename(out.join(format!("{name}.tmp")), out.join(name)).map_err(io_err)?;
    File::open(out)
        .and_then(|dir| dir.sync_all())
        .map_err(io_err)
}

pub fn write_proposals(out: &Path, proposals: &Proposals) -> Result<ArtifactRef, ArtifactError> {
    for proposal in &proposals.proposals {
        proposal
            .validate()
            .map_err(|err| ArtifactError::Invalid(PROPOSALS_JSON, err.to_string()))?;
    }
    let bytes = serde_json::to_vec(proposals)
        .map_err(|err| ArtifactError::Invalid(PROPOSALS_JSON, err.to_string()))?;
    write_atomic(out, PROPOSALS_JSON, &bytes)
}

/// 写 `result.json`。调用方保证其他产物都已写完；本函数先做本地一致性校验。
pub fn write_result(out: &Path, result: &ResultJson) -> Result<ArtifactRef, ArtifactError> {
    result
        .validate()
        .map_err(|err| ArtifactError::Invalid(RESULT_JSON, err.to_string()))?;
    let bytes = serde_json::to_vec(result)
        .map_err(|err| ArtifactError::Invalid(RESULT_JSON, err.to_string()))?;
    write_atomic(out, RESULT_JSON, &bytes)
}

/// `report.md` 的大小上限（worker.md §7.5）。
pub const MAX_REPORT_BYTES: usize = 256 * 1024;
const REPORT_TRUNCATED: &str = "\n\n[report truncated by gantry-worker]\n";

/// 写 `report.md`，超过上限时在字符边界截断并附说明行；返回引用与是否截断。
pub fn write_report(out: &Path, text: &str) -> Result<(ArtifactRef, bool), ArtifactError> {
    if text.len() <= MAX_REPORT_BYTES {
        return write_atomic(out, REPORT_MD, text.as_bytes()).map(|r| (r, false));
    }
    let mut end = MAX_REPORT_BYTES - REPORT_TRUNCATED.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bytes = text.as_bytes()[..end].to_vec();
    bytes.extend_from_slice(REPORT_TRUNCATED.as_bytes());
    write_atomic(out, REPORT_MD, &bytes).map(|r| (r, true))
}
