//! GitHub 快照投影（#150，store.md §7.2 `pr_snapshots` / `issue_snapshots`）。
//!
//! 快照列只由 `forge.synced` 折叠（forge 在 #150 的约定第 5 条）：`forge.<kind>`
//! 只驱动业务语义，不再从它们的 data 推一遍快照，避免两条路径不一致。
//! 折叠只读 `changes[].to`；`from` 在本地已有值时必须与之相等，否则拒绝
//! （forge 按"回读值 − 当前快照"做差，`from` 不等说明两边已经漂移）。本地没有
//! 值时不比较 `from`，因此首次快照无论按 #150 的方案 A（`from = null`）还是
//! 别的方式落地，折叠规则都不用改。
//!
//! 快照按 `(project_id, number)` 存放；读取按 `(repo, number)`，由
//! `project.added` / `project.updated` 维护的 `project → repo` 索引解析。PR 的
//! 全部字段都折叠到过才对外可读；缺字段时读取返回 `None`，不编造默认值。
//!
//! 函数是纯的：不读时钟、不做 IO，同一事件序列在任何调用方得到同一结果。

use std::collections::BTreeMap;
use std::fmt::Debug;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{
    ActorKind, CheckSummary, IssueRef, IssueState, PullRequestRef, PullRequestState, ReviewState,
};
use gantry_core::events::{
    Event, EventType, ForgeSyncedChange, ForgeSyncedData, ForgeSyncedState, ForgeSyncedTargetKind,
    ProjectEventData, SchemaError,
};
use gantry_core::forge::IssueStateReason;
use gantry_core::ids::ProjectId;
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use serde_json::Value;

use crate::projection::FoldError;

/// PR 快照行：每列在第一次被 `forge.synced` 写到之前为 `None`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrSnapshotRow {
    pub node_id: String,
    pub title: Option<UntrustedText>,
    pub state: Option<PullRequestState>,
    pub head_sha: Option<GitOid>,
    pub base_ref: Option<String>,
    pub base_sha: Option<GitOid>,
    pub draft: Option<bool>,
    pub mergeable_state: Option<String>,
    pub in_merge_queue: Option<bool>,
    pub checks: Option<CheckSummary>,
    pub review_state: Option<ReviewState>,
    pub synced_at: Option<Timestamp>,
}

/// Issue 快照行。`state_reason` 的 `null` 是真实值（event-data §2.2.5.1），
/// 所以用两层 `Option`：外层 `None` 才表示"还没同步到"。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueSnapshotRow {
    pub node_id: String,
    pub title: Option<UntrustedText>,
    pub state: Option<IssueState>,
    pub state_reason: Option<Option<IssueStateReason>>,
    pub synced_at: Option<Timestamp>,
}

/// 全部 GitHub 快照与 `project → repo` 索引。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GithubSnapshots {
    repos: BTreeMap<ProjectId, RepoSlug>,
    prs: BTreeMap<(ProjectId, u64), PrSnapshotRow>,
    issues: BTreeMap<(ProjectId, u64), IssueSnapshotRow>,
}

impl GithubSnapshots {
    /// 本投影折叠的事件类型。
    pub fn folds(ty: EventType) -> bool {
        matches!(
            ty,
            EventType::ProjectAdded | EventType::ProjectUpdated | EventType::ForgeSynced
        )
    }

    /// 折叠一条事件；失败时 `self` 不变。
    pub fn apply(&mut self, event: &Event<Value>) -> Result<(), FoldError> {
        match event.ty {
            EventType::ProjectAdded | EventType::ProjectUpdated => {
                // 这里只维护索引；项目事件的信封与仓库唯一性归项目投影（§7.2 `projects`）。
                let data: ProjectEventData = decode(event)?;
                self.repos.insert(data.id, data.repo);
                Ok(())
            }
            EventType::ForgeSynced => self.apply_synced(event),
            other => Err(FoldError::Precondition(format!(
                "{} is not a GitHub snapshot event",
                other.as_str()
            ))),
        }
    }

