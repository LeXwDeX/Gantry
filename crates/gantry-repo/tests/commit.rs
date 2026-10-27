//! repo.md §12.E（提交，M1）、§12.B 中依赖提交的锁条目、§12.G 镜像仓重建后重放、
//! §12.H 提交对重放幂等。
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{git, refs, write, Fixture, TempDir};
use gantry_core::canon::GitOid;
use gantry_core::entities::{Actor, ActorKind, LockHolder, Seq, Worktree};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{AttemptId, DeliveryId};
use gantry_core::ports::{
    BranchName, CommitMessage, CommitPatch, CreateWorktree, DeclaredPatch, LockToken, MirrorHealth,
    PatchOutcome, PatchPolicyVerdict, RepoError, RepoService, ValidatePatch, WorktreeLocks,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::{PatchReason, PatchVerdict, PathConstraints, PolicyCode};
use gantry_repo::cas::{CasKind, PatchCas};
use serde_json::json;

const LEASE: Duration = Duration::from_secs(900);
/// 确定性时间戳（取 ResultBundle.collected_at）。
const COLLECTED_AT: i64 = 1_830_000_000_000;

async fn set_identity(fx: &Fixture) {
    let mut tx = fx.store.begin_write().await.unwrap();
    let base_rev = tx.read().config_rev(None);
    tx.append(NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::parse("settings").unwrap(),
        project_id: None,
        actor: Actor::new(ActorKind::User, "admin"),
        causation_id: None,
        correlation_id: None,
        data: json!({
            "scope": "global",
            "resource": "github",
            "schema_version": 1,
            "base_rev": base_rev.as_u64(),
            "value": {"commit_identity": {"name": "gantry-app[bot]", "email": "123+gantry-app[bot]@users.noreply.github.com"}},
            "relaxes": [],
            "secret_changes": []
        }),
        dedup_key: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

struct Setup {
    fx: Fixture,
    base: GitOid,
    wt: Worktree,
    delivery: DeliveryId,
    token: LockToken,
}

async fn setup(label: &str) -> Setup {
    let fx = Fixture::new(label).await;
    let base = fx.seed().await;
    set_identity(&fx).await;
    let delivery = fx.delivery();
    let wt = fx
        .repo
        .create_worktree(
            &fx.ctx(),
            CreateWorktree {
                project_id: fx.project,
                delivery_id: delivery,
                branch: BranchName::new(format!("gantry/{delivery}")),
                base_ref: "main".into(),
                base_sha: base,
            },
        )
        .await
        .unwrap();
    let token = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    Setup {
        fx,
        base,
        wt,
        delivery,
        token,
    }
}

/// 在 `base` 的独立克隆里写文件，得到补丁与独立 tree。
fn patch_on(fx: &Fixture, base: GitOid, files: &[(&str, &str)]) -> (Vec<u8>, GitOid) {
    let tmp = TempDir::new("commit-edit");
    git(
        tmp.path(),
        &["clone", "-q", fx.mirror_dir().to_str().unwrap(), "c"],
    );
    let dir = tmp.path().join("c");
    git(&dir, &["checkout", "-q", &base.to_string()]);
    for (path, content) in files {
        write(&dir, path, content);
    }
    git(&dir, &["add", "-A"]);
    let patch = git(&dir, &["diff", "--cached", "--full-index", "HEAD"]);
    let tree = GitOid::parse(git(&dir, &["write-tree"]).trim()).unwrap();
    (patch.into_bytes(), tree)
}

impl Setup {
    async fn validated(&self, files: &[(&str, &str)]) -> (AttemptId, GitOid) {
        self.validated_on(self.base, files).await
    }

    async fn validated_on(&self, base: GitOid, files: &[(&str, &str)]) -> (AttemptId, GitOid) {
        let (bytes, tree) = patch_on(&self.fx, base, files);
        let job = self.fx.ids.next().unwrap();
        let digest = self.fx.cas.put(&bytes, CasKind::Artifact).await.unwrap();
        let outcome = self
            .fx
            .repo
            .validate_patch(
                &self.fx.ctx(),
                ValidatePatch {
                    project_id: self.fx.project,
                    delivery_id: self.delivery,
                    job_id: job,
                    worktree_id: Some(self.wt.id),
                    base_sha: base,
                    patch: digest,
                    declared: DeclaredPatch {
                        sha256: digest,
                        bytes: bytes.len() as u64,
                        base_sha: base,
                        files_changed: files.len() as u32,
                    },
                    job_paths: PathConstraints {
                        allow: vec!["**".into()],
                        deny: Vec::new(),
                        approval_required: Vec::new(),
                    },
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, PatchOutcome::Validated(ref p) if p.result_tree == Some(tree)),
            "{outcome:?}"
        );
        (job, tree)
    }

    fn commit_req(&self, job: AttemptId, message: CommitMessage) -> CommitPatch {
        CommitPatch {
            project_id: self.fx.project,
            job_id: job,
            worktree_id: self.wt.id,
            lock: self.token.clone(),
            approval: None,
            message,
            timestamp: Timestamp::from_millis(COLLECTED_AT),
        }
    }

    async fn commit(&self, job: AttemptId) -> Result<gantry_core::ports::CommitRecord, RepoError> {
        self.fx
            .repo
            .commit_patch(
                &self.fx.ctx(),
                self.commit_req(job, message("feat: add file", "")),
            )
            .await
    }

    fn cat(&self, oid: &GitOid) -> String {
        git(
            &self.fx.mirror_dir(),
            &["cat-file", "commit", &oid.to_string()],
        )
    }
}

fn message(subject: &str, body: &str) -> CommitMessage {
    CommitMessage {
        subject: subject.into(),
        body: body.into(),
    }
}

#[tokio::test]
async fn commit_uses_base_tree_identity_timestamp_and_trailer() {
    let s = setup("commit-basic").await;
    let (job, tree) = s.validated(&[("src/new.rs", "pub fn two() {}\n")]).await;
    let record = s.commit(job).await.unwrap();
    assert_eq!(record.parent, s.base);
    assert_eq!(record.tree_oid, tree);
    assert_eq!(record.job_id, job);
    assert_eq!(record.worktree_id, s.wt.id);

    let raw = s.cat(&record.commit_oid);
    assert!(raw.contains(&format!("tree {tree}\nparent {}\n", s.base)));
    let stamp = COLLECTED_AT / 1000;
    let who = "gantry-app[bot] <123+gantry-app[bot]@users.noreply.github.com>";
    assert!(
        raw.contains(&format!("author {who} {stamp} +0000")),
        "{raw}"
    );
    assert!(
        raw.contains(&format!("committer {who} {stamp} +0000")),
        "{raw}"
    );
    assert!(
        raw.ends_with(&format!("\n\nGantry-Attempt: {job}\n")),
        "{raw}"
    );
    let info =
        s.fx.repo
            .commit_info(&s.fx.project, &record.commit_oid)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        info.trailers,
        vec![("Gantry-Attempt".into(), job.to_string())]
    );
    assert_eq!(
        s.fx.repo.branch_head(&s.wt.id).await.unwrap(),
        record.commit_oid
    );
    assert!(refs(&s.fx.mirror_dir())
        .contains(&format!("refs/gantry/attempt/{job} {}", record.commit_oid)));
}

#[tokio::test]
async fn missing_commit_identity_creates_nothing() {
    let fx = Fixture::new("commit-noid").await;
    let base = fx.seed().await;
    let delivery = fx.delivery();
    let wt = fx
        .repo
        .create_worktree(
            &fx.ctx(),
            CreateWorktree {
                project_id: fx.project,
                delivery_id: delivery,
                branch: BranchName::new("gantry/noid"),
                base_ref: "main".into(),
                base_sha: base,
            },
        )
        .await
        .unwrap();
    let token = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    let s = Setup {
        fx,
        base,
        wt,
        delivery,
        token,
    };
    let (job, _) = s.validated(&[("a.txt", "a\n")]).await;
    let before = refs(&s.fx.mirror_dir());
    assert!(matches!(
        s.commit(job).await,
        Err(RepoError::CommitIdentityMissing)
    ));
    assert_eq!(refs(&s.fx.mirror_dir()), before);
}

#[tokio::test]
async fn commit_message_is_sanitized_and_secrets_fall_back_to_template() {
    let s = setup("commit-msg").await;
    let (job, _) = s.validated(&[("a.txt", "a\n")]).await;
    let body = "Body line @alice owner/repo#12\nFixes #3\nCo-authored-by: Mallory <m@x>\nSigned-off-by: Mallory\nGantry-Attempt: att_forged\nnul\u{0}and\u{202e}bidi";
    let record =
        s.fx.repo
            .commit_patch(
                &s.fx.ctx(),
                s.commit_req(job, message("feat: <b>x</b>", body)),
            )
            .await
            .unwrap();
    let raw = s.cat(&record.commit_oid);
    let text = raw.split_once("\n\n").unwrap().1;
    assert!(text.starts_with("feat: x\n\n"), "{text:?}");
    for forged in [
        "Co-authored-by",
        "Signed-off-by",
        "att_forged",
        "\u{202e}",
        "\u{0}",
    ] {
        assert!(!text.contains(forged), "{forged:?} survived: {text:?}");
    }
    for neutral in ["`@alice`", "`owner/repo#12`", "`Fixes #3`"] {
        assert!(text.contains(neutral), "{neutral} missing: {text:?}");
    }
    assert_eq!(text.matches("Gantry-Attempt:").count(), 1);

    s.fx.policy
        .script_secret_token("AKIAEXAMPLEKEY1234567", "aws_access_key");
    let (job, _) = s.validated_on(record.commit_oid, &[("b.txt", "b\n")]).await;
    let leaked =
        s.fx.repo
            .commit_patch(
                &s.fx.ctx(),
                s.commit_req(job, message("leak AKIAEXAMPLEKEY1234567", "")),
            )
            .await
            .unwrap();
    let raw = s.cat(&leaked.commit_oid);
    assert!(!raw.contains("AKIAEXAMPLEKEY1234567"));
    assert!(raw.contains(&format!("Apply validated patch for {job}")));
}

#[tokio::test]
async fn crash_between_commit_tree_and_update_ref_replays_to_the_same_oid() {
    let s = setup("commit-crash").await;
    let (job, _) = s.validated(&[("a.txt", "a\n")]).await;
    let first = s.commit(job).await.unwrap();
    let mirror = s.fx.mirror_dir();
    let objects_before = git(&mirror, &["count-objects", "-v"]);

    // 崩溃点 1：commit-tree 已写对象，两次 update-ref 都没做。
    git(
        &mirror,
        &["update-ref", "-d", &format!("refs/gantry/attempt/{job}")],
    );
    git(
        &mirror,
        &[
            "update-ref",
            &format!("refs/gantry/wt/{}", s.wt.id),
            &s.base.to_string(),
        ],
    );
    let replay = s.commit(job).await.unwrap();
    assert_eq!(replay, first);
    assert_eq!(
        s.fx.repo.branch_head(&s.wt.id).await.unwrap(),
        first.commit_oid
    );
    assert!(refs(&mirror).contains(&format!("refs/gantry/attempt/{job} {}", first.commit_oid)));

    // 崩溃点 2：锚点已建，分支头没推进。
    git(
        &mirror,
        &[
            "update-ref",
            &format!("refs/gantry/wt/{}", s.wt.id),
            &s.base.to_string(),
        ],
    );
    let replay = s.commit(job).await.unwrap();
    assert_eq!(replay, first);
    assert_eq!(
        s.fx.repo.branch_head(&s.wt.id).await.unwrap(),
        first.commit_oid
    );

    // 正常重放：什么都不变，没有第二个提交。
    let before = refs(&mirror);
    assert_eq!(s.commit(job).await.unwrap(), first);
    assert_eq!(refs(&mirror), before);
    let count = git(
        &mirror,
        &["rev-list", "--count", &first.commit_oid.to_string()],
    );
    assert_eq!(count.trim(), "2", "base + one commit");
    assert_eq!(git(&mirror, &["count-objects", "-v"]), objects_before);
}

#[tokio::test]
async fn moved_branch_head_is_base_moved() {
    let s = setup("commit-moved").await;
    let (first_job, _) = s.validated(&[("a.txt", "a\n")]).await;
    let (second_job, _) = s.validated(&[("b.txt", "b\n")]).await;
    let first = s.commit(first_job).await.unwrap();
    let before = refs(&s.fx.mirror_dir());
    assert_eq!(
        s.commit(second_job).await,
        Err(RepoError::BaseMoved {
            base_sha: s.base,
            branch_head: first.commit_oid
        })
    );
    assert_eq!(refs(&s.fx.mirror_dir()), before);
}

#[tokio::test]
async fn lock_is_required_and_fenced() {
    let s = setup("commit-locks").await;
    let (job, _) = s.validated(&[("a.txt", "a\n")]).await;
    let before = refs(&s.fx.mirror_dir());

    // 无锁。
    s.fx.repo.release_lock(&s.fx.ctx(), &s.token).await.unwrap();
    assert!(matches!(s.commit(job).await, Err(RepoError::LockRequired)));

    // human 持锁：engine 提交得到 WorktreeLocked。
    s.fx.repo
        .acquire_human(&s.fx.ctx(), &s.wt.id, Duration::from_secs(600))
        .await
        .unwrap();
    assert!(matches!(
        s.commit(job).await,
        Err(RepoError::WorktreeLocked { lock }) if lock.holder == LockHolder::Human
    ));
    s.fx.repo
        .release_human(&s.fx.ctx(), &s.wt.id)
        .await
        .unwrap();

    // 围栏：旧持有者租约过期、新持有者取锁后，旧令牌 StaleLock。
    let old =
        s.fx.repo
            .acquire_lock(
                &s.fx.ctx(),
                &s.wt.id,
                LockHolder::Engine,
                &s.delivery,
                LEASE,
            )
            .await
            .unwrap();
    s.fx.clock.advance_secs(901);
    s.fx.repo
        .acquire_lock(
            &s.fx.ctx(),
            &s.wt.id,
            LockHolder::Guardian,
            &s.delivery,
            LEASE,
        )
        .await
        .unwrap();
    let mut req = s.commit_req(job, message("x", ""));
    req.lock = old;
    assert!(matches!(
        s.fx.repo.commit_patch(&s.fx.ctx(), req).await,
        Err(RepoError::StaleLock)
    ));
    assert_eq!(refs(&s.fx.mirror_dir()), before, "no ref or object changes");
}

#[tokio::test]
async fn expired_lease_cannot_commit() {
    let s = setup("commit-expired").await;
    let (job, _) = s.validated(&[("a.txt", "a\n")]).await;
    s.fx.clock.advance_secs(900);
    assert!(matches!(s.commit(job).await, Err(RepoError::StaleLock)));
}

#[tokio::test]
async fn only_allowed_verdicts_are_committable() {
    let s = setup("commit-verdicts").await;
    let (bytes, _) = patch_on(&s.fx, s.base, &[(".github/workflows/ci.yml", "on: push\n")]);
    for (verdict, want) in [
        (PatchVerdict::Deny, PatchVerdict::Deny),
        (PatchVerdict::NeedsApproval, PatchVerdict::NeedsApproval),
    ] {
        let job: AttemptId = s.fx.ids.next().unwrap();
        s.fx.policy.script_patch_verdict(
            job,
            PatchPolicyVerdict {
                verdict,
                reasons: vec![PatchReason {
                    code: PolicyCode::new("policy.path_protected:workflows"),
                    path: None,
                    rule_id: Some("workflows".into()),
                }],
                matched_approval_rules: Vec::new(),
                report: None,
                config_rev: Seq::ZERO,
            },
        );
        let digest = s.fx.cas.put(&bytes, CasKind::Artifact).await.unwrap();
        s.fx.repo
            .validate_patch(
                &s.fx.ctx(),
                ValidatePatch {
                    project_id: s.fx.project,
                    delivery_id: s.delivery,
                    job_id: job,
                    worktree_id: Some(s.wt.id),
                    base_sha: s.base,
                    patch: digest,
                    declared: DeclaredPatch {
                        sha256: digest,
                        bytes: bytes.len() as u64,
                        base_sha: s.base,
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
            .unwrap();
        // NeedsApproval 的 held 路径是 M2；M1 两者都不可提交。
        assert_eq!(
            s.commit(job).await,
            Err(RepoError::VerdictNotCommittable(want))
        );
    }
    let unknown: AttemptId = s.fx.ids.next().unwrap();
    assert_eq!(s.commit(unknown).await, Err(RepoError::PatchRecordMissing));
}

#[tokio::test]
async fn provenance_only_vouches_for_our_commits() {
    let mut s = setup("commit-provenance").await;
    let (job, _) = s.validated(&[("a.txt", "a\n")]).await;
    let record = s.commit(job).await.unwrap();
    assert_eq!(
        s.fx.repo
            .provenance(&s.fx.project, &record.commit_oid)
            .await
            .unwrap(),
        Some(record.clone())
    );

    // GitHub 来源的提交，说明里伪造了 trailer。
    let upstream = s.fx.upstream();
    write(&upstream, "forged.txt", "x\n");
    git(&upstream, &["add", "-A"]);
    git(
        &upstream,
        &[
            "commit",
            "-q",
            "-m",
            &format!("forged\n\nGantry-Attempt: {job}"),
        ],
    );
    let forged = s.fx.push_upstream(&upstream);
    assert_eq!(
        s.fx.repo.provenance(&s.fx.project, &forged).await.unwrap(),
        None
    );
    assert_eq!(
        s.fx.repo.provenance(&s.fx.project, &s.base).await.unwrap(),
        None
    );

    s.fx.reopen().await;
    assert_eq!(
        s.fx.repo
            .provenance(&s.fx.project, &record.commit_oid)
            .await
            .unwrap(),
        Some(record)
    );
}

#[tokio::test]
async fn rebuilt_mirror_replays_to_the_same_commit() {
    let s = setup("commit-rebuild").await;
    let (job, _) = s.validated(&[("a.txt", "a\n")]).await;
    let original = s.commit(job).await.unwrap();

    // 损坏：gc 打包后删掉 pack。
    s.fx.repo.gc(&s.fx.project).await.unwrap();
    for entry in std::fs::read_dir(s.fx.mirror_dir().join("objects/pack"))
        .unwrap()
        .flatten()
    {
        if entry.path().extension().is_some_and(|ext| ext == "pack") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    assert!(matches!(
        s.fx.repo
            .check_mirror(&s.fx.ctx(), &s.fx.project)
            .await
            .unwrap(),
        MirrorHealth::NeedsRebuild { .. }
    ));

    // 重建：旧目录改名保留，forge 重新拉取（这里由测试推送上游代替）。
    s.fx.repo.rebuild_mirror(&s.fx.project).await.unwrap();
    s.fx.push_upstream(&s.fx.upstream());
    assert_eq!(
        s.fx.repo
            .check_mirror(&s.fx.ctx(), &s.fx.project)
            .await
            .unwrap(),
        MirrorHealth::Healthy
    );
    let aside: Vec<_> = std::fs::read_dir(s.fx.mirror_dir().parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".broken-"))
        .collect();
    assert_eq!(aside.len(), 1, "old mirror is kept aside");

    // engine 以相同输入重放：tree 由 CAS 补丁重放，得到同一个提交。
    let replay = s.commit(job).await.unwrap();
    assert_eq!(replay.commit_oid, original.commit_oid);
    assert_eq!(
        s.fx.repo.branch_head(&s.wt.id).await.unwrap(),
        original.commit_oid
    );
}
