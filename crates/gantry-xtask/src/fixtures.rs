//! 契约夹具：原样提取文档 JSON 块、记录来源与哈希、按注解校验。
//!
//! 注解只声明“这一块对应哪个 schema / Rust 类型”。没有注解的块保持
//! `unverified` 并列入清单，不补字段、不假装通过。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::docs;

pub const DOCUMENTS: &[&str] = &[
    "docs/interfaces/api.md",
    "docs/interfaces/config.md",
    "docs/interfaces/egress-call-terminal.md",
    "docs/interfaces/egress-control.md",
    "docs/interfaces/forge-actions.md",
    "docs/interfaces/worker-protocol.md",
];

#[derive(Serialize)]
pub struct Manifest<'a> {
    pub generated_by: &'a str,
    pub extraction: &'a str,
    pub blocks: Vec<BlockEntry>,
    pub summary: BTreeMap<String, usize>,
}

#[derive(Serialize, Clone)]
pub struct BlockEntry {
    pub id: String,
    pub source: String,
    pub line: usize,
    pub context: String,
    pub sha256: String,
    pub bytes: usize,
    pub file: String,
    pub status: String,
    pub schema: Option<String>,
    pub rust: Option<String>,
    pub note: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Annotation {
    pub sha256: String,
    pub schema: String,
    pub rust: String,
    /// `verify`（默认）：schema + Rust 都必须通过；`partial`：示例省略了可选/必填
    /// 字段时，schema 仍校验已出现的字段，但不做 Rust round-trip；`skip`：显式
    /// 不验证，并必须在 `unverified.json` 登记原因。
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Deserialize)]
pub struct AnnotationFile {
    pub version: u32,
    pub annotations: Vec<Annotation>,
}

/// 显式登记无法按当前 schema 完整验证的文档块。
///
/// 只允许两类：`example`（原文示意/省略，不能据此证明实现）与
/// `contract-undefined`（权威契约未定义）。实现漏项没有登记类别：任何新增
/// 未验证块都直接失败，避免用 warning 隐藏待修复问题。
#[derive(Deserialize)]
pub struct UnverifiedFile {
    pub version: u32,
    pub blocks: Vec<UnverifiedBlock>,
}

#[derive(Deserialize, Clone)]
pub struct UnverifiedBlock {
    pub sha256: String,
    pub kind: String,
    pub note: String,
    pub source: String,
    pub line: usize,
}

pub struct Extracted {
    pub manifest: Manifest<'static>,
    pub files: Vec<(PathBuf, Vec<u8>)>,
}

/// 提取全部块并写出原文；返回需要写入磁盘的文件。
pub fn extract(root: &Path) -> Result<Extracted, String> {
    let annotations = annotations(root)?;
    let by_sha: BTreeMap<String, &Annotation> = annotations
        .iter()
        .map(|annotation| (annotation.sha256.clone(), annotation))
        .collect();
    let mut entries = Vec::new();
    let mut files = Vec::new();
    for document in DOCUMENTS {
        let stem = Path::new(document)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| format!("bad document path {document}"))?;
        for fence in docs::json_fences(root, document)? {
            let id = format!("{stem}-L{}", fence.line);
            let relative = format!("fixtures/contracts/blocks/{stem}/L{}.json", fence.line);
            let parsed = serde_json::from_str::<Value>(&fence.body);
            let annotation = by_sha.get(&fence.sha256);
            let (status, schema, rust, note, error) = match parsed {
                Err(error) => (
                    "invalid_json".to_owned(),
                    annotation.map(|a| a.schema.clone()),
                    annotation.map(|a| a.rust.clone()),
                    annotation.and_then(|a| a.note.clone()),
                    Some(error.to_string()),
                ),
                Ok(_) => match annotation {
                    Some(annotation) => {
                        let status = match annotation.mode.as_deref() {
                            Some("skip") => "skipped",
                            Some("partial") => "partial",
                            None | Some("verify") if annotation.rust == "none" => "schema-only",
                            None | Some("verify") => "verified",
                            Some(_) => "annotated",
                        };
                        (
                            status.to_owned(),
                            Some(annotation.schema.clone()),
                            Some(annotation.rust.clone()),
                            annotation.note.clone(),
                            None,
                        )
                    }
                    None => (
                        "unverified".to_owned(),
                        None,
                        None,
                        Some("未注解：示意、省略或没有唯一的 schema 映射".to_owned()),
                        None,
                    ),
                },
            };
            entries.push(BlockEntry {
                id,
                source: fence.source.clone(),
                line: fence.line,
                context: fence.context.clone(),
                sha256: fence.sha256.clone(),
                bytes: fence.body.len(),
                file: relative.clone(),
                status,
                schema,
                rust,
                note,
                error,
            });
            files.push((PathBuf::from(relative), fence.body.into_bytes()));
        }
    }
    let mut summary = BTreeMap::new();
    for entry in &entries {
        *summary.entry(entry.status.clone()).or_insert(0) += 1;
    }
    let manifest = Manifest {
        generated_by: "cargo xtask schemas",
        extraction: "raw fenced JSON body; `line` is the 1-based opening fence line; sha256 is over the raw body without trailing newline",
        blocks: entries,
        summary,
    };
    Ok(Extracted { manifest, files })
}

