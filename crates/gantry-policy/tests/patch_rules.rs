//! policy.md §12.2「补丁策略」的 M1 条目（纯判定，不经 Store）。

use std::collections::BTreeMap;

use gantry_core::canon::sha256;
use gantry_core::entities::{InboxKind, Seq};
use gantry_core::ids::Id;
use gantry_core::ports::{AddedLineSource, PatchPolicyVerdict, RuleId};
use gantry_core::settings::{builtin_resolved, Policies, ResolvedSettings, SettingsResource};
use gantry_core::worker::{
    FileMode, FileStatus, PatchFacts, PatchFile, PatchVerdict, PathConstraints, RepoPath,
};
use gantry_policy::patch::evaluate;
use gantry_policy::{PolicyBuild, PolicyRules};

const MS: u64 = 1_700_000_000_000;

/// 内存版新增行来源；`Err` 条目模拟读取失败。
#[derive(Default)]
struct VecAddedLines(BTreeMap<String, Vec<Result<String, ()>>>);

impl VecAddedLines {
    fn with(mut self, path: &str, lines: &[&str]) -> Self {
        self.0.insert(
            path.to_owned(),
            lines.iter().map(|line| Ok((*line).to_owned())).collect(),
        );
        self
    }
}

impl AddedLineSource for VecAddedLines {
    fn added_lines<'a>(
        &'a self,
        path: &'a RepoPath,
    ) -> Box<dyn Iterator<Item = std::io::Result<String>> + 'a> {
        let lines = self.0.get(path.as_str()).cloned().unwrap_or_default();
        Box::new(
            lines
                .into_iter()
                .map(|line| line.map_err(|()| std::io::Error::other("read failed"))),
        )
    }
}

fn file(path: &str) -> PatchFile {
    PatchFile {
        path: RepoPath::parse(path).expect("test path"),
        status: FileStatus::Modified,
        old_mode: Some(FileMode::Mode100644),
        new_mode: Some(FileMode::Mode100644),
        added: Some(1),
        deleted: Some(0),
        is_binary: false,
        is_symlink: false,
        is_submodule: false,
        size_after: Some(10),
    }
}

fn facts(files: Vec<PatchFile>) -> PatchFacts {
    PatchFacts {
        project_id: Id::from_parts(MS, 1).expect("id"),
        delivery_id: Id::from_parts(MS, 2).expect("id"),
        attempt_id: Id::from_parts(MS, 3).expect("id"),
        patch_sha256: sha256(b"patch"),
        patch_bytes: 100,
        applies_cleanly: true,
        total_changed_lines: files.len() as u64,
        files,
        job_paths: PathConstraints {
            allow: vec!["**".into()],
            deny: Vec::new(),
            approval_required: Vec::new(),
        },
    }
}

fn m1(facts: &PatchFacts) -> PatchPolicyVerdict {
    evaluate(
        &PolicyRules::builtin(),
        PolicyBuild::M1,
        facts,
        &VecAddedLines::default(),
    )
}

fn codes(verdict: &PatchPolicyVerdict) -> Vec<&str> {
    verdict
        .reasons
        .iter()
        .map(|reason| reason.code.as_str())
        .collect()
}

fn builtin_policies() -> Policies {
    match builtin_resolved(SettingsResource::Policies) {
        ResolvedSettings::Policies(policies) => policies,
        other => panic!("unexpected resource {:?}", other.resource()),
    }
}

#[test]
fn plain_patch_is_allowed() {
    let verdict = m1(&facts(vec![file("src/main.rs"), file("README.md")]));
    assert_eq!(verdict.verdict, PatchVerdict::Allow);
    assert!(verdict.reasons.is_empty());
    assert_eq!(verdict.report, None);
    assert_eq!(verdict.config_rev, Seq::ZERO);
}

