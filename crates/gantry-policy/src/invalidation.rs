//! 事件驱动的失效与过期清扫（policy.md §7.7）。
//!
//! `process_events` 是 `InvalidationSubscriber` 的一步：从持久游标（消费者
//! `"policy"`）之后读一批事件，对受影响的活跃审批做转移，并在**同一事务**里推进
//! 游标。转移受 `machines::approval::check` 约束、只对 pending / granted 生效，
//! 所以重放无副作用。`sweep_expired` 是 `ExpirySweeper` 的一步。两者的定时驱动
//! （`tokio::time::interval`）属于 `gantryd` 装配，不在本 crate。

use gantry_core::canon::{GitOid, RepoSlug, SubjectDigest};
use gantry_core::entities::{ApprovalAction, ApprovalStatus, ApprovalTarget, ForgeActionKind, Seq};
use gantry_core::events::{ApprovalInvalidatedData, Event, EventType};
use gantry_core::forge::ForgeEventKind;
use gantry_core::ids::DeliveryId;
use gantry_core::machines::approval as machine;
use gantry_core::ports::PolicyError;
use gantry_store::{EventFilter, TypePattern};
use serde_json::Value;

use crate::approvals::{self, port_error, Record};
use crate::engine::PolicyEngine;

/// 持久游标的消费者名（policy.md §3.1）。
pub const CURSOR_CONSUMER: &str = "policy";

/// 一批处理的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessReport {
    /// 本批读到的相关事件数。
    pub events: usize,
    /// 本批产生的 `approval.invalidated` 数。
    pub invalidated: usize,
    /// 处理后的游标位置。
    pub cursor: Seq,
}

/// 一条事件对审批的影响。
enum Effect {
    /// PR head 变化：绑定旧 head 的 `merge_pr` 审批失效。
    HeadChanged {
        repo: RepoSlug,
        number: u64,
        head: GitOid,
    },
    /// PR 关闭：该 PR 的全部活跃审批失效。
    PrClosed { repo: RepoSlug, number: u64 },
    /// PR 合并：pending 的失效，granted 的留给消费路径。
    PrMerged { repo: RepoSlug, number: u64 },
    /// 交付单元放弃：该交付单元的活跃审批失效。
    DeliveryAbandoned(DeliveryId),
}

fn filter() -> EventFilter {
    EventFilter {
        project_ids: None,
        include: vec![
            TypePattern::Exact(EventType::Forge(ForgeEventKind::PrSynchronized)),
            TypePattern::Exact(EventType::Forge(ForgeEventKind::PrClosed)),
            TypePattern::Exact(EventType::Forge(ForgeEventKind::PrMerged)),
            TypePattern::Exact(EventType::DeliveryAbandoned),
        ],
        exclude: Vec::new(),
    }
}

/// 从 forge 信封里取 `repo` 与 `target.number`（PR 类事件）。
fn pr_of(data: &Value) -> Option<(RepoSlug, u64)> {
    let repo = RepoSlug::parse(data.get("repo")?.as_str()?).ok()?;
    let target = data.get("target")?;
    if target.get("type")?.as_str()? != "pr" {
        return None;
    }
    Some((repo, target.get("number")?.as_u64()?))
}

fn effect(event: &Event<Value>) -> Option<Effect> {
    match event.ty {
        EventType::Forge(ForgeEventKind::PrSynchronized) => {
            let (repo, number) = pr_of(&event.data)?;
            let head = event.data.get("data")?.get("head_sha")?.as_str()?;
            Some(Effect::HeadChanged {
                repo,
                number,
                head: GitOid::parse(head).ok()?,
            })
        }
        EventType::Forge(ForgeEventKind::PrClosed) => {
            let (repo, number) = pr_of(&event.data)?;
            Some(Effect::PrClosed { repo, number })
        }
        EventType::Forge(ForgeEventKind::PrMerged) => {
            let (repo, number) = pr_of(&event.data)?;
            Some(Effect::PrMerged { repo, number })
        }
        EventType::DeliveryAbandoned => event
            .subject
            .as_str()
            .parse()
            .ok()
            .map(Effect::DeliveryAbandoned),
        _ => None,
    }
}

