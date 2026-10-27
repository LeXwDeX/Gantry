//! repo.md §12.D（补丁闸门，覆盖 worker-protocol §11.3 的对抗用例）、§12.A（幂等一致性）、
//! §12.F（一次性检出不执行仓库内容、被拒补丁不留痕）与 §12.G（并发与重启）。
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{commit_all, git, refs, write, Fixture, TempDir};
use gantry_core::canon::{sha256, GitOid};
use gantry_core::entities::Seq;
use gantry_core::events::{EventType, PatchRejectedData, PatchValidatedData};
use gantry_core::ports::{
    BranchName, CreateWorktree, DeclaredPatch, PatchOutcome, PatchPolicyVerdict, RepoService,
    ValidatePatch,
};
use gantry_core::worker::{
    FileMode, FileStatus, PatchReason, PatchRejectReason, PatchVerdict, PathConstraints,
    PolicyCode, RepoPath,
};
use gantry_fakes::repo::{patch_idempotency_conformance, FakeRepo};
use gantry_repo::cas::{CasKind, PatchCas};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/patches")
            .join(name),
    )
    .unwrap()
}

fn paths() -> PathConstraints {
    PathConstraints {
        allow: vec!["**".into()],
        deny: Vec::new(),
        approval_required: Vec::new(),
    }
}

async fn request(fx: &Fixture, base: GitOid, bytes: &[u8]) -> ValidatePatch {
    let digest = fx.cas.put(bytes, CasKind::Artifact).await.unwrap();
    ValidatePatch {
        project_id: fx.project,
        delivery_id: fx.delivery(),
        job_id: fx.ids.next().unwrap(),
        worktree_id: None,
        base_sha: base,
        patch: digest,
        declared: DeclaredPatch {
            sha256: digest,
            bytes: bytes.len() as u64,
            base_sha: base,
            files_changed: 1,
        },
        job_paths: paths(),
    }
}

async fn validate(fx: &Fixture, base: GitOid, bytes: &[u8]) -> (ValidatePatch, PatchOutcome) {
    let req = request(fx, base, bytes).await;
    let outcome = fx
        .repo
        .validate_patch(&fx.ctx(), req.clone())
        .await
        .unwrap();
    (req, outcome)
}

/// 在上游 base 的独立克隆里改文件，产出 `git diff --cached` 补丁与独立得到的 tree。
struct Edit {
    dir: PathBuf,
    _tmp: TempDir,
}

impl Edit {
    fn new(fx: &Fixture) -> Self {
        let tmp = TempDir::new("edit");
        let dir = tmp.path().join("clone");
        git(
            tmp.path(),
            &["clone", "-q", fx.upstream().to_str().unwrap(), "clone"],
        );
        Self { dir, _tmp: tmp }
    }

    fn write(&self, rel: &str, content: &str) -> &Self {
        write(&self.dir, rel, content);
        self
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.dir, args)
    }

    /// (补丁字节, 独立 `git write-tree` 得到的 tree)
    fn diff(&self) -> (Vec<u8>, GitOid) {
        self.git(&["add", "-A"]);
        let patch = self.git(&["diff", "--cached", "--full-index", "--no-renames", "HEAD"]);
        let tree = GitOid::parse(self.git(&["write-tree"]).trim()).unwrap();
        (patch.into_bytes(), tree)
    }
}

fn scratch_is_clean(fx: &Fixture) {
    let scratch = fx.data_dir().join("scratch");
    let left: Vec<_> = std::fs::read_dir(&scratch).unwrap().collect();
    assert!(left.is_empty(), "scratch/ must be empty: {left:?}");
    let list = git(&fx.mirror_dir(), &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 1, "{list}");
}

fn rejected_reason(outcome: &PatchOutcome) -> PatchRejectReason {
    match outcome {
        PatchOutcome::Rejected { reason, .. } => *reason,
        other => panic!("expected a mechanical rejection, got {other:?}"),
    }
}

