//! `validate_patch` 的流水线（V5，repo.md §7.4）。
//!
//! 结论以 `patch.validated` / `patch.rejected` 为索引：同一 `job_id` 只追加一次事件
//! （`dedup_key = repo:patch:<job_id>`），重复调用返回同一结论；重启时从事件重建。
//! `BaseConflict` / `NoChange` 不产生事件（P10、P11），重放时重新计算（校验是纯函数）。

use std::sync::Arc;

use gantry_core::canon::{sha256, GitOid, Sha256Digest};
use gantry_core::entities::Seq;
use gantry_core::events::{
    Event, EventType, NewEvent, PatchRejectedData, PatchValidatedData, Subject,
};
use gantry_core::ids::{AttemptId, EventId, ProjectId};
use gantry_core::ports::{
    ActorCtx, PatchOutcome, PatchPolicyVerdict, RepoError, StoreError as PortStoreError,
    ValidatePatch,
};
use gantry_core::worker::{PatchFacts, PatchRejectReason, PatchVerdict, ValidatedPatch};
use gantry_store::{Appended, EventFilter, TypePattern};
use serde_json::Value;

use crate::cas::CasKind;
use crate::git::GitCmd;
use crate::mirror::{self, io_error};
use crate::patch::{self, AddedLines, Mechanical};
use crate::{store_error, GitRepo};

/// 一个 job 的校验结论。
#[derive(Clone, Debug)]
#[allow(dead_code)] // project_id / validated / patch_record 由 commit_patch（#91 下一片）使用
pub struct PatchRecord {
    pub project_id: ProjectId,
    pub outcome: PatchOutcome,
    /// 从 `patch.rejected(policy_denied)` 重建时事件里没有 `base_sha`，取调用方的。
    pub base_known: bool,
}

impl PatchRecord {
    fn outcome_for(&self, req: &ValidatePatch) -> PatchOutcome {
        match &self.outcome {
            PatchOutcome::Validated(patch) if !self.base_known => {
                let mut patch = patch.clone();
                patch.base_sha = req.base_sha;
                PatchOutcome::Validated(patch)
            }
            other => other.clone(),
        }
    }

    /// 可提交的校验结论（`commit_patch` 用）。
    #[allow(dead_code)]
    pub fn validated(&self) -> Option<&ValidatedPatch> {
        match &self.outcome {
            PatchOutcome::Validated(patch) if self.base_known => Some(patch),
            _ => None,
        }
    }
}

/// 一次性检出里的结果；只在一次调用内短暂存在，不为变体大小装箱。
#[allow(clippy::large_enum_variant)]
enum Scratch {
    Reject(Mechanical),
    NoChange,
    Evaluated {
        facts: PatchFacts,
        verdict: PatchPolicyVerdict,
        tree: Option<GitOid>,
        denied_by: Option<EventId>,
    },
}

fn dedup_key(job: &AttemptId) -> String {
    format!("repo:patch:{job}")
}

impl GitRepo {
    pub(crate) async fn validate_impl(
        &self,
        ctx: &ActorCtx,
        req: ValidatePatch,
    ) -> Result<PatchOutcome, RepoError> {
        let job_lock = self.job_lock(&req.job_id);
        let _serial = job_lock.lock().await;
        if let Some(record) = self.inner.patches.lock().await.get(&req.job_id) {
            return Ok(record.outcome_for(&req));
        }

        let entry = self.mirror_entry(&req.project_id)?;
        let _mirror = mirror::lock(&entry.git_dir, false).await?;
        let bytes = self
            .inner
            .deps
            .cas
            .get_bytes(&req.patch, patch::MAX_PATCH_BYTES)
            .await?
            .ok_or(RepoError::Store(PortStoreError::NotFound))?;
        let digest = sha256(&bytes);

        // 1. 声明与重算比对。
        let mismatch = if digest != req.declared.sha256 || digest != req.patch {
            Some("sha256 does not match the declared digest")
        } else if bytes.len() as u64 != req.declared.bytes {
            Some("byte count does not match the declaration")
        } else if req.declared.base_sha != req.base_sha {
            Some("declared base_sha does not match the job's base_sha")
        } else {
            None
        };
        if let Some(why) = mismatch {
            let rejection = Mechanical {
                reason: PatchRejectReason::DeclaredMismatch,
                detail: why.to_owned(),
            };
            return self.reject(ctx, &req, digest, &bytes, rejection).await;
        }

        // P11：声明了补丁但内容为空。
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(PatchOutcome::NoChange);
        }