#[test]
fn m1_dependency_manifests_and_lockfiles_are_denied_with_escalation() {
    for (path, rule) in [
        ("package.json", "dependency_manifests"),
        ("Cargo.lock", "lockfiles"),
        ("go.sum", "lockfiles"),
        ("CODEOWNERS", "repo_meta"),
    ] {
        let verdict = m1(&facts(vec![file(path)]));
        let code = format!("policy.path_protected:{rule}");
        assert_eq!(verdict.verdict, PatchVerdict::Deny, "{path}");
        assert_eq!(codes(&verdict), vec![code.as_str()], "{path}");
        assert_eq!(verdict.reasons[0].rule_id.as_deref(), Some(rule));
        let report = verdict.report.expect("deny carries a report hint");
        assert_eq!(report.inbox_kind, InboxKind::Escalation, "{path}");
        assert_eq!(report.reason_code.as_str(), code);
        assert!(!report.attach_patch);
        assert!(verdict.matched_approval_rules.is_empty());
    }
}

#[test]
fn workflow_patch_is_denied_with_manual_action_and_patch_attached() {
    for build in [
        PolicyBuild::M1,
        PolicyBuild {
            patch_approvals: true,
            ..PolicyBuild::M1
        },
    ] {
        let verdict = evaluate(
            &PolicyRules::builtin(),
            build,
            &facts(vec![file(".github/workflows/x.yml"), file("Cargo.lock")]),
            &VecAddedLines::default(),
        );
        assert_eq!(verdict.verdict, PatchVerdict::Deny);
        assert!(codes(&verdict).contains(&"policy.path_protected:workflows"));
        let report = verdict.report.expect("report hint");
        assert_eq!(report.inbox_kind, InboxKind::DecisionNeeded);
        assert_eq!(report.payload_kind.as_deref(), Some("manual_action"));
        assert_eq!(
            report.reason_code.as_str(),
            "policy.path_protected:workflows"
        );
        assert!(report.attach_patch);
    }
}

#[test]
fn m2_build_turns_approve_rules_into_needs_approval() {
    let build = PolicyBuild {
        patch_approvals: true,
        ..PolicyBuild::M1
    };
    let verdict = evaluate(
        &PolicyRules::builtin(),
        build,
        &facts(vec![
            file("go.sum"),
            file("Cargo.lock"),
            file("package.json"),
        ]),
        &VecAddedLines::default(),
    );
    assert_eq!(verdict.verdict, PatchVerdict::NeedsApproval);
    assert_eq!(
        verdict.matched_approval_rules,
        vec![
            RuleId::new("dependency_manifests"),
            RuleId::new("lockfiles")
        ]
    );
    assert_eq!(verdict.report, None);
}

#[test]
fn allow_and_deny_scopes() {
    let mut outside = facts(vec![file("docs/a.md")]);
    outside.job_paths.allow = vec!["src/**".into()];
    assert_eq!(codes(&m1(&outside)), vec!["policy.path_outside_allow"]);

    let mut empty_allow = facts(vec![file("src/a.rs")]);
    empty_allow.job_paths.allow.clear();
    assert_eq!(codes(&m1(&empty_allow)), vec!["policy.path_outside_allow"]);

    let mut job_deny = facts(vec![file("src/gen/x.rs")]);
    job_deny.job_paths.deny = vec!["src/gen/**".into()];
    assert_eq!(codes(&m1(&job_deny)), vec!["policy.path_denied"]);

    let mut policies = builtin_policies();
    policies.paths.insert(
        "vendor".into(),
        gantry_core::settings::PathRule {
            globs: vec!["vendor/**".into()],
            disposition: gantry_core::settings::PathDisposition::Deny,
        },
    );
    let rules = PolicyRules::load(&policies, Seq::new(7).expect("seq")).expect("valid");
    let verdict = evaluate(
        &rules,
        PolicyBuild::M1,
        &facts(vec![file("vendor/lib.c")]),
        &VecAddedLines::default(),
    );
    assert_eq!(codes(&verdict), vec!["policy.path_denied"]);
    assert_eq!(verdict.reasons[0].rule_id.as_deref(), Some("vendor"));
    assert_eq!(verdict.config_rev, Seq::new(7).expect("seq"));

    let verdict = m1(&facts(vec![file(".env"), file("deploy/server.key")]));
    assert_eq!(codes(&verdict), vec!["policy.secret", "policy.secret"]);
}

