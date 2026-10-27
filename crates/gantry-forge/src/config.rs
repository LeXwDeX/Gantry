//! 引导配置（forge.md §10）。`forge.*` 是引导配置或内部常量，不是设置（I7）。

use std::collections::{BTreeMap, BTreeSet};

use gantry_core::entities::{ForgeActionKind, Project};
use gantry_core::ids::ProjectId;

/// `forge.push.branch_prefix` 的默认值（forge-actions §11）。
pub const DEFAULT_BRANCH_PREFIX: &str = "gantry/";

/// forge-actions §9.1 的仓库权限（只列写路径会用到的项）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    ContentsWrite,
    PullRequestsWrite,
    IssuesWrite,
    ActionsWrite,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContentsWrite => "contents:write",
            Self::PullRequestsWrite => "pull_requests:write",
            Self::IssuesWrite => "issues:write",
            Self::ActionsWrite => "actions:write",
        }
    }

    /// 动作所需权限（forge-actions §3.1 "App 权限"列）。评论与标签类接口官方接受
    /// Issues 或 Pull requests 写权限之一；这里按目标类型保守取 Issues，M2 实现时细化。
    pub fn required_for(kind: ForgeActionKind) -> Self {
        use ForgeActionKind::*;
        match kind {
            PushBranch | MergePr => Self::ContentsWrite,
            CreatePr | UpdatePr | ReplyReviewThread | ResolveReviewThread | RequestReviewers => {
                Self::PullRequestsWrite
            }
            UpsertStatusComment | CreateComment | CreateIssue | UpdateIssue | CloseIssue
            | AddLabels => Self::IssuesWrite,
            RerunFailedJobs => Self::ActionsWrite,
        }
    }
}

/// 一个 Project 在 `forge` 里的绑定：实体快照 + 权限自检的结论（forge.md §7.12）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectBinding {
    pub project: Project,
    /// 安装实际获批的写权限。权限不明按缺失处理（forge.md §8 fail-closed）。
    pub granted: BTreeSet<Permission>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeConfig {
    pub branch_prefix: String,
    pub projects: BTreeMap<ProjectId, ProjectBinding>,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            branch_prefix: DEFAULT_BRANCH_PREFIX.to_owned(),
            projects: BTreeMap::new(),
        }
    }
}

impl ForgeConfig {
    pub fn with_project(mut self, binding: ProjectBinding) -> Self {
        self.projects.insert(binding.project.id, binding);
        self
    }
}
