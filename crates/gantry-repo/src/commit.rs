//! 提交生成与溯源（R6、R8，repo.md §7.5）。
//!
//! 提交是确定性的：parent = `base_sha`，tree = `result_tree`，作者 / 提交者取
//! `github.commit_identity`（P-69），日期取调用方给的时间戳，说明由本模块净化并追加
//! `Gantry-Attempt` trailer。幂等锚点是 `refs/gantry/attempt/<job_id>`（只创建不改写）；
//! trailer 可以被 GitHub 来源的提交伪造，只给人看，不当信任锚。

use std::path::Path;

use gantry_core::canon::GitOid;
use gantry_core::entities::SettingsResource;
use gantry_core::ids::{AttemptId, ProjectId, WorktreeId};
use gantry_core::ports::{CommitMessage, CommitPatch, CommitRecord, RepoError};
use gantry_core::settings::{CommitIdentity, SettingsScope};
use gantry_core::text::{normalize_untrusted, NormalizeLimits};
use gantry_core::worker::{PatchVerdict, ValidatedPatch};

use crate::git::GitCmd;
use crate::mirror::{self, io_error};
use crate::patch;
use crate::worktree::check_token;
use crate::{store_error, wt_ref, zero_oid, GitRepo};

/// 提交说明的固定 trailer。
pub const TRAILER: &str = "Gantry-Attempt";

/// 幂等锚点（repo.md §7.1）。
pub fn attempt_ref(job: &AttemptId) -> String {
    format!("refs/gantry/attempt/{job}")
}

/// 正文里会被剥离的"形似 trailer"的键（小写；另加 `gantry-*` 前缀）。
const FORGEABLE_TRAILERS: &[&str] = &[
    "co-authored-by",
    "signed-off-by",
    "reviewed-by",
    "acked-by",
    "tested-by",
    "reported-by",
    "suggested-by",
    "helped-by",
    "cc",
    "change-id",
];

