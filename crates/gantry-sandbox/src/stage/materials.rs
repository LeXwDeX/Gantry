//! 材料打包流水线（sandbox.md §7.2 第 2 步，worker-protocol §4.4）。
//!
//! 规范化 → 截断 → nonce → 包装 → 登记。标记行只含服务端生成的值或经模式校验的
//! 标识符（W5）：`id` 是受限字符集，`kind` 来自枚举，`source` 必须是
//! `owner/repo#N` 或 `owner/repo#N/<item_id>`，否则 `InvalidSpec`。

use std::collections::BTreeSet;

use gantry_core::canon::{sha256, RepoSlug};
use gantry_core::ports::sandbox::RawMaterial;
use gantry_core::settings::resource::MaterialIo;
use gantry_core::text::{normalize_untrusted, NormalizeLimits, NormalizeStats};
use gantry_core::worker::{JobIsolation, JobMaterial, MaterialKind, MaterialSource};

use super::StageError;

/// 材料在容器里的目录。
pub const MATERIALS_DIR: &str = "/gantry/inputs/materials";

/// 总量放不下时只留的标记行（OQ-S12 [提议]）。
pub const OMITTED_MARKER: &str = "[…truncated…]";

/// 后续条目至少还要有这么多剩余额度才保留正文（§7.2 [提议]：至少 1 KiB 头部）。
pub const MIN_ITEM_BYTES: u64 = 1024;

/// nonce 与材料冲突时最多重新生成几次；连续冲突说明随机源坏了。
const NONCE_ATTEMPTS: usize = 8;

/// 128 位随机数来源；测试注入固定序列以制造冲突。
pub trait NonceSource {
    fn fill(&mut self, buf: &mut [u8; 16]) -> Result<(), StageError>;
}

/// 操作系统 CSPRNG。
pub struct OsNonce;

impl NonceSource for OsNonce {
    fn fill(&mut self, buf: &mut [u8; 16]) -> Result<(), StageError> {
        getrandom::fill(buf).map_err(|err| StageError::Io(format!("OS random unavailable: {err}")))
    }
}

/// 一条打包好的材料：登记项 + 包装后的文件字节 + 规范化统计。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedMaterial {
    pub meta: JobMaterial,
    pub file_name: String,
    pub wrapped: Vec<u8>,
    pub stats: NormalizeStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedMaterials {
    pub nonce: String,
    pub items: Vec<PackedMaterial>,
}

/// 材料 `id` 进文件名与标记行：只允许 `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`，且不能全是点。
pub fn valid_material_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 解析 `owner/repo#N` 或 `owner/repo#N/<item_id>`；任何多余字符都拒绝。
pub fn parse_source(source: &str) -> Option<MaterialSource> {
    let (repo, rest) = source.split_once('#')?;
    let repo = RepoSlug::parse(repo).ok()?;
    let (number, item) = match rest.split_once('/') {
        Some((number, item)) => (number, Some(item)),
        None => (rest, None),
    };
    let number = parse_decimal(number)?;
    let item_id = match item {
        Some(item) => Some(parse_decimal(item)?),
        None => None,
    };
    Some(MaterialSource {
        repo,
        number,
        item_id,
    })
}

fn parse_decimal(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

fn source_text(source: &MaterialSource) -> String {
    match source.item_id {
        Some(item) => format!("{}#{}/{item}", source.repo, source.number),
        None => format!("{}#{}", source.repo, source.number),
    }
}

fn kind_text(kind: MaterialKind) -> &'static str {
    match kind {
        MaterialKind::IssueBody => "issue_body",
        MaterialKind::IssueComment => "issue_comment",
        MaterialKind::PrBody => "pr_body",
        MaterialKind::PrComment => "pr_comment",
        MaterialKind::ReviewComment => "review_comment",
        MaterialKind::ReviewThread => "review_thread",
        MaterialKind::CiLog => "ci_log",
        MaterialKind::CommitMessage => "commit_message",
        MaterialKind::DependencyDoc => "dependency_doc",
        MaterialKind::FileExcerpt => "file_excerpt",
        MaterialKind::UpstreamOutput => "upstream_output",
    }
}

/// 规范化 + 截断后的正文（尚未包装）。
struct Body {
    text: String,
    stats: NormalizeStats,
}