        // 2. git 预解析（不碰文件系统），应用前的机械拒绝。
        let slot = self.slot(&req.project_id);
        let permit = slot
            .acquire()
            .await
            .map_err(|_| io_error("git slots closed"))?;
        let pre = patch::preparse(&self.inner.env, &entry.git_dir, &bytes).await?;
        drop(permit);
        let pre = match pre {
            Ok(pre) => pre,
            Err(stderr) => {
                let rejection = patch::classify_headers(&bytes).unwrap_or_else(|| Mechanical {
                    reason: PatchRejectReason::NotApplicable,
                    detail: patch::sanitize(&stderr),
                });
                return self.reject(ctx, &req, digest, &bytes, rejection).await;
            }
        };
        if let Err(rejection) = patch::check_preparsed(&pre, &bytes) {
            return self.reject(ctx, &req, digest, &bytes, rejection).await;
        }
        // git 解析时会规整路径（例如引号路径里的 `\000` 被截断）；头部原文里的路径
        // 再查一遍，只用于拒绝。
        if let Some(rejection) = patch::classify_headers(&bytes) {
            return self.reject(ctx, &req, digest, &bytes, rejection).await;
        }

        // 3. P10：工作树分支头必须等于 base_sha，不自动 rebase。
        if let Some(worktree) = req.worktree_id {
            let head = self.branch_head_of(&worktree).await?;
            if head != req.base_sha {
                return Ok(PatchOutcome::BaseConflict {
                    base_sha: req.base_sha,
                    branch_head: head,
                });
            }
        }
        if !mirror::has_commit(&self.inner.env, &entry.git_dir, &req.base_sha).await? {
            return Err(RepoError::BaseNotInMirror { oid: req.base_sha });
        }
        if let Some(rejection) =
            patch::collisions_before_apply(&self.inner.env, &entry.git_dir, &req.base_sha, &pre)
                .await?
        {
            return self.reject(ctx, &req, digest, &bytes, rejection).await;
        }

        // 4–9. 一次性检出：成功、失败、超时都删除。
        let dir = self.inner.cfg.scratch_dir().join(req.job_id.to_string());
        let permit = slot
            .acquire()
            .await
            .map_err(|_| io_error("git slots closed"))?;
        let result =
            match patch::add_scratch(&self.inner.env, &entry.git_dir, &dir, &req.base_sha).await {
                Ok(()) => self.in_scratch(ctx, &req, &dir, &bytes, digest).await,
                Err(error) => Err(error),
            };
        patch::remove_scratch(&self.inner.env, &entry.git_dir, &dir).await;
        drop(permit);

