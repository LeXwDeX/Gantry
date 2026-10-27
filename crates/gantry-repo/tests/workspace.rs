//! repo.md §12.C（沙盒工作区，M1）与 §12.G（gc 独占锁时等待）。
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{commit_all, git, write, Fixture, TempDir};
use gantry_core::canon::GitOid;
use gantry_core::ports::{PrepareWorkspace, RepoError, RepoService};

fn request(fx: &Fixture, base: GitOid, dest: PathBuf) -> PrepareWorkspace {
    PrepareWorkspace {
        project_id: fx.project,
        job_id: fx.ids.next().unwrap(),
        base_sha: base,
        pr_base_sha: None,
        dest,
        max_bytes: 64 * 1024 * 1024,
        want_diff: false,
    }
}

fn dest(tmp: &TempDir, name: &str) -> PathBuf {
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 目录下所有文件（相对路径 → 内容），用于前后比对。
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    out
}

#[tokio::test]
async fn workspace_meets_worker_protocol_postconditions() {
    let fx = Fixture::new("ws-post").await;
    let base = fx.seed().await;
    // 镜像仓里另有本地提交与上游 ref，工作区里都不应出现。
    let work = dest(&fx.tmp, "ws");
    let prepared = fx
        .repo
        .prepare_workspace(&fx.ctx(), request(&fx, base, work.clone()))
        .await
        .unwrap();
    assert_eq!(prepared.work_dir, work);
    assert_eq!(prepared.head, base);
    let tree = git(&fx.upstream(), &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(prepared.tree, GitOid::parse(tree.trim()).unwrap());
    assert_eq!(prepared.diff_patch, None);
    assert!(prepared.stats.files >= 2 && prepared.stats.bytes > 0);

    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    assert_eq!(git(&work, &["rev-parse", "HEAD"]).trim(), base.to_string());
    assert!(!common::git_output(&work, &["symbolic-ref", "-q", "HEAD"])
        .status
        .success());
    let config = std::fs::read_to_string(work.join(".git/config")).unwrap();
    for banned in ["remote", "url", "include", "credential"] {
        assert!(
            !config.contains(banned),
            "config contains {banned}: {config}"
        );
    }
    let hooks = work.join(".git/hooks");
    assert!(!hooks.exists() || std::fs::read_dir(&hooks).unwrap().next().is_none());
    assert!(!work.join(".git/objects/info/alternates").exists());
    for file in files_under(&work.join(".git")) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(&file).unwrap();
        if meta.is_file() {
            assert_eq!(meta.nlink(), 1, "{} is hardlinked", file.display());
        }
    }
    git(&work, &["fsck", "--no-progress"]);
    assert_eq!(
        git(&work, &["for-each-ref", "--format=%(refname)"]),
        "",
        "no refs/mirror/* or refs/gantry/* in the workspace"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("README.md")).unwrap(),
        "hello\n"
    );
}

#[tokio::test]
async fn writing_workspace_git_files_does_not_touch_the_mirror() {
    let fx = Fixture::new("ws-isolation").await;
    let base = fx.seed().await;
    let work = dest(&fx.tmp, "ws");
    fx.repo
        .prepare_workspace(&fx.ctx(), request(&fx, base, work.clone()))
        .await
        .unwrap();
    let before = snapshot(&fx.mirror_dir());
    for file in files_under(&work.join(".git")) {
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&file, perms).unwrap();
        std::fs::write(&file, b"sandbox was here").unwrap();
    }
    assert_eq!(snapshot(&fx.mirror_dir()), before);
}