fn verdict(v: PatchVerdict, reasons: Vec<PatchReason>) -> PatchPolicyVerdict {
    PatchPolicyVerdict {
        verdict: v,
        reasons,
        matched_approval_rules: Vec::new(),
        report: None,
        config_rev: Seq::new(7).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// 黄金路径与统计
// ---------------------------------------------------------------------------

#[tokio::test]
async fn golden_patch_is_allowed_with_independent_tree_and_no_ref_changes() {
    let fx = Fixture::new("gate-golden").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("README.md", "hello\nworld\n")
        .write("src/new.rs", "pub fn two() -> u32 { 2 }\n");
    let (bytes, tree) = edit.diff();
    let before = refs(&fx.mirror_dir());

    let (req, outcome) = validate(&fx, base, &bytes).await;
    let PatchOutcome::Validated(patch) = outcome else {
        panic!("expected Validated");
    };
    assert_eq!(patch.verdict, PatchVerdict::Allow);
    assert_eq!(patch.result_tree, Some(tree));
    assert_eq!(patch.sha256, sha256(&bytes));
    assert_eq!(patch.base_sha, base);
    assert_eq!(
        refs(&fx.mirror_dir()),
        before,
        "mirror refs/ must not change"
    );
    scratch_is_clean(&fx);

    let events = fx.events(EventType::PatchValidated).await;
    assert_eq!(events.len(), 1);
    let data: PatchValidatedData = serde_json::from_value(events[0].data.clone()).unwrap();
    assert_eq!(data.job_id, req.job_id);
    assert_eq!(data.result_tree, tree);
    assert_eq!(data.config_rev, patch.config_rev);
    assert!(fx.events(EventType::PatchRejected).await.is_empty());
}

#[tokio::test]
async fn facts_match_independent_git_statistics() {
    let fx = Fixture::new("gate-stats").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("README.md", "hello\nsecond\nthird\n")
        .write("docs/guide.md", "# guide\n\nbody\n");
    edit.git(&["rm", "-q", "src/lib.rs"]);
    let (bytes, _) = edit.diff();
    let numstat = edit.git(&["diff", "--cached", "--numstat", "--no-renames", "HEAD"]);

    let (_, outcome) = validate(&fx, base, &bytes).await;
    assert!(matches!(outcome, PatchOutcome::Validated(_)));
    let facts = fx.counting.last_facts().expect("policy was consulted");
    assert_eq!(facts.patch_bytes, bytes.len() as u64);
    assert!(facts.applies_cleanly);
    let mut total = 0u64;
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let added: u32 = parts.next().unwrap().parse().unwrap();
        let deleted: u32 = parts.next().unwrap().parse().unwrap();
        let path = parts.next().unwrap();
        total += u64::from(added + deleted);
        let file = facts
            .files
            .iter()
            .find(|f| f.path.as_str() == path)
            .unwrap_or_else(|| panic!("{path} missing from facts"));
        assert_eq!((file.added, file.deleted), (Some(added), Some(deleted)));
        assert!(!file.is_binary && !file.is_symlink && !file.is_submodule);
    }
    assert_eq!(facts.files.len(), numstat.lines().count());
    assert_eq!(facts.total_changed_lines, total);
    let guide = facts
        .files
        .iter()
        .find(|f| f.path.as_str() == "docs/guide.md")
        .unwrap();
    assert_eq!(guide.status, FileStatus::Added);
    assert_eq!(guide.size_after, Some("# guide\n\nbody\n".len() as u64));
    let removed = facts
        .files
        .iter()
        .find(|f| f.path.as_str() == "src/lib.rs")
        .unwrap();
    assert_eq!(removed.status, FileStatus::Deleted);
    assert_eq!(removed.size_after, None);
}

#[tokio::test]
async fn mode_changes_are_recorded_for_policy() {
    let fx = Fixture::new("gate-modes").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("bin/run.sh", "#!/bin/sh\necho hi\n");
    edit.git(&["add", "bin/run.sh"]);
    edit.git(&["update-index", "--chmod=+x", "bin/run.sh"]);
    edit.git(&["update-index", "--chmod=+x", "README.md"]);
    let patch = edit.git(&["diff", "--cached", "--full-index", "HEAD"]);

    let (_, outcome) = validate(&fx, base, patch.as_bytes()).await;
    assert!(matches!(outcome, PatchOutcome::Validated(_)));
    let facts = fx.counting.last_facts().unwrap();
    let readme = facts
        .files
        .iter()
        .find(|f| f.path.as_str() == "README.md")
        .unwrap();
    assert_eq!(readme.status, FileStatus::ModeOnly);
    assert_eq!(readme.old_mode, Some(FileMode::Mode100644));
    assert_eq!(readme.new_mode, Some(FileMode::Mode100755));
    let script = facts
        .files
        .iter()
        .find(|f| f.path.as_str() == "bin/run.sh")
        .unwrap();
    assert_eq!(script.status, FileStatus::Added);
    assert_eq!(script.new_mode, Some(FileMode::Mode100755));
}

// ---------------------------------------------------------------------------
// 机械拒绝（P-68、P-69：不调用 policy、不产生 policy.denied）
// ---------------------------------------------------------------------------

async fn assert_pre_apply_rejection(
    fx: &Fixture,
    base: GitOid,
    name: &str,
    want: PatchRejectReason,
) {
    let bytes = fixture(name);
    let before = refs(&fx.mirror_dir());
    let calls = fx.counting.evaluations();
    let (req, outcome) = validate(fx, base, &bytes).await;
    assert_eq!(rejected_reason(&outcome), want, "{name}: {outcome:?}");
    assert_eq!(
        fx.counting.evaluations(),
        calls,
        "{name}: policy must not be called"
    );
    assert_eq!(refs(&fx.mirror_dir()), before, "{name}: no ref changes");
    scratch_is_clean(fx);
    assert!(
        fx.cas.is_quarantined(&req.patch),
        "{name}: original kept in quarantine"
    );
    assert!(fx.events(EventType::PolicyDenied).await.is_empty());
    let rejected = fx.events(EventType::PatchRejected).await;
    let data: PatchRejectedData =
        serde_json::from_value(rejected.last().unwrap().data.clone()).unwrap();
    assert_eq!(data.job_id, req.job_id);
    assert_eq!(data.reason, want);
    assert!(data.reasons.is_empty() && data.config_rev.is_none());
}

#[tokio::test]
async fn symlinks_gitlinks_and_gitmodules_are_rejected_without_policy() {
    let fx = Fixture::new("gate-p5").await;
    let base = fx.seed().await;
    assert_pre_apply_rejection(&fx, base, "symlink.patch", PatchRejectReason::Symlink).await;
    assert_pre_apply_rejection(&fx, base, "gitlink.patch", PatchRejectReason::Submodule).await;
    assert_pre_apply_rejection(&fx, base, "gitmodules.patch", PatchRejectReason::Submodule).await;
}

#[tokio::test]
async fn dotgit_variants_are_rejected() {
    let fx = Fixture::new("gate-dotgit").await;
    let base = fx.seed().await;
    for name in [
        "dotgit-hooks.patch",
        "dotgit-upper.patch",
        "dotgit-8dot3.patch",
        "dotgit-ntfs-ads.patch",
        "dotgit-hfs.patch",
        "dotgit-nested.patch",
    ] {
        assert_pre_apply_rejection(&fx, base, name, PatchRejectReason::Dotgit).await;
    }
}

#[tokio::test]
async fn invalid_paths_are_rejected_before_apply() {
    let fx = Fixture::new("gate-paths").await;
    let base = fx.seed().await;
    // 这一组同时验证"预解析先于应用"：否则会被 git apply 拒绝并归为 not_applicable。
    for name in [
        "path-traversal.patch",
        "path-dotdot.patch",
        "path-absolute.patch",
        "path-backslash.patch",
        "path-nul.patch",
        "path-empty-component.patch",
    ] {
        assert_pre_apply_rejection(&fx, base, name, PatchRejectReason::PathInvalid).await;
    }
}

#[tokio::test]
async fn case_and_unicode_collisions_are_path_invalid() {
    let fx = Fixture::new("gate-collide").await;
    let base = fx.seed().await;
    assert_pre_apply_rejection(
        &fx,
        base,
        "path-case-collision.patch",
        PatchRejectReason::PathInvalid,
    )
    .await;
    assert_pre_apply_rejection(
        &fx,
        base,
        "path-nfc-nfd.patch",
        PatchRejectReason::PathInvalid,
    )
    .await;
}

#[tokio::test]
async fn binary_patches_are_rejected_without_approval_path() {
    let fx = Fixture::new("gate-binary").await;
    let base = fx.seed().await;
    assert_pre_apply_rejection(&fx, base, "binary-git.patch", PatchRejectReason::Binary).await;
    assert_pre_apply_rejection(&fx, base, "binary-files.patch", PatchRejectReason::Binary).await;
}

#[tokio::test]
async fn post_apply_facts_catch_symlink_edits_without_policy() {
    let fx = Fixture::new("gate-post").await;
    fx.seed().await;
    // base 里已有一个符号链接；只改它的目标，summary 里没有模式行，要靠应用后事实发现。
    let upstream = fx.upstream();
    std::os::unix::fs::symlink("README.md", upstream.join("link")).unwrap();
    let base = commit_all(&upstream, "add link");
    fx.push_upstream(&upstream);
    let edit = Edit::new(&fx);
    std::fs::remove_file(edit.dir.join("link")).unwrap();
    std::os::unix::fs::symlink("src/lib.rs", edit.dir.join("link")).unwrap();
    let (bytes, _) = edit.diff();

    let calls = fx.counting.evaluations();
    let (_, outcome) = validate(&fx, base, &bytes).await;
    assert_eq!(rejected_reason(&outcome), PatchRejectReason::Symlink);
    assert_eq!(fx.counting.evaluations(), calls);
    scratch_is_clean(&fx);
}

#[tokio::test]
async fn wrong_base_or_context_is_not_applicable_and_atomic() {
    let fx = Fixture::new("gate-context").await;
    let base = fx.seed().await;
    let before = refs(&fx.mirror_dir());
    let (_, outcome) = validate(&fx, base, &fixture("context-mismatch.patch")).await;
    assert_eq!(rejected_reason(&outcome), PatchRejectReason::NotApplicable);

    // 补丁基于新提交生成，却声称 base 是旧提交。
    let upstream = fx.upstream();
    write(&upstream, "README.md", "hello\nnewer\n");
    let newer = commit_all(&upstream, "newer");
    fx.push_upstream(&upstream);
    let edit = Edit::new(&fx);
    edit.write("README.md", "hello\nnewer\nnewest\n");
    let (bytes, _) = edit.diff();
    let (_, outcome) = validate(&fx, base, &bytes).await;
    assert_eq!(rejected_reason(&outcome), PatchRejectReason::NotApplicable);
    assert_eq!(fx.counting.evaluations(), 0);
    scratch_is_clean(&fx);
    let after = refs(&fx.mirror_dir());
    assert_eq!(
        after.replace(&newer.to_string(), &base.to_string()),
        before,
        "only the upstream push moved refs"
    );
}

#[tokio::test]
async fn declared_mismatch_is_rejected_before_apply() {
    let fx = Fixture::new("gate-declared").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("README.md", "changed\n");
    let (bytes, _) = edit.diff();
    for tweak in 0..3 {
        let mut req = request(&fx, base, &bytes).await;
        match tweak {
            0 => req.declared.sha256 = sha256(b"something else"),
            1 => req.declared.bytes += 1,
            _ => req.declared.base_sha = GitOid::parse(&"5".repeat(40)).unwrap(),
        }
        let outcome = fx.repo.validate_patch(&fx.ctx(), req).await.unwrap();
        assert_eq!(
            rejected_reason(&outcome),
            PatchRejectReason::DeclaredMismatch
        );
    }
    assert_eq!(fx.counting.evaluations(), 0);
    scratch_is_clean(&fx);
}

#[tokio::test]
async fn empty_patch_is_no_change() {
    let fx = Fixture::new("gate-empty").await;
    let base = fx.seed().await;
    let (_, outcome) = validate(&fx, base, b"").await;
    assert_eq!(outcome, PatchOutcome::NoChange);
    let (_, outcome) = validate(&fx, base, b"\n  \n").await;
    assert_eq!(outcome, PatchOutcome::NoChange);
    assert!(fx.events(EventType::PatchValidated).await.is_empty());
    assert!(fx.events(EventType::PatchRejected).await.is_empty());
}

#[tokio::test]
async fn rejection_detail_is_sanitized() {
    let fx = Fixture::new("gate-bidi").await;
    let base = fx.seed().await;
    let (_, outcome) = validate(&fx, base, &fixture("bidi-dotgit.patch")).await;
    let PatchOutcome::Rejected { reason, detail } = outcome else {
        panic!("expected rejection");
    };
    assert_eq!(reason, PatchRejectReason::Dotgit);
    assert!(!detail.contains('\u{202e}'), "{detail:?}");
    let data: PatchRejectedData =
        serde_json::from_value(fx.events(EventType::PatchRejected).await[0].data.clone()).unwrap();
    assert!(!data.detail.contains('\u{202e}'));
}

// ---------------------------------------------------------------------------
// policy 的规则判定（FakePolicy 回放）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policy_deny_becomes_policy_denied_rejection() {
    let fx = Fixture::new("gate-deny").await;
    let base = fx.seed().await;
    for (rule, path) in [
        ("workflows", ".github/workflows/ci.yml"),
        ("lockfiles", "Cargo.lock"),
    ] {
        let edit = Edit::new(&fx);
        edit.write(path, "changed: true\n");
        let (bytes, _) = edit.diff();
        let mut req = request(&fx, base, &bytes).await;
        let reason = PatchReason {
            code: PolicyCode::new(format!("policy.path_protected:{rule}")),
            path: Some(RepoPath::parse(path).unwrap()),
            rule_id: Some(rule.into()),
        };
        fx.policy.script_patch_verdict(
            req.job_id,
            verdict(PatchVerdict::Deny, vec![reason.clone()]),
        );
        req.job_paths = paths();
        let outcome = fx
            .repo
            .validate_patch(&fx.ctx(), req.clone())
            .await
            .unwrap();
        let PatchOutcome::Validated(patch) = outcome else {
            panic!("policy deny is Validated(Deny)");
        };
        assert_eq!(patch.verdict, PatchVerdict::Deny);
        assert_eq!(patch.result_tree, None, "deny writes no tree");
        assert_eq!(patch.reasons, vec![reason.clone()]);
        assert!(fx.cas.is_quarantined(&req.patch));
        let rejected = fx.events(EventType::PatchRejected).await;
        let data: PatchRejectedData =
            serde_json::from_value(rejected.last().unwrap().data.clone()).unwrap();
        assert_eq!(data.reason, PatchRejectReason::PolicyDenied);
        assert_eq!(data.reasons, vec![reason]);
        assert_eq!(data.config_rev, Some(Seq::new(7).unwrap()));
        assert!(data.detail.contains(path));
    }
    assert!(fx.events(EventType::PatchValidated).await.is_empty());
    scratch_is_clean(&fx);
}