/// GitHub 的关闭关键字（forge-actions §2.4）。
const CLOSING_KEYWORDS: &[&str] = &[
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

impl GitRepo {
    pub(crate) async fn commit_impl(&self, req: CommitPatch) -> Result<CommitRecord, RepoError> {
        // 1. 按 job_id 读回校验结论，不接受调用方传入的判定。
        let record = self
            .patch_record(&req.job_id)
            .await
            .filter(|record| record.project_id == req.project_id)
            .ok_or(RepoError::PatchRecordMissing)?;
        let patch = record
            .validated()
            .cloned()
            .ok_or(RepoError::PatchRecordMissing)?;
        match patch.verdict {
            PatchVerdict::Allow => {}
            // NeedsApproval 的 held 路径（审批消费）是 M2（repo.md §1.1、R7）。
            verdict => return Err(RepoError::VerdictNotCommittable(verdict)),
        }
        let result_tree = patch.result_tree.ok_or(RepoError::PatchRecordMissing)?;
        // 1a. 提交身份。
        let identity = self.commit_identity().await?;

        // 2. 锁：持有工作树表直到 ref 更新完成，期间锁状态不会变化。
        let table = self.inner.worktrees.lock().await;
        let state = table.get(&req.worktree_id)?;
        if state.worktree.project_id != req.project_id {
            return Err(RepoError::WorktreeNotFound);
        }
        check_token(state, &req.lock, self.inner.deps.clock.now())?;
        let entry = self.mirror_entry(&req.project_id)?;
        let _mirror = mirror::lock(&entry.git_dir, false).await?;
        let git_dir = entry.git_dir.as_path();

        // 3. 幂等锚点。
        let anchor = attempt_ref(&req.job_id);
        if let Some(existing) = self.read_ref(git_dir, &anchor).await? {
            let info = mirror::commit_info(&self.inner.env, git_dir, &existing)
                .await?
                .ok_or(RepoError::PatchRecordMissing)?;
            if info.tree != result_tree || info.parents != vec![patch.base_sha] {
                return Err(RepoError::PatchDigestMismatch);
            }
            // 崩溃在两次 update-ref 之间：补上分支头。
            let head = self
                .branch_head_in(git_dir, &req.worktree_id, &patch.base_sha)
                .await?;
            if head == patch.base_sha {
                self.update_ref(
                    git_dir,
                    &wt_ref(&req.worktree_id),
                    &existing,
                    &patch.base_sha,
                )
                .await?;
            }
            return Ok(self.record(&req, &patch, existing, result_tree));
        }

        // 4. P10：分支头必须仍是 base_sha，不自动 rebase。
        let head = self
            .branch_head_in(git_dir, &req.worktree_id, &patch.base_sha)
            .await?;
        if head != patch.base_sha {
            return Err(RepoError::BaseMoved {
                base_sha: patch.base_sha,
                branch_head: head,
            });
        }

        // 镜像仓重建后 tree 对象可能不在了：用 CAS 里的补丁在 base 上重放出同一个 tree。
        self.ensure_tree(&req.project_id, git_dir, &req.job_id, &patch, &result_tree)
            .await?;

        // 5–6. 净化说明，确定性 commit-tree。
        let message = self.render_message(&req.message, &req.job_id);
        let date = format!("{} +0000", req.timestamp.as_millis().div_euclid(1000));
        let out = GitCmd::new("commit-tree", git_dir)
            .args(["commit-tree", "-F", "-", "-p"])
            .arg(patch.base_sha.to_string())
            .arg(result_tree.to_string())
            .env("GIT_AUTHOR_NAME", identity.name.as_str())
            .env("GIT_AUTHOR_EMAIL", identity.email.as_str())
            .env("GIT_AUTHOR_DATE", date.as_str())
            .env("GIT_COMMITTER_NAME", identity.name.as_str())
            .env("GIT_COMMITTER_EMAIL", identity.email.as_str())
            .env("GIT_COMMITTER_DATE", date.as_str())
            .stdin(message.into_bytes())
            .run(&self.inner.env)
            .await?;
        let commit = GitOid::parse(out.stdout_str()?.trim())
            .map_err(|_| io_error("bad commit oid from commit-tree"))?;

        // 7. 先建锚点（仅创建），再比较并交换分支头。
        self.update_ref(git_dir, &anchor, &commit, &zero_oid())
            .await?;
        self.update_ref(git_dir, &wt_ref(&req.worktree_id), &commit, &patch.base_sha)
            .await?;
        drop(table);
        Ok(self.record(&req, &patch, commit, result_tree))
    }

    fn record(
        &self,
        req: &CommitPatch,
        patch: &ValidatedPatch,
        commit: GitOid,
        tree: GitOid,
    ) -> CommitRecord {
        let record = CommitRecord {
            commit_oid: commit,
            tree_oid: tree,
            parent: patch.base_sha,
            patch_sha256: patch.sha256,
            job_id: req.job_id,
            worktree_id: req.worktree_id,
        };
        self.inner
            .commits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((req.project_id, commit), record.clone());
        record
    }

    /// R8：只有本模块经 `refs/gantry/attempt/<job_id>` 创建、且对得上 `patch.validated`
    /// 的提交才有记录；镜像仓里来自 GitHub 的提交（即使伪造了 trailer）返回 `None`。
    pub(crate) async fn provenance_impl(
        &self,
        project: &ProjectId,
        oid: &GitOid,
    ) -> Result<Option<CommitRecord>, RepoError> {
        let entry = self.mirror_entry(project)?;
        let git_dir = entry.git_dir.as_path();
        let out = GitCmd::new("for-each-ref", git_dir)
            .args(["for-each-ref", "--format=%(refname)", "--points-at"])
            .arg(oid.to_string())
            .arg("refs/gantry/attempt/")
            .run(&self.inner.env)
            .await?;
        let Some(job) = out
            .stdout_str()?
            .lines()
            .filter_map(|name| name.strip_prefix("refs/gantry/attempt/"))
            .find_map(|job| job.parse::<AttemptId>().ok())
        else {
            return Ok(None);
        };
        let Some(record) = self.patch_record(&job).await else {
            return Ok(None);
        };
        let Some(patch) = record
            .validated()
            .filter(|p| p.verdict == PatchVerdict::Allow)
        else {
            return Ok(None);
        };
        let Some(info) = mirror::commit_info(&self.inner.env, git_dir, oid).await? else {
            return Ok(None);
        };
        if Some(info.tree) != patch.result_tree || info.parents != vec![patch.base_sha] {
            return Ok(None);
        }
        let known = self
            .inner
            .commits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(*project, *oid))
            .cloned();
        if let Some(known) = known {
            return Ok(Some(known));
        }
        // 重启后内存表为空：工作树由包含该提交的分支头推出。
        let Some(worktree_id) = self.worktree_containing(git_dir, oid).await? else {
            return Ok(None);
        };
        Ok(Some(CommitRecord {
            commit_oid: *oid,
            tree_oid: info.tree,
            parent: patch.base_sha,
            patch_sha256: patch.sha256,
            job_id: job,
            worktree_id,
        }))
    }

    async fn worktree_containing(
        &self,
        git_dir: &Path,
        oid: &GitOid,
    ) -> Result<Option<WorktreeId>, RepoError> {
        let out = GitCmd::new("for-each-ref", git_dir)
            .args(["for-each-ref", "--format=%(refname)", "--contains"])
            .arg(oid.to_string())
            .arg("refs/gantry/wt/")
            .run(&self.inner.env)
            .await?;
        Ok(out
            .stdout_str()?
            .lines()
            .filter_map(|name| name.strip_prefix("refs/gantry/wt/"))
            .find_map(|id| id.parse().ok()))
    }

    /// `github.commit_identity`（P-69）；未配置 → `CommitIdentityMissing`。
    async fn commit_identity(&self) -> Result<CommitIdentity, RepoError> {
        let snapshot = self
            .inner
            .deps
            .store
            .snapshot()
            .await
            .map_err(store_error)?;
        let entry = snapshot.settings_current(&SettingsScope::Global, SettingsResource::Github);
        let identity = entry
            .value
            .as_ref()
            .and_then(|value| value.get("commit_identity"))
            .filter(|value| !value.is_null())
            .ok_or(RepoError::CommitIdentityMissing)?;
        let identity: CommitIdentity = serde_json::from_value(identity.clone())
            .map_err(|_| RepoError::CommitIdentityMissing)?;
        if identity.name.trim().is_empty() || identity.email.trim().is_empty() {
            return Err(RepoError::CommitIdentityMissing);
        }
        Ok(identity)
    }

    /// 提交说明：净化后追加 `Gantry-Attempt`；命中密钥扫描 → 模板说明。
    fn render_message(&self, message: &CommitMessage, job: &AttemptId) -> String {
        let subject = sanitize_line(&message.subject);
        let body = sanitize_body(&message.body);
        let leaked = !self
            .inner
            .deps
            .policy
            .scan_text(&format!("{}\n{}", message.subject, message.body))
            .is_empty();
        let (subject, body) = if leaked || subject.is_empty() {
            (format!("Apply validated patch for {job}"), String::new())
        } else {
            (subject, body)
        };
        let mut text = subject;
        text.push_str("\n\n");
        if !body.is_empty() {
            text.push_str(&body);
            text.push_str("\n\n");
        }
        text.push_str(&format!("{TRAILER}: {job}\n"));
        text
    }

    async fn ensure_tree(
        &self,
        project: &ProjectId,
        git_dir: &Path,
        job: &AttemptId,
        patch: &ValidatedPatch,
        tree: &GitOid,
    ) -> Result<(), RepoError> {
        let present = GitCmd::new("cat-file", git_dir)
            .args(["cat-file", "-e"])
            .arg(format!("{tree}^{{tree}}"))
            .output(&self.inner.env)
            .await?;
        if present.success() {
            return Ok(());
        }
        if !mirror::has_commit(&self.inner.env, git_dir, &patch.base_sha).await? {
            return Err(RepoError::BaseNotInMirror {
                oid: patch.base_sha,
            });
        }
        let bytes = self
            .inner
            .deps
            .cas
            .get_bytes(&patch.sha256, patch::MAX_PATCH_BYTES)
            .await?
            .ok_or(RepoError::PatchRecordMissing)?;
        let dir = self.inner.cfg.scratch_dir().join(format!("replay-{job}"));
        let admin = self.worktree_admin(project);
        let result = async {
            patch::add_scratch(&self.inner.env, git_dir, &admin, &dir, &patch.base_sha).await?;
            GitCmd::new("apply", &dir)
                .args(["apply", "--index", "--whitespace=nowarn", "-p1", "-"])
                .stdin(bytes)
                .run(&self.inner.env)
                .await?;
            let out = GitCmd::new("write-tree", &dir)
                .arg("write-tree")
                .run(&self.inner.env)
                .await?;
            GitOid::parse(out.stdout_str()?.trim()).map_err(|_| io_error("bad tree oid"))
        }
        .await;
        patch::remove_scratch(&self.inner.env, git_dir, &admin, &dir).await;
        if result? != *tree {
            return Err(RepoError::PatchDigestMismatch);
        }
        Ok(())
    }

    async fn read_ref(&self, git_dir: &Path, name: &str) -> Result<Option<GitOid>, RepoError> {
        let out = GitCmd::new("rev-parse", git_dir)
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(name)
            .output(&self.inner.env)
            .await?;
        if !out.success() {
            return Ok(None);
        }
        Ok(GitOid::parse(out.stdout_str()?.trim()).ok())
    }

    async fn update_ref(
        &self,
        git_dir: &Path,
        name: &str,
        new: &GitOid,
        old: &GitOid,
    ) -> Result<(), RepoError> {
        GitCmd::new("update-ref", git_dir)
            .args(["update-ref", "--no-deref"])
            .arg(name)
            .arg(new.to_string())
            .arg(old.to_string())
            .run(&self.inner.env)
            .await?;
        Ok(())
    }

    /// 工作树分支头；ref 丢失（镜像仓重建）时在 `base` 上恢复（repo.md §7.1 末段）。
    pub(crate) async fn branch_head_in(
        &self,
        git_dir: &Path,
        id: &WorktreeId,
        base: &GitOid,
    ) -> Result<GitOid, RepoError> {
        let name = wt_ref(id);
        if let Some(head) = self.read_ref(git_dir, &name).await? {
            return Ok(head);
        }
        if !mirror::has_commit(&self.inner.env, git_dir, base).await? {
            return Err(RepoError::BaseNotInMirror { oid: *base });
        }
        self.update_ref(git_dir, &name, base, &zero_oid()).await?;
        Ok(*base)
    }
}

