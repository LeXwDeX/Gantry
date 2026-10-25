//! `FakeForge`：内存版 `OutboxPort` + `ForgeReader` + `PrRefresher`。
//!
//! 只实现 M0 替身需要的可观察语义：入队幂等（同一 epoch）、epoch 递增产生新
//! 动作、取消状态机、事务提交/回滚、快照读取与 webhook 注入。没有 GitHub
//! 调用、没有 store 依赖，也不模拟真实限流与重试。
//!
//! 事务：`FakeForge::begin_write()` 返回 [`FakeForgeTx`]，它实现
//! `core::ports::TxHandle`。`enqueue_in` 只接受这个类型；提交前的改动对读者
//! 不可见，`commit` 原子替换，`rollback` 丢弃。其他 `TxHandle`（例如未来
//! `store::WriteTx`）会明确返回 `StoreError::Rejected`，不静默退化成自开事务。

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gantry_core::canon::{GitOid, IdempotencyKey, RepoSlug};
use gantry_core::entities::{
    CheckSummary, IssueRef, OutboxAction, OutboxStatus, Project, PullRequestRef,
};
use gantry_core::forge::{ActionPayload, PrHead};
use gantry_core::ids::{ActionId, ProjectId};
use gantry_core::ports::{
    ActorCtx, Annotation, CancelOutcome, CancelReason, ChangedFile, CiLog, CiLogQuery, Discussion,
    DiscussionQuery, DiscussionTarget, DriftKind, EnqueueError, EnqueueOutcome, EnqueueRequest,
    ForgeError, ForgeReader, MirrorWant, ModuleError, OutboxPort, PrRefresher, ProjectSnapshot,
    ReconcileReport, ReconcileTarget, ReviewThreadView, StoreError, ThreadNodeId, TxHandle,
};
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PrEntry {
    head_ref: String,
    snapshot: PullRequestRef,
}

#[derive(Clone, Debug)]
struct ForgeState {
    now: Timestamp,
    generation: u64,
    counter: u128,
    projects: BTreeMap<ProjectId, Project>,
    prs: BTreeMap<(ProjectId, u64), PrEntry>,
    issues: BTreeMap<(ProjectId, u64), IssueRef>,
    discussions: BTreeMap<(ProjectId, u64, bool), Discussion>,
    ci_logs: BTreeMap<(ProjectId, u64), CiLog>,
    changed_files: BTreeMap<(ProjectId, u64, GitOid), Vec<ChangedFile>>,
    review_threads: BTreeMap<(ProjectId, ThreadNodeId), ReviewThreadView>,
    annotations: BTreeMap<(ProjectId, u64), Vec<Annotation>>,
    unreachable: BTreeSet<ProjectId>,
    webhooks: BTreeMap<String, FakeWebhook>,
    reconciles: Vec<ReconcileTarget>,
    mirrors: Vec<(ProjectId, MirrorWant)>,
    outbox: BTreeMap<ActionId, OutboxAction>,
    keys: BTreeMap<String, ActionId>,
}

impl ForgeState {
    fn new(now: Timestamp) -> Self {
        Self {
            now,
            generation: 0,
            counter: 0,
            projects: BTreeMap::new(),
            prs: BTreeMap::new(),
            issues: BTreeMap::new(),
            discussions: BTreeMap::new(),
            ci_logs: BTreeMap::new(),
            changed_files: BTreeMap::new(),
            review_threads: BTreeMap::new(),
            annotations: BTreeMap::new(),
            unreachable: BTreeSet::new(),
            webhooks: BTreeMap::new(),
            reconciles: Vec::new(),
            mirrors: Vec::new(),
            outbox: BTreeMap::new(),
            keys: BTreeMap::new(),
        }
    }

    fn next_id<K: gantry_core::ids::IdKind>(&mut self) -> gantry_core::ids::Id<K> {
        self.counter += 1;
        let ms = self.now.as_millis().max(0) as u64;
        gantry_core::ids::Id::from_parts(ms, self.counter)
            .expect("fake forge counter must fit in 80 bits")
    }
}

// ---------------------------------------------------------------------------
// FakeForge
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct FakeForge {
    state: Arc<Mutex<ForgeState>>,
}

impl std::fmt::Debug for FakeForge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("FakeForge")
            .field("projects", &state.projects.len())
            .field("prs", &state.prs.len())
            .field("outbox", &state.outbox.len())
            .field("now", &state.now)
            .finish()
    }
}