#[tokio::test]
async fn needs_approval_keeps_the_result_tree() {
    let fx = Fixture::new("gate-approval").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("migrations/001.sql", "create table t();\n");
    let (bytes, tree) = edit.diff();
    let req = request(&fx, base, &bytes).await;
    fx.policy
        .script_patch_verdict(req.job_id, verdict(PatchVerdict::NeedsApproval, Vec::new()));
    let outcome = fx.repo.validate_patch(&fx.ctx(), req).await.unwrap();
    let PatchOutcome::Validated(patch) = outcome else {
        panic!("expected Validated");
    };
    assert_eq!(patch.verdict, PatchVerdict::NeedsApproval);
    assert_eq!(patch.result_tree, Some(tree));
    let data: PatchValidatedData =
        serde_json::from_value(fx.events(EventType::PatchValidated).await[0].data.clone()).unwrap();
    assert_eq!(data.verdict, PatchVerdict::NeedsApproval);
}

#[tokio::test]
async fn secrets_in_added_lines_reach_policy_from_git_output() {
    let fx = Fixture::new("gate-secret").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("README.md", "hello\n+++ AKIAEXAMPLEKEY1234567\n");
    let (bytes, _) = edit.diff();
    let req = request(&fx, base, &bytes).await;
    fx.policy.script_patch_verdict(
        req.job_id,
        verdict(
            PatchVerdict::Deny,
            vec![PatchReason {
                code: PolicyCode::new("policy.secret"),
                path: Some(RepoPath::parse("README.md").unwrap()),
                rule_id: None,
            }],
        ),
    );
    let outcome = fx.repo.validate_patch(&fx.ctx(), req).await.unwrap();
    assert!(matches!(outcome, PatchOutcome::Validated(ref p) if p.verdict == PatchVerdict::Deny));
    let added = fx.counting.added.lock().unwrap().clone();
    let readme = added.iter().find(|(path, _)| path == "README.md").unwrap();
    assert_eq!(readme.1, vec!["+++ AKIAEXAMPLEKEY1234567".to_owned()]);
}