pub fn annotations(root: &Path) -> Result<Vec<Annotation>, String> {
    let path = root.join("fixtures/contracts/annotations.json");
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "{}: {error}（缺少注解文件时无法区分已验证与示意块）",
            path.display()
        )
    })?;
    let file: AnnotationFile =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    if file.version != 1 {
        return Err(format!("annotations.json: 未知 version {}", file.version));
    }
    Ok(file.annotations)
}

#[derive(Deserialize)]
pub struct NegativeCase {
    pub id: String,
    pub schema: String,
    pub rust: String,
    #[serde(default)]
    pub event_type: Option<String>,
    pub data: Value,
    pub should_fail: Vec<String>,
}

pub fn negative_cases(root: &Path) -> Result<Vec<NegativeCase>, String> {
    let path = root.join("fixtures/contracts/negative/cases.json");
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

pub struct FixtureCheck {
    pub failures: Vec<String>,
    pub warnings: Vec<String>,
    /// 完整验证：schema + Rust round-trip（或 validate）。
    pub verified: usize,
    /// 只验证 schema：注解为 `rust=none`，没有 Rust 证据。
    pub schema_only: usize,
    /// 部分验证：只校验 schema，示例省略了必填字段，不做 Rust round-trip。
    pub partial: usize,
    /// `unverified.json` 显式登记的块（skip / 未注解示意）。
    pub allowlisted: usize,
    /// 其中注解为 `skip` 的块数。
    pub skipped: usize,
    pub unverified: BTreeSet<String>,
}

/// 校验已注解块：`verified` 必须通过 schema 与 Rust round-trip；`schema-only` 只校验
/// schema 且要求 `rust=none`；`partial` 只校验 schema 且要求 `rust=none`；`skipped` /
/// 未注解块必须在 `unverified.json` 登记。
pub fn check(root: &Path, manifest: &Manifest<'_>) -> Result<FixtureCheck, String> {
    let mut failures = Vec::new();
    let warnings = Vec::new();
    let mut verified = 0;
    let mut schema_only = 0;
    let mut partial = 0;
    let mut allowlisted = 0;
    let mut skipped = 0;
    let mut unverified = BTreeSet::new();
    let annotations = annotations(root)?;
    let allowance = unverified_blocks(root)?;
    for block in allowance.values() {
        if !matches!(block.kind.as_str(), "example" | "contract-undefined") {
            failures.push(format!(
                "unverified.json 条目 {}:{} 的 kind `{}` 不被允许（实现漏项必须修复，不能登记为 warning）",
                block.source, block.line, block.kind
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for annotation in &annotations {
        seen.insert(annotation.sha256.clone());
        if annotation.schema.starts_with("openapi:") {
            let name = &annotation.schema["openapi:".len()..];
            let text = fs::read_to_string(root.join("schemas/openapi.yaml"))
                .map_err(|error| format!("schemas/openapi.yaml: {error}"))?;
            let document: Value = serde_json::from_str(&text)
                .map_err(|error| format!("schemas/openapi.yaml: {error}"))?;
            if document
                .pointer(&format!("/components/schemas/{name}"))
                .is_none()
            {
                failures.push(format!(
                    "annotations.json 引用的 OpenAPI 组件不存在：{name}"
                ));
            }
        } else if let Some(scope) = annotation.schema.strip_prefix("settings-aggregate:") {
            if !matches!(scope, "global" | "project") {
                failures.push(format!(
                    "annotations.json 引用了未知 settings 聚合范围：{scope}"
                ));
            }
        } else if !root.join(&annotation.schema).is_file() {
            failures.push(format!(
                "annotations.json 引用的 schema 不存在：{}",
                annotation.schema
            ));
        }
    }
    for entry in &manifest.blocks {
        if entry.status == "invalid_json" && entry.schema.is_some() {
            failures.push(format!(
                "{}:{} 已注解但块不是合法 JSON：{}",
                entry.source,
                entry.line,
                entry.error.as_deref().unwrap_or("unknown")
            ));
            continue;
        }
        if entry.status == "annotated" {
            failures.push(format!(
                "{}:{} 的注解 mode 未知；只允许 verify / partial / skip",
                entry.source, entry.line
            ));
            continue;
        }
        if !matches!(
            entry.status.as_str(),
            "verified" | "partial" | "schema-only"
        ) {
            match allowance.get(&entry.sha256) {
                Some(block) if block.source == entry.source && block.line == entry.line => {
                    unverified.insert(format!(
                        "{}:{} status={} kind={} note={}",
                        entry.source, entry.line, entry.status, block.kind, block.note
                    ));
                    allowlisted += 1;
                    if entry.status == "skipped" {
                        skipped += 1;
                    }
                }
                Some(block) => {
                    unverified.insert(format!(
                        "{}:{} status={} context={}",
                        entry.source, entry.line, entry.status, entry.context
                    ));
                    failures.push(format!(
                        "unverified.json 条目 {}:{} 与文档块 {}:{} 的来源不一致（sha256 相同但行号/文件不同）",
                        block.source, block.line, entry.source, entry.line
                    ));
                }
                None => {
                    unverified.insert(format!(
                        "{}:{} status={} context={}",
                        entry.source, entry.line, entry.status, entry.context
                    ));
                    failures.push(format!(
                        "{}:{} status={} 未在 unverified.json 登记；新增未验证块必须先修复或显式说明是示意/契约缺口",
                        entry.source, entry.line, entry.status
                    ));
                }
            }
            continue;
        }
        let Some(annotation) = annotations
            .iter()
            .find(|annotation| annotation.sha256 == entry.sha256)
        else {
            failures.push(format!(
                "{}:{} 有 status={} 但没有匹配注解",
                entry.source, entry.line, entry.status
            ));
            continue;
        };
        let mode = annotation.mode.as_deref().unwrap_or("verify");
        let status_mode = if entry.status == "partial" {
            "partial"
        } else {
            "verify"
        };
        if mode != status_mode {
            failures.push(format!(
                "{}:{} manifest status={} 与注解 mode={mode} 不一致（manifest 需重新生成）",
                entry.source, entry.line, entry.status
            ));
            continue;
        }
        let body = fs::read_to_string(root.join(&entry.file))
            .map_err(|error| format!("{}: {error}", entry.file))?;
        let data: Value =
            serde_json::from_str(&body).map_err(|error| format!("{}: {error}", entry.file))?;
        if let Some(scope) = annotation.schema.strip_prefix("settings-aggregate:") {
            if mode == "partial" {
                failures.push(format!(
                    "{}:{} settings 聚合注解不支持 partial（逐 resource schema 已能验证省略字段）",
                    entry.source, entry.line
                ));
                continue;
            }
            if let Err(error) = check_settings_aggregate(scope, &data) {
                failures.push(format!(
                    "{}:{} 未通过 settings 聚合资源校验：{error}",
                    entry.source, entry.line
                ));
                continue;
            }
        } else {
            let schema = match load_annotation_schema(root, annotation, mode == "partial") {
                Ok(schema) => schema,
                Err(error) => {
                    failures.push(format!(
                        "{}:{} 引用的 schema {} 读取/展开失败：{error}",
                        entry.source, entry.line, annotation.schema
                    ));
                    continue;
                }
            };
            match jsonschema::validator_for(&schema) {
                Err(error) => {
                    failures.push(format!(
                        "{}:{} schema {} 无法编译：{error}",
                        entry.source, entry.line, annotation.schema
                    ));
                    continue;
                }
                Ok(validator) => {
                    if let Err(error) = validator.validate(&data) {
                        failures.push(format!(
                            "{}:{} 未通过 {}（{mode}）：{error}",
                            entry.source, entry.line, annotation.schema
                        ));
                        continue;
                    }
                }
            }
        }
        if mode == "partial" {
            if annotation.rust != "none" {
                failures.push(format!(
                    "{}:{} partial 注解要求 rust=none，实际为 {}（示例省略字段时不能声明完整 round-trip）",
                    entry.source, entry.line, annotation.rust
                ));
            } else {
                partial += 1;
            }
            continue;
        }
        if entry.status == "schema-only" {
            if annotation.rust != "none" {
                failures.push(format!(
                    "{}:{} schema-only 要求 rust=none，实际为 {}",
                    entry.source, entry.line, annotation.rust
                ));
            } else {
                schema_only += 1;
            }
            continue;
        }
        match crate::rust::check_annotation(&annotation.rust, &data) {
            Ok(()) => verified += 1,
            Err(error) => failures.push(format!(
                "{}:{} Rust {} 校验失败：{error}",
                entry.source, entry.line, annotation.rust
            )),
        }
    }
    let manifest_shas: BTreeSet<String> = manifest
        .blocks
        .iter()
        .map(|entry| entry.sha256.clone())
        .collect();
    for sha in seen {
        if !manifest_shas.contains(&sha) {
            failures.push(format!(
                "annotations.json 中的 sha256 {sha} 不再匹配任何文档块（文档已变，需复核）"
            ));
        }
    }
    for (sha, block) in &allowance {
        if !manifest_shas.contains(sha) {
            failures.push(format!(
                "unverified.json 中的 sha256 {} ({}) 不再匹配任何文档块（文档已变，需复核）",
                sha, block.source
            ));
        }
    }
    Ok(FixtureCheck {
        failures,
        warnings,
        verified,
        schema_only,
        partial,
        allowlisted,
        skipped,
        unverified,
    })
}

/// config.md §12 的聚合文档：顶层只允许该 scope 的 resource 键，每个 resource
/// 直接用 `gantry_core::settings::resource_schema`（生产 `*Patch` 类型派生）校验。
///
/// 这不是新的线上契约：请求仍是逐 resource 的 `PUT`；这里只验证原文聚合示例，
/// 不为它生成独立 wrapper schema。
fn check_settings_aggregate(scope: &str, data: &Value) -> Result<(), String> {
    use gantry_core::settings::{resource_schema, SettingsResource};

    let resources: &[(&str, SettingsResource)] = match scope {
        "global" => &[
            ("models", SettingsResource::Models),
            ("levels", SettingsResource::Levels),
            ("policies", SettingsResource::Policies),
            ("guardian", SettingsResource::Guardian),
            ("sandbox", SettingsResource::Sandbox),
            ("egress", SettingsResource::Egress),
            ("notify", SettingsResource::Notify),
            ("github", SettingsResource::Github),
        ],
        "project" => &[
            ("models", SettingsResource::Models),
            ("levels", SettingsResource::Levels),
            ("policies", SettingsResource::Policies),
            ("guardian", SettingsResource::Guardian),
            ("sandbox", SettingsResource::Sandbox),
            ("egress", SettingsResource::Egress),
        ],
        other => return Err(format!("未知 settings 聚合范围：{other}")),
    };
    let object = data
        .as_object()
        .ok_or_else(|| "聚合文档必须是 JSON 对象".to_owned())?;
    let expected: BTreeSet<&str> = resources.iter().map(|(name, _)| *name).collect();
    if let Some(unknown) = object.keys().find(|key| !expected.contains(key.as_str())) {
        return Err(format!("未知 resource `{unknown}`"));
    }
    for (name, resource) in resources {
        let value = object
            .get(*name)
            .ok_or_else(|| format!("缺少 resource `{name}`"))?;
        let schema = resource_schema(*resource).to_value();
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| format!("{name}: schema 无法编译：{error}"))?;
        validator
            .validate(value)
            .map_err(|error| format!("{name}: {error}"))?;
    }
    Ok(())
}

fn load_annotation_schema(
    root: &Path,
    annotation: &Annotation,
    partial: bool,
) -> Result<Value, String> {
    let mut schema = match annotation.schema.strip_prefix("openapi:") {
        Some(name) => {
            let text = fs::read_to_string(root.join("schemas/openapi.yaml"))
                .map_err(|error| format!("schemas/openapi.yaml: {error}"))?;
            let document: Value = serde_json::from_str(&text)
                .map_err(|error| format!("schemas/openapi.yaml: {error}"))?;
            let components = document
                .get("components")
                .cloned()
                .ok_or_else(|| "schemas/openapi.yaml 缺少 components".to_owned())?;
            if components.pointer(&format!("/schemas/{name}")).is_none() {
                return Err(format!("OpenAPI 组件不存在：{name}"));
            }
            serde_json::json!({
                "$schema": crate::schema::DIALECT,
                "allOf": [{"$ref": format!("#/components/schemas/{name}")}],
                "components": components,
            })
        }
        None => {
            let text = fs::read_to_string(root.join(&annotation.schema))
                .map_err(|error| format!("{}: {error}", annotation.schema))?;
            serde_json::from_str(&text)
                .map_err(|error| format!("{}: {error}", annotation.schema))?
        }
    };
    if partial {
        relax_required(&mut schema);
    }
    Ok(schema)
}

/// 仅用于 `partial` 校验：删除所有 `required`，保留类型、枚举、格式与
/// `additionalProperties: false`。这样省略字段的示例可以验证，但未知字段、
/// 错误类型和错误枚举仍必须失败。
fn relax_required(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if matches!(map.get("required"), Some(Value::Array(_))) {
                map.remove("required");
            }
            for (key, child) in map.iter_mut() {
                if key == "properties" {
                    // `properties` 的键是属性名；属性名叫 `required` 时不能删除。
                    if let Value::Object(properties) = child {
                        for property in properties.values_mut() {
                            relax_required(property);
                        }
                    }
                } else {
                    relax_required(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                relax_required(item);
            }
        }
        _ => {}
    }
}

fn unverified_blocks(root: &Path) -> Result<BTreeMap<String, UnverifiedBlock>, String> {
    let path = root.join("fixtures/contracts/unverified.json");
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "{}: {error}（缺少显式未验证清单时无法区分示意块与实现缺漏）",
            path.display()
        )
    })?;
    let file: UnverifiedFile =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    if file.version != 1 {
        return Err(format!("{}: 未知 version {}", path.display(), file.version));
    }
    let mut blocks = BTreeMap::new();
    for block in file.blocks {
        if blocks.insert(block.sha256.clone(), block).is_some() {
            return Err(format!("{}: 重复的 sha256 条目", path.display()));
        }
    }
    Ok(blocks)
}

/// 校验负例：声明的检查必须失败，否则 fail。
pub fn check_negative(root: &Path) -> Result<(usize, Vec<String>), String> {
    let cases = negative_cases(root)?;
    let mut failures = Vec::new();
    let mut count = 0;
    for case in &cases {
        count += 1;
        let schema_path = root.join(&case.schema);
        let schema_text = fs::read_to_string(&schema_path)
            .map_err(|error| format!("{}: {error}", schema_path.display()))?;
        let schema: Value = serde_json::from_str(&schema_text)
            .map_err(|error| format!("{}: {error}", schema_path.display()))?;
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| format!("{}: {error}", case.schema))?;
        let schema_ok = validator.validate(&case.data).is_ok();
        let rust_result = match &case.event_type {
            Some(ty) => crate::rust::check_event_data(ty, &case.data),
            None => crate::rust::check_annotation(&case.rust, &case.data),
        };
        let rust_ok = rust_result.is_ok();
        if case.should_fail.iter().any(|what| what == "schema") && schema_ok {
            failures.push(format!("负例 {} 应被 schema 拒绝但通过了", case.id));
        }
        if case.should_fail.iter().any(|what| what == "rust") && rust_ok {
            failures.push(format!(
                "负例 {} 应被 Rust 校验拒绝但通过了（{}）",
                case.id, case.rust
            ));
        }
        if case.should_fail.iter().all(|what| what != "schema") && !schema_ok {
            failures.push(format!(
                "负例 {} 未声明 schema 失败，但 schema 拒绝了；请更新 should_fail 或夹具",
                case.id
            ));
        }
        if case.should_fail.iter().all(|what| what != "rust") && !rust_ok {
            failures.push(format!(
                "负例 {} 未声明 rust 失败，但 Rust 拒绝了；请更新 should_fail 或夹具",
                case.id
            ));
        }
    }
    Ok((count, failures))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// `oneOf` 不能退化成“全部拒绝”：有效动作必须通过，kind/payload 错配必须失败。
    #[test]
    fn outbox_action_schema_accepts_valid_action_and_rejects_mismatch() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = fs::read_to_string(root.join("schemas/forge/outbox-action.json"))
            .expect("schemas/forge/outbox-action.json must be generated");
        let schema: Value = serde_json::from_str(&text).expect("schema must be JSON");
        let validator = jsonschema::validator_for(&schema).expect("schema must compile");
        let actions: BTreeMap<String, Value> = serde_json::from_str(include_str!(
            "../../gantry-core/fixtures/protocol/forge/actions.json"
        ))
        .expect("actions fixture must decode");

        let action = serde_json::json!({
            "id": "act_01JBA1F3Q9TX2N7B5D8H0M4RVC",
            "project_id": "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK",
            "delivery_id": "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM",
            "idempotency_key": "gk1_anbfwcip7r36is6wsqcuxxfd34",
            "kind": "create_pr",
            "payload": actions["create_pr"].clone(),
            "requires_approval": null,
            "depends_on": [],
            "priority": "normal",
            "not_before": null,
            "epoch": 0,
            "status": "queued",
            "attempts": 0,
            "lease_until": null,
            "last_error": null,
            "expected_effects": [],
            "github_ref": null,
            "created_at": "2026-10-24T12:41:03.120Z",
            "updated_at": "2026-10-24T12:41:03.120Z"
        });
        assert!(
            validator.validate(&action).is_ok(),
            "a valid OutboxAction must pass the generated schema"
        );

        let mut mismatch = action;
        mismatch["kind"] = serde_json::json!("push_branch");
        assert!(
            validator.validate(&mismatch).is_err(),
            "kind/payload mismatch must be rejected by the schema"
        );
    }

    /// `settings.updated` 的 16 个 scope × resource 分支都必须可解析，且空文档可以验证。
    #[test]
    fn settings_updated_schema_covers_every_resource_branch() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = fs::read_to_string(root.join("schemas/events/settings.updated.json"))
            .expect("schemas/events/settings.updated.json must be generated");
        let schema: Value = serde_json::from_str(&text).expect("schema must be JSON");
        let validator = jsonschema::validator_for(&schema).expect("schema must compile");
        for resource in [
            "models", "levels", "policies", "guardian", "sandbox", "egress", "notify", "github",
        ] {
            let valid = serde_json::json!({
                "scope": "global",
                "resource": resource,
                "schema_version": 1,
                "base_rev": 1,
                "value": {},
                "relaxes": [],
                "secret_changes": []
            });
            assert!(
                validator.validate(&valid).is_ok(),
                "empty {resource} document must pass"
            );

            let mut deleted = valid.clone();
            deleted["scope"] = serde_json::json!("project");
            deleted["value"] = Value::Null;
            assert!(
                validator.validate(&deleted).is_ok(),
                "project DELETE must allow null for {resource}"
            );

            deleted["scope"] = serde_json::json!("global");
            assert!(
                validator.validate(&deleted).is_err(),
                "global settings.updated must reject null for {resource}"
            );

            let mut missing = valid;
            missing.as_object_mut().unwrap().remove("value");
            assert!(
                validator.validate(&missing).is_err(),
                "settings.updated must require value for {resource}"
            );
        }
    }
}
