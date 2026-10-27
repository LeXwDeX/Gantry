//! worker.md §12.J（镜像、selftest、二进制依赖规则）与 §12.G 的版本钉死。

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::TempDir;
use gantry_worker::claude_cli::PINNED_CLAUDE_VERSION;
use gantry_worker::selftest::{run_checks, scan_setid, Check, Probe};
use serde_json::Value;

const SUCCESS: &str = include_str!("../fixtures/claude-stream-json/success.jsonl");
const MAX_TURNS: &str = include_str!("../fixtures/claude-stream-json/max_turns.jsonl");
const CONTAINERFILE: &str = include_str!("../image/Containerfile");
const GITCONFIG: &str = include_str!("../image/gitconfig");

/// 一切合格的镜像；各用例只改一处。
struct FakeProbe {
    uid: u32,
    on_path: Vec<&'static str>,
    outputs: BTreeMap<String, Option<String>>,
    setid: Vec<PathBuf>,
}

impl FakeProbe {
    fn good() -> Self {
        Self {
            uid: 1000,
            on_path: vec!["git", "rg", "jq", "claude"],
            outputs: BTreeMap::from([
                (
                    "git config --system --get-all credential.helper".into(),
                    Some("\n".into()),
                ),
                (
                    "git config --system --get-all safe.directory".into(),
                    Some("*\n".into()),
                ),
                (
                    "claude --version".into(),
                    Some(format!("{PINNED_CLAUDE_VERSION} (Claude Code)\n")),
                ),
            ]),
            setid: Vec::new(),
        }
    }
}

impl Probe for FakeProbe {
    fn uid(&self) -> u32 {
        self.uid
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        self.on_path
            .contains(&program)
            .then(|| PathBuf::from("/usr/bin").join(program))
    }

    fn output(&self, program: &str, args: &[&str]) -> Option<String> {
        let key = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.outputs.get(&key).cloned().flatten()
    }

    fn setid_files(&self) -> Vec<PathBuf> {
        self.setid.clone()
    }
}

type Mutation = Box<dyn Fn(&mut FakeProbe)>;

fn failing(checks: &[Check]) -> Vec<&'static str> {
    checks.iter().filter(|c| !c.ok).map(|c| c.name).collect()
}

#[test]
fn selftest_passes_on_a_good_image() {
    let checks = run_checks(&FakeProbe::good());
    assert!(failing(&checks).is_empty(), "{checks:#?}");
    for name in [
        "non_root",
        "no_setid_files",
        "no_gh",
        "git_credential_helper_empty",
        "git_safe_directory",
        "tool_on_path",
        "claude_version_pinned",
    ] {
        assert!(checks.iter().any(|c| c.name == name), "{name}");
    }
}

#[test]
fn selftest_flags_each_violation() {
    let cases: Vec<(Mutation, &str)> = vec![
        (Box::new(|p| p.uid = 0), "non_root"),
        (
            Box::new(|p| p.setid.push("/usr/bin/su".into())),
            "no_setid_files",
        ),
        (Box::new(|p| p.on_path.push("gh")), "no_gh"),
        (
            Box::new(|p| {
                p.outputs.insert(
                    "git config --system --get-all credential.helper".into(),
                    Some("store\n".into()),
                );
            }),
            "git_credential_helper_empty",
        ),
        (
            Box::new(|p| {
                p.outputs
                    .insert("git config --system --get-all safe.directory".into(), None);
            }),
            "git_safe_directory",
        ),
        (
            Box::new(|p| p.on_path.retain(|t| *t != "jq")),
            "tool_on_path",
        ),
        (
            Box::new(|p| {
                p.outputs.insert(
                    "claude --version".into(),
                    Some("2.0.0 (Claude Code)\n".into()),
                );
            }),
            "claude_version_pinned",
        ),
    ];
    for (mutate, expected) in cases {
        let mut probe = FakeProbe::good();
        mutate(&mut probe);
        assert_eq!(failing(&run_checks(&probe)), [expected]);
    }
}

#[cfg(unix)]
#[test]
fn setid_scan_finds_setuid_and_setgid_files_without_following_links() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new("setid");
    let bin = root.path().join("usr/bin");
    fs::create_dir_all(&bin).unwrap();
    for (name, mode) in [("plain", 0o755), ("suid", 0o4755), ("sgid", 0o2755)] {
        let path = bin.join(name);
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    std::os::unix::fs::symlink(bin.join("suid"), root.path().join("link")).unwrap();
    let mut found = Vec::new();
    scan_setid(root.path(), &mut found);
    found.sort();
    assert_eq!(found, [bin.join("sgid"), bin.join("suid")]);
}

/// 钉死的 CLI 版本 = 录制夹具的版本 = 镜像构建参数：升级时三者必须一起改。
#[test]
fn pinned_cli_version_matches_recordings_and_image() {
    for recording in [SUCCESS, MAX_TURNS] {
        let init: Value = serde_json::from_str(recording.lines().next().unwrap()).unwrap();
        assert_eq!(init["subtype"], "init");
        assert_eq!(init["claude_code_version"], PINNED_CLAUDE_VERSION);
    }
    assert!(CONTAINERFILE.contains(&format!(
        "ARG CLAUDE_CODE_VERSION={PINNED_CLAUDE_VERSION}\n"
    )));
}

#[test]
fn image_definition_meets_the_image_contract() {
    // 非 root、无 setuid、selftest 是构建的最后一步、入口是 gantry-worker run。
    assert!(CONTAINERFILE.contains("USER 1000:1000"));
    assert!(CONTAINERFILE.contains("-perm /6000 -exec chmod a-s"));
    let run_steps: Vec<&str> = CONTAINERFILE
        .lines()
        .filter(|l| l.starts_with("RUN "))
        .collect();
    assert_eq!(run_steps.last(), Some(&"RUN gantry-worker selftest"));
    assert!(CONTAINERFILE
        .trim_end()
        .ends_with(r#"ENTRYPOINT ["gantry-worker", "run"]"#));
    let instructions: Vec<&str> = CONTAINERFILE
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    assert!(
        instructions.iter().all(|l| !l
            .split_whitespace()
            .any(|w| w == "gh" || w == "openssh-client")),
        "no gh or ssh client installed in the image"
    );
    // 系统级 git：凭据助手置空、safe.directory=*。
    let config: Vec<&str> = GITCONFIG
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .collect();
    assert_eq!(
        config,
        ["[credential]", "helper =", "[safe]", "directory = *"]
    );
}

/// 二进制只依赖 core 与少量通用库：不得引入服务端 crate（worker.md §3、§12.J）。
#[test]
fn binary_dependency_tree_has_no_server_crates() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args([
            "tree", "--locked", "-e", "normal", "--prefix", "none", "--format", "{p}",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .args(["-p", "gantry-worker"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).unwrap();
    let crates: Vec<&str> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert!(crates.contains(&"gantry-core"), "{tree}");
    for forbidden in [
        "sqlx",
        "axum",
        "rusqlite",
        "hyper",
        "reqwest",
        "gantry-store",
        "gantry-secrets",
        "gantry-sandbox",
        "gantry-engine",
        "gantry-api",
        "gantry-egress",
    ] {
        assert!(
            !crates.contains(&forbidden),
            "{forbidden} in the worker binary tree"
        );
    }
    let internal: Vec<&str> = crates
        .iter()
        .copied()
        .filter(|c| c.starts_with("gantry-"))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(internal, ["gantry-core", "gantry-worker"]);
}