// ---------------------------------------------------------------------------
// 提交说明净化（forge-actions §2.4 的规则表；共享实现见 OQ-R8）
// ---------------------------------------------------------------------------

fn normalize(text: &str) -> String {
    normalize_untrusted(text.as_bytes(), NormalizeLimits::default()).text
}

/// 标题：单行、净化、最长 200 字符。
pub fn sanitize_line(text: &str) -> String {
    let line = neutralize(&strip_html(&normalize(text)).replace(['\n', '\t'], " "));
    let line = line.trim();
    line.chars().take(200).collect()
}

/// 正文：净化每一行，剥离形似 trailer 的行。
pub fn sanitize_body(text: &str) -> String {
    let cleaned = strip_html(&normalize(text));
    let lines: Vec<String> = cleaned
        .lines()
        .filter(|line| !is_forgeable_trailer(line))
        .map(|line| {
            let line = neutralize(line);
            // 行首斜杠命令：加零宽字符，防止被当成命令（forge-actions §2.4）。
            if line.trim_start().starts_with('/') {
                format!("\u{200B}{line}")
            } else {
                line
            }
        })
        .collect();
    lines.join("\n").trim().to_owned()
}

fn is_forgeable_trailer(line: &str) -> bool {
    let Some((key, _)) = line.split_once(':') else {
        return false;
    };
    let key = key.trim().to_ascii_lowercase();
    !key.is_empty()
        && !key.contains(' ')
        && (FORGEABLE_TRAILERS.contains(&key.as_str()) || key.starts_with("gantry-"))
}