        match result? {
            Scratch::Reject(rejection) => self.reject(ctx, &req, digest, &bytes, rejection).await,
            Scratch::NoChange => Ok(PatchOutcome::NoChange),
            Scratch::Evaluated {
                facts,
                verdict,
                tree,
                denied_by,
            } => {
                self.record_verdict(ctx, &req, &bytes, facts, verdict, tree, denied_by)
                    .await
            }
        }
    }

    async fn in_scratch(
        &self,
        ctx: &ActorCtx,
        req: &ValidatePatch,
        dir: &std::path::Path,
        bytes: &[u8],
        digest: Sha256Digest,
    ) -> Result<Scratch, RepoError> {
        let env = &self.inner.env;
        // P2：严格应用（不加 --3way / --reject / --recount / --unidiff-zero / --ignore-whitespace）。
        let applied = GitCmd::new("apply", dir)
            .args(["apply", "--index", "--whitespace=nowarn", "-p1", "-"])
            .stdin(bytes.to_vec())
            .output(env)
            .await?;
        if !applied.success() {
            return Ok(Scratch::Reject(Mechanical {
                reason: PatchRejectReason::NotApplicable,
                detail: patch::sanitize(applied.stderr_tail.as_untrusted_str()),
            }));
        }
        // P3：应用后事实是权威，重做 P4 / P5 / P7。
        let enumerated = match patch::enumerate(env, dir).await? {
            Ok(enumerated) => enumerated,
            Err(rejection) => return Ok(Scratch::Reject(rejection)),
        };
        // P11
        if enumerated.files.is_empty() {
            return Ok(Scratch::NoChange);
        }
        let facts = PatchFacts {
            project_id: req.project_id,
            delivery_id: req.delivery_id,
            attempt_id: req.job_id,
            patch_sha256: digest,
            patch_bytes: bytes.len() as u64,
            applies_cleanly: true,
            files: enumerated.files,
            total_changed_lines: enumerated.total_changed_lines,
            job_paths: req.job_paths.clone(),
        };
        let added = AddedLines::collect(env, dir, &facts.files).await?;
        let before = self
            .inner
            .deps
            .store
            .events()
            .head()
            .await
            .map_err(store_error)?;
        // 失败闭合：policy 不可用 → 错误，不给 allow。
        let verdict = self
            .inner
            .deps
            .policy
            .evaluate_patch(&facts, &added, ctx)
            .await
            .map_err(|error| RepoError::Policy(error.to_string()))?;
        let (tree, denied_by) = if verdict.verdict == PatchVerdict::Deny {
            (
                None,
                self.policy_denied_after(before, &req.project_id).await?,
            )
        } else {
            let out = GitCmd::new("write-tree", dir)
                .arg("write-tree")
                .run(env)
                .await?;
            let tree = GitOid::parse(out.stdout_str()?.trim())
                .map_err(|_| io_error("bad tree oid from write-tree"))?;
            (Some(tree), None)
        };
        Ok(Scratch::Evaluated {
            facts,
            verdict,
            tree,
            denied_by,
        })
    }

    /// `policy` 在 Deny 时追加的 `policy.denied`（P-69）；用于 `patch.rejected` 的 `causation_id`。
    async fn policy_denied_after(
        &self,
        after: Seq,
        project: &ProjectId,
    ) -> Result<Option<EventId>, RepoError> {
        let page = self
            .inner
            .deps
            .store
            .events()
            .scan(
                after,
                &EventFilter {
                    project_ids: None,
                    include: vec![TypePattern::Exact(EventType::PolicyDenied)],
                    exclude: Vec::new(),
                },
                200,
            )
            .await
            .map_err(store_error)?;
        Ok(page
            .events
            .iter()
            .rev()
            .find(|event| event.project_id.is_none() || event.project_id == Some(*project))
            .map(|event| event.id))
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_verdict(
        &self,
        ctx: &ActorCtx,
        req: &ValidatePatch,
        bytes: &[u8],
        facts: PatchFacts,
        verdict: PatchPolicyVerdict,
        tree: Option<GitOid>,
        denied_by: Option<EventId>,
    ) -> Result<PatchOutcome, RepoError> {
        let validated = ValidatedPatch {
            sha256: facts.patch_sha256,
            base_sha: req.base_sha,
            result_tree: tree,
            files: facts.files,
            verdict: verdict.verdict,
            reasons: verdict.reasons.clone(),
            matched_approval_rules: verdict
                .matched_approval_rules
                .iter()
                .map(|rule| rule.as_str().to_owned())
                .collect(),
            config_rev: verdict.config_rev,
        };
        let (ty, data, causation) = match (verdict.verdict, tree) {
            (PatchVerdict::Deny, _) => {
                if verdict.reasons.is_empty() {
                    return Err(RepoError::Policy("policy denied without reasons".into()));
                }
                self.inner.deps.cas.put(bytes, CasKind::Quarantine).await?;
                let detail = verdict
                    .reasons
                    .iter()
                    .map(|reason| match &reason.path {
                        Some(path) => format!("{} {path}", reason.code),
                        None => reason.code.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let data = PatchRejectedData {
                    job_id: req.job_id,
                    sha256: validated.sha256,
                    reason: PatchRejectReason::PolicyDenied,
                    detail: patch::sanitize(&detail),
                    reasons: validated.reasons.clone(),
                    config_rev: Some(validated.config_rev),
                };
                (
                    EventType::PatchRejected,
                    to_value(&data)?,
                    denied_by.or(ctx.causation_id),
                )
            }
            (_, Some(result_tree)) => {
                // 先固化进 CAS，再追加事件（repo.md §7.7）。
                self.inner.deps.cas.put(bytes, CasKind::Artifact).await?;
                let record = serde_json::to_vec(&validated)
                    .map_err(|error| io_error(&format!("validated patch record: {error}")))?;
                self.inner.deps.cas.put(&record, CasKind::Artifact).await?;
                let data = PatchValidatedData {
                    job_id: req.job_id,
                    sha256: validated.sha256,
                    base_sha: validated.base_sha,
                    result_tree,
                    verdict: validated.verdict,
                    files: validated.files.clone(),
                    reasons: validated.reasons.clone(),
                    config_rev: validated.config_rev,
                };
                (
                    EventType::PatchValidated,
                    to_value(&data)?,
                    ctx.causation_id,
                )
            }
            (_, None) => return Err(io_error("allowed patch without result tree")),
        };
        self.append_patch_event(ctx, req, ty, data, causation)
            .await?;
        let outcome = PatchOutcome::Validated(validated);
        self.remember(req, outcome.clone(), true).await;
        Ok(outcome)
    }

    /// 机械拒绝：补丁原文进 CAS 隔离区，发 `patch.rejected(reason)`，不调用 policy。
    async fn reject(
        &self,
        ctx: &ActorCtx,
        req: &ValidatePatch,
        digest: Sha256Digest,
        bytes: &[u8],
        rejection: Mechanical,
    ) -> Result<PatchOutcome, RepoError> {
        self.inner.deps.cas.put(bytes, CasKind::Quarantine).await?;
        let data = PatchRejectedData {
            job_id: req.job_id,
            sha256: digest,
            reason: rejection.reason,
            detail: rejection.detail.clone(),
            reasons: Vec::new(),
            config_rev: None,
        };
        self.append_patch_event(
            ctx,
            req,
            EventType::PatchRejected,
            to_value(&data)?,
            ctx.causation_id,
        )
        .await?;
        let outcome = PatchOutcome::Rejected {
            reason: rejection.reason,
            detail: rejection.detail,
        };
        self.remember(req, outcome.clone(), true).await;
        Ok(outcome)
    }

    async fn append_patch_event(
        &self,
        ctx: &ActorCtx,
        req: &ValidatePatch,
        ty: EventType,
        data: Value,
        causation: Option<EventId>,
    ) -> Result<(), RepoError> {
        let subject = Subject::parse(&req.job_id.to_string())
            .map_err(|_| io_error("job id is not a valid subject"))?;
        let mut tx = self
            .inner
            .deps
            .store
            .begin_write()
            .await
            .map_err(store_error)?;
        let appended = tx
            .append(NewEvent {
                ty,
                subject,
                project_id: Some(req.project_id),
                actor: ctx.actor.clone(),
                causation_id: causation,
                correlation_id: None,
                data,
                dedup_key: Some(dedup_key(&req.job_id)),
            })
            .await
            .map_err(store_error)?;
        if let Appended::New(_) = appended {
            tx.commit().await.map_err(store_error)?;
        }
        Ok(())
    }

    async fn remember(&self, req: &ValidatePatch, outcome: PatchOutcome, base_known: bool) {
        self.inner.patches.lock().await.insert(
            req.job_id,
            PatchRecord {
                project_id: req.project_id,
                outcome,
                base_known,
            },
        );
    }

    fn job_lock(&self, job: &AttemptId) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .job_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(*job)
            .or_default()
            .clone()
    }

    async fn branch_head_of(&self, id: &gantry_core::ids::WorktreeId) -> Result<GitOid, RepoError> {
        crate::RepoService::branch_head(self, id).await
    }

    /// 从 `patch.*` 事件重建校验结论（启动时调用）。
    pub(crate) async fn rebuild_patches(&self) -> Result<(), RepoError> {
        let log = self.inner.deps.store.events();
        let filter = EventFilter {
            project_ids: None,
            include: vec![TypePattern::Prefix("patch.".into())],
            exclude: Vec::new(),
        };
        let mut patches = self.inner.patches.lock().await;
        let mut after = Seq::ZERO;
        loop {
            let page = log.scan(after, &filter, 200).await.map_err(store_error)?;
            for event in &page.events {
                if let Some((job, record)) = record_from_event(event)? {
                    patches.insert(job, record);
                }
            }
            if page.scanned_to >= page.head || page.scanned_to == after {
                break;
            }
            after = page.scanned_to;
        }
        Ok(())
    }

    /// `commit_patch` 用：按 job_id 读回校验结论（不接受调用方传入的判定）。
    #[allow(dead_code)]
    pub(crate) async fn patch_record(&self, job: &AttemptId) -> Option<PatchRecord> {
        self.inner.patches.lock().await.get(job).cloned()
    }
}

fn record_from_event(event: &Event<Value>) -> Result<Option<(AttemptId, PatchRecord)>, RepoError> {
    let Some(project_id) = event.project_id else {
        return Ok(None);
    };
    match event.ty {
        EventType::PatchValidated => {
            let data: PatchValidatedData = from_value(&event.data)?;
            let outcome = PatchOutcome::Validated(ValidatedPatch {
                sha256: data.sha256,
                base_sha: data.base_sha,
                result_tree: Some(data.result_tree),
                files: data.files,
                verdict: data.verdict,
                reasons: data.reasons,
                // 事件不含命中的审批规则（M2 才用到）。
                matched_approval_rules: Vec::new(),
                config_rev: data.config_rev,
            });
            Ok(Some((
                data.job_id,
                PatchRecord {
                    project_id,
                    outcome,
                    base_known: true,
                },
            )))
        }
        EventType::PatchRejected => {
            let data: PatchRejectedData = from_value(&event.data)?;
            let record = if data.reason == PatchRejectReason::PolicyDenied {
                PatchRecord {
                    project_id,
                    outcome: PatchOutcome::Validated(ValidatedPatch {
                        sha256: data.sha256,
                        base_sha: GitOid::parse("0000000000000000000000000000000000000000")
                            .expect("zero oid"),
                        result_tree: None,
                        // 被拒补丁的文件清单不在事件里；重启后返回空列表。
                        files: Vec::new(),
                        verdict: PatchVerdict::Deny,
                        reasons: data.reasons,
                        matched_approval_rules: Vec::new(),
                        config_rev: data.config_rev.unwrap_or(Seq::ZERO),
                    }),
                    base_known: false,
                }
            } else {
                PatchRecord {
                    project_id,
                    outcome: PatchOutcome::Rejected {
                        reason: data.reason,
                        detail: data.detail,
                    },
                    base_known: true,
                }
            };
            Ok(Some((data.job_id, record)))
        }
        _ => Ok(None),
    }
}

fn to_value<T: serde::Serialize>(data: &T) -> Result<Value, RepoError> {
    serde_json::to_value(data).map_err(|error| io_error(&format!("event data: {error}")))
}

fn from_value<T: serde::de::DeserializeOwned>(data: &Value) -> Result<T, RepoError> {
    serde_json::from_value(data.clone())
        .map_err(|error| io_error(&format!("patch event data: {error}")))
}
