//! repo.md §12.B（镜像仓部分）与 §12.G（gc 互斥、损坏检测）。
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{git, write, Fixture};
use gantry_core::canon::GitOid;
use gantry_core::ports::{MirrorHealth, MirrorMode, RepoError, RepoService, RevSpec};
use gantry_repo::FETCH_REFSPEC;

#[tokio::test]
async fn ensure_mirror_is_idempotent_and_registers_layout() {
    let fx = Fixture::new("mirror-idem").await;
    let first = fx
        .repo
        .ensure_mirror(&fx.ctx(), &fx.project, &fx.slug)
        .await
        .unwrap();
    let second = fx
        .repo
        .ensure_mirror(&fx.ctx(), &fx.project, &fx.slug)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.git_dir, fx.mirror_dir());
    assert_eq!(first.fetch_refspecs, vec![FETCH_REFSPEC.to_owned()]);
    assert_eq!(
        git(&first.git_dir, &["rev-parse", "--is-bare-repository"]).trim(),
        "true"
    );
}

#[tokio::test]
async fn half_created_mirror_leaves_no_product_and_restart_cleans_it() {
    let mut fx = Fixture::new("mirror-crash").await;
    // 模拟创建中途崩溃：临时目录里留下半成品，最终路径不存在。
    let tmp = fx
        .data_dir()
        .join(format!("repo-tmp/mirror-{}", fx.project));
    std::fs::create_dir_all(tmp.join("objects")).unwrap();
    std::fs::write(tmp.join("HEAD"), "garbage").unwrap();
    assert!(!fx.mirror_dir().exists());

    fx.reopen().await;
    assert!(!tmp.exists(), "restart must clear the temp area");
    assert!(matches!(
        fx.repo
            .mirror_access(&fx.ctx(), &fx.project, MirrorMode::Read)
            .await,
        Err(RepoError::MirrorMissing)
    ));
    fx.repo
        .ensure_mirror(&fx.ctx(), &fx.project, &fx.slug)
        .await
        .unwrap();
    assert_eq!(
        fx.repo.check_mirror(&fx.ctx(), &fx.project).await.unwrap(),
        MirrorHealth::Healthy
    );
}

#[tokio::test]
async fn mirror_has_empty_hooks_and_safe_config() {
    let fx = Fixture::new("mirror-config").await;
    let info = fx
        .repo
        .ensure_mirror(&fx.ctx(), &fx.project, &fx.slug)
        .await
        .unwrap();
    let hooks: Vec<_> = std::fs::read_dir(info.git_dir.join("hooks"))
        .unwrap()
        .collect();
    assert!(hooks.is_empty(), "hooks/ must be empty: {hooks:?}");

    let config = std::fs::read_to_string(info.git_dir.join("config")).unwrap();
    for line in config.lines() {
        let lower = line.to_ascii_lowercase();
        for banned in ["url", "include", "credential", "@", "token", "password"] {
            assert!(
                !lower.contains(banned),
                "config line {line:?} contains {banned}"
            );
        }
    }
    let get = |key: &str| {
        git(&info.git_dir, &["config", "--get", key])
            .trim()
            .to_owned()
    };
    assert_eq!(get("gc.auto"), "0");
    assert_eq!(get("transfer.fsckObjects"), "true");
    assert_eq!(get("fetch.fsckObjects"), "true");
}

