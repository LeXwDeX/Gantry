//! worker.md §12.C：补丁生成（A5，M1）。用真实 `git` 在临时仓库上运行。
//! 沙盒是 Linux；用到文件模式与符号链接，只在 unix 上编译。
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{TempDir, JOB_SPEC};
use gantry_core::canon::sha256;
use gantry_core::canon::GitOid;
use gantry_core::worker::{JobSpec, PatchMode};
use gantry_worker::events::WarnCode;
use gantry_worker::patch::{package_patch, CleanGit, Git, PatchError, PatchOutcome, GIT_TIMEOUT};

/// 测试自己的 git（模拟仓库准备方与 agent）；与被测代码无关，只隔离全局配置。
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn blob_id(dir: &Path, content: &str) -> String {
    let path = dir.join("..").join("blob-input");
    fs::write(&path, content).unwrap();
    git(dir, &["hash-object", path.to_str().unwrap()])
}

struct Repo {
    root: TempDir,
    work: PathBuf,
    base: GitOid,
}

impl Repo {
    /// 仓库准备方：提交基线文件，HEAD 即 `base_sha`。
    fn new(files: &[(&str, &str)]) -> Self {
        let root = TempDir::new("patch");
        let work = root.path().join("work");
        fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q"]);
        for (path, content) in files {
            write(&work, path, content);
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "base"]);
        let base = git(&work, &["rev-parse", "HEAD"]).parse().unwrap();
        Self { root, work, base }
    }

    fn job(&self) -> JobSpec {
        let mut job: JobSpec = serde_json::from_str(JOB_SPEC).unwrap();
        job.workspace.base_sha = self.base;
        job
    }

    /// 每次用新的 scratch 与 out 目录生成一次补丁。
    fn package(&self, job: &JobSpec, tag: &str) -> (PatchOutcome, Option<Vec<u8>>) {
        let scratch = self.root.path().join(format!("scratch-{tag}"));
        let out = self.root.path().join(format!("out-{tag}"));
        fs::create_dir_all(&scratch).unwrap();
        fs::create_dir_all(&out).unwrap();
        let mut runner = CleanGit::prepare(&self.work, &scratch, GIT_TIMEOUT).unwrap();
        let outcome = package_patch(job, &mut runner, &out).unwrap();
        let bytes = fs::read(out.join("changes.patch")).ok();
        (outcome, bytes)
    }
}

