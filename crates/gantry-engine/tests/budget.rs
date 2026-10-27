//! 预算检查与记账（engine.md §7.9）。

use gantry_core::entities::BudgetLimits;
use gantry_core::entities::{BudgetDimension, Tier};
use gantry_core::events::DeliveryCreatedData;
use gantry_core::ids::{BudgetId, DeliveryId, ProjectId};
use gantry_core::level::Level;
use gantry_core::time::Timestamp;
use gantry_engine::budget::{charge, check, ledger_for_delivery, remaining};
use serde_json::json;

/// 测试夹具的限额，不代表 `levels.low` 的默认值（默认值见 settings/defaults/levels.json）。
fn low_limits() -> BudgetLimits {
    let tokens = [
        (Tier::Utility, 0),
        (Tier::Planner, 0),
        (Tier::Worker, 1_000_000),
    ]
    .into_iter()
    .collect();
    BudgetLimits::delivery(tokens, 8, 0, 3600, 1, 20)
}

fn created() -> DeliveryCreatedData {
    serde_json::from_value(json!({
        "title": "fix login timeout",
        "level_cap": "Low",
        "level_effective": "Low",
        "origin": "conversation",
        "source_message_id": null, "user_text": "修复登录超时",
        "issues": [],
        "budget": {"template": "low.single", "limits": low_limits()},
        "config_rev": 7
    }))
    .unwrap()
}

fn ledger() -> gantry_core::entities::BudgetLedger {
    ledger_for_delivery(
        "bdg_01JB7M4Q1T5X9V2N6R8K3W7YZC"
            .parse::<BudgetId>()
            .unwrap(),
        "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA"
            .parse::<ProjectId>()
            .unwrap(),
        "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
            .parse::<DeliveryId>()
            .unwrap(),
        &created(),
        Timestamp::from_millis(1_700_000_000_000),
    )
}

#[test]
fn ledger_is_fixed_at_creation() {
    let ledger = ledger();
    assert_eq!(ledger.template.as_deref(), Some("low.single"));
    assert_eq!(ledger.limits.nodes, 8);
    assert_eq!(ledger.used.nodes, 0);
    assert_eq!(ledger.config_rev.map(|seq| seq.as_u64()), Some(7));
    assert_eq!(ledger.limits.tokens_by_tier[&Tier::Worker], 1_000_000);
}

#[test]
fn budget_is_exhausted_when_used_reaches_limit() {
    let mut ledger = ledger();
    for n in 1..=8 {
        check(&ledger, BudgetDimension::Nodes).expect("below limit");
        let exhausted = charge(&mut ledger, BudgetDimension::Nodes, 1);
        assert_eq!(exhausted.is_some(), n == 8, "charge {n}");
    }
    let error = check(&ledger, BudgetDimension::Nodes).expect_err("8/8 must be exhausted");
    assert_eq!(error.limit, 8);
    assert_eq!(error.used, 8);
    assert_eq!(remaining(&ledger, BudgetDimension::Nodes), 0);
}

/// 计量用量是事实：超额也记账，并报告耗尽（§7.9；direct 调用可超支一次）。
#[test]
fn charge_records_overrun_and_reports_exhaustion() {
    let mut ledger = ledger();
    assert!(charge(&mut ledger, BudgetDimension::ForgeWrites, 19).is_none());
    let exhausted =
        charge(&mut ledger, BudgetDimension::ForgeWrites, 5).expect("over forge_writes");
    assert_eq!(exhausted.limit, 20);
    assert_eq!(exhausted.used, 24);
    assert_eq!(ledger.used.forge_writes, 24);
    assert_eq!(remaining(&ledger, BudgetDimension::ForgeWrites), 0);
}

#[test]
fn token_tiers_are_independent() {
    let mut ledger = ledger();
    assert!(charge(
        &mut ledger,
        BudgetDimension::TokensByTier(Tier::Worker),
        999_999,
    )
    .is_none());
    assert!(check(&ledger, BudgetDimension::TokensByTier(Tier::Worker)).is_ok());
    assert!(check(&ledger, BudgetDimension::TokensByTier(Tier::Planner)).is_err());
    assert!(charge(&mut ledger, BudgetDimension::TokensByTier(Tier::Worker), 1).is_some());
    assert!(check(&ledger, BudgetDimension::TokensByTier(Tier::Worker)).is_err());
}

#[test]
fn wall_time_and_rounds_use_the_same_rule() {
    let mut ledger = ledger();
    assert!(charge(&mut ledger, BudgetDimension::WallTime, 3600).is_some());
    assert!(check(&ledger, BudgetDimension::WallTime).is_err());
    assert!(check(&ledger, BudgetDimension::Rounds).is_err());
}

#[test]
fn concurrency_is_left_to_the_scheduler() {
    let mut ledger = ledger();
    assert!(check(&ledger, BudgetDimension::Concurrency).is_ok());
    assert!(charge(&mut ledger, BudgetDimension::Concurrency, 99).is_none());
    assert_eq!(ledger.used.nodes, 0);
}

#[test]
fn level_cap_is_recorded_but_not_used_for_limits() {
    let data = created();
    assert_eq!(data.level_cap, Level::Low);
    let ledger = ledger();
    assert_eq!(ledger.limits.nodes, low_limits().nodes);
}
