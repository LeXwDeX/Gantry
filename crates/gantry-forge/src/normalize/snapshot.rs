//! forge 自己的 PR / Issue 快照（forge.md §7.3 `forge_snapshot_ext` 的 M1 形状）。
//!
//! 快照存在模块私有表 `forge_snapshot`，与事件追加同事务写。它是"上次回读值"，归一化用
//! "回读值 − 快照"做差产生事件；store 的 `pr_snapshots` / `issue_snapshots` 由同一批
//! `forge.synced` 折叠，两边从同一差值出发。
//!
//! shortcut: `head_epoch` 以快照为缓存。P-61 要求快照重建时从事件日志取最大值；快照重建
//! 随对账切片实现（M1 内快照不会被丢弃）。

use gantry_core::canon::GitOid;
use gantry_core::entities::{CheckSummary, ReviewState};
use gantry_core::events::{ForgeSyncedChange, ForgeSyncedInitial, ForgeSyncedState};
use gantry_core::forge::IssueStateReason;
use gantry_core::ids::ProjectId;
use gantry_core::text::UntrustedText;
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_TABLE: &str = "forge_snapshot";

pub fn pr_key(project: &ProjectId, number: u64) -> String {
    format!("pr|{project}|{number:020}")
}

pub fn pr_prefix(project: &ProjectId) -> String {
    format!("pr|{project}|")
}

pub fn issue_key(project: &ProjectId, number: u64) -> String {
    format!("issue|{project}|{number:020}")
}

pub fn branch_key(project: &ProjectId, branch: &str) -> String {
    format!("branch|{project}|{branch}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnap {
    pub number: u64,
    pub node_id: String,
    pub state: ForgeSyncedState,
    pub title: String,
    pub body: String,
    pub head_ref: String,
    pub head_sha: GitOid,
    pub base_ref: String,
    pub base_sha: GitOid,
    pub draft: bool,
    pub labels: Vec<String>,
    pub mergeable_state: String,
    pub head_epoch: u64,
    /// §7.5 第 7 点的口径（REST reviews 推出）；旧行没有该字段时取 `none`。
    #[serde(default = "no_review")]
    pub review_state: ReviewState,
}

fn no_review() -> ReviewState {
    ReviewState::None
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSnap {
    pub number: u64,
    pub node_id: String,
    pub state: ForgeSyncedState,
    pub state_reason: Option<IssueStateReason>,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSnap {
    pub sha: GitOid,
}

fn title_text(title: &str, kind: &str) -> UntrustedText {
    UntrustedText::new(title.to_owned(), format!("github:{kind}.title"), false)
}

/// M1 还没有回读 checks 汇总（需要 GraphQL / check-runs 回读）：快照用保守值。
fn pending_checks() -> CheckSummary {
    CheckSummary {
        passed: 0,
        failed: 0,
        pending: 0,
        required_ok: false,
    }
}

impl PrSnap {
    pub fn initial(&self) -> ForgeSyncedInitial {
        ForgeSyncedInitial::Pr {
            state: self.state,
            title: title_text(&self.title, "pr"),
            head_sha: self.head_sha,
            base_ref: self.base_ref.clone(),
            base_sha: self.base_sha,
            draft: self.draft,
            mergeable_state: self.mergeable_state.clone(),
            in_merge_queue: false,
            checks: pending_checks(),
            review_state: self.review_state.clone(),
        }
    }

    /// 白名单字段的差值，按 event-data §2.2.5.1 的字段顺序。
    pub fn changes(&self, next: &PrSnap) -> Vec<ForgeSyncedChange> {
        let mut out = Vec::new();
        if self.state != next.state {
            out.push(ForgeSyncedChange::State {
                from: self.state,
                to: next.state,
            });
        }
        if self.title != next.title {
            out.push(ForgeSyncedChange::Title {
                from: title_text(&self.title, "pr"),
                to: title_text(&next.title, "pr"),
            });
        }
        if self.head_sha != next.head_sha {
            out.push(ForgeSyncedChange::HeadSha {
                from: self.head_sha,
                to: next.head_sha,
            });
        }
        if self.base_ref != next.base_ref {
            out.push(ForgeSyncedChange::BaseRef {
                from: self.base_ref.clone(),
                to: next.base_ref.clone(),
            });
        }
        if self.base_sha != next.base_sha {
            out.push(ForgeSyncedChange::BaseSha {
                from: self.base_sha,
                to: next.base_sha,
            });
        }
        if self.draft != next.draft {
            out.push(ForgeSyncedChange::Draft {
                from: self.draft,
                to: next.draft,
            });
        }
        if self.mergeable_state != next.mergeable_state {
            out.push(ForgeSyncedChange::MergeableState {
                from: self.mergeable_state.clone(),
                to: next.mergeable_state.clone(),
            });
        }
        if self.review_state != next.review_state {
            out.push(ForgeSyncedChange::ReviewState {
                from: self.review_state.clone(),
                to: next.review_state.clone(),
            });
        }
        out
    }
}

impl IssueSnap {
    pub fn initial(&self) -> ForgeSyncedInitial {
        ForgeSyncedInitial::Issue {
            state: self.state,
            state_reason: self.state_reason,
            title: title_text(&self.title, "issue"),
        }
    }

    pub fn changes(&self, next: &IssueSnap) -> Vec<ForgeSyncedChange> {
        let mut out = Vec::new();
        if self.state != next.state {
            out.push(ForgeSyncedChange::State {
                from: self.state,
                to: next.state,
            });
        }
        if self.state_reason != next.state_reason {
            out.push(ForgeSyncedChange::StateReason {
                from: self.state_reason,
                to: next.state_reason,
            });
        }
        if self.title != next.title {
            out.push(ForgeSyncedChange::Title {
                from: title_text(&self.title, "issue"),
                to: title_text(&next.title, "issue"),
            });
        }
        out
    }
}