#[test]
fn invalid_job_globs_fail_closed() {
    let mut facts = facts(vec![file("src/a.rs")]);
    facts.job_paths.allow = vec!["!src/**".into()];
    facts.job_paths.deny = vec!["[".into()];
    assert_eq!(
        codes(&m1(&facts)),
        vec!["policy.path_outside_allow", "policy.path_denied"]
    );
}

#[test]
fn current_rules_are_used_for_each_evaluation() {
    let facts = facts(vec![file("src/a.rs")]);
    let r1 = PolicyRules::builtin();
    assert_eq!(m1(&facts).verdict, PatchVerdict::Allow);
    let mut policies = builtin_policies();
    policies.allow_globs = vec!["docs/**".into()];
    let r2 = PolicyRules::load(&policies, Seq::new(9).expect("seq")).expect("valid");
    let verdict = evaluate(&r2, PolicyBuild::M1, &facts, &VecAddedLines::default());
    assert_eq!(verdict.verdict, PatchVerdict::Deny);
    assert_eq!(verdict.config_rev, Seq::new(9).expect("seq"));
    assert_eq!(r1.config_rev, Seq::ZERO);
}

#[test]
fn size_limits_are_exclusive_upper_bounds() {
    let limits = PolicyRules::builtin().patch;

    let mut at_limit = facts(vec![file("a.rs")]);
    at_limit.patch_bytes = limits.max_bytes;
    at_limit.total_changed_lines = limits.max_changed_lines;
    at_limit.files[0].size_after = Some(limits.max_file_bytes);
    assert_eq!(m1(&at_limit).verdict, PatchVerdict::Allow);

    let mut bytes = facts(vec![file("a.rs")]);
    bytes.patch_bytes = limits.max_bytes + 1;
    assert_eq!(codes(&m1(&bytes)), vec!["policy.size"]);

    let mut lines = facts(vec![file("a.rs")]);
    lines.total_changed_lines = limits.max_changed_lines + 1;
    assert_eq!(codes(&m1(&lines)), vec!["policy.size"]);

    let mut single = facts(vec![file("a.rs")]);
    single.files[0].size_after = Some(limits.max_file_bytes + 1);
    assert_eq!(codes(&m1(&single)), vec!["policy.size"]);

    let count = usize::try_from(limits.max_files).expect("fits");
    let names: Vec<String> = (0..=count).map(|i| format!("f{i}.rs")).collect();
    let many = facts(names.iter().map(|name| file(name)).collect());
    assert_eq!(codes(&m1(&many)), vec!["policy.size"]);
    let exact = facts(names[..count].iter().map(|name| file(name)).collect());
    assert_eq!(m1(&exact).verdict, PatchVerdict::Allow);
}

#[test]
fn mechanical_facts_are_rechecked_without_approval_path() {
    let mut symlink = file("link");
    symlink.new_mode = Some(FileMode::Mode120000);
    let mut flagged_symlink = file("link2");
    flagged_symlink.is_symlink = true;
    let mut submodule = file("vendor/sub");
    submodule.new_mode = Some(FileMode::Mode160000);
    let mut binary = file("img.png");
    binary.is_binary = true;
    let m2 = PolicyBuild {
        patch_approvals: true,
        ..PolicyBuild::M1
    };
    for (file, code) in [
        (symlink, "policy.symlink"),
        (flagged_symlink, "policy.symlink"),
        (submodule, "policy.submodule"),
        (file(".gitmodules"), "policy.submodule"),
        (binary, "policy.binary"),
    ] {
        let verdict = evaluate(
            &PolicyRules::builtin(),
            m2,
            &facts(vec![file]),
            &VecAddedLines::default(),
        );
        assert_eq!(verdict.verdict, PatchVerdict::Deny, "{code}");
        assert_eq!(codes(&verdict), vec![code]);
    }
}