#[tokio::test]
async fn facts_given_to_policy_are_clean() {
    let fx = Fixture::new("gate-invariants").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("a.txt", "a\n").write("b/c.txt", "c\n");
    let (bytes, _) = edit.diff();
    validate(&fx, base, &bytes).await;
    for facts in fx.counting.facts.lock().unwrap().iter() {
        assert!(facts.applies_cleanly);
        assert_eq!(facts.job_paths, paths());
        for file in &facts.files {
            assert!(!file.is_binary && !file.is_symlink && !file.is_submodule);
        }
    }
}

// ---------------------------------------------------------------------------
// 幂等、P10、一致性
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validate_is_idempotent_and_matches_fake_conformance() {
    let fx = Fixture::new("gate-idem").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("README.md", "idempotent\n");
    let (bytes, _) = edit.diff();
    let req = request(&fx, base, &bytes).await;
    patch_idempotency_conformance(&fx.repo, &fx.ctx(), req.clone())
        .await
        .unwrap();
    assert_eq!(fx.events(EventType::PatchValidated).await.len(), 1);
    assert_eq!(fx.counting.evaluations(), 1);

    // 机械拒绝同样只追加一次事件。
    let rejected = request(&fx, base, &fixture("symlink.patch")).await;
    patch_idempotency_conformance(&fx.repo, &fx.ctx(), rejected)
        .await
        .unwrap();
    assert_eq!(fx.events(EventType::PatchRejected).await.len(), 1);

    let fake = FakeRepo::new(gantry_core::time::Timestamp::from_millis(common::T0));
    patch_idempotency_conformance(&fake, &fx.ctx(), req)
        .await
        .unwrap();
}

