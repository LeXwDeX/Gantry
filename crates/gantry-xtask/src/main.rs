//! `cargo xtask`：生成 `schemas/` 与契约夹具，并检查漂移、元校验、夹具与 OpenAPI 一致性。
//!
//! 只写本仓的生成目录；`schemas/openapi.yaml` 归 `api` 契约，永远只读。

mod catalog;
mod corecheck;
mod docs;
mod fixtures;
mod openapi;
mod rust;
mod schema;
mod util;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use crate::util::Report;

const GENERATED_SCHEMA_DIRS: &[&str] = &[
    "schemas/config",
    "schemas/egress",
    "schemas/entities",
    "schemas/events",
    "schemas/forge",
    "schemas/worker",
];

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let mut args = std::env::args().skip(1);
    let mut command = "schemas".to_owned();
    let mut check = false;
    let mut root = default_root();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "schemas" => command = "schemas".to_owned(),
            "check" => {
                command = "schemas".to_owned();
                check = true;
            }
            "--check" => check = true,
            "--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--root 需要一个路径".to_owned())?;
                root = PathBuf::from(value);
            }
            "-h" | "--help" => {
                println!(
                    "用法：cargo xtask schemas [--check] [--root <dir>]\n\
                     \n\
                     生成 schemas/{{entities,events,forge,worker,config,egress}} 与 fixtures/contracts。\n\
                     --check 只读比较：生成物漂移、元校验、夹具、OpenAPI 与 core 约束任一失败则退出 1。"
                );
                return Ok(ExitCode::SUCCESS);
            }
            other => return Err(format!("未知参数 `{other}`")),
        }
    }
    if command != "schemas" {
        return Err(format!("未知命令 `{command}`"));
    }
    if !root.join("crates/gantry-core").is_dir() {
        return Err(format!(
            "{} 不是仓库根：缺少 crates/gantry-core",
            root.display()
        ));
    }
    if check {
        check_all(&root)
    } else {
        generate(&root)
    }
}

fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gantry-xtask has a repo root")
        .to_path_buf()
}

fn generate(root: &Path) -> Result<ExitCode, String> {
    let artifacts = catalog::artifacts();
    let mut written = 0;
    for artifact in &artifacts {
        let path = root.join(&artifact.path);
        write_file(&path, &schema::render(&artifact.value))?;
        written += 1;
    }
    remove_stale(root, &artifacts)?;
    write_fixtures(root)?;
    println!(
        "generated {written} JSON Schemas into schemas/ and fixtures under fixtures/contracts/",
    );
    Ok(ExitCode::SUCCESS)
}