fn write(work: &Path, path: &str, content: &str) {
    let path = work.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn golden_patch_for_two_changed_files() {
    let repo = Repo::new(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
    // agent：改一个、加一个。
    write(&repo.work, "a.txt", "one\nmore\n");
    write(&repo.work, "c.txt", "new\n");
    let (outcome, bytes) = repo.package(&repo.job(), "golden");
    let expected = format!(
        "diff --git a/a.txt b/a.txt\n\
         index {}..{} 100644\n\
         --- a/a.txt\n\
         +++ b/a.txt\n\
         @@ -1 +1,2 @@\n \
         one\n\
         +more\n\
         diff --git a/c.txt b/c.txt\n\
         new file mode 100644\n\
         index 0000000000000000000000000000000000000000..{}\n\
         --- /dev/null\n\
         +++ b/c.txt\n\
         @@ -0,0 +1 @@\n\
         +new\n",
        blob_id(&repo.work, "one\n"),
        blob_id(&repo.work, "one\nmore\n"),
        blob_id(&repo.work, "new\n"),
    );
    let bytes = bytes.unwrap();
    assert_eq!(String::from_utf8(bytes.clone()).unwrap(), expected);
    let artifact = outcome.artifact.unwrap();
    assert_eq!(artifact.path, "changes.patch");
    assert_eq!(artifact.sha256, sha256(&bytes));
    assert_eq!(artifact.bytes, bytes.len() as u64);
    assert_eq!(artifact.base_sha, repo.base);
    assert_eq!(artifact.files_changed, 2);
    assert!(outcome.warnings.is_empty());
}

#[test]
fn patch_is_relative_to_base_regardless_of_agent_history() {
    let repo = Repo::new(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
    write(&repo.work, "a.txt", "changed\n");
    fs::remove_file(repo.work.join("b.txt")).unwrap();
    write(&repo.work, "d/e.txt", "e\n");
    let (_, before) = repo.package(&repo.job(), "before");
    // agent 自己提交、改写历史、切分支、硬重置到自己的提交：树不变，补丁不变。
    git(&repo.work, &["add", "-A"]);
    git(&repo.work, &["commit", "-q", "-m", "agent"]);
    git(&repo.work, &["commit", "-q", "--amend", "-m", "rewritten"]);
    git(&repo.work, &["checkout", "-q", "-b", "elsewhere"]);
    git(&repo.work, &["reset", "-q", "--hard", "HEAD"]);
    let (outcome, after) = repo.package(&repo.job(), "after");
    assert_eq!(before, after);
    assert_eq!(outcome.artifact.unwrap().files_changed, 3);
    // 确定性：同一棵树再生成一次，字节相同。
    let (_, again) = repo.package(&repo.job(), "again");
    assert_eq!(after, again);
}

#[test]
fn renames_modes_crlf_binary_symlink_and_gitlink() {
    let repo = Repo::new(&[
        ("old.txt", "keep\n"),
        ("run.sh", "echo\n"),
        ("crlf.txt", "a\r\nb\r\n"),
    ]);
    fs::rename(repo.work.join("old.txt"), repo.work.join("new.txt")).unwrap();
    fs::set_permissions(repo.work.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    write(&repo.work, "crlf.txt", "a\r\nB\r\n");
    fs::write(repo.work.join("bin.dat"), [0u8, 159, 146, 150, 0, 1]).unwrap();
    std::os::unix::fs::symlink("new.txt", repo.work.join("link")).unwrap();
    // 嵌套仓库 → gitlink。
    let sub = repo.work.join("sub");
    fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q"]);
    write(&sub, "x", "x\n");
    git(&sub, &["add", "-A"]);
    git(&sub, &["commit", "-q", "-m", "sub"]);

    let (outcome, bytes) = repo.package(&repo.job(), "kinds");
    let text = String::from_utf8_lossy(&bytes.unwrap()).into_owned();
    // --no-renames：删除 + 新增。
    assert!(text.contains("diff --git a/old.txt b/old.txt\ndeleted file mode 100644"));
    assert!(text.contains("diff --git a/new.txt b/new.txt\nnew file mode 100644"));
    assert!(!text.contains("rename from"));
    assert!(text.contains("old mode 100644\nnew mode 100755"));
    assert!(text.contains("-B\r\n") || text.contains("+B\r\n"));
    assert!(text.contains("Binary files /dev/null and b/bin.dat differ"));
    assert!(!text.contains("GIT binary patch"));
    assert!(text.contains("new file mode 120000"));
    assert!(text.contains("new file mode 160000"));
    for code in [
        WarnCode::BinaryDetected,
        WarnCode::SymlinkDetected,
        WarnCode::GitlinkDetected,
    ] {
        assert!(outcome.warnings.contains(&code), "{code:?}");
    }
}

/// 工作区 `.git` 的配置、include、hooks 与 `.gitattributes` 引用的驱动都不被执行。
#[test]
fn hostile_dot_git_is_never_executed() {
    let repo = Repo::new(&[("a.txt", "one\n")]);
    let marker = repo.root.path().join("marker");
    let script = repo.root.path().join("evil.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let s = script.display();
    let included = repo.root.path().join("included.cfg");
    fs::write(
        &included,
        format!("[filter \"inc\"]\n\tclean = {s}\n\tsmudge = {s}\n\trequired = true\n[diff \"inc\"]\n\ttextconv = {s}\n\tcommand = {s}\n"),
    )
    .unwrap();
    let hooks = repo.root.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let dot_git = repo.work.join(".git");
    for dir in [hooks.clone(), dot_git.join("hooks")] {
        for name in [
            "pre-commit",
            "post-index-change",
            "post-checkout",
            "fsmonitor-watchman",
        ] {
            fs::copy(&script, dir.join(name)).unwrap();
        }
    }
    let mut config = fs::read_to_string(dot_git.join("config")).unwrap();
    config.push_str(&format!(
        "[core]\n\tfsmonitor = {s}\n\thooksPath = {}\n\tattributesFile = {}\n\
         [include]\n\tpath = {}\n\
         [diff]\n\texternal = {s}\n\
         [filter \"x\"]\n\tclean = {s}\n\tsmudge = {s}\n\trequired = true\n\
         [diff \"y\"]\n\ttextconv = {s}\n\tcommand = {s}\n",
        hooks.display(),
        repo.root.path().join("attrs").display(),
        included.display()
    ));
    fs::write(dot_git.join("config"), config).unwrap();
    fs::write(repo.root.path().join("attrs"), "* filter=inc diff=inc\n").unwrap();
    fs::create_dir_all(dot_git.join("info")).unwrap();
    fs::write(
        dot_git.join("info").join("attributes"),
        "* filter=x diff=y\n",
    )
    .unwrap();
    // agent 的改动里也带上引用驱动的 .gitattributes。
    write(
        &repo.work,
        ".gitattributes",
        "* filter=x diff=y\n*.txt filter=inc diff=inc\n",
    );
    write(&repo.work, "a.txt", "two\n");

    let (outcome, bytes) = repo.package(&repo.job(), "hostile");
    assert!(
        !marker.exists(),
        "a hostile git driver or hook was executed"
    );
    let text = String::from_utf8(bytes.unwrap()).unwrap();
    assert!(text.contains("+two\n"), "{text}");
    assert!(text.contains("diff --git a/.gitattributes b/.gitattributes"));
    assert_eq!(outcome.artifact.unwrap().files_changed, 2);

    // 反向对照：直接在工作区上跑 git（读取它自己的 .git/config），驱动确实会被执行。
    git(&repo.work, &["add", "-A"]);
    assert!(
        marker.exists(),
        "control: plain git should run the hostile filter"
    );
}

#[test]
fn empty_change_produces_no_file() {
    let repo = Repo::new(&[("a.txt", "one\n")]);
    let (outcome, bytes) = repo.package(&repo.job(), "empty");
    assert_eq!(outcome.artifact, None);
    assert!(outcome.warnings.is_empty());
    assert!(bytes.is_none());
}

#[test]
fn oversize_patch_only_warns() {
    let repo = Repo::new(&[("a.txt", "one\n")]);
    write(&repo.work, "a.txt", "two\n");
    let mut job = repo.job();
    job.constraints.patch.as_mut().unwrap().max_files = 0;
    let (outcome, bytes) = repo.package(&job, "large");
    assert!(bytes.is_some());
    assert_eq!(outcome.warnings, [WarnCode::PatchTooLarge]);
}

#[derive(Default)]
struct RecordingGit(Vec<Vec<String>>);

impl Git for RecordingGit {
    fn run(&mut self, _step: &'static str, args: &[&str]) -> Result<Vec<u8>, PatchError> {
        self.0.push(args.iter().map(|a| a.to_string()).collect());
        Ok(Vec::new())
    }
}

#[test]
fn forbidden_patch_runs_no_git_at_all() {
    let mut job: JobSpec = serde_json::from_str(JOB_SPEC).unwrap();
    job.output.patch = PatchMode::Forbidden;
    let out = TempDir::new("forbidden");
    let mut git = RecordingGit::default();
    let outcome = package_patch(&job, &mut git, out.path()).unwrap();
    assert_eq!(outcome.artifact, None);
    assert!(git.0.is_empty(), "{:?}", git.0);

    // 对照：允许补丁时会 read-tree / add。
    let mut job: JobSpec = serde_json::from_str(JOB_SPEC).unwrap();
    job.output.patch = PatchMode::Required;
    let mut git = RecordingGit::default();
    package_patch(&job, &mut git, out.path()).unwrap();
    assert_eq!(git.0[1][0], "add");
}

#[test]
fn workspace_without_objects_is_an_error() {
    let root = TempDir::new("noobj");
    assert!(matches!(
        CleanGit::prepare(root.path(), root.path(), GIT_TIMEOUT),
        Err(PatchError::Setup(_))
    ));
}