#[tokio::test]
async fn submodules_stay_gitlinks_and_lfs_pointers_are_untouched() {
    let fx = Fixture::new("ws-submodule").await;
    fx.seed().await;
    let upstream = fx.upstream();
    let pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";
    write(
        &upstream,
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    );
    write(&upstream, "assets/blob.bin", pointer);
    write(
        &upstream,
        ".gitmodules",
        "[submodule \"vendor/lib\"]\n\tpath = vendor/lib\n\turl = https://example.invalid/lib.git\n",
    );
    git(
        &upstream,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,3333333333333333333333333333333333333333,vendor/lib",
        ],
    );
    // 不用 `add -A`：工作目录里没有 vendor/lib，会把 gitlink 记成删除。
    git(
        &upstream,
        &["add", ".gitattributes", ".gitmodules", "assets"],
    );
    git(&upstream, &["commit", "-q", "-m", "submodule and lfs"]);
    let head = common::head(&upstream);
    fx.push_upstream(&upstream);

    let work = dest(&fx.tmp, "ws");
    fx.repo
        .prepare_workspace(&fx.ctx(), request(&fx, head, work.clone()))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(work.join("assets/blob.bin")).unwrap(),
        pointer
    );
    let sub = work.join("vendor/lib");
    assert!(!sub.exists() || std::fs::read_dir(&sub).unwrap().next().is_none());
    assert!(git(&work, &["ls-files", "-s", "vendor/lib"]).starts_with("160000 "));
    assert!(!work.join(".git/modules").exists());
}

#[tokio::test]
async fn missing_base_and_oversized_workspace_are_rejected_and_cleaned() {
    let fx = Fixture::new("ws-errors").await;
    let base = fx.seed().await;
    let missing = GitOid::parse(&"4".repeat(40)).unwrap();
    let work = dest(&fx.tmp, "ws-missing");
    assert!(matches!(
        fx.repo
            .prepare_workspace(&fx.ctx(), request(&fx, missing, work.clone()))
            .await,
        Err(RepoError::BaseNotInMirror { oid }) if oid == missing
    ));
    assert!(std::fs::read_dir(&work).unwrap().next().is_none());

    let work = dest(&fx.tmp, "ws-large");
    let mut req = request(&fx, base, work.clone());
    req.max_bytes = 16;
    assert!(matches!(
        fx.repo.prepare_workspace(&fx.ctx(), req).await,
        Err(RepoError::WorkspaceTooLarge { max: 16, .. })
    ));
    assert!(work.exists(), "dest itself belongs to sandbox");
    assert!(
        std::fs::read_dir(&work).unwrap().next().is_none(),
        "dest must be emptied"
    );
}

#[tokio::test]
async fn checkout_does_not_execute_repository_content() {
    let fx = Fixture::new("ws-traps").await;
    fx.seed().await;
    let marker = fx.tmp.path().join("pwned");
    // 镜像仓 config 里的 fsmonitor / filter 驱动，与仓库里的 .gitattributes 联手。
    let mirror = fx.mirror_dir();
    let touch = format!("touch {}", marker.display());
    git(&mirror, &["config", "core.fsmonitor", &touch]);
    git(&mirror, &["config", "filter.evil.smudge", &touch]);
    git(&mirror, &["config", "filter.evil.required", "true"]);
    let upstream = fx.upstream();
    write(&upstream, ".gitattributes", "* filter=evil diff=evil\n");
    write(&upstream, "x.txt", "x\n");
    let head = commit_all(&upstream, "traps");
    fx.push_upstream(&upstream);

    let work = dest(&fx.tmp, "ws");
    fx.repo
        .prepare_workspace(&fx.ctx(), request(&fx, head, work))
        .await
        .unwrap();
    fx.repo.has_commit(&fx.project, &head).await.unwrap();
    assert!(!marker.exists(), "repository content must never execute");
}

#[tokio::test]
async fn prepare_waits_while_gc_holds_the_exclusive_lock() {
    let fx = Fixture::new("ws-gc").await;
    let base = fx.seed().await;
    let exclusive = gantry_repo::mirror::lock(&fx.mirror_dir(), true)
        .await
        .unwrap();
    let repo = fx.repo.clone();
    let ctx = fx.ctx();
    let req = request(&fx, base, dest(&fx.tmp, "ws"));
    let task = tokio::spawn(async move { repo.prepare_workspace(&ctx, req).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "prepare must wait, not fail");
    drop(exclusive);
    let prepared = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(prepared.head, base);
}

#[tokio::test]
async fn diff_patch_is_not_offered_in_m1() {
    let fx = Fixture::new("ws-diff").await;
    let base = fx.seed().await;
    let work = dest(&fx.tmp, "ws");
    let mut req = request(&fx, base, work.clone());
    req.want_diff = true;
    req.pr_base_sha = Some(base);
    assert!(fx.repo.prepare_workspace(&fx.ctx(), req).await.is_err());
    assert!(std::fs::read_dir(&work).unwrap().next().is_none());
}