fn write_fixtures(root: &Path) -> Result<(), String> {
    let extracted = fixtures::extract(root)?;
    for (relative, bytes) in &extracted.files {
        write_file(&root.join(relative), bytes)?;
    }
    let stale = stale_block_files(root, &extracted.files)?;
    for path in stale {
        fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let manifest = serde_json::to_vec_pretty(&extracted.manifest)
        .map_err(|error| format!("manifest: {error}"))?;
    write_file(&root.join("fixtures/contracts/manifest.json"), &manifest)?;
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn remove_stale(root: &Path, artifacts: &[catalog::Artifact]) -> Result<(), String> {
    let expected: BTreeSet<PathBuf> = artifacts
        .iter()
        .map(|artifact| root.join(&artifact.path))
        .collect();
    for directory in GENERATED_SCHEMA_DIRS {
        let directory = root.join(directory);
        if !directory.is_dir() {
            continue;
        }
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("json")
                && !expected.contains(&path)
            {
                fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn stale_block_files(
    root: &Path,
    generated: &[(PathBuf, Vec<u8>)],
) -> Result<Vec<PathBuf>, String> {
    let expected: BTreeSet<PathBuf> = generated.iter().map(|(path, _)| root.join(path)).collect();
    let blocks = root.join("fixtures/contracts/blocks");
    let mut stale = Vec::new();
    if blocks.is_dir() {
        collect_json_files(&blocks, &mut stale)?;
    }
    Ok(stale
        .into_iter()
        .filter(|path| !expected.contains(path))
        .collect())
}

fn collect_json_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, files)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            files.push(path);
        }
    }
    Ok(())
}

fn check_all(root: &Path) -> Result<ExitCode, String> {
    let mut report = Report::default();
    let artifacts = catalog::artifacts();
    check_drift(root, &artifacts, &mut report)?;
    check_meta(root, &mut report);
    check_event_contracts(&artifacts, &mut report);
    check_fixtures(root, &mut report)?;
    openapi::check(root, &artifacts, &mut report);
    corecheck::check(root, &mut report);
    print_gaps(root, &mut report);
    report.print("cargo xtask schemas --check");
    if report.failures.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

fn check_drift(
    root: &Path,
    artifacts: &[catalog::Artifact],
    report: &mut Report,
) -> Result<(), String> {
    let mut checked = 0;
    let mut expected = BTreeSet::new();
    for artifact in artifacts {
        checked += 1;
        let path = root.join(&artifact.path);
        expected.insert(path.clone());
        let rendered = schema::render(&artifact.value);
        match fs::read(&path) {
            Ok(existing) if existing == rendered => {}
            Ok(existing) => report.fail(format!(
                "{} 漂移（磁盘 {} 字节，生成 {} 字节，首个差异 {}）",
                artifact.path,
                existing.len(),
                rendered.len(),
                first_difference(&existing, &rendered)
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.fail(format!(
                "{} 缺失（运行 cargo xtask schemas 生成）",
                artifact.path
            )),
            Err(error) => report.fail(format!("{}: {error}", artifact.path)),
        }
    }
    for directory in GENERATED_SCHEMA_DIRS {
        let directory = root.join(directory);
        let mut files = Vec::new();
        if directory.is_dir() {
            collect_json_files(&directory, &mut files)?;
        }
        for file in files {
            if !expected.contains(&file) {
                report.fail(format!(
                    "{} 是生成目录中的多余文件（不是 catalog 产物）",
                    file.strip_prefix(root).unwrap_or(&file).display()
                ));
            }
        }
    }
    let extracted = fixtures::extract(root)?;
    let expected_blocks: BTreeSet<PathBuf> = extracted
        .files
        .iter()
        .map(|(path, _)| root.join(path))
        .collect();
    for (relative, bytes) in &extracted.files {
        let path = root.join(relative);
        match fs::read(&path) {
            Ok(existing) if &existing == bytes => {}
            Ok(existing) => report.fail(format!(
                "{} 漂移（首个差异 {}）",
                relative.display(),
                first_difference(&existing, bytes)
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report.fail(format!("{} 缺失", relative.display()))
            }
            Err(error) => report.fail(format!("{}: {error}", relative.display())),
        }
    }
    let blocks = root.join("fixtures/contracts/blocks");
    let mut block_files = Vec::new();
    if blocks.is_dir() {
        collect_json_files(&blocks, &mut block_files)?;
    }
    for file in block_files {
        if !expected_blocks.contains(&file) {
            report.fail(format!(
                "{} 是多余夹具（文档行号或内容已变）",
                file.strip_prefix(root).unwrap_or(&file).display()
            ));
        }
    }
    let manifest_path = root.join("fixtures/contracts/manifest.json");
    let manifest = serde_json::to_vec_pretty(&extracted.manifest)
        .map_err(|error| format!("manifest: {error}"))?;
    match fs::read(&manifest_path) {
        Ok(existing) if existing == manifest => {}
        Ok(existing) => report.fail(format!(
            "fixtures/contracts/manifest.json 漂移（首个差异 {}）",
            first_difference(&existing, &manifest)
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.fail("fixtures/contracts/manifest.json 缺失")
        }
        Err(error) => report.fail(format!("{}: {error}", manifest_path.display())),
    }
    report.note(format!("schema 漂移检查：{checked} 个生成文件"));
    Ok(())
}

fn check_meta(root: &Path, report: &mut Report) {
    let mut checked = 0;
    for directory in GENERATED_SCHEMA_DIRS {
        let directory = root.join(directory);
        let mut files = Vec::new();
        if let Err(error) = collect_json_files(&directory, &mut files) {
            report.fail(error);
            continue;
        }
        files.sort();
        for file in files {
            let relative = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .display()
                .to_string();
            let text = match fs::read_to_string(&file) {
                Ok(text) => text,
                Err(error) => {
                    report.fail(format!("{relative}: {error}"));
                    continue;
                }
            };
            let schema: Value = match serde_json::from_str(&text) {
                Ok(schema) => schema,
                Err(error) => {
                    report.fail(format!("{relative}: {error}"));
                    continue;
                }
            };
            checked += 1;
            if let Err(error) = jsonschema::meta::validate(&schema) {
                report.fail(format!("{relative}: 未通过 2020-12 元校验：{error}"));
            }
            if schema.get("$schema").and_then(Value::as_str) != Some(schema::DIALECT) {
                report.fail(format!("{relative}: 缺少 2020-12 `$schema`"));
            }
        }
    }
    report.note(format!("元校验：{checked} 个 schema"));
}

/// 事件契约的自洽：未定义事件必须是拒绝型，定义事件不得是拒绝型。
fn check_event_contracts(artifacts: &[catalog::Artifact], report: &mut Report) {
    use gantry_core::events::contract_undefined_types;
    let undefined: BTreeSet<String> = contract_undefined_types()
        .iter()
        .map(|ty| ty.as_str().to_owned())
        .collect();
    let mut checked = 0;
    for artifact in artifacts {
        let Some(ty) = artifact
            .value
            .get("x-gantry-event-type")
            .and_then(Value::as_str)
        else {
            continue;
        };
        checked += 1;
        let rejection = artifact
            .value
            .get("not")
            .and_then(Value::as_object)
            .map(|not| not.is_empty())
            .unwrap_or(false);
        let unresolved = artifact.value.get("x-gantry-unresolved").is_some();
        if undefined.contains(ty) {
            if !rejection || !unresolved {
                report.fail(format!(
                    "{ty}: 契约未定义 data，但 schema 不是拒绝型或缺少 x-gantry-unresolved"
                ));
                continue;
            }
            match jsonschema::validator_for(&artifact.value) {
                Ok(validator) => {
                    if validator.validate(&serde_json::json!({})).is_ok() {
                        report.fail(format!("{ty}: 拒绝型 schema 竟然接受了空对象"));
                    }
                }
                Err(error) => report.fail(format!("{ty}: 拒绝型 schema 无法编译：{error}")),
            }
        } else if rejection {
            report.fail(format!(
                "{ty}: 契约已定义 data，但 schema 仍是拒绝型（生成映射过期）"
            ));
        }
    }
    report.note(format!("事件契约自洽：{checked} 个事件 schema"));
}

fn check_fixtures(root: &Path, report: &mut Report) -> Result<(), String> {
    let extracted = fixtures::extract(root)?;
    let check = fixtures::check(root, &extracted.manifest)?;
    report.failures.extend(check.failures);
    report.warnings.extend(check.warnings);
    report.note(format!(
        "夹具：{} 个完整验证（schema + Rust），{} 个部分验证（仅 schema；示例省略字段），{} 个块在 unverified.json 显式登记（其中 {} 个 skip）",
        check.verified, check.partial, check.allowlisted, check.skipped
    ));
    for entry in check.unverified.iter().take(40) {
        report.note(format!("未验证块 {entry}"));
    }
    if check.unverified.len() > 40 {
        report.note(format!(
            "其余 {} 个未验证块见 fixtures/contracts/manifest.json",
            check.unverified.len() - 40
        ));
    }
    let (count, failures) = fixtures::check_negative(root)?;
    report.failures.extend(failures);
    report.note(format!("负例夹具：{count} 个"));
    Ok(())
}

fn print_gaps(root: &Path, report: &mut Report) {
    let path = root.join("fixtures/contracts/contract-gaps.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            report.warn(format!("{}: {error}", path.display()));
            return;
        }
    };
    let gaps: Value = match serde_json::from_str(&text) {
        Ok(gaps) => gaps,
        Err(error) => {
            report.warn(format!("{}: {error}", path.display()));
            return;
        }
    };
    let Some(items) = gaps.get("gaps").and_then(Value::as_array) else {
        report.warn("contract-gaps.json 缺少 gaps[]");
        return;
    };
    for item in items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("?");
        let summary = item.get("summary").and_then(Value::as_str).unwrap_or("");
        report.note(format!("契约缺口 {id}: {summary}"));
    }
}

fn first_difference(left: &[u8], right: &[u8]) -> String {
    let limit = left.len().min(right.len());
    for index in 0..limit {
        if left[index] != right[index] {
            return format!(
                "byte {index}: disk={} generated={}",
                left[index], right[index]
            );
        }
    }
    format!("length only: disk={} generated={}", left.len(), right.len())
}