#[tokio::test]
async fn restart_returns_the_recorded_outcome_without_new_events() {
    let mut fx = Fixture::new("gate-restart").await;
    let base = fx.seed().await;
    let edit = Edit::new(&fx);
    edit.write("README.md", "restart\n");
    let (bytes, _) = edit.diff();
    let req = request(&fx, base, &bytes).await;
    let first = fx
        .repo
        .validate_patch(&fx.ctx(), req.clone())
        .await
        .unwrap();
    let denied = request(&fx, base, &bytes).await;
    fx.policy.script_patch_verdict(
        denied.job_id,
        verdict(
            PatchVerdict::Deny,
            vec![PatchReason {
                code: PolicyCode::new("policy.path_denied"),
                path: None,
                rule_id: None,
            }],
        ),
    );
    let denied_first = fx
        .repo
        .validate_patch(&fx.ctx(), denied.clone())
        .await
        .unwrap();

    fx.reopen().await;
    assert_eq!(fx.repo.validate_patch(&fx.ctx(), req).await.unwrap(), first);
    let PatchOutcome::Validated(again) = fx.repo.validate_patch(&fx.ctx(), denied).await.unwrap()
    else {
        panic!("expected Validated(Deny)");
    };
    let PatchOutcome::Validated(before) = denied_first else {
        unreachable!()
    };
    assert_eq!(again.verdict, PatchVerdict::Deny);
    assert_eq!(again.reasons, before.reasons);
    assert_eq!(again.base_sha, before.base_sha);
    assert_eq!(fx.events(EventType::PatchValidated).await.len(), 1);
    assert_eq!(fx.events(EventType::PatchRejected).await.len(), 1);
    assert_eq!(
        fx.counting.evaluations(),
        2,
        "no re-evaluation after restart"
    );
}

