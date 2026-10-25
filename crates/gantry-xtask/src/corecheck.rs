//! `core` 的静态约束：无 IO、禁用依赖、`unsafe` 禁止。
//!
//! 不调用 cargo（避免在检查里触发构建或写锁文件）；只读清单与源码。

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::util::Report;

const ALLOWED_DEPENDENCIES: &[&str] = &[
    "serde",
    "serde_json",
    "schemars",
    "thiserror",
    "sha2",
    "serde_jcs",
    "async-trait",
    "futures-core",
];

const FORBIDDEN_DEPENDENCIES: &[&str] = &[
    "tokio",
    "axum",
    "hyper",
    "reqwest",
    "sqlx",
    "rusqlite",
    "diesel",
    "actix-web",
    "warp",
    "tungstenite",
    "redis",
    "aws-sdk-s3",
    "rcgen",
    "p256",
];

const FORBIDDEN_CODE: &[&str] = &[
    "std::fs::",
    "std::net::Tcp",
    "std::process::",
    "std::env::",
    "std::thread::",
    "tokio::",
    "reqwest::",
    "File::open",
    "File::create",
    "Command::new",
    "println!",
    "eprintln!",
];

pub fn check(root: &Path, report: &mut Report) {
    check_manifest(root, report);
    check_sources(root, report);
}

fn check_manifest(root: &Path, report: &mut Report) {
    let path = root.join("crates/gantry-core/Cargo.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            report.fail(format!("{}: {error}", path.display()));
            return;
        }
    };
    let mut section = String::new();
    let mut dependencies = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.trim_matches(&['[', ']'][..]).to_owned();
            continue;
        }
        if !(section == "dependencies" || section == "build-dependencies") {
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = trimmed.split_once('=') {
            let name = name.trim().split('.').next().unwrap_or(name).to_owned();
            dependencies.insert(name);
        }
    }
    for dependency in &dependencies {
        if FORBIDDEN_DEPENDENCIES.contains(&dependency.as_str()) {
            report.fail(format!("core 依赖了禁止的运行时/IO crate `{dependency}`"));
        } else if !ALLOWED_DEPENDENCIES.contains(&dependency.as_str()) {
            report.fail(format!(
                "core 依赖 `{dependency}` 不在 core.md §3 的允许清单内"
            ));
        }
    }
    report.note(format!(
        "core 依赖清单：{}",
        dependencies.into_iter().collect::<Vec<_>>().join(", ")
    ));
}

fn check_sources(root: &Path, report: &mut Report) {
    let src = root.join("crates/gantry-core/src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    files.sort();
    let lib = match fs::read_to_string(src.join("lib.rs")) {
        Ok(text) => text,
        Err(error) => {
            report.fail(format!("core/src/lib.rs: {error}"));
            return;
        }
    };
    if !lib.contains("#![forbid(unsafe_code)]") {
        report.fail("core/src/lib.rs 缺少 `#![forbid(unsafe_code)]`");
    }
    for file in &files {
        let relative = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) => {
                report.fail(format!("{relative}: {error}"));
                continue;
            }
        };
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains("unsafe ") || trimmed.contains("unsafe{") {
                report.fail(format!(
                    "{relative}:{}: core 不得出现 unsafe 代码",
                    index + 1
                ));
            }
            for needle in FORBIDDEN_CODE {
                if line.contains(needle) {
                    report.fail(format!(
                        "{relative}:{}: core 不得使用 `{needle}`",
                        index + 1
                    ));
                }
            }
        }
    }
    report.note(format!("core 源码无 IO：扫描 {} 个文件", files.len()));
}

fn collect_rust_files(directory: &Path, files: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}
