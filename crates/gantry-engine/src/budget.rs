//! 交付台账与预算检查（engine.md §7.9）。
//!
//! 只实现已定语义：创建时固化 limits / used 全零、按维度检查“达到上限即耗尽”、
//! 按维度累加已用，以及根据台账计算同批次的平分预留。
//! `concurrency` 没有已用计数，由调度器用在途数比较 `limits.concurrency`。

use gantry_core::entities::{
    BudgetDimension, BudgetLedger, BudgetLimits, BudgetSubject, BudgetUsed, Tier,
};
use gantry_core::events::DeliveryCreatedData;
use gantry_core::ids::{BudgetId, DeliveryId, ProjectId};
use gantry_core::time::Timestamp;

use crate::decide::ReservationPolicy;

/// 根据台账的剩余额度平分同批次尝试；不记录在途预留。
#[derive(Clone, Copy, Debug, Default)]
pub struct EqualShareReservation;

impl ReservationPolicy for EqualShareReservation {
    fn reserve(&self, ledger: &BudgetLedger, tier: Tier, concurrent_in_batch: usize) -> u64 {
        u64::try_from(concurrent_in_batch)
            .ok()
            .and_then(|count| {
                remaining(ledger, BudgetDimension::TokensByTier(tier)).checked_div(count)
            })
            .unwrap_or(0)
    }
}

/// 从 `delivery.created` 固化交付台账（P-57）。
pub fn ledger_for_delivery(
    id: BudgetId,
    project_id: ProjectId,
    delivery_id: DeliveryId,
    data: &DeliveryCreatedData,
    now: Timestamp,
) -> BudgetLedger {
    BudgetLedger {
        id,
        subject: BudgetSubject::Delivery,
        delivery_id: Some(delivery_id),
        conversation_id: None,
        project_id,
        day: None,
        template: Some(data.budget.template.clone()),
        limits: data.budget.limits.clone(),
        used: BudgetUsed::default(),
        config_rev: Some(data.config_rev),
        created_at: now,
        updated_at: now,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetExceeded {
    pub dimension: BudgetDimension,
    pub limit: u64,
    pub used: u64,
}

fn limit_for(limits: &BudgetLimits, dimension: BudgetDimension) -> u64 {
    match dimension {
        BudgetDimension::TokensByTier(tier) => {
            limits.tokens_by_tier.get(&tier).copied().unwrap_or(0)
        }
        BudgetDimension::Nodes => limits.nodes,
        BudgetDimension::Rounds => limits.rounds,
        BudgetDimension::WallTime => limits.wall_time,
        BudgetDimension::Concurrency => limits.concurrency,
        BudgetDimension::ForgeWrites => limits.forge_writes,
    }
}

fn used_for(used: &BudgetUsed, dimension: BudgetDimension) -> u64 {
    match dimension {
        BudgetDimension::TokensByTier(tier) => used.tokens_by_tier.get(&tier).copied().unwrap_or(0),
        BudgetDimension::Nodes => used.nodes,
        BudgetDimension::Rounds => used.rounds,
        BudgetDimension::WallTime => used.wall_time,
        BudgetDimension::Concurrency => 0,
        BudgetDimension::ForgeWrites => used.forge_writes,
    }
}

/// “任一维度达到上限即耗尽”（§7.9）。`concurrency` 由调度器单独检查。
pub fn check(ledger: &BudgetLedger, dimension: BudgetDimension) -> Result<(), BudgetExceeded> {
    if dimension == BudgetDimension::Concurrency {
        return Ok(());
    }
    let limit = limit_for(&ledger.limits, dimension);
    let used = used_for(&ledger.used, dimension);
    if used >= limit {
        Err(BudgetExceeded {
            dimension,
            limit,
            used,
        })
    } else {
        Ok(())
    }
}

/// 记账：用量是已发生的事实（Egress 计量、已创建的节点），一律累加，不因超额拒绝；
/// 记账后达到或超过上限时返回耗尽信息，由调用方写 `budget.exhausted`（§7.9）。
/// 是否允许发起新的消耗由调用方事先用 `check` / `remaining` 判断。
pub fn charge(
    ledger: &mut BudgetLedger,
    dimension: BudgetDimension,
    delta: u64,
) -> Option<BudgetExceeded> {
    if dimension == BudgetDimension::Concurrency {
        return None;
    }
    let limit = limit_for(&ledger.limits, dimension);
    let next = used_for(&ledger.used, dimension).saturating_add(delta);
    match dimension {
        BudgetDimension::TokensByTier(tier) => {
            ledger.used.tokens_by_tier.insert(tier, next);
        }
        BudgetDimension::Nodes => ledger.used.nodes = next,
        BudgetDimension::Rounds => ledger.used.rounds = next,
        BudgetDimension::WallTime => ledger.used.wall_time = next,
        BudgetDimension::Concurrency => {}
        BudgetDimension::ForgeWrites => ledger.used.forge_writes = next,
    }
    (next >= limit).then_some(BudgetExceeded {
        dimension,
        limit,
        used: next,
    })
}

/// 剩余额度；已耗尽返回 0。
pub fn remaining(ledger: &BudgetLedger, dimension: BudgetDimension) -> u64 {
    limit_for(&ledger.limits, dimension).saturating_sub(used_for(&ledger.used, dimension))
}
