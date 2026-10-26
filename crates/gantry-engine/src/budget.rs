//! 交付台账与预算检查（engine.md §7.9）。
//!
//! 只实现已定语义：创建时固化 limits / used 全零、按维度检查“达到上限即耗尽”、
//! 按维度累加已用。预留策略依赖 OQ-9 / worker-protocol OQ-14，尚未实现；
//! `concurrency` 没有已用计数，由调度器用在途数比较 `limits.concurrency`。

use gantry_core::entities::{
    BudgetDimension, BudgetLedger, BudgetLimits, BudgetSubject, BudgetUsed, Tier,
};
use gantry_core::events::DeliveryCreatedData;
use gantry_core::ids::{BudgetId, DeliveryId, ProjectId};
use gantry_core::time::Timestamp;

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

/// 记账并检查上限；达到或超过上限的增量被拒绝。
pub fn charge(
    ledger: &mut BudgetLedger,
    dimension: BudgetDimension,
    delta: u64,
) -> Result<(), BudgetExceeded> {
    if dimension == BudgetDimension::Concurrency {
        return Ok(());
    }
    let limit = limit_for(&ledger.limits, dimension);
    let used = used_for(&ledger.used, dimension);
    let next = used.saturating_add(delta);
    if next > limit {
        return Err(BudgetExceeded {
            dimension,
            limit,
            used,
        });
    }
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
    Ok(())
}

/// 剩余额度；已耗尽返回 0。
pub fn remaining(ledger: &BudgetLedger, dimension: BudgetDimension) -> u64 {
    limit_for(&ledger.limits, dimension).saturating_sub(used_for(&ledger.used, dimension))
}

/// 供测试与调用方构造 Low 档台账。
pub fn low_limits() -> BudgetLimits {
    let tokens = [
        (Tier::Utility, 0),
        (Tier::Planner, 0),
        (Tier::Worker, 1_000_000),
    ]
    .into_iter()
    .collect();
    BudgetLimits::delivery(tokens, 8, 0, 3600, 1, 20)
}