#[tokio::test]
async fn read_only_queries_resolve_mirror_objects() {
    let fx = Fixture::new("mirror-query").await;
    let base = fx.seed().await;
    let upstream = fx.upstream();
    write(&upstream, "b.txt", "b\n");
    git(&upstream, &["add", "-A"]);
    git(
        &upstream,
        &[
            "commit",
            "-q",
            "-m",
            "second\n\nbody\n\nGantry-Attempt: att_forged\nSigned-off-by: someone",
        ],
    );
    let second = fx.push_upstream(&upstream);

    let resolved = fx
        .repo
        .resolve(&fx.project, &RevSpec::new("refs/mirror/heads/main"))
        .await
        .unwrap();
    assert_eq!(resolved, second);
    assert!(fx.repo.has_commit(&fx.project, &base).await.unwrap());

    let info = fx
        .repo
        .commit_info(&fx.project, &second)
        .await
        .unwrap()
        .expect("commit exists");
    assert_eq!(info.parents, vec![base]);
    let tree = git(&upstream, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(info.tree, GitOid::parse(tree.trim()).unwrap());
    assert!(info
        .trailers
        .contains(&("Gantry-Attempt".into(), "att_forged".into())));

    let missing = GitOid::parse("1111111111111111111111111111111111111111").unwrap();
    assert!(!fx.repo.has_commit(&fx.project, &missing).await.unwrap());
    assert_eq!(
        fx.repo.commit_info(&fx.project, &missing).await.unwrap(),
        None
    );
    for rev in ["refs/mirror/heads/nope", "--all", "main main"] {
        assert!(
            matches!(
                fx.repo.resolve(&fx.project, &RevSpec::new(rev)).await,
                Err(RepoError::BaseNotInMirror { .. })
            ),
            "{rev} must not resolve"
        );
    }
}

#[tokio::test]
async fn restart_rediscovers_mirrors() {
    let mut fx = Fixture::new("mirror-restart").await;
    let base = fx.seed().await;
    fx.reopen().await;
    assert!(fx.repo.has_commit(&fx.project, &base).await.unwrap());
    let guard = fx
        .repo
        .mirror_access(&fx.ctx(), &fx.project, MirrorMode::Fetch)
        .await
        .unwrap();
    assert_eq!(guard.git_dir, fx.mirror_dir());
}

#[tokio::test]
async fn corrupted_mirror_needs_rebuild() {
    let fx = Fixture::new("mirror-corrupt").await;
    fx.seed().await;
    assert_eq!(
        fx.repo.check_mirror(&fx.ctx(), &fx.project).await.unwrap(),
        MirrorHealth::Healthy
    );
    fx.repo.gc(&fx.project).await.unwrap();
    let pack_dir = fx.mirror_dir().join("objects/pack");
    let mut removed = 0;
    for entry in std::fs::read_dir(&pack_dir).unwrap().flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "pack") {
            std::fs::remove_file(entry.path()).unwrap();
            removed += 1;
        }
    }
    assert!(removed > 0, "gc must have produced a pack");
    assert!(matches!(
        fx.repo.check_mirror(&fx.ctx(), &fx.project).await.unwrap(),
        MirrorHealth::NeedsRebuild { .. }
    ));
    // 损坏后只允许 Fetch（交 forge 重新拉取），读 / 推送拒绝。
    assert!(matches!(
        fx.repo
            .mirror_access(&fx.ctx(), &fx.project, MirrorMode::Read)
            .await,
        Err(RepoError::MirrorNeedsRebuild)
    ));
}

#[tokio::test]
async fn gc_waits_for_mirror_guards() {
    let fx = Fixture::new("mirror-gc").await;
    fx.seed().await;
    let lease = fx
        .repo
        .mirror_access(&fx.ctx(), &fx.project, MirrorMode::Push)
        .await
        .unwrap();
    let repo = fx.repo.clone();
    let project = fx.project;
    let gc = tokio::spawn(async move { repo.gc(&project).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!gc.is_finished(), "gc must wait for the shared lease");
    // 共享锁之间互不阻塞。
    let second = fx
        .repo
        .mirror_access(&fx.ctx(), &fx.project, MirrorMode::Read)
        .await;
    assert!(second.is_ok());
    drop(second);
    drop(lease);
    tokio::time::timeout(Duration::from_secs(30), gc)
        .await
        .expect("gc finishes once leases drop")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn unknown_project_has_no_mirror() {
    let fx = Fixture::new("mirror-missing").await;
    let other = fx.ids.next().unwrap();
    assert!(matches!(
        fx.repo
            .has_commit(&other, &GitOid::parse(&"a".repeat(40)).unwrap())
            .await,
        Err(RepoError::MirrorMissing)
    ));
}
