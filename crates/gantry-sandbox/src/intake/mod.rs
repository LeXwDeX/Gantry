//! intake：先停后取的产物提取规则（sandbox.md §7.8，worker-protocol §8.2 V2）。
//!
//! 规则与读取方式无关：[`host::read_out_dir`]（方式 A，主机直读）与
//! [`tar::read_out_tar`]（方式 B，运行时归档导出）把 `out/` 的目录项当作不可信
//! 条目流，逐项交给本文件的同一套判定。调用方负责保证容器已停止（§7.5）；
//! 本模块不解释产物内容，`result.json` 的 schema 等语义校验归 `engine`（V3、V4）。

#[cfg(unix)]
pub mod host;
pub mod tar;

use std::collections::{BTreeMap, BTreeSet};

use gantry_core::json_strict::{parse_json_strict, JsonLimits};
use gantry_core::ports::sandbox::{ArtifactName, UnexpectedArtifact};
use gantry_core::settings::resource::ArtifactIo;
use gantry_core::worker::{PatchLimits, PatchMode};

/// 白名单里的四个产物（区分大小写，名字里不得含路径分隔）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Artifact {
    ResultJson,
    ChangesPatch,
    ReportMd,
    ProposalsJson,
}

impl Artifact {
    pub const ALL: [Artifact; 4] = [
        Artifact::ResultJson,
        Artifact::ChangesPatch,
        Artifact::ReportMd,
        Artifact::ProposalsJson,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::ResultJson => "result.json",
            Self::ChangesPatch => "changes.patch",
            Self::ReportMd => "report.md",
            Self::ProposalsJson => "proposals.json",
        }
    }

    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|artifact| artifact.file_name().as_bytes() == name)
    }
}

/// `out/` 的取舍参数，由 `sandbox.io.artifacts.*` 与 JobSpec 合成。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntakePlan {
    pub result_max_bytes: u64,
    pub report_max_bytes: u64,
    pub proposals_max_bytes: u64,
    pub patch_max_bytes: u64,
    pub patch: PatchMode,
    /// `out/` 总配额（`sandbox.out_quota_bytes`）；方式 B 的归档读取以它封顶。
    pub total_max_bytes: u64,
}

impl IntakePlan {
    /// `changes.patch` 的上限取 `JobSpec.constraints.patch.max_bytes`；没有时（`forbidden`）为 0。
    pub fn new(
        io: &ArtifactIo,
        patch: PatchMode,
        patch_limits: Option<&PatchLimits>,
        total_max_bytes: u64,
    ) -> Self {
        Self {
            result_max_bytes: io.result_max_bytes,
            report_max_bytes: io.report_max_bytes,
            proposals_max_bytes: io.proposals_max_bytes,
            patch_max_bytes: patch_limits.map_or(0, |limits| limits.max_bytes),
            patch,
            total_max_bytes,
        }
    }

    pub fn limit(&self, artifact: Artifact) -> u64 {
        match artifact {
            Artifact::ResultJson => self.result_max_bytes,
            Artifact::ChangesPatch => self.patch_max_bytes,
            Artifact::ReportMd => self.report_max_bytes,
            Artifact::ProposalsJson => self.proposals_max_bytes,
        }
    }
}

/// 提取结果：通过判定的原始字节（之后落 CAS，只用 CAS 副本）与丢弃项。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntakeResult {
    pub files: BTreeMap<ArtifactName, Vec<u8>>,
    pub unexpected: Vec<UnexpectedArtifact>,
}

/// 丢弃原因（稳定的 snake_case，进 `ResultBundle.validation.unexpected_artifacts`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    NotWhitelisted,
    InvalidName,
    Symlink,
    Hardlink,
    Directory,
    Fifo,
    Device,
    Socket,
    OtherType,
    TooLarge,
    InvalidUtf8,
    InvalidJson,
    PatchForbidden,
    DuplicateEntry,
    Changed,
    ArchiveExtensionHeader,
    ArchiveSparse,
    ArchiveMalformed,
    ArchiveTotalExceeded,
    TooManyEntries,
}

impl Reject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotWhitelisted => "not_whitelisted",
            Self::InvalidName => "invalid_name",
            Self::Symlink => "symlink",
            Self::Hardlink => "hardlink",
            Self::Directory => "directory",
            Self::Fifo => "fifo",
            Self::Device => "device",
            Self::Socket => "socket",
            Self::OtherType => "not_regular_file",
            Self::TooLarge => "too_large",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::InvalidJson => "invalid_json",
            Self::PatchForbidden => "patch_forbidden",
            Self::DuplicateEntry => "duplicate_entry",
            Self::Changed => "changed_during_read",
            Self::ArchiveExtensionHeader => "archive_extension_header",
            Self::ArchiveSparse => "archive_sparse",
            Self::ArchiveMalformed => "archive_malformed",
            Self::ArchiveTotalExceeded => "archive_total_exceeded",
            Self::TooManyEntries => "too_many_entries",
        }
    }
}

