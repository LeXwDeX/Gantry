//! 集成测试支撑：临时数据目录、共享假时钟的内存 Store、用真实 git 造镜像仓内容。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind};
use gantry_core::events::{Event, EventType};
use gantry_core::ids::{DeliveryId, IdGen, ProjectId};
use gantry_core::ports::{ActorCtx, RepoService};
use gantry_repo::{GitRepo, RepoConfig, RepoDeps};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{EventFilter, Store, StoreDeps};
use serde_json::Value;

pub const T0: i64 = 1_800_000_000_000;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 测试结束时删除的临时目录。
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("gantry-repo-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        // macOS 的 temp_dir 经过 /var → /private/var 符号链接；取规范路径便于比较。
        Self(path.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Fixture {
    pub tmp: TempDir,
    pub clock: Arc<FakeClock>,
    pub ids: Arc<IdGen>,
    pub store: Store,
    pub repo: GitRepo,
    pub project: ProjectId,
    pub slug: RepoSlug,
}

impl Fixture {
    pub async fn new(label: &str) -> Self {
        Self::with_config(label, |_| {}).await
    }

    pub async fn with_config(label: &str, tweak: impl FnOnce(&mut RepoConfig)) -> Self {
        let tmp = TempDir::new(label);
        let clock = Arc::new(FakeClock::new(T0));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
        let store = Store::open_in_memory(
            StoreDeps {
                clock: clock.clone(),
                ids: ids.clone(),
            },
            &[],
        )
        .await
        .unwrap();
        let mut cfg = RepoConfig::new(tmp.path().join("data"));
        tweak(&mut cfg);
        let repo = GitRepo::open(
            cfg,
            RepoDeps {
                store: store.clone(),
                clock: clock.clone(),
                ids: ids.clone(),
            },
        )
        .await
        .unwrap();
        let project = ids.next().unwrap();
        Self {
            tmp,
            clock,
            ids,
            store,
            repo,
            project,
            slug: RepoSlug::parse("acme/widgets").unwrap(),
        }
    }

    /// 模拟服务端重启：同一 Store、同一数据目录上重新 open。
    pub async fn reopen(&mut self) {
        let cfg = self.repo.config().clone();
        self.repo = GitRepo::open(
            cfg,
            RepoDeps {
                store: self.store.clone(),
                clock: self.clock.clone(),
                ids: self.ids.clone(),
            },
        )
        .await
        .unwrap();
    }

    pub fn data_dir(&self) -> PathBuf {
        self.repo.config().data_dir.clone()
    }

    pub fn ctx(&self) -> ActorCtx {
        ActorCtx {
            actor: Actor::new(ActorKind::Engine, "engine"),
            device: None,
            role: None,
            client_profile: None,
            request_id: self.ids.next_ulid().unwrap(),
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        }
    }

    pub fn delivery(&self) -> DeliveryId {
        self.ids.next().unwrap()
    }

    pub fn mirror_dir(&self) -> PathBuf {
        self.data_dir().join("mirrors/acme/widgets.git")
    }

    /// 建镜像仓并推入一个上游分支（模拟 forge 的 fetch），返回其头提交。
    pub async fn seed(&self) -> GitOid {
        self.repo
            .ensure_mirror(&self.ctx(), &self.project, &self.slug)
            .await
            .unwrap();
        let upstream = self.upstream();
        write(&upstream, "README.md", "hello\n");
        write(&upstream, "src/lib.rs", "pub fn one() -> u32 { 1 }\n");
        commit_all(&upstream, "initial");
        self.push_upstream(&upstream)
    }

    /// 上游工作目录（相当于 GitHub 上的仓库），首次调用时初始化。
    pub fn upstream(&self) -> PathBuf {
        let dir = self.tmp.path().join("upstream");
        if !dir.exists() {
            std::fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q", "-b", "main"]);
        }
        dir
    }

    pub fn push_upstream(&self, upstream: &Path) -> GitOid {
        let mirror = self.mirror_dir();
        git(
            upstream,
            &[
                "push",
                "-q",
                "--force",
                mirror.to_str().unwrap(),
                "HEAD:refs/mirror/heads/main",
            ],
        );
        head(upstream)
    }

    pub async fn events(&self, ty: EventType) -> Vec<Event<Value>> {
        let page = self
            .store
            .events()
            .scan(
                gantry_core::entities::Seq::ZERO,
                &EventFilter {
                    project_ids: None,
                    include: vec![gantry_store::TypePattern::Exact(ty)],
                    exclude: Vec::new(),
                },
                200,
            )
            .await
            .unwrap();
        page.events
    }
}

// ---------------------------------------------------------------------------
// 测试侧的 git（不经 GitCmd；只用于构造夹具与独立核对）
// ---------------------------------------------------------------------------

pub fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
    #[allow(clippy::disallowed_methods)] // 测试夹具，不属于 repo 的 git 调用
    let mut command = std::process::Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2027-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2027-01-01T00:00:00Z")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args);
    command.output().unwrap()
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

pub fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

pub fn commit_all(dir: &Path, message: &str) -> GitOid {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
    head(dir)
}

pub fn head(dir: &Path) -> GitOid {
    GitOid::parse(git(dir, &["rev-parse", "HEAD"]).trim()).unwrap()
}

/// 目录下所有 ref（`for-each-ref` 输出），用于"前后不变"断言。
pub fn refs(git_dir: &Path) -> String {
    git(
        git_dir,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )
}