#[tokio::test]
async fn moved_branch_head_is_a_base_conflict() {
    let fx = Fixture::new("gate-p10").await;
    let base = fx.seed().await;
    let upstream = fx.upstream();
    write(&upstream, "README.md", "moved\n");
    let other = commit_all(&upstream, "other");
    fx.push_upstream(&upstream);
    let wt = fx
        .repo
        .create_worktree(
            &fx.ctx(),
            CreateWorktree {
                project_id: fx.project,
                delivery_id: fx.delivery(),
                branch: BranchName::new("gantry/p10"),
                base_ref: "main".into(),
                base_sha: other,
            },
        )
        .await
        .unwrap();
    let edit = Edit::new(&fx);
    edit.git(&["checkout", "-q", &base.to_string()]);
    edit.write("README.md", "patch on old base\n");
    let (bytes, _) = edit.diff();
    let mut req = request(&fx, base, &bytes).await;
    req.worktree_id = Some(wt.id);
    let outcome = fx.repo.validate_patch(&fx.ctx(), req).await.unwrap();
    assert_eq!(
        outcome,
        PatchOutcome::BaseConflict {
            base_sha: base,
            branch_head: other
        }
    );
    assert_eq!(fx.counting.evaluations(), 0);
    scratch_is_clean(&fx);
}