/// `out/` 里最多看这么多个目录项；更多的不再枚举。
pub const MAX_ENTRIES: usize = 1024;
/// `unexpected` 最多记这么多条，其余只计数（避免沙盒用文件名撑大结果包）。
pub const MAX_UNEXPECTED: usize = 64;
/// `unexpected` 里名字的上限（字节）。
pub const MAX_NAME_BYTES: usize = 255;

/// 两种读取方式共用的累加器：判定、记录、去重。
#[derive(Debug, Default)]
pub(crate) struct Collector {
    result: IntakeResult,
    seen: BTreeSet<Artifact>,
    unexpected_overflow: u64,
}

impl Collector {
    pub(crate) fn reject(&mut self, raw_name: &[u8], reason: Reject) {
        if self.result.unexpected.len() >= MAX_UNEXPECTED {
            self.unexpected_overflow += 1;
            return;
        }
        self.result.unexpected.push(UnexpectedArtifact {
            name: display_name(raw_name),
            reason: reason.as_str().to_owned(),
        });
    }

    /// 读到的字节已不超过 `limit + 1`；这里做大小、文本与去重判定。
    /// 同名条目出现第二次（归档里可能）时，该产物整体作废。
    pub(crate) fn offer(&mut self, artifact: Artifact, bytes: Vec<u8>, plan: &IntakePlan) {
        let raw = artifact.file_name().as_bytes();
        if !self.seen.insert(artifact) {
            self.result
                .files
                .remove(&ArtifactName::new(artifact.file_name()));
            self.reject(raw, Reject::DuplicateEntry);
            return;
        }
        match check_content(artifact, &bytes, plan) {
            Ok(()) => {
                self.result
                    .files
                    .insert(ArtifactName::new(artifact.file_name()), bytes);
            }
            Err(reason) => self.reject(raw, reason),
        }
    }

    /// 归档级 / 目录级的结构性记录（`<archive>` 损坏或超限、条目过多）：不受条数上限影响，
    /// 否则沙盒可以先用大量垃圾文件占满 `unexpected`，再把"读取被中止"藏起来。
    pub(crate) fn note(&mut self, label: &str, reason: Reject) {
        self.result.unexpected.push(UnexpectedArtifact {
            name: label.to_owned(),
            reason: reason.as_str().to_owned(),
        });
    }

    pub(crate) fn finish(mut self) -> IntakeResult {
        if self.unexpected_overflow > 0 {
            let label = format!("<{} more>", self.unexpected_overflow);
            self.note(&label, Reject::TooManyEntries);
        }
        self.result
    }
}

/// 读字节之前就能做的判定：白名单与 `output.patch`。
pub(crate) fn pre_check(name: &[u8], plan: &IntakePlan) -> Result<Artifact, Reject> {
    if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
        return Err(Reject::InvalidName);
    }
    let artifact = Artifact::from_name(name).ok_or(Reject::NotWhitelisted)?;
    if artifact == Artifact::ChangesPatch && plan.patch == PatchMode::Forbidden {
        return Err(Reject::PatchForbidden);
    }
    Ok(artifact)
}

fn check_content(artifact: Artifact, bytes: &[u8], plan: &IntakePlan) -> Result<(), Reject> {
    if bytes.len() as u64 > plan.limit(artifact) {
        return Err(Reject::TooLarge);
    }
    match artifact {
        Artifact::ResultJson | Artifact::ProposalsJson => {
            std::str::from_utf8(bytes).map_err(|_| Reject::InvalidUtf8)?;
            parse_json_strict(bytes, JsonLimits::permissive()).map_err(|_| Reject::InvalidJson)?;
        }
        Artifact::ReportMd => {
            std::str::from_utf8(bytes).map_err(|_| Reject::InvalidUtf8)?;
        }
        Artifact::ChangesPatch => {}
    }
    Ok(())
}

/// 不可信文件名的展示形式：有损 UTF-8、去控制字符、截断。
fn display_name(raw: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(raw);
    let mut out = String::new();
    for ch in lossy.chars() {
        if ch.is_control() || is_bidi_or_tag(ch) {
            out.push('\u{FFFD}');
        } else {
            out.push(ch);
        }
        if out.len() >= MAX_NAME_BYTES {
            break;
        }
    }
    out
}

fn is_bidi_or_tag(ch: char) -> bool {
    matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{E0000}'..='\u{E007F}')
}
