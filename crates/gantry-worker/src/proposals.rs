//! proposals 的本地过滤（worker.md §7.5；worker-protocol §5.4）。
//!
//! 只为减少服务端拒收，**不替代**服务端校验（V6）。agent 给出的每一条都是不可信输入：
//! 解析不了、`kind` 不在 `allowed_kinds`、`target` 与 kind 不配或不在 `JobSpec.targets`、
//! `id` 重复、超过条数或大小上限的，一律丢弃并发 `warn(proposal_dropped)`。

use std::collections::BTreeSet;

use gantry_core::worker::{
    JobSpec, Proposal, ProposalKind, Proposals, ProposalsMode, TargetKind, WorkerProtocol,
};
use serde_json::Value;

/// `proposals.json` 的条数上限（worker.md §7.5）。
pub const MAX_PROPOSALS: usize = 20;
/// `proposals.json` 的大小上限。
pub const MAX_PROPOSALS_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// 不是合法的 Proposal（未知 kind、params 不配、多余字段……）。
    Malformed,
    /// 本 job 不允许提议，或 kind 不在 `allowed_kinds`。
    KindNotAllowed,
    /// `target` 不在 `JobSpec.targets`，或与 kind 不配。
    BadTarget,
    DuplicateId,
    /// 超过 `max` 或 20 条。
    OverCount,
    /// 超过 256 KiB。
    OverSize,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::KindNotAllowed => "kind_not_allowed",
            Self::BadTarget => "bad_target",
            Self::DuplicateId => "duplicate_id",
            Self::OverCount => "over_count",
            Self::OverSize => "over_size",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filtered {
    /// 无提议时为 `None`：不产生 `proposals.json`，`artifacts.proposals = null`。
    pub proposals: Option<Proposals>,
    /// 按输入顺序，被丢弃条目的下标与原因。
    pub dropped: Vec<(usize, DropReason)>,
}

pub fn filter_proposals(job: &JobSpec, raw: &[Value]) -> Filtered {
    let (allowed, max) = match &job.output.proposals {
        ProposalsMode::Forbidden => (Vec::new(), 0),
        ProposalsMode::Allowed { allowed_kinds, max } => {
            (allowed_kinds.clone(), (*max as usize).min(MAX_PROPOSALS))
        }
    };
    let mut kept: Vec<(usize, Proposal)> = Vec::new();
    let mut dropped = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, value) in raw.iter().enumerate() {
        let reason = match serde_json::from_value::<Proposal>(value.clone()) {
            Err(_) => Some(DropReason::Malformed),
            Ok(p) if p.validate().is_err() => Some(DropReason::Malformed),
            Ok(p) if !allowed.contains(&p.kind) => Some(DropReason::KindNotAllowed),
            Ok(p) if !target_ok(job, &p) => Some(DropReason::BadTarget),
            Ok(p) if ids.contains(&p.id) => Some(DropReason::DuplicateId),
            Ok(_) if kept.len() >= max => Some(DropReason::OverCount),
            Ok(p) => {
                ids.insert(p.id.clone());
                kept.push((index, p));
                None
            }
        };
        if let Some(reason) = reason {
            dropped.push((index, reason));
        }
    }
    // 超过大小上限时从尾部丢弃。
    loop {
        let size = encoded_len(job, kept.iter().map(|(_, p)| p.clone()).collect());
        if size <= MAX_PROPOSALS_BYTES || kept.is_empty() {
            break;
        }
        let (index, _) = kept.pop().expect("non-empty");
        dropped.push((index, DropReason::OverSize));
    }
    dropped.sort_by_key(|(index, _)| *index);
    let proposals = (!kept.is_empty()).then(|| Proposals {
        protocol: WorkerProtocol,
        job_id: job.job_id,
        proposals: kept.into_iter().map(|(_, p)| p).collect(),
    });
    Filtered { proposals, dropped }
}

/// `open_pr` / `escalate` 必须 `target = null`；线程类必须指向 `review_thread` 句柄。
fn target_ok(job: &JobSpec, proposal: &Proposal) -> bool {
    match proposal.kind {
        ProposalKind::OpenPr | ProposalKind::Escalate => proposal.target.is_none(),
        ProposalKind::ReplyReviewThread | ProposalKind::ResolveReviewThread => {
            proposal.target.as_ref().is_some_and(|handle| {
                job.targets
                    .iter()
                    .any(|t| &t.handle == handle && t.kind == TargetKind::ReviewThread)
            })
        }
    }
}

fn encoded_len(job: &JobSpec, proposals: Vec<Proposal>) -> usize {
    let doc = Proposals {
        protocol: WorkerProtocol,
        job_id: job.job_id,
        proposals,
    };
    serde_json::to_vec(&doc)
        .map(|b| b.len())
        .unwrap_or(usize::MAX)
}
