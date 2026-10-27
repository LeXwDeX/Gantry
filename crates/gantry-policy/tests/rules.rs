//! policy.md §12.2「动作判定」的出厂默认表与硬底线，以及路径规则集。

use std::sync::Arc;

use gantry_core::entities::{PolicyActionId, Seq};
use gantry_core::ports::{Disposition, PathRuleSet};
use gantry_core::settings::{
    builtin_resolved, Disposition as SettingsDisposition, PathDisposition, Policies,
    ResolvedSettings, SettingsResource,
};
use gantry_core::worker::RepoPath;
use gantry_policy::{path_rules_snapshot, FloorViolation, PolicyBuild, PolicyRules, RulePaths};

fn builtin_policies() -> Policies {
    match builtin_resolved(SettingsResource::Policies) {
        ResolvedSettings::Policies(policies) => policies,
        other => panic!("unexpected resource {:?}", other.resource()),
    }
}

fn path(value: &str) -> RepoPath {
    RepoPath::parse(value).expect("test path")
}

/// config.md §5.1 的 20 行，逐行写死，防止默认值悄悄漂移。
#[test]
fn builtin_action_dispositions_match_config_table() {
    use Disposition::{Approve, Auto, Deny};
    use PolicyActionId as Id;
    let table = [
        (Id::MergePr, Approve),
        (Id::ForcePush, Approve),
        (Id::PushOutsidePrefix, Approve),
        (Id::ResolveReviewThreadHuman, Deny),
        (Id::DeleteBranch, Deny),
        (Id::PublishRelease, Deny),
        (Id::ChangeRepoSettings, Deny),
        (Id::PushBranch, Auto),
        (Id::CreatePr, Auto),
        (Id::UpdatePr, Auto),
        (Id::UpsertStatusComment, Auto),
        (Id::CreateComment, Auto),
        (Id::ReplyReviewThread, Auto),
        (Id::ResolveReviewThread, Auto),
        (Id::CreateIssue, Auto),
        (Id::UpdateIssue, Auto),
        (Id::CloseIssue, Auto),
        (Id::AddLabels, Auto),
        (Id::RequestReviewers, Auto),
        (Id::RerunFailedJobs, Auto),
    ];
    assert_eq!(table.len(), PolicyActionId::ALL.len());
    let rules = PolicyRules::builtin();
    for (id, expected) in table {
        assert_eq!(rules.disposition(id), expected, "{id}");
    }
    assert_eq!(rules.unlisted_action, Approve);
    assert_eq!(rules.config_rev, Seq::ZERO);
    assert_eq!(rules.approval_ttl.as_secs(), 86_400);
}

#[test]
fn dangerous_actions_configured_auto_are_rejected() {
    for id in PolicyActionId::ALL
        .into_iter()
        .filter(|id| id.is_dangerous())
    {
        let mut policies = builtin_policies();
        policies.actions.insert(id, SettingsDisposition::Auto);
        assert_eq!(
            PolicyRules::load(&policies, Seq::ZERO).err(),
            Some(FloorViolation::DangerousActionAuto(id)),
            "{id}"
        );
    }
    assert_eq!(
        PolicyActionId::ALL
            .iter()
            .filter(|id| id.is_dangerous())
            .count(),
        7
    );
}

#[test]
fn workflows_and_sensitive_files_must_stay_deny() {
    for rule in ["workflows", "sensitive_files"] {
        let mut policies = builtin_policies();
        policies
            .paths
            .get_mut(rule)
            .expect("default rule")
            .disposition = PathDisposition::Approve;
        assert!(
            matches!(
                PolicyRules::load(&policies, Seq::ZERO),
                Err(FloorViolation::PathRuleMustDeny(name)) if name == rule
            ),
            "{rule}"
        );
        let mut policies = builtin_policies();
        policies.paths.remove(rule);
        assert!(
            PolicyRules::load(&policies, Seq::ZERO).is_err(),
            "{rule} removed"
        );
    }
}