/// 去掉 HTML 注释与标签（含伪造的 `<!-- gantry:… -->`）。
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = if tail.starts_with("<!--") {
            tail.find("-->").map(|i| i + 3)
        } else if tail[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!')
        {
            tail.find('>').map(|i| i + 1)
        } else {
            None
        };
        match end {
            Some(end) => rest = &tail[end..],
            None => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 中和 `@提及`、跨仓库引用、关闭关键字：用反引号包起来，不触发通知 / 回链 / 关闭。
fn neutralize(line: &str) -> String {
    let words: Vec<&str> = line.split(' ').collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        let bare = word.trim_matches([',', '.', ';', ':', '(', ')']);
        let keyword = bare.to_ascii_lowercase();
        if CLOSING_KEYWORDS.contains(&keyword.as_str())
            && words.get(i + 1).is_some_and(|next| is_issue_ref(next))
        {
            out.push(format!("`{} {}`", word, words[i + 1]));
            i += 2;
            continue;
        }
        if is_mention(bare) || is_cross_repo_ref(bare) {
            out.push(word.replace(bare, &format!("`{bare}`")));
        } else {
            out.push(word.to_owned());
        }
        i += 1;
    }
    out.join(" ")
}

fn is_mention(word: &str) -> bool {
    word.strip_prefix('@').is_some_and(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'))
    })
}