    fn apply_synced(&mut self, event: &Event<Value>) -> Result<(), FoldError> {
        // 信封约束（event-data §2.2.5.1）：core 只校验 data，信封在这里把关。
        if event.actor.kind != ActorKind::Forge {
            return Err(FoldError::Precondition(
                "only actor.kind = forge may update GitHub snapshots".to_owned(),
            ));
        }
        let project = event.project_id.ok_or_else(|| {
            FoldError::Data(SchemaError::consistency(
                "/project_id",
                "forge.synced requires project_id",
            ))
        })?;
        if event.subject.as_str() != project.to_string() {
            return Err(FoldError::Data(SchemaError::consistency(
                "/subject",
                "forge.synced subject must equal project_id",
            )));
        }
        let data: ForgeSyncedData = decode(event)?;
        let key = (project, data.target.number);
        match data.target.kind {
            ForgeSyncedTargetKind::Pr => {
                let mut row = self
                    .prs
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| PrSnapshotRow {
                        node_id: data.target.node_id.clone(),
                        ..PrSnapshotRow::default()
                    });
                check_node(&row.node_id, &data.target.node_id)?;
                for change in &data.changes {
                    row.apply(change)?;
                }
                row.synced_at = Some(data.synced_at);
                self.prs.insert(key, row);
            }
            ForgeSyncedTargetKind::Issue => {
                let mut row = self
                    .issues
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| IssueSnapshotRow {
                        node_id: data.target.node_id.clone(),
                        ..IssueSnapshotRow::default()
                    });
                check_node(&row.node_id, &data.target.node_id)?;
                for change in &data.changes {
                    row.apply(change)?;
                }
                row.synced_at = Some(data.synced_at);
                self.issues.insert(key, row);
            }
        }
        Ok(())
    }

    /// 仓库恰好绑定一个项目时返回它；绑定了多个项目时有歧义，返回 `None`。
    pub fn project_by_repo(&self, repo: &RepoSlug) -> Option<ProjectId> {
        let mut bound = self
            .repos
            .iter()
            .filter(|(_, r)| *r == repo)
            .map(|(id, _)| *id);
        let first = bound.next()?;
        bound.next().is_none().then_some(first)
    }

    pub fn pr_row(&self, project: &ProjectId, number: u64) -> Option<&PrSnapshotRow> {
        self.prs.get(&(*project, number))
    }

    pub fn issue_row(&self, project: &ProjectId, number: u64) -> Option<&IssueSnapshotRow> {
        self.issues.get(&(*project, number))
    }

    /// store.md §6.2 `pr_snapshot`：所有列都已同步才返回。
    pub fn pr_snapshot(&self, repo: &RepoSlug, number: u64) -> Option<PullRequestRef> {
        let project = self.project_by_repo(repo)?;
        self.pr_row(&project, number)?.complete(repo, number)
    }

    /// store.md §6.2 `issue_snapshot`：`title` 与 `state` 已同步才返回。
    pub fn issue_snapshot(&self, repo: &RepoSlug, number: u64) -> Option<IssueRef> {
        let project = self.project_by_repo(repo)?;
        self.issue_row(&project, number)?.complete(repo, number)
    }
}

impl PrSnapshotRow {
    fn apply(&mut self, change: &ForgeSyncedChange) -> Result<(), FoldError> {
        match change {
            ForgeSyncedChange::State { from, to } => {
                set(&mut self.state, "state", pr_state(*from), pr_state(*to))
            }
            ForgeSyncedChange::Title { from, to } => {
                set(&mut self.title, "title", from.clone(), to.clone())
            }
            ForgeSyncedChange::HeadSha { from, to } => {
                set(&mut self.head_sha, "head_sha", *from, *to)
            }
            ForgeSyncedChange::BaseRef { from, to } => {
                set(&mut self.base_ref, "base_ref", from.clone(), to.clone())
            }
            ForgeSyncedChange::BaseSha { from, to } => {
                set(&mut self.base_sha, "base_sha", *from, *to)
            }
            ForgeSyncedChange::Draft { from, to } => set(&mut self.draft, "draft", *from, *to),
            ForgeSyncedChange::MergeableState { from, to } => set(
                &mut self.mergeable_state,
                "mergeable_state",
                from.clone(),
                to.clone(),
            ),
            ForgeSyncedChange::InMergeQueue { from, to } => {
                set(&mut self.in_merge_queue, "in_merge_queue", *from, *to)
            }
            ForgeSyncedChange::Checks { from, to } => {
                set(&mut self.checks, "checks", from.clone(), to.clone())
            }
            ForgeSyncedChange::ReviewState { from, to } => set(
                &mut self.review_state,
                "review_state",
                from.clone(),
                to.clone(),
            ),
            ForgeSyncedChange::StateReason { .. } => Err(FoldError::Precondition(
                "state_reason does not apply to a pull request".to_owned(),
            )),
        }
    }

