//! models.md §12.2「无 key 依赖」：`gantry-models` 及其工作区内依赖都不依赖
//! `gantry-secrets`（P-16：模型 key 只存在于 Egress）。

use std::path::{Path, PathBuf};

fn manifest(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("Cargo.toml")).expect("read Cargo.toml")
}

/// 从 `[dependencies]` 段取出 `path = "../<crate>"` 形式的工作区依赖。
fn path_deps(text: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line == "[dependencies]" || line.ends_with(".dependencies]");
            continue;
        }
        if !in_deps {
            continue;
        }
        if let Some(rest) = line.split("path = \"../").nth(1) {
            if let Some(name) = rest.split('"').next() {
                deps.push(name.to_owned());
            }
        }
    }
    deps
}

#[test]
fn models_does_not_depend_on_secrets_transitively() {
    let crates: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf();
    let mut stack = vec!["gantry-models".to_owned()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let text = manifest(&crates.join(&name));
        assert!(
            !text.contains("gantry-secrets"),
            "{name} must not depend on gantry-secrets"
        );
        stack.extend(path_deps(&text));
    }
    assert!(seen.contains("gantry-core"));
    assert!(!seen.contains("gantry-secrets"));
}