fn is_issue_ref(word: &str) -> bool {
    let word = word.trim_end_matches([',', '.', ';', ':', ')']);
    let target = word.rsplit_once('#').map(|(_, n)| n);
    target.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        || word.contains("/issues/")
        || word.contains("/pull/")
}

/// `owner/repo#N`、`owner/repo@sha`、指向 GitHub Issue / PR 的 URL。
fn is_cross_repo_ref(word: &str) -> bool {
    if word.starts_with("http://") || word.starts_with("https://") {
        return word.contains("github.com/")
            && (word.contains("/issues/") || word.contains("/pull/"));
    }
    let Some((repo, _)) = word.split_once(['#', '@']) else {
        return false;
    };
    let mut parts = repo.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forged_trailers_are_stripped() {
        let body = "real text\nCo-authored-by: evil <e@x>\nSigned-off-by: someone\nGantry-Attempt: att_fake\ngantry-delivery: dlv_x\nNote: keep me";
        assert_eq!(sanitize_body(body), "real text\nNote: keep me");
    }

    #[test]
    fn mentions_refs_and_keywords_are_neutralized() {
        let body = "ping @alice and @org/team, see other/repo#12 and x/y@abc123.\nFixes #3 and closes other/repo#4\nhttps://github.com/a/b/issues/9 /gantry approve";
        let out = sanitize_body(body);
        assert!(out.contains("`@alice`"));
        assert!(out.contains("`@org/team`"));
        assert!(out.contains("`other/repo#12`"));
        assert!(out.contains("`x/y@abc123`"));
        assert!(out.contains("`Fixes #3`"));
        assert!(out.contains("`closes other/repo#4`"));
        assert!(out.contains("`https://github.com/a/b/issues/9`"));
        assert!(!out.contains("\n/gantry"));
    }

    #[test]
    fn control_bidi_and_html_are_removed() {
        let out = sanitize_body("a\0b\u{202e}c <!-- gantry:v1 act=act_x --> <b>bold</b>\u{1b}[31m");
        assert!(!out.contains('\0') && !out.contains('\u{202e}') && !out.contains('\u{1b}'));
        assert!(!out.contains("gantry:v1") && !out.contains("<b>"));
        assert_eq!(sanitize_line("line\none"), "line one");
    }
}