impl FakeForge {
    pub fn new(now: Timestamp) -> Self {
        Self {
            state: Arc::new(Mutex::new(ForgeState::new(now))),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ForgeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set_now(&self, now: Timestamp) {
        self.lock().now = now;
    }

    pub fn advance_ms(&self, ms: i64) {
        let mut state = self.lock();
        state.now = Timestamp::from_millis(state.now.as_millis().saturating_add(ms));
    }

    pub fn now(&self) -> Timestamp {
        self.lock().now
    }

    // ---- 快照注入 ----

    pub fn insert_project(&self, project: Project) {
        self.lock().projects.insert(project.id, project);
    }

    pub fn remove_project(&self, project: &ProjectId) {
        self.lock().projects.remove(project);
    }

    pub fn upsert_pr(&self, project: ProjectId, head_ref: impl Into<String>, pr: PullRequestRef) {
        let mut state = self.lock();
        let synced_at = state.now;
        let mut pr = pr;
        pr.synced_at = synced_at;
        state.prs.insert(
            (project, pr.number),
            PrEntry {
                head_ref: head_ref.into(),
                snapshot: pr,
            },
        );
    }

    pub fn upsert_issue(&self, project: ProjectId, issue: IssueRef) {
        let mut state = self.lock();
        let synced_at = state.now;
        let mut issue = issue;
        issue.synced_at = synced_at;
        state.issues.insert((project, issue.number), issue);
    }

    pub fn set_checks(&self, project: ProjectId, number: u64, checks: CheckSummary) {
        let mut state = self.lock();
        let now = state.now;
        if let Some(entry) = state.prs.get_mut(&(project, number)) {
            entry.snapshot.checks = checks;
            entry.snapshot.synced_at = now;
        }
    }

    pub fn set_discussion(
        &self,
        project: ProjectId,
        target: DiscussionTarget,
        discussion: Discussion,
    ) {
        self.lock()
            .discussions
            .insert(discussion_key(project, target), discussion);
    }

    pub fn set_ci_log(&self, project: ProjectId, run_id: u64, log: CiLog) {
        self.lock().ci_logs.insert((project, run_id), log);
    }

    pub fn set_changed_files(
        &self,
        project: ProjectId,
        number: u64,
        head_sha: GitOid,
        files: Vec<ChangedFile>,
    ) {
        self.lock()
            .changed_files
            .insert((project, number, head_sha), files);
    }

    pub fn set_review_thread(&self, project: ProjectId, view: ReviewThreadView) {
        self.lock()
            .review_threads
            .insert((project, view.thread_node_id.clone()), view);
    }

    pub fn set_annotations(&self, project: ProjectId, check_run_id: u64, items: Vec<Annotation>) {
        self.lock()
            .annotations
            .insert((project, check_run_id), items);
    }

    pub fn set_mirror_unreachable(&self, project: ProjectId, unreachable: bool) {
        let mut state = self.lock();
        if unreachable {
            state.unreachable.insert(project);
        } else {
            state.unreachable.remove(&project);
        }
    }

    /// 注入一条 webhook。返回 `false` 表示同一 delivery 已处理过（去重）。
    pub fn inject_webhook(&self, delivery_id: impl Into<String>, hook: FakeWebhook) -> bool {
        let delivery_id = delivery_id.into();
        let mut state = self.lock();
        if state.webhooks.contains_key(&delivery_id) {
            return false;
        }
        let now = state.now;
        match &hook {
            FakeWebhook::PrUpserted {
                project,
                head_ref,
                pr,
            } => {
                let mut pr = pr.clone();
                pr.synced_at = now;
                state.prs.insert(
                    (*project, pr.number),
                    PrEntry {
                        head_ref: head_ref.clone(),
                        snapshot: pr,
                    },
                );
            }
            FakeWebhook::PrChecksUpdated {
                project,
                number,
                checks,
            } => {
                if let Some(entry) = state.prs.get_mut(&(*project, *number)) {
                    entry.snapshot.checks = checks.clone();
                    entry.snapshot.synced_at = now;
                }
            }
            FakeWebhook::PrClosed { project, number } => {
                if let Some(entry) = state.prs.get_mut(&(*project, *number)) {
                    entry.snapshot.state = gantry_core::entities::PullRequestState::Closed;
                    entry.snapshot.synced_at = now;
                }
            }
            FakeWebhook::IssueUpserted { project, issue } => {
                let mut issue = issue.clone();
                issue.synced_at = now;
                state.issues.insert((*project, issue.number), issue);
            }
            FakeWebhook::InstallationUnreachable { project } => {
                state.unreachable.insert(*project);
            }
        }
        state.webhooks.insert(delivery_id, hook);
        true
    }

    pub fn webhook_count(&self) -> usize {
        self.lock().webhooks.len()
    }

    pub fn reconcile_count(&self) -> usize {
        self.lock().reconciles.len()
    }

    pub fn mirror_sync_count(&self) -> usize {
        self.lock().mirrors.len()
    }

    // ---- Outbox 观测 ----

    pub fn outbox_len(&self) -> usize {
        self.lock().outbox.len()
    }

    pub fn outbox_action(&self, id: &ActionId) -> Option<OutboxAction> {
        self.lock().outbox.get(id).cloned()
    }

    pub fn outbox_list(&self) -> Vec<OutboxAction> {
        self.lock().outbox.values().cloned().collect()
    }

    // ---- 事务 ----

    pub fn begin_write(&self) -> FakeForgeTx {
        let snapshot = self.lock().clone();
        FakeForgeTx {
            forge: self.clone(),
            snapshot,
        }
    }

    fn enqueue_on(
        state: &mut ForgeState,
        req: &EnqueueRequest,
        _ctx: &ActorCtx,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        if !state.projects.contains_key(&req.project_id) {
            return Err(EnqueueError::ProjectUnknown);
        }
        if matches!(req.payload, ActionPayload::MergePr(_)) && req.requires_approval.is_none() {
            return Err(EnqueueError::ApprovalRequired {
                reason: "merge_pr requires an approval id at enqueue time",
            });
        }
        let payload_value =
            serde_json::to_value(&req.payload).map_err(|error| EnqueueError::InvalidPayload {
                pointer: "/payload".into(),
                message: error.to_string(),
            })?;
        // shortcut: target/intent 推导在 core OQ-9 冻结前由 fake 给出最小确定性
        // 形式；真实 forge 可按同一输入实现，但不需要与本函数逐字一致。
        let target = payload_target(&req.payload);
        let key = IdempotencyKey::derive(
            &req.project_id,
            &req.delivery_id,
            req.payload.kind(),
            &target,
            &payload_value,
            req.epoch,
        )
        .map_err(|error| EnqueueError::InvalidPayload {
            pointer: "/payload".into(),
            message: error.to_string(),
        })?;
        if let Some(existing) = state.keys.get(key.as_str()) {
            let action = state
                .outbox
                .get(existing)
                .cloned()
                .ok_or(EnqueueError::ProjectUnknown)?;
            return Ok(EnqueueOutcome {
                action,
                created: false,
            });
        }
        let now = state.now;
        let id: ActionId = state.next_id();
        let action = OutboxAction {
            id,
            project_id: req.project_id,
            delivery_id: req.delivery_id,
            idempotency_key: key.clone(),
            kind: req.payload.kind(),
            payload: req.payload.clone(),
            requires_approval: req.requires_approval,
            depends_on: req.depends_on.clone(),
            priority: req.priority,
            not_before: req.not_before,
            epoch: req.epoch,
            status: OutboxStatus::Queued,
            attempts: 0,
            lease_until: None,
            last_error: None,
            expected_effects: Vec::new(),
            github_ref: None,
            created_at: now,
            updated_at: now,
        };
        state.outbox.insert(id, action.clone());
        state.keys.insert(key.as_str().to_owned(), id);
        Ok(EnqueueOutcome {
            action,
            created: true,
        })
    }
}

fn discussion_key(project: ProjectId, target: DiscussionTarget) -> (ProjectId, u64, bool) {
    match target {
        DiscussionTarget::Issue(number) => (project, number, false),
        DiscussionTarget::PullRequest(number) => (project, number, true),
    }
}

fn payload_target(payload: &ActionPayload) -> String {
    use ActionPayload::*;
    match payload {
        PushBranch(p) => format!("branch:{}:{}", p.repo, p.branch),
        CreatePr(p) => format!("pr:{}:{}->{}", p.repo, p.head, p.base),
        UpdatePr(p) => format!("pr:{}:{}", p.repo, p.pull_number),
        UpsertStatusComment(p) => format!(
            "comment:{}:{:?}:{}:{}",
            p.repo, p.target.kind, p.target.number, p.slot
        ),
        CreateComment(p) => format!(
            "comment:{}:{:?}:{}:{}",
            p.repo, p.target.kind, p.target.number, p.client_ref
        ),
        ReplyReviewThread(p) => format!("thread:{}:{}", p.repo, p.thread_node_id),
        ResolveReviewThread(p) => format!("thread:{}:{}", p.repo, p.thread_node_id),
        CreateIssue(p) => format!("issue:{}:{}", p.repo, p.client_ref),
        UpdateIssue(p) => format!("issue:{}:{}", p.repo, p.number),
        CloseIssue(p) => format!("issue:{}:{}", p.repo, p.number),
        AddLabels(p) => format!("issue:{}:{}", p.repo, p.number),
        RequestReviewers(p) => format!("pr:{}:{}", p.repo, p.pull_number),
        MergePr(p) => format!("pr:{}:{}", p.repo, p.pull_number),
        RerunFailedJobs(p) => format!("run:{}:{}", p.repo, p.run_id),
    }
}

// ---------------------------------------------------------------------------
// 事务句柄
// ---------------------------------------------------------------------------

pub struct FakeForgeTx {
    forge: FakeForge,
    snapshot: ForgeState,
}

impl FakeForgeTx {
    pub async fn commit(mut self) -> Result<(), StoreError> {
        let mut shared = self.forge.lock();
        if shared.generation != self.snapshot.generation {
            return Err(StoreError::Conflict {
                code: "fake_forge_tx_conflict",
            });
        }
        self.snapshot.generation += 1;
        *shared = self.snapshot;
        Ok(())
    }

    pub async fn rollback(self) -> Result<(), StoreError> {
        Ok(())
    }
}

impl TxHandle for FakeForgeTx {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn tx_required() -> StoreError {
    StoreError::Rejected {
        message: "FakeForge::enqueue_in requires a FakeForgeTx".into(),
    }
}

// ---------------------------------------------------------------------------
// OutboxPort
// ---------------------------------------------------------------------------

#[async_trait]
impl OutboxPort for FakeForge {
    async fn enqueue_in(
        &self,
        tx: &mut dyn TxHandle,
        req: EnqueueRequest,
        ctx: &ActorCtx,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        let tx = tx
            .as_any_mut()
            .downcast_mut::<FakeForgeTx>()
            .ok_or_else(|| EnqueueError::Store(tx_required()))?;
        if !Arc::ptr_eq(&tx.forge.state, &self.state) {
            return Err(EnqueueError::Store(StoreError::Rejected {
                message: "FakeForgeTx belongs to another FakeForge".into(),
            }));
        }
        FakeForge::enqueue_on(&mut tx.snapshot, &req, ctx)
    }

    async fn enqueue(
        &self,
        req: EnqueueRequest,
        ctx: &ActorCtx,
    ) -> Result<EnqueueOutcome, EnqueueError> {
        let mut tx = self.begin_write();
        let outcome = FakeForge::enqueue_on(&mut tx.snapshot, &req, ctx)?;
        tx.commit().await?;
        Ok(outcome)
    }

    async fn cancel(
        &self,
        id: &ActionId,
        reason: CancelReason,
        _ctx: &ActorCtx,
    ) -> Result<CancelOutcome, ForgeError> {
        let mut state = self.lock();
        let now = state.now;
        let action = state
            .outbox
            .get_mut(id)
            .ok_or_else(|| ForgeError::InvalidTarget {
                message: format!("outbox action {id} not found"),
            })?;
        match action.status {
            OutboxStatus::Queued => {
                action.status = OutboxStatus::Cancelled;
                action.updated_at = now;
                action.last_error = Some(gantry_core::entities::OutboxError {
                    code: cancel_reason_code(&reason).to_owned(),
                    message: String::new(),
                });
                Ok(CancelOutcome::Cancelled)
            }
            OutboxStatus::Executing => Ok(CancelOutcome::NotCancellable),
            other => Ok(CancelOutcome::AlreadyTerminal(other)),
        }
    }
}

fn cancel_reason_code(reason: &CancelReason) -> &str {
    match reason {
        CancelReason::Superseded => "superseded",
        CancelReason::DeliveryAbandoned => "delivery_abandoned",
        CancelReason::ApprovalInvalidated => "approval_invalidated",
        CancelReason::DeliveryClosed => "delivery_closed",
        CancelReason::Shutdown => "shutdown",
        CancelReason::Other(_) => "other",
    }
}

// ---------------------------------------------------------------------------
// ForgeReader
// ---------------------------------------------------------------------------

#[async_trait]
impl ForgeReader for FakeForge {
    async fn pull_request(
        &self,
        project: &ProjectId,
        number: u64,
    ) -> Result<Option<PullRequestRef>, ForgeError> {
        Ok(self
            .lock()
            .prs
            .get(&(*project, number))
            .map(|entry| entry.snapshot.clone()))
    }

    async fn issue(
        &self,
        project: &ProjectId,
        number: u64,
    ) -> Result<Option<IssueRef>, ForgeError> {
        Ok(self.lock().issues.get(&(*project, number)).cloned())
    }

    async fn project_snapshot(&self, project: &ProjectId) -> Result<ProjectSnapshot, ForgeError> {
        let state = self.lock();
        let project = state
            .projects
            .get(project)
            .cloned()
            .ok_or(ForgeError::NotFound)?;
        Ok(ProjectSnapshot {
            project,
            synced_at: state.now,
        })
    }

    async fn discussion(
        &self,
        project: &ProjectId,
        target: DiscussionTarget,
        query: DiscussionQuery,
    ) -> Result<Discussion, ForgeError> {
        let state = self.lock();
        let source = state
            .discussions
            .get(&discussion_key(*project, target))
            .cloned()
            .ok_or(ForgeError::NotFound)?;
        let mut comments = source.comments;
        if let Some(since) = query.comments_since {
            comments.retain(|comment| comment.created_at > since);
        }
        let limit = query.max_comments as usize;
        let mut truncated = source.truncated || comments.len() > limit;
        comments.truncate(limit);
        let mut bytes = 0usize;
        let mut kept = Vec::new();
        for comment in comments {
            let len = comment.body.as_untrusted_str().len();
            if bytes.saturating_add(len) > query.max_bytes as usize {
                truncated = true;
                break;
            }
            bytes += len;
            kept.push(comment);
        }
        Ok(Discussion {
            title: source.title,
            body: if query.include_body {
                source.body
            } else {
                None
            },
            comments: kept,
            truncated,
            fetched_at: state.now,
        })
    }

    async fn ci_log(&self, project: &ProjectId, query: CiLogQuery) -> Result<CiLog, ForgeError> {
        let state = self.lock();
        let source = state
            .ci_logs
            .get(&(*project, query.run_id))
            .cloned()
            .ok_or(ForgeError::NotFound)?;
        let text = source.text.as_untrusted_str();
        let (text, truncated) = truncate_text(text, query.max_bytes as usize);
        Ok(CiLog {
            text: UntrustedText::new(text.to_owned(), "fake:ci_log".into(), truncated),
            truncated: source.truncated || truncated,
            fetched_at: state.now,
        })
    }

    async fn changed_files(
        &self,
        project: &ProjectId,
        number: u64,
        head_sha: &GitOid,
    ) -> Result<Vec<ChangedFile>, ForgeError> {
        Ok(self
            .lock()
            .changed_files
            .get(&(*project, number, *head_sha))
            .cloned()
            .unwrap_or_default())
    }

    async fn review_thread(
        &self,
        project: &ProjectId,
        thread: &ThreadNodeId,
    ) -> Result<ReviewThreadView, ForgeError> {
        self.lock()
            .review_threads
            .get(&(*project, thread.clone()))
            .cloned()
            .ok_or(ForgeError::NotFound)
    }

    async fn check_annotations(
        &self,
        project: &ProjectId,
        check_run_id: u64,
    ) -> Result<Vec<Annotation>, ForgeError> {
        Ok(self
            .lock()
            .annotations
            .get(&(*project, check_run_id))
            .cloned()
            .unwrap_or_default())
    }

    async fn reconcile_now(
        &self,
        target: ReconcileTarget,
        _ctx: &ActorCtx,
    ) -> Result<ReconcileReport, ForgeError> {
        let mut state = self.lock();
        let now = state.now;
        state.reconciles.push(target);
        Ok(ReconcileReport {
            emitted: Vec::new(),
            drifts: Vec::<DriftKind>::new(),
            synced_at: now,
        })
    }

    async fn sync_mirror(
        &self,
        project: &ProjectId,
        want: MirrorWant,
        _ctx: &ActorCtx,
    ) -> Result<(), ForgeError> {
        let mut state = self.lock();
        if !state.projects.contains_key(project) {
            return Err(ForgeError::NotFound);
        }
        if state.unreachable.contains(project) {
            return Err(ForgeError::Unavailable {
                message: "installation unreachable".into(),
            });
        }
        state.mirrors.push((*project, want));
        Ok(())
    }
}

fn truncate_text(text: &str, max_bytes: usize) -> (&str, bool) {
    if max_bytes == 0 || text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

// ---------------------------------------------------------------------------
// PrRefresher
// ---------------------------------------------------------------------------

#[async_trait]
impl PrRefresher for FakeForge {
    async fn refresh_pr(
        &self,
        repo: &RepoSlug,
        number: u64,
        _ctx: &ActorCtx,
    ) -> Result<PrHead, ModuleError> {
        let mut state = self.lock();
        let now = state.now;
        let project = state
            .projects
            .values()
            .find(|project| project.repo == *repo)
            .map(|project| project.id)
            .ok_or(ModuleError::NotFound)?;
        let entry = state
            .prs
            .get_mut(&(project, number))
            .ok_or(ModuleError::NotFound)?;
        entry.snapshot.synced_at = now;
        Ok(PrHead {
            ref_name: entry.head_ref.clone(),
            sha: entry.snapshot.head_sha,
            repo: repo.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Webhook 注入类型
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeWebhook {
    PrUpserted {
        project: ProjectId,
        head_ref: String,
        pr: PullRequestRef,
    },
    PrChecksUpdated {
        project: ProjectId,
        number: u64,
        checks: CheckSummary,
    },
    PrClosed {
        project: ProjectId,
        number: u64,
    },
    IssueUpserted {
        project: ProjectId,
        issue: IssueRef,
    },
    InstallationUnreachable {
        project: ProjectId,
    },
}

// ---------------------------------------------------------------------------
// 一致性用例
// ---------------------------------------------------------------------------

/// 入队幂等、epoch 递增与取消状态机；真实 `OutboxPort` 实现也应通过。
///
/// 前置条件：`req.project_id` 已在实现中登记；`req` 对应的动作尚未入队。
pub async fn outbox_conformance(
    port: &dyn OutboxPort,
    req: EnqueueRequest,
    ctx: &ActorCtx,
) -> Result<(), String> {
    let first = port
        .enqueue(req.clone(), ctx)
        .await
        .map_err(|error| format!("first enqueue failed: {error}"))?;
    if !first.created {
        return Err("first enqueue must create an action".into());
    }
    let second = port
        .enqueue(req.clone(), ctx)
        .await
        .map_err(|error| format!("second enqueue failed: {error}"))?;
    if second.created || second.action.id != first.action.id {
        return Err("same epoch must deduplicate to the same action".into());
    }
    let mut retry = req;
    retry.epoch += 1;
    let retried = port
        .enqueue(retry, ctx)
        .await
        .map_err(|error| format!("epoch retry failed: {error}"))?;
    if !retried.created || retried.action.id == first.action.id {
        return Err("a new epoch must create a new action".into());
    }
    let cancelled = port
        .cancel(&retried.action.id, CancelReason::Superseded, ctx)
        .await
        .map_err(|error| format!("cancel failed: {error}"))?;
    if cancelled != CancelOutcome::Cancelled {
        return Err("queued action must be cancellable".into());
    }
    let again = port
        .cancel(&retried.action.id, CancelReason::Shutdown, ctx)
        .await
        .map_err(|error| format!("second cancel failed: {error}"))?;
    if again != CancelOutcome::AlreadyTerminal(OutboxStatus::Cancelled) {
        return Err("second cancel must report the terminal state".into());
    }
    Ok(())
}

// 编译期检查：FakeForge 实现全部三个端口。
const _: fn() = || {
    fn assert_outbox<T: OutboxPort>() {}
    fn assert_reader<T: ForgeReader>() {}
    fn assert_refresher<T: PrRefresher>() {}
    assert_outbox::<FakeForge>();
    assert_reader::<FakeForge>();
    assert_refresher::<FakeForge>();
};
