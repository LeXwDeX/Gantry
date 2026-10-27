//! `ForgeReader`（forge.md §6）：读 GitHub，不写 GitHub。
//!
//! 本切片实现 `discussion`（Issue / PR 正文与评论按需拉取，供 `engine`，P-59）：结果全部是
//! `UntrustedText`（I5），受 `max_comments` / `max_bytes` 截断，不进快照、不追加任何事件——
//! 读取器根本不持有 store。截断语义与 `FakeForge` 一致（§12 D 组）：`comments_since` 严格
//! 晚于；评论按时间序累加，第一条会让累计字节超过 `max_bytes` 的评论及其后全部丢弃；
//! 标题与正文不计入评论预算，正文单独按 `max_bytes` 截断。
//!
//! shortcut: 快照类读取（`pull_request` / `issue` / `project_snapshot`、`reconcile_now`）在
//! 对账切片实现；`ci_log` / `changed_files` / `review_thread` / `check_annotations` 是 M2
//! （forge.md §1.1）；`sync_mirror` 见 [`crate::Executor::sync_mirror`]。它们目前返回
//! `Unavailable`，不静默返回空结果。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::canon::GitOid;
use gantry_core::entities::{IssueRef, PullRequestRef};
use gantry_core::ids::{Clock, ProjectId};
use gantry_core::ports::{
    ActorCtx, Annotation, ChangedFile, CiLog, CiLogQuery, Discussion, DiscussionComment,
    DiscussionQuery, DiscussionTarget, ForgeError, ForgeReader, MirrorWant, ProjectSnapshot,
    ReconcileReport, ReconcileTarget, ReviewThreadView, ThreadNodeId,
};
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use serde_json::Value;

use crate::auth::{TokenProvider, TokenPurpose};
use crate::config::ForgeConfig;
use crate::github::{ApiError, GithubClient};

/// GitHub 单页评论上限。
const PER_PAGE: u32 = 100;

pub struct ReaderDeps {
    pub tokens: Arc<dyn TokenProvider>,
    /// GitHub REST 根地址；测试里是 `FakeGitHub::base_url()`。
    pub api_base: String,
    pub clock: Arc<dyn Clock>,
    pub http_timeout: Duration,
}

pub struct Reader {
    config: Arc<ForgeConfig>,
    tokens: Arc<dyn TokenProvider>,
    github: GithubClient,
    clock: Arc<dyn Clock>,
}

impl Reader {
    pub(crate) fn new(config: Arc<ForgeConfig>, deps: ReaderDeps) -> Self {
        Self {
            config,
            tokens: deps.tokens,
            github: GithubClient::new(deps.api_base, deps.http_timeout),
            clock: deps.clock,
        }
    }

    /// 读路径用只读令牌；401 时丢弃缓存重试一次（forge.md §7.12）。
    async fn with_token<T, F, Fut>(&self, project: &ProjectId, call: F) -> Result<T, ForgeError>
    where
        F: Fn(Arc<gantry_core::secret::SecretString>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        for attempt in 0..2 {
            let token = self
                .tokens
                .token(project, TokenPurpose::Read)
                .await
                .map_err(|error| unavailable(format!("token: {error}")))?;
            match call(token).await {
                Err(ApiError::Status { status: 401, .. }) if attempt == 0 => {
                    self.tokens.invalidate(project);
                }
                other => return other.map_err(forge_error),
            }
        }
        Err(unavailable("token rejected twice".into()))
    }
}

#[async_trait]
impl ForgeReader for Reader {
    async fn pull_request(
        &self,
        _project: &ProjectId,
        _number: u64,
    ) -> Result<Option<PullRequestRef>, ForgeError> {
        Err(not_yet("pull_request"))
    }

    async fn issue(
        &self,
        _project: &ProjectId,
        _number: u64,
    ) -> Result<Option<IssueRef>, ForgeError> {
        Err(not_yet("issue"))
    }

    async fn project_snapshot(&self, _project: &ProjectId) -> Result<ProjectSnapshot, ForgeError> {
        Err(not_yet("project_snapshot"))
    }