#[test]
fn mode_change_is_denied_in_m1_and_needs_approval_in_m2() {
    let mut changed = file("run.sh");
    changed.status = FileStatus::ModeOnly;
    changed.new_mode = Some(FileMode::Mode100755);
    let verdict = m1(&facts(vec![changed.clone()]));
    assert_eq!(verdict.verdict, PatchVerdict::Deny);
    assert_eq!(
        codes(&verdict),
        vec!["policy.path_protected:file_mode_change"]
    );

    let m2 = PolicyBuild {
        patch_approvals: true,
        ..PolicyBuild::M1
    };
    let verdict = evaluate(
        &PolicyRules::builtin(),
        m2,
        &facts(vec![changed]),
        &VecAddedLines::default(),
    );
    assert_eq!(verdict.verdict, PatchVerdict::NeedsApproval);
    assert_eq!(
        verdict.matched_approval_rules,
        vec![RuleId::new("file_mode_change")]
    );

    let mut new_exec = file("new.sh");
    new_exec.status = FileStatus::Added;
    new_exec.old_mode = None;
    new_exec.new_mode = Some(FileMode::Mode100755);
    assert_eq!(m1(&facts(vec![new_exec])).verdict, PatchVerdict::Allow);
}

#[test]
fn secrets_in_added_lines_are_denied_without_plaintext() {
    let ghp = format!("ghp_{}", "x".repeat(36));
    let sk = format!("sk-{}", "y".repeat(40));
    let key = format!("-----BEGIN {} PRIVATE KEY-----", "OPENSSH");
    for (path, secret) in [("a.rs", &sk), ("b.rs", &ghp), ("c.pem.txt", &key)] {
        let added = VecAddedLines::default().with(path, &["fn ok() {}", secret.as_str()]);
        let verdict = evaluate(
            &PolicyRules::builtin(),
            PolicyBuild::M1,
            &facts(vec![file(path)]),
            &added,
        );
        assert_eq!(verdict.verdict, PatchVerdict::Deny, "{path}");
        assert_eq!(codes(&verdict), vec!["policy.secret"]);
        assert!(!format!("{verdict:?}").contains(secret.as_str()), "{path}");
    }
}

#[test]
fn unreadable_added_lines_fail_closed() {
    let mut added = VecAddedLines::default();
    added.0.insert("a.rs".into(), vec![Err(())]);
    let verdict = evaluate(
        &PolicyRules::builtin(),
        PolicyBuild::M1,
        &facts(vec![file("a.rs")]),
        &added,
    );
    assert_eq!(codes(&verdict), vec!["policy.secret"]);
}

#[test]
fn strictest_wins_and_rename_paths_both_count() {
    // 重命名在 --no-renames 下是删除 + 新增两条记录。
    let mut old = file("Cargo.lock");
    old.status = FileStatus::Deleted;
    let mut new = file("src/renamed.rs");
    new.status = FileStatus::Added;
    let verdict = evaluate(
        &PolicyRules::builtin(),
        PolicyBuild {
            patch_approvals: true,
            ..PolicyBuild::M1
        },
        &facts(vec![old, new, file(".env")]),
        &VecAddedLines::default(),
    );
    assert_eq!(verdict.verdict, PatchVerdict::Deny);
    assert_eq!(
        codes(&verdict),
        vec!["policy.path_protected:lockfiles", "policy.secret"]
    );
    assert!(verdict.matched_approval_rules.is_empty());
}

#[test]
fn adversarial_paths_are_rejected() {
    // `a/../../x` 连 RepoPath 都构造不出来（repo 的 P4 先拒）。
    assert!(RepoPath::parse("a/../../x").is_err());
    assert!(serde_json::from_value::<RepoPath>(serde_json::json!("a/../../x")).is_err());
    for path in [".GIT/config", "GIT~1/config", "sub/.Git/HEAD"] {
        let verdict = m1(&facts(vec![file(path)]));
        assert_eq!(verdict.verdict, PatchVerdict::Deny, "{path}");
        assert_eq!(codes(&verdict), vec!["policy.dotgit"], "{path}");
    }
}