/// 按给定顺序分配总量额度：单条不超过 `max_item_bytes`；后续条目取剩余额度，
/// 剩余不足 [`MIN_ITEM_BYTES`] 时只留 [`OMITTED_MARKER`]（OQ-S12 [提议]）。
fn normalize_all(raw: &[RawMaterial], io: &MaterialIo) -> Vec<Body> {
    let mut remaining = io.max_total_bytes;
    raw.iter()
        .map(|material| {
            let input = material.text.as_untrusted_str().as_bytes();
            if remaining < MIN_ITEM_BYTES {
                let mut stats = normalize_untrusted(
                    input,
                    NormalizeLimits {
                        max_bytes: usize::MAX,
                        head_bytes: 0,
                        tail_bytes: 0,
                    },
                )
                .stats;
                stats.truncated = true;
                return Body {
                    text: OMITTED_MARKER.to_owned(),
                    stats,
                };
            }
            let limit = io.max_item_bytes.min(remaining);
            let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
            // 额度很小时整段给头部；否则头尾各半。
            let (head, tail) = if limit < 2 * MIN_ITEM_BYTES {
                (limit_usize, 0)
            } else {
                (limit_usize - limit_usize / 2, limit_usize / 2)
            };
            let normalized = normalize_untrusted(
                input,
                NormalizeLimits {
                    max_bytes: limit_usize,
                    head_bytes: head,
                    tail_bytes: tail,
                },
            );
            remaining = remaining.saturating_sub(normalized.text.len() as u64);
            Body {
                text: normalized.text,
                stats: normalized.stats,
            }
        })
        .collect()
}

fn expand(
    template: &str,
    nonce: &str,
    id: &str,
    kind: &str,
    source: &str,
    truncated: bool,
) -> String {
    template
        .replace("{nonce}", nonce)
        .replace("{id}", id)
        .replace("{kind}", kind)
        .replace("{source}", source)
        .replace("{truncated}", if truncated { "true" } else { "false" })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 校验、规范化、截断、生成 nonce 并包装全部材料。
pub fn pack_materials(
    raw: &[RawMaterial],
    io: &MaterialIo,
    isolation: &JobIsolation,
    rng: &mut dyn NonceSource,
) -> Result<PackedMaterials, StageError> {
    // 模板必须带 nonce，否则结束标记可以被材料伪造。
    if !isolation.begin.contains("{nonce}") || !isolation.end.contains("{nonce}") {
        return Err(StageError::InvalidSpec(
            "isolation templates must contain {nonce}".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut sources = Vec::with_capacity(raw.len());
    for material in raw {
        if !valid_material_id(&material.id) {
            return Err(StageError::InvalidSpec(format!(
                "material id {:?} is not a plain identifier",
                material.id
            )));
        }
        if !ids.insert(material.id.to_ascii_lowercase()) {
            return Err(StageError::InvalidSpec(format!(
                "duplicate material id {}",
                material.id
            )));
        }
        let source = parse_source(&material.source).ok_or_else(|| {
            StageError::InvalidSpec(format!(
                "material {} source must be owner/repo#N or owner/repo#N/<item_id>",
                material.id
            ))
        })?;
        sources.push(source);
    }

    let bodies = normalize_all(raw, io);

    // 材料文本在 nonce 生成之前就已固定；仍检查碰撞（含伪造的标记串）。
    let mut nonce = None;
    for _ in 0..NONCE_ATTEMPTS {
        let mut buf = [0u8; 16];
        rng.fill(&mut buf)?;
        let candidate = hex(&buf);
        if bodies.iter().all(|body| !body.text.contains(&candidate)) {
            nonce = Some(candidate);
            break;
        }
    }
    let nonce = nonce.ok_or_else(|| {
        StageError::Io("could not generate a nonce absent from the materials".into())
    })?;

    let items = raw
        .iter()
        .zip(sources)
        .zip(bodies)
        .map(|((material, source), body)| {
            let kind = kind_text(material.kind);
            let source_str = source_text(&source);
            let truncated = body.stats.truncated;
            let begin = expand(
                &isolation.begin,
                &nonce,
                &material.id,
                kind,
                &source_str,
                truncated,
            );
            let end = expand(
                &isolation.end,
                &nonce,
                &material.id,
                kind,
                &source_str,
                truncated,
            );
            let mut wrapped = String::with_capacity(begin.len() + body.text.len() + end.len() + 2);
            wrapped.push_str(&begin);
            wrapped.push('\n');
            wrapped.push_str(&body.text);
            if !body.text.ends_with('\n') {
                wrapped.push('\n');
            }
            wrapped.push_str(&end);
            wrapped.push('\n');
            let wrapped = wrapped.into_bytes();
            let file_name = format!("{}.txt", material.id);
            PackedMaterial {
                meta: JobMaterial {
                    id: material.id.clone(),
                    kind: material.kind,
                    source,
                    path: format!("{MATERIALS_DIR}/{file_name}"),
                    sha256: sha256(&wrapped),
                    bytes: wrapped.len() as u64,
                    truncated,
                    original_bytes: body.stats.original_bytes as u64,
                },
                file_name,
                wrapped,
                stats: body.stats,
            }
        })
        .collect();
    Ok(PackedMaterials { nonce, items })
}