// ---------------------------------------------------------------------------
// §12.F / §12.G
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scratch_checkout_never_executes_repository_content() {
    let fx = Fixture::new("gate-traps").await;
    fx.seed().await;
    let upstream = fx.upstream();
    write(&upstream, ".gitattributes", "* filter=evil diff=evil\n");
    let outside = fx.tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, upstream.join("link")).unwrap();
    let base = commit_all(&upstream, "traps");
    fx.push_upstream(&upstream);
    // 陷阱在推送之后再装：夹具自己的 push 会在镜像仓里跑 receive-pack 与 hook。
    let marker = fx.tmp.path().join("pwned");
    let touch = format!("#!/bin/sh\ntouch {}\n", marker.display());
    let mirror = fx.mirror_dir();
    git(
        &mirror,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch {}", marker.display()),
        ],
    );
    for hook in [
        "post-checkout",
        "pre-commit",
        "post-index-change",
        "reference-transaction",
    ] {
        let path = mirror.join("hooks").join(hook);
        std::fs::write(&path, &touch).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // 一份正常补丁走完整条流水线（检出、应用、枚举、diff、write-tree）。
    let edit = Edit::new(&fx);
    edit.write("README.md", "trap run\n");
    let (bytes, _) = edit.diff();
    let (_, outcome) = validate(&fx, base, &bytes).await;
    assert!(matches!(outcome, PatchOutcome::Validated(_)), "{outcome:?}");
    // 经由检出外的符号链接写文件：git apply 拒绝。
    let (_, outcome) = validate(&fx, base, &fixture("symlink-escape.patch")).await;
    assert_eq!(rejected_reason(&outcome), PatchRejectReason::NotApplicable);
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    assert!(!marker.exists(), "repository content must never execute");
    scratch_is_clean(&fx);
}

#[tokio::test]
async fn concurrent_validations_do_not_interfere() {
    let fx = Fixture::new("gate-concurrent").await;
    let base = fx.seed().await;
    let mut jobs = Vec::new();
    for i in 0..20 {
        let edit = Edit::new(&fx);
        edit.write(&format!("file-{i}.txt"), &format!("content {i}\n"));
        let (bytes, tree) = edit.diff();
        jobs.push((request(&fx, base, &bytes).await, tree));
    }
    let repo = Arc::new(fx.repo.clone());
    let mut tasks = Vec::new();
    for (req, tree) in jobs {
        let repo = repo.clone();
        let ctx = fx.ctx();
        tasks.push(tokio::spawn(async move {
            (repo.validate_patch(&ctx, req).await, tree)
        }));
    }
    for task in tasks {
        let (outcome, tree) = task.await.unwrap();
        let PatchOutcome::Validated(patch) = outcome.unwrap() else {
            panic!("expected Validated");
        };
        assert_eq!(patch.result_tree, Some(tree));
    }
    assert_eq!(fx.events(EventType::PatchValidated).await.len(), 20);
    scratch_is_clean(&fx);
}