#[test]
fn approval_ttl_and_glob_syntax_are_checked() {
    let mut policies = builtin_policies();
    policies.approval.ttl_s = 604_801;
    assert_eq!(
        PolicyRules::load(&policies, Seq::ZERO).err(),
        Some(FloorViolation::ApprovalTtl(604_801))
    );
    let mut policies = builtin_policies();
    policies
        .paths
        .get_mut("lockfiles")
        .expect("default rule")
        .globs
        .push("!Cargo.lock".into());
    assert!(matches!(
        PolicyRules::load(&policies, Seq::ZERO),
        Err(FloorViolation::Glob { pattern, .. }) if pattern == "!Cargo.lock"
    ));
}

#[test]
fn path_rule_set_reports_strictest_configured_rule() {
    let rules = RulePaths::new(Arc::new(PolicyRules::builtin()));
    let hits = rules.evaluate(&[
        path(".github/workflows/ci.yml"),
        path("package.json"),
        path("web/package.json"),
        path("Cargo.lock"),
        path("crates/x/Cargo.lock"),
        path(".env"),
        path("config/.env.local"),
        path("certs/server.pem"),
        path("CODEOWNERS"),
        path("src/main.rs"),
    ]);
    let summary: Vec<(&str, Option<&str>, Disposition)> = hits
        .iter()
        .map(|hit| {
            (
                hit.path.as_str(),
                hit.rule_id.as_ref().map(|id| id.as_str()),
                hit.disposition,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                ".github/workflows/ci.yml",
                Some("workflows"),
                Disposition::Deny
            ),
            (
                "package.json",
                Some("dependency_manifests"),
                Disposition::Approve
            ),
            (
                "web/package.json",
                Some("dependency_manifests"),
                Disposition::Approve
            ),
            ("Cargo.lock", Some("lockfiles"), Disposition::Approve),
            (
                "crates/x/Cargo.lock",
                Some("lockfiles"),
                Disposition::Approve
            ),
            (".env", Some("sensitive_files"), Disposition::Deny),
            (
                "config/.env.local",
                Some("sensitive_files"),
                Disposition::Deny
            ),
            (
                "certs/server.pem",
                Some("sensitive_files"),
                Disposition::Deny
            ),
            ("CODEOWNERS", Some("repo_meta"), Disposition::Approve),
            ("src/main.rs", None, Disposition::Auto),
        ]
    );
    assert!(hits.iter().all(|hit| !hit.outside_allow));
}

#[test]
fn path_rule_set_is_case_sensitive_and_marks_outside_allow() {
    let mut policies = builtin_policies();
    policies.allow_globs = vec!["src/**".into()];
    let rules = RulePaths::new(Arc::new(
        PolicyRules::load(&policies, Seq::ZERO).expect("valid policies"),
    ));
    let hits = rules.evaluate(&[path("src/lib.rs"), path("docs/a.md"), path("Package.json")]);
    assert!(!hits[0].outside_allow);
    assert!(hits[1].outside_allow);
    assert_eq!(hits[2].rule_id, None);
}

#[test]
fn m1_snapshot_folds_approve_rules_into_deny() {
    let rules = PolicyRules::builtin();
    let m1 = path_rules_snapshot(&rules, PolicyBuild::M1);
    assert_eq!(m1.allow, vec!["**".to_owned()]);
    assert!(m1.approval_required.is_empty());
    for pattern in [".github/workflows/**", "Cargo.lock", ".env", "CODEOWNERS"] {
        assert!(m1.deny.iter().any(|p| p == pattern), "{pattern}");
    }
    let m2 = path_rules_snapshot(
        &rules,
        PolicyBuild {
            patch_approvals: true,
            ..PolicyBuild::M1
        },
    );
    assert!(m2.approval_required.iter().any(|p| p == "Cargo.lock"));
    assert!(!m2.deny.iter().any(|p| p == "Cargo.lock"));
    assert!(m2.deny.iter().any(|p| p == ".github/workflows/**"));
}
