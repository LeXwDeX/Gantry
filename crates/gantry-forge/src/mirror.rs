//! 镜像仓拉取（forge.md §7.10）：`Executor` 与 `Reader` 共用的受保护 fetch。
//!
//! 只做本 crate 内部共享，不暴露成跨模块端口。`ensure_mirror` → `mirror_access(Fetch)`
//! 守卫 → `git fetch --prune`；令牌只经 [`GitRunner`] 的环境与 askpass 注入。

use std::sync::Arc;
use std::time::Duration;

use gantry_core::ids::ProjectId;
use gantry_core::ports::{
    ActorCtx, ForgeError, MirrorMode, MirrorWant, RepoError, RepoService, RevSpec,
};

use crate::auth::{TokenProvider, TokenPurpose};
use crate::config::ForgeConfig;
use crate::git::GitRunner;

pub(crate) struct MirrorSync {
    config: Arc<ForgeConfig>,
    repo: Arc<dyn RepoService>,
    tokens: Arc<dyn TokenProvider>,
    git_base: String,
    git_timeout: Duration,
}

impl MirrorSync {
    pub(crate) fn new(
        config: Arc<ForgeConfig>,
        repo: Arc<dyn RepoService>,
        tokens: Arc<dyn TokenProvider>,
        git_base: String,
        git_timeout: Duration,
    ) -> Self {
        Self {
            config,
            repo,
            tokens,
            git_base: git_base.trim_end_matches('/').to_owned(),
            git_timeout,
        }
    }

    pub(crate) async fn sync(
        &self,
        project: &ProjectId,
        want: &MirrorWant,
        ctx: &ActorCtx,
    ) -> Result<(), ForgeError> {
        let binding =
            self.config
                .projects
                .get(project)
                .ok_or_else(|| ForgeError::InvalidTarget {
                    message: "project is not bound to forge".into(),
                })?;
        let unavailable = |message: String| ForgeError::Unavailable { message };
        self.repo
            .ensure_mirror(ctx, project, &binding.project.repo)
            .await
            .map_err(|error| unavailable(format!("ensure_mirror: {error}")))?;
        // 守卫持有镜像仓共享锁，活到本函数结束：fetch 与目标校验全程挡住 gc / 重建。
        let guard = self
            .repo
            .mirror_access(ctx, project, MirrorMode::Fetch)
            .await
            .map_err(|error| unavailable(format!("mirror_access: {error}")))?;
        let token = self
            .tokens
            .token(project, TokenPurpose::Read)
            .await
            .map_err(|error| unavailable(format!("token: {error}")))?;
        let git = GitRunner::new(self.git_timeout)
            .map_err(|error| unavailable(format!("git helper: {error}")))?;
        let url = format!("{}/{}.git", self.git_base, binding.project.repo);
        let mut args = vec!["fetch", "--prune", "--no-tags", url.as_str()];
        args.extend(guard.fetch_refspecs.iter().map(String::as_str));
        let output = git
            .run(&guard.git_dir, &args, Some(&token))
            .await
            .map_err(|error| unavailable(error.to_string()))?;
        if !output.success {
            return Err(unavailable(format!(
                "git fetch failed: {}",
                first_line(&output.stderr)
            )));
        }
        match want {
            // `All` 的完整性来自 repo 给出的 refspec；forge.md §7.10 要求它覆盖 heads。
            MirrorWant::All => Ok(()),
            MirrorWant::Oid(oid) => match self.repo.has_commit(project, oid).await {
                Ok(true) => Ok(()),
                Ok(false) => Err(ForgeError::NotFound),
                Err(error) => Err(unavailable(error.to_string())),
            },
            MirrorWant::Branch(branch) => {
                let rev = RevSpec::new(format!("refs/mirror/heads/{branch}"));
                match self.repo.resolve(project, &rev).await {
                    Ok(_) => Ok(()),
                    Err(RepoError::BaseNotInMirror { .. }) => Err(ForgeError::NotFound),
                    Err(error) => Err(unavailable(error.to_string())),
                }
            }
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}
