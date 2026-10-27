//! repo.md §12.H 第 1 条的 repo 部分：真实补丁、真实 git、真实 policy v0
//! （`gantry_policy::PolicyEngine`，`PolicyBuild::M1` 内置规则）。
//!
//! 断言 `policy.denied` 含 `policy.path_protected:<rule_id>`，`patch.rejected(reason =
//! policy_denied)` 的 `causation_id` 指向它。上报 InboxItem 归 engine 侧验收。
#![cfg(unix)]

mod common;

use std::sync::Arc;

use common::{git, write, Fixture, PolicyParts, TempDir};
use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{MergeMethod, Project};
use gantry_core::events::{EventType, PatchRejectedData};
use gantry_core::ports::{DeclaredPatch, PatchOutcome, PolicyPort, RepoService, ValidatePatch};
use gantry_core::worker::{PatchRejectReason, PatchVerdict, PathConstraints};
use gantry_fakes::forge::FakeForge;
use gantry_policy::{PolicyDeps, PolicyEngine};
use gantry_repo::cas::{CasKind, PatchCas};

fn engine(parts: PolicyParts) -> Arc<dyn PolicyPort> {
    let forge = Arc::new(FakeForge::new_in(parts.store.clone()));
    let now = parts.store.clock_now();
    forge.insert_project(Project {
        id: parts.project,
        repo: RepoSlug::parse("acme/widgets").unwrap(),
        installation_id: 42,
        bot_user_id: 9,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Merge],
        merge_queue_enabled: false,
        created_at: now,
        updated_at: now,
    });
    Arc::new(PolicyEngine::new(PolicyDeps {
        store: parts.store,
        forge,
        clock: parts.clock,
        ids: parts.ids,
    }))
}

fn patch_on(fx: &Fixture, files: &[(&str, &str)]) -> Vec<u8> {
    let tmp = TempDir::new("v0-edit");
    git(
        tmp.path(),
        &["clone", "-q", fx.upstream().to_str().unwrap(), "c"],
    );
    let dir = tmp.path().join("c");
    for (path, content) in files {
        write(&dir, path, content);
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["diff", "--cached", "--full-index", "HEAD"]).into_bytes()
}

async fn validate(fx: &Fixture, base: GitOid, bytes: &[u8]) -> PatchOutcome {
    let digest = fx.cas.put(bytes, CasKind::Artifact).await.unwrap();
    fx.repo
        .validate_patch(
            &fx.ctx(),
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
                job_paths: PathConstraints {
                    allow: vec!["**".into()],
                    deny: Vec::new(),
                    approval_required: Vec::new(),
                },
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn workflow_manifest_and_lockfile_patches_are_denied_by_policy_v0() {
    let fx = Fixture::with_policy("v0", engine).await;
    let base = fx.seed().await;
    for (path, rule) in [
        (".github/workflows/ci.yml", "workflows"),
        ("Cargo.lock", "lockfiles"),
        ("go.sum", "lockfiles"),
    ] {
        let bytes = patch_on(&fx, &[(path, "changed\n")]);
        let outcome = validate(&fx, base, &bytes).await;
        let PatchOutcome::Validated(patch) = outcome else {
            panic!("{path}: expected Validated(Deny), got {outcome:?}");
        };
        let code = format!("policy.path_protected:{rule}");
        assert_eq!(patch.verdict, PatchVerdict::Deny, "{path}");
        assert!(
            patch.reasons.iter().any(|r| r.code.as_str() == code),
            "{path}"
        );

        let denied = fx.events(EventType::PolicyDenied).await;
        let denied = denied.last().expect("policy appends policy.denied");
        assert!(denied.data.to_string().contains(&code), "{}", denied.data);
        let rejected = fx.events(EventType::PatchRejected).await;
        let rejected = rejected.last().unwrap();
        assert_eq!(
            rejected.causation_id,
            Some(denied.id),
            "causation points at policy.denied"
        );
        let data: PatchRejectedData = serde_json::from_value(rejected.data.clone()).unwrap();
        assert_eq!(data.reason, PatchRejectReason::PolicyDenied);
        assert!(data.reasons.iter().any(|r| r.code.as_str() == code));
        assert!(data.detail.contains(path));
    }
    assert_eq!(fx.events(EventType::PolicyDenied).await.len(), 3);

    // 普通源码改动在 v0 下放行，不产生 policy.denied。
    let bytes = patch_on(&fx, &[("src/lib.rs", "pub fn one() -> u32 { 2 }\n")]);
    assert!(matches!(
        validate(&fx, base, &bytes).await,
        PatchOutcome::Validated(ref p) if p.verdict == PatchVerdict::Allow
    ));
    assert_eq!(fx.events(EventType::PolicyDenied).await.len(), 3);
}

#[tokio::test]
async fn mechanical_rejections_never_reach_policy_v0() {
    let fx = Fixture::with_policy("v0-mech", engine).await;
    let base = fx.seed().await;
    let bytes = patch_on(&fx, &[(".gitmodules", "[submodule \"x\"]\n")]);
    let outcome = validate(&fx, base, &bytes).await;
    assert!(matches!(
        outcome,
        PatchOutcome::Rejected {
            reason: PatchRejectReason::Submodule,
            ..
        }
    ));
    assert!(fx.events(EventType::PolicyDenied).await.is_empty());
}

#[tokio::test]
async fn secrets_in_added_lines_are_denied_by_policy_v0() {
    let fx = Fixture::with_policy("v0-secret", engine).await;
    let base = fx.seed().await;
    let bytes = patch_on(
        &fx,
        &[(
            "src/config.rs",
            "const KEY: &str = \"AKIAIOSFODNN7EXAMPLE\";\n",
        )],
    );
    let PatchOutcome::Validated(patch) = validate(&fx, base, &bytes).await else {
        panic!("expected Validated");
    };
    assert_eq!(patch.verdict, PatchVerdict::Deny);
    assert!(patch
        .reasons
        .iter()
        .any(|r| r.code.as_str() == "policy.secret"));
}
