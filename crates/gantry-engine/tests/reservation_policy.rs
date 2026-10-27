//! 同批次预算平分（engine.md §7.9）。

use gantry_core::entities::{BudgetLedger, BudgetLimits, BudgetSubject, BudgetUsed, Tier};
use gantry_core::time::Timestamp;
use gantry_engine::budget::EqualShareReservation;
use gantry_engine::decide::ReservationPolicy;

fn ledger() -> BudgetLedger {
    let now = Timestamp::from_millis(1_700_000_000_000);
    BudgetLedger {
        id: "bdg_01JB7M4Q1T5X9V2N6R8K3W7YZC".parse().unwrap(),
        subject: BudgetSubject::Delivery,
        delivery_id: Some("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap()),
        conversation_id: None,
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap(),
        day: None,
        template: None,
        limits: BudgetLimits::delivery(Default::default(), 0, 0, 0, 0, 0),
        used: BudgetUsed::default(),
        config_rev: None,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn each_tier_uses_its_own_remaining_tokens() {
    let mut ledger = ledger();
    ledger.limits.tokens_by_tier = [
        (Tier::Utility, 11),
        (Tier::Planner, 30),
        (Tier::Worker, 100),
    ]
    .into_iter()
    .collect();
    ledger.used.tokens_by_tier = [(Tier::Utility, 2), (Tier::Planner, 10), (Tier::Worker, 70)]
        .into_iter()
        .collect();

    let policy = EqualShareReservation;
    for (tier, remaining) in [(Tier::Utility, 9), (Tier::Planner, 20), (Tier::Worker, 30)] {
        assert_eq!(policy.reserve(&ledger, tier, 0), 0);
        assert_eq!(policy.reserve(&ledger, tier, 1), remaining);
        assert_eq!(policy.reserve(&ledger, tier, 2), remaining / 2);
    }
}

#[test]
fn missing_and_exhausted_limits_return_zero() {
    let mut ledger = ledger();
    ledger.limits.tokens_by_tier.insert(Tier::Planner, 10);
    ledger.limits.tokens_by_tier.insert(Tier::Worker, 10);
    ledger.used.tokens_by_tier.insert(Tier::Utility, 5);
    ledger.used.tokens_by_tier.insert(Tier::Planner, 10);
    ledger.used.tokens_by_tier.insert(Tier::Worker, 11);

    for tier in [Tier::Utility, Tier::Planner, Tier::Worker] {
        assert_eq!(EqualShareReservation.reserve(&ledger, tier, 1), 0);
        assert_eq!(EqualShareReservation.reserve(&ledger, tier, 2), 0);
    }
}

#[test]
fn maximum_values_do_not_overflow() {
    let mut ledger = ledger();
    ledger.limits.tokens_by_tier.insert(Tier::Worker, u64::MAX);

    let policy = EqualShareReservation;
    assert_eq!(policy.reserve(&ledger, Tier::Worker, 1), u64::MAX);
    assert_eq!(policy.reserve(&ledger, Tier::Worker, 2), u64::MAX / 2);
    assert_eq!(
        policy.reserve(&ledger, Tier::Worker, usize::MAX),
        (u64::MAX as u128 / usize::MAX as u128) as u64
    );

    ledger
        .used
        .tokens_by_tier
        .insert(Tier::Worker, u64::MAX - 1);
    assert_eq!(policy.reserve(&ledger, Tier::Worker, 1), 1);
    assert_eq!(policy.reserve(&ledger, Tier::Worker, 2), 0);
}