    async fn discussion(
        &self,
        project: &ProjectId,
        target: DiscussionTarget,
        query: DiscussionQuery,
    ) -> Result<Discussion, ForgeError> {
        let binding = self
            .config
            .projects
            .get(project)
            .ok_or_else(|| invalid("project not bound"))?;
        let repo = &binding.project.repo;
        let (number, pull) = match target {
            DiscussionTarget::Issue(number) => (number, false),
            DiscussionTarget::PullRequest(number) => (number, true),
        };
        let head = self
            .with_token(project, |token| async move {
                self.github.issue_or_pull(&token, repo, number, pull).await
            })
            .await?;
        // `issues/{n}` 对 PR 也会返回对象（带 `pull_request` 字段）；目标类型不符按不存在处理。
        if !pull && head.get("pull_request").is_some() {
            return Err(ForgeError::NotFound);
        }
        let max_bytes = query.max_bytes as usize;
        let title = untrusted(str_field(&head, "title"), "github:title", usize::MAX);
        let body = query
            .include_body
            .then(|| untrusted(str_field(&head, "body"), "github:body", max_bytes));

        let since = query.comments_since.map(|ts| ts.to_string());
        let limit = query.max_comments as usize;
        let mut comments = Vec::new();
        let mut truncated = body.as_ref().is_some_and(UntrustedText::truncated);
        let mut page = 1;
        // 多取一条，用来判断是否被 max_comments 截断。
        while comments.len() <= limit {
            let since = since.as_deref();
            let items = self
                .with_token(project, |token| async move {
                    self.github
                        .list_comments(&token, repo, number, since, page, PER_PAGE)
                        .await
                })
                .await?;
            let full_page = items.len() == PER_PAGE as usize;
            for item in &items {
                let comment = comment(item)?;
                if query
                    .comments_since
                    .is_some_and(|since| comment.created_at <= since)
                {
                    continue;
                }
                comments.push(comment);
            }
            if !full_page {
                break;
            }
            page += 1;
        }
        if comments.len() > limit {
            truncated = true;
            comments.truncate(limit);
        }
        let mut bytes = 0usize;
        let mut kept = Vec::with_capacity(comments.len());
        for comment in comments {
            let len = comment.body.as_untrusted_str().len();
            if bytes.saturating_add(len) > max_bytes {
                truncated = true;
                break;
            }
            bytes += len;
            kept.push(comment);
        }
        Ok(Discussion {
            title,
            body,
            comments: kept,
            truncated,
            fetched_at: self.clock.now(),
        })
    }

    async fn ci_log(&self, _project: &ProjectId, _query: CiLogQuery) -> Result<CiLog, ForgeError> {
        Err(not_yet("ci_log"))
    }

    async fn changed_files(
        &self,
        _project: &ProjectId,
        _number: u64,
        _head_sha: &GitOid,
    ) -> Result<Vec<ChangedFile>, ForgeError> {
        Err(not_yet("changed_files"))
    }

    async fn review_thread(
        &self,
        _project: &ProjectId,
        _thread: &ThreadNodeId,
    ) -> Result<ReviewThreadView, ForgeError> {
        Err(not_yet("review_thread"))
    }

    async fn check_annotations(
        &self,
        _project: &ProjectId,
        _check_run_id: u64,
    ) -> Result<Vec<Annotation>, ForgeError> {
        Err(not_yet("check_annotations"))
    }

    async fn reconcile_now(
        &self,
        _target: ReconcileTarget,
        _ctx: &ActorCtx,
    ) -> Result<ReconcileReport, ForgeError> {
        Err(not_yet("reconcile_now"))
    }

    async fn sync_mirror(
        &self,
        _project: &ProjectId,
        _want: MirrorWant,
        _ctx: &ActorCtx,
    ) -> Result<(), ForgeError> {
        Err(not_yet("sync_mirror"))
    }
}

fn comment(item: &Value) -> Result<DiscussionComment, ForgeError> {
    let decode = |what: &str| unavailable(format!("decode comment: {what}"));
    let id = item
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| decode("id"))?;
    let created_at = item
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|raw| Timestamp::parse_rfc3339(raw).ok())
        .ok_or_else(|| decode("created_at"))?;
    Ok(DiscussionComment {
        id,
        author_login: item
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        author_association: item
            .get("author_association")
            .and_then(Value::as_str)
            .map(str::to_owned),
        created_at,
        body: untrusted(
            str_field(item, "body"),
            &format!("github:comment:{id}"),
            usize::MAX,
        ),
    })
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// 按字节截断到字符边界。
fn untrusted(text: &str, source: &str, max_bytes: usize) -> UntrustedText {
    if text.len() <= max_bytes {
        return UntrustedText::new(text.to_owned(), source.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    UntrustedText::new(text[..end].to_owned(), source.to_owned(), true)
}

fn forge_error(error: ApiError) -> ForgeError {
    match error {
        ApiError::Status { status: 404, .. } => ForgeError::NotFound,
        ApiError::Status {
            rate_limited: true,
            retry_after,
            ..
        } => ForgeError::RateLimited { retry_after },
        ApiError::Status {
            status, message, ..
        } if (400..500).contains(&status) => ForgeError::GitHub {
            message: format!("{status}: {message}"),
        },
        other => unavailable(other.to_string()),
    }
}

fn unavailable(message: String) -> ForgeError {
    ForgeError::Unavailable { message }
}

fn invalid(message: &str) -> ForgeError {
    ForgeError::InvalidTarget {
        message: message.to_owned(),
    }
}

fn not_yet(method: &str) -> ForgeError {
    unavailable(format!(
        "ForgeReader::{method} is not implemented in this slice"
    ))
}
