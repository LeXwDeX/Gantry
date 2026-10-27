//! 公开 API 清单（secrets.md §12.1"沙盒内读不到任何模型 key"）：库里没有任何把值导出到
//! 环境变量、挂载或文件的公开函数。扫描 `src/` 中全部 `pub fn`，与白名单逐一比对；
//! 新增公开函数必须先更新本清单并通过评审。

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::Path;

const ALLOWED: &[&str] = &[
    // key.rs
    "from_bytes",
    "generate",
    "load",
    "new",
    // lib.rs
    "init",
    "open",
    "reader",
    "scoped",
    "admin",
    "dir",
    "harden_process",
    "staged_all",
];

const FORBIDDEN_FRAGMENTS: &[&str] = &["env", "mount", "export_to", "to_file", "write_to", "dump"];

fn collect(dir: &Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        // 只看非测试代码。
        let text = text.split("#[cfg(test)]").next().unwrap();
        let mut in_pub_trait = false;
        for raw in text.lines() {
            let line = raw.trim_start();
            if raw.starts_with("pub trait ") {
                in_pub_trait = true;
            } else if raw.starts_with('}') {
                in_pub_trait = false;
            }
            // 公开函数，以及公开 trait 里的方法（对实现者同样公开）。
            let rest = line
                .strip_prefix("pub fn ")
                .or_else(|| in_pub_trait.then(|| line.strip_prefix("fn ")).flatten());
            if let Some(rest) = rest {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                out.insert(name);
            }
        }
    }
}

#[test]
fn public_functions_match_allowlist() {
    let mut found = BTreeSet::new();
    collect(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    let allowed: BTreeSet<String> = ALLOWED.iter().map(|s| s.to_string()).collect();
    assert_eq!(found, allowed, "公开函数清单变化，需评审后更新白名单");
    for name in &found {
        for bad in FORBIDDEN_FRAGMENTS {
            assert!(!name.contains(bad), "疑似导出路径：{name}");
        }
    }
}