fn on_pr(record: &Record, repo: &RepoSlug, number: u64) -> bool {
    matches!(&record.approval.target,
        ApprovalTarget::Pr { repo: r, number: n } if r == repo && *n == number)
}

/// 该审批在这条事件下应转成的失效原因；不受影响返回 `None`。
fn invalidation(record: &Record, effect: &Effect) -> Option<ApprovalInvalidatedData> {
    if !record.is_live() {
        return None;
    }
    let approval = &record.approval;
    match effect {
        Effect::HeadChanged { repo, number, head } => {
            let is_merge = approval.action == ApprovalAction::Forge(ForgeActionKind::MergePr);
            if !is_merge || !on_pr(record, repo, *number) {
                return None;
            }
            machine::on_subject_changed(approval, &SubjectDigest::git(*head))?;
            let SubjectDigest::Git(previous) = approval.subject_digest else {
                return None;
            };
            Some(ApprovalInvalidatedData::HeadChanged {
                previous_head_sha: previous,
                current_head_sha: *head,
            })
        }
        Effect::PrClosed { repo, number } => {
            on_pr(record, repo, *number).then_some(ApprovalInvalidatedData::PrClosed)
        }
        Effect::PrMerged { repo, number } => (on_pr(record, repo, *number)
            && approval.status == ApprovalStatus::Pending)
            .then_some(ApprovalInvalidatedData::PrMerged),
        Effect::DeliveryAbandoned(delivery) => (approval.delivery_id == Some(*delivery))
            .then_some(ApprovalInvalidatedData::DeliveryAbandoned),
    }
}

impl PolicyEngine {
    /// 处理游标之后至多 `limit`（1..=200）条相关事件；转移与游标推进同一事务提交。
    pub async fn process_events(&self, limit: u32) -> Result<ProcessReport, PolicyError> {
        let log = self.store.events();
        let cursor = self
            .store
            .cursors()
            .load(CURSOR_CONSUMER)
            .await
            .map_err(port_error)?;
        let page = log
            .scan(cursor, &filter(), limit)
            .await
            .map_err(port_error)?;
        let mut records = approvals::load_all(&log).await?;

        let mut tx = self.store.begin_write().await.map_err(port_error)?;
        let mut invalidated = 0;
        for event in &page.events {
            let Some(effect) = effect(event) else {
                continue;
            };
            for record in records.iter_mut() {
                let Some(data) = invalidation(record, &effect) else {
                    continue;
                };
                *record = approvals::transition(
                    &mut tx,
                    record,
                    ApprovalStatus::Invalidated,
                    approvals::invalidated_data(data),
                    approvals::policy_actor(),
                    Some(event.id),
                )
                .await?;
                invalidated += 1;
            }
        }
        tx.advance_cursor(CURSOR_CONSUMER, page.scanned_to)
            .await
            .map_err(port_error)?;
        tx.commit().await.map_err(port_error)?;
        Ok(ProcessReport {
            events: page.events.len(),
            invalidated,
            cursor: page.scanned_to,
        })
    }

    /// 已到期仍为 pending / granted 的审批逐个转 `expired`；返回本次转移数。
    pub async fn sweep_expired(&self) -> Result<usize, PolicyError> {
        let now = self.clock.now();
        let log = self.store.events();
        let mut expired = 0;
        for record in approvals::load_all(&log).await? {
            if !record.is_due(now) {
                continue;
            }
            let next = approvals::expire_if_due(&self.store, record, now).await?;
            if next.approval.status == ApprovalStatus::Expired {
                expired += 1;
            }
        }
        Ok(expired)
    }
}