    fn complete(&self, repo: &RepoSlug, number: u64) -> Option<PullRequestRef> {
        Some(PullRequestRef {
            repo: repo.clone(),
            number,
            node_id: self.node_id.clone(),
            title: self.title.clone()?,
            state: self.state.clone()?,
            head_sha: self.head_sha?,
            base_ref: self.base_ref.clone()?,
            base_sha: self.base_sha?,
            draft: self.draft?,
            mergeable_state: self.mergeable_state.clone()?,
            in_merge_queue: self.in_merge_queue?,
            checks: self.checks.clone()?,
            review_state: self.review_state.clone()?,
            synced_at: self.synced_at?,
        })
    }
}

impl IssueSnapshotRow {
    fn apply(&mut self, change: &ForgeSyncedChange) -> Result<(), FoldError> {
        match change {
            ForgeSyncedChange::State { from, to } => set(
                &mut self.state,
                "state",
                issue_state(*from)?,
                issue_state(*to)?,
            ),
            ForgeSyncedChange::StateReason { from, to } => {
                set(&mut self.state_reason, "state_reason", *from, *to)
            }
            ForgeSyncedChange::Title { from, to } => {
                set(&mut self.title, "title", from.clone(), to.clone())
            }
            _ => Err(FoldError::Precondition(
                "only state, state_reason and title apply to an issue".to_owned(),
            )),
        }
    }

    fn complete(&self, repo: &RepoSlug, number: u64) -> Option<IssueRef> {
        Some(IssueRef {
            repo: repo.clone(),
            number,
            node_id: self.node_id.clone(),
            title: self.title.clone()?,
            state: self.state.clone()?,
            state_reason: self.state_reason.flatten().map(reason_str),
            synced_at: self.synced_at?,
        })
    }
}

/// 写入一列：本地已有值时要求 `from` 与之相等。
fn set<T: PartialEq + Debug>(
    slot: &mut Option<T>,
    field: &str,
    from: T,
    to: T,
) -> Result<(), FoldError> {
    if let Some(current) = slot.as_ref() {
        if *current != from {
            return Err(FoldError::Precondition(format!(
                "forge.synced {field}: from {from:?} does not match snapshot {current:?}"
            )));
        }
    }
    *slot = Some(to);
    Ok(())
}

fn check_node(current: &str, incoming: &str) -> Result<(), FoldError> {
    if current != incoming {
        return Err(FoldError::Precondition(format!(
            "forge.synced node_id {incoming} does not match snapshot {current}"
        )));
    }
    Ok(())
}

fn pr_state(state: ForgeSyncedState) -> PullRequestState {
    match state {
        ForgeSyncedState::Open => PullRequestState::Open,
        ForgeSyncedState::Closed => PullRequestState::Closed,
        ForgeSyncedState::Merged => PullRequestState::Merged,
    }
}

fn issue_state(state: ForgeSyncedState) -> Result<IssueState, FoldError> {
    match state {
        ForgeSyncedState::Open => Ok(IssueState::Open),
        ForgeSyncedState::Closed => Ok(IssueState::Closed),
        ForgeSyncedState::Merged => Err(FoldError::Precondition(
            "an issue cannot be merged".to_owned(),
        )),
    }
}

/// `IssueRef.state_reason` 仍是 `Option<String>`（event-data §2.2.5.1 记为待对齐），
/// 取值与 `IssueStateReason` 的 serde 名一致。
fn reason_str(reason: IssueStateReason) -> String {
    match reason {
        IssueStateReason::Completed => "completed",
        IssueStateReason::NotPlanned => "not_planned",
    }
    .to_owned()
}

fn decode<T: gantry_core::events::EventData>(event: &Event<Value>) -> Result<T, FoldError> {
    gantry_core::events::decode_event_data(event).map_err(FoldError::Data)
}
