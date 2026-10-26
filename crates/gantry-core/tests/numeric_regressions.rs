//! 数值边界回归：预算总量、跨 tier 预留、整数安全、时间范围。
//! 不依赖 `testing` feature，默认 `cargo test` 即可运行。

use std::collections::BTreeMap;

use gantry_core::canon::{
    jcs, ApprovalPayload, CanonError, GitOid, IdempotencyKey, Nonce, RepoSlug, SubjectDigest,
    APPROVAL_SIGNING_PREFIX,
};
use gantry_core::entities::*;
use gantry_core::ids::*;
use gantry_core::json_strict::{parse_json_strict, JsonLimits};
use gantry_core::time::{Date, Timestamp};
use serde_json::json;

fn project_id() -> ProjectId {
    "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK".parse().unwrap()
}

fn delivery_id() -> DeliveryId {
    "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM".parse().unwrap()
}

fn conversation_id() -> ConversationId {
    "cnv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap()
}

fn budget_id() -> BudgetId {
    "bdg_01JB7Q1S4V6Y8A0C2E4G6J8KMP".parse().unwrap()
}

fn conversation_ledger(used: BudgetUsed, limit: u64) -> BudgetLedger {
    BudgetLedger {
        id: budget_id(),
        subject: BudgetSubject::Conversation,
        delivery_id: None,
        conversation_id: Some(conversation_id()),
        project_id: project_id(),
        day: Some("2026-10-24".parse().unwrap()),
        template: None,
        limits: BudgetLimits::conversation(limit),
        used,
        config_rev: None,
        created_at: Timestamp::from_millis(0),
        updated_at: Timestamp::from_millis(0),
    }
}

fn delivery_ledger(limits: BudgetLimits, used: BudgetUsed) -> BudgetLedger {
    BudgetLedger {
        id: budget_id(),
        subject: BudgetSubject::Delivery,
        delivery_id: Some(delivery_id()),
        conversation_id: None,
        project_id: project_id(),
        day: None,
        template: Some("low.single".into()),
        limits,
        used,
        config_rev: Some(Seq::new(1).unwrap()),
        created_at: Timestamp::from_millis(0),
        updated_at: Timestamp::from_millis(0),
    }
}

fn delta(tier: Tier, weighted_tokens: u64) -> BudgetDelta {
    BudgetDelta {
        tokens: TokenUsage::new(weighted_tokens, 0, 0, 0, 1),
        tier,
        ..BudgetDelta::default()
    }
}

fn used_by_tier(entries: &[(Tier, u64)]) -> BudgetUsed {
    BudgetUsed {
        tokens_by_tier: entries.iter().copied().collect::<BTreeMap<_, _>>(),
        ..BudgetUsed::default()
    }
}

#[test]
fn conversation_budget_uses_total_across_all_tiers() {
    let weights = TokenWeights::default();

    let mut ledger = conversation_ledger(used_by_tier(&[(Tier::Utility, 60)]), 100);
    let planner = delta(Tier::Planner, 30);
    assert!(ledger.would_exceed(&planner, &weights).is_empty());
    assert!(ledger.apply(&planner, &weights).is_empty());
    assert_eq!(ledger.used.total_tokens(), 90);
    assert_eq!(ledger.used.nodes, 0);

    let worker = delta(Tier::Worker, 30);
    assert_eq!(
        ledger.would_exceed(&worker, &weights),
        vec![BudgetDimension::TokensByTier(Tier::Worker)]
    );
    assert_eq!(
        ledger.apply(&worker, &weights),
        vec![BudgetDimension::TokensByTier(Tier::Worker)]
    );
    assert_eq!(ledger.used.total_tokens(), 120);
    assert!(ledger.is_exhausted(BudgetDimension::TokensByTier(Tier::Utility), &weights));
}

#[test]
fn conversation_budget_boundary_is_exhausted_but_not_exceeded() {
    let weights = TokenWeights::default();
    let mut ledger = conversation_ledger(used_by_tier(&[(Tier::Utility, 60)]), 100);
    let planner = delta(Tier::Planner, 40);
    assert!(ledger.would_exceed(&planner, &weights).is_empty());
    assert_eq!(
        ledger.apply(&planner, &weights),
        vec![BudgetDimension::TokensByTier(Tier::Planner)]
    );
    assert_eq!(ledger.used.total_tokens(), 100);
    assert!(ledger.is_exhausted(BudgetDimension::TokensByTier(Tier::Planner), &weights));
}

#[test]
fn delivery_would_exceed_does_not_charge_other_tiers() {
    let weights = TokenWeights::default();
    let mut ledger = delivery_ledger(
        BudgetLimits::delivery(
            BTreeMap::from([
                (Tier::Utility, 100),
                (Tier::Planner, 95),
                (Tier::Worker, 1000),
            ]),
            10,
            10,
            1000,
            2,
            10,
        ),
        used_by_tier(&[(Tier::Utility, 90), (Tier::Planner, 90)]),
    );

    let worker = delta(Tier::Worker, 20);
    assert!(ledger.would_exceed(&worker, &weights).is_empty());
    assert!(ledger.apply(&worker, &weights).is_empty());
    assert_eq!(ledger.used.tokens_by_tier.get(&Tier::Utility), Some(&90));
    assert_eq!(ledger.used.tokens_by_tier.get(&Tier::Planner), Some(&90));
    assert_eq!(ledger.used.tokens_by_tier.get(&Tier::Worker), Some(&20));

    let utility = delta(Tier::Utility, 20);
    assert_eq!(
        ledger.would_exceed(&utility, &weights),
        vec![BudgetDimension::TokensByTier(Tier::Utility)]
    );
    let boundary = delta(Tier::Utility, 10);
    assert!(ledger.would_exceed(&boundary, &weights).is_empty());
}

#[test]
fn delivery_budget_rejects_daily_token_limit() {
    let limits = BudgetLimits::delivery(BTreeMap::from([(Tier::Worker, 300)]), 2, 1, 100, 1, 2);
    let mut ledger = delivery_ledger(limits, BudgetUsed::default());
    assert!(ledger.validate().is_ok());
    ledger.limits.tokens = Some(100);
    assert_eq!(ledger.validate(), Err(EntityError::BudgetSubject));

    let conversation = conversation_ledger(BudgetUsed::default(), 100);
    assert!(conversation.validate().is_ok());
}

#[test]
fn weighted_tokens_saturate_on_multiplication_overflow() {
    let weights = TokenWeights {
        input: 1e308,
        output: 1.0,
        cache_read: 1.0,
        cache_write: 1.0,
    };
    for input in [1, 2, 1_000_000, u64::MAX] {
        assert_eq!(
            TokenUsage::new(input, 0, 0, 0, 1).weighted(&weights),
            u64::MAX,
            "input={input}"
        );
    }
}

#[test]
fn weighted_tokens_saturate_on_sum_overflow() {
    let weights = TokenWeights {
        input: 1e308,
        output: 1e308,
        cache_read: 1e308,
        cache_write: 1e308,
    };
    assert_eq!(
        TokenUsage::new(10, 10, 10, 10, 1).weighted(&weights),
        u64::MAX
    );
}

#[test]
fn weighted_tokens_are_monotonic_for_legal_weights() {
    let saturated = TokenWeights {
        input: 1e308,
        output: 1e308,
        cache_read: 1e308,
        cache_write: 1e308,
    };
    for weights in [TokenWeights::default(), saturated] {
        let mut previous = 0;
        for input in [0, 1, 2, 10, 1_000_000, u64::MAX] {
            let weighted = TokenUsage::new(input, 0, 0, 0, 1).weighted(&weights);
            assert!(weighted >= previous, "input={input}");
            previous = weighted;
        }
    }
}

#[test]
fn weighted_tokens_keep_rounding_and_known_boundaries() {
    let default = TokenWeights::default();
    assert_eq!(TokenUsage::new(0, 0, 0, 0, 1).weighted(&default), 0);
    assert_eq!(TokenUsage::new(3, 2, 0, 0, 1).weighted(&default), 13);
    assert_eq!(TokenUsage::new(0, 0, 15, 0, 1).weighted(&default), 2);

    let half = TokenWeights {
        input: 0.5,
        output: 1.0,
        cache_read: 1.0,
        cache_write: 1.0,
    };
    assert_eq!(TokenUsage::new(1, 0, 0, 0, 1).weighted(&half), 1);
    let below_half = TokenWeights {
        input: 0.49,
        ..half
    };
    assert_eq!(TokenUsage::new(1, 0, 0, 0, 1).weighted(&below_half), 0);

    let unit = TokenWeights {
        input: 1.0,
        output: 1.0,
        cache_read: 1.0,
        cache_write: 1.0,
    };
    assert_eq!(
        TokenUsage::new(1 << 53, 0, 0, 0, 1).weighted(&unit),
        1 << 53
    );
    assert_eq!(
        TokenUsage::new(u64::MAX, 0, 0, 0, 1).weighted(&unit),
        u64::MAX
    );
}

#[test]
fn budget_ledger_records_saturated_weighted_tokens() {
    let weights = TokenWeights {
        input: 1e308,
        output: 1.0,
        cache_read: 1.0,
        cache_write: 1.0,
    };
    let mut ledger = conversation_ledger(BudgetUsed::default(), u64::MAX);
    let usage = delta(Tier::Planner, 2);

    assert!(ledger.would_exceed(&usage, &weights).is_empty());
    assert_eq!(
        ledger.apply(&usage, &weights),
        vec![BudgetDimension::TokensByTier(Tier::Planner)]
    );
    assert_eq!(
        ledger.used.tokens_by_tier.get(&Tier::Planner),
        Some(&u64::MAX)
    );
    assert_eq!(ledger.used.total_tokens(), u64::MAX);

    assert!(ledger.apply(&usage, &weights).is_empty());
    assert_eq!(
        ledger.used.tokens_by_tier.get(&Tier::Planner),
        Some(&u64::MAX)
    );
}

#[test]
fn strict_json_rejects_unsafe_integer_float_forms() {
    let limits = JsonLimits::default();
    for text in [
        "9007199254740992e0",
        "9007199254740993e0",
        "-9007199254740992e0",
        "-9007199254740993e0",
        "9007199254740992.0",
        "-9007199254740992.0",
        "1e16",
        "1e19",
    ] {
        assert_eq!(
            parse_json_strict(text.as_bytes(), limits).unwrap_err().code,
            "unsafe_integer",
            "{text} must be rejected"
        );
    }

    for text in [
        "9007199254740991e0",
        "-9007199254740991e0",
        "9007199254740991.0",
        "1.5",
        "2e-3",
        "4.50",
        "1e30",
        "1e300",
    ] {
        assert!(
            parse_json_strict(text.as_bytes(), limits).is_ok(),
            "{text} must be accepted"
        );
    }

    assert_eq!(
        parse_json_strict(b"1e9999", limits).unwrap_err().code,
        "nan_or_infinity"
    );
    assert_eq!(
        parse_json_strict(b"9007199254740993", limits)
            .unwrap_err()
            .code,
        "unsafe_integer"
    );
    assert!(parse_json_strict(b"-9007199254740991", limits).is_ok());
}

#[test]
fn jcs_rejects_unsafe_integers_and_keeps_large_floats() {
    assert!(matches!(
        jcs(&json!({"nested": [9007199254740993u64]})),
        Err(CanonError::UnsafeInteger)
    ));
    assert!(matches!(
        jcs(&json!([9007199254740992u64])),
        Err(CanonError::UnsafeInteger)
    ));
    assert!(matches!(
        jcs(&json!([-9007199254740992i64])),
        Err(CanonError::UnsafeInteger)
    ));
    assert!(matches!(
        jcs(&json!([u64::MAX])),
        Err(CanonError::UnsafeInteger)
    ));
    assert!(jcs(&json!([9007199254740991u64])).is_ok());
    assert!(jcs(&json!([-9007199254740991i64])).is_ok());

    // RFC 8785 的 ES6 数字形式仍然保留；f64 大浮点数不受整数检查影响。
    assert_eq!(
        String::from_utf8(jcs(&json!([1E30, 4.50, 2e-3])).unwrap()).unwrap(),
        "[1e+30,4.5,0.002]"
    );
}

fn approval_payload(number: u64) -> ApprovalPayload {
    ApprovalPayload {
        action: ApprovalAction::Forge(ForgeActionKind::MergePr),
        approval_id: "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK".parse().unwrap(),
        delivery_id: Some(delivery_id()),
        device_id: "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().unwrap(),
        expires_at: Timestamp::parse_canonical("2026-10-24T12:52:00.000Z").unwrap(),
        nonce: Nonce::parse("lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js").unwrap(),
        subject_digest: SubjectDigest::Git(
            GitOid::parse("9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345").unwrap(),
        ),
        target: ApprovalTarget::Pr {
            repo: RepoSlug::parse("acme/web").unwrap(),
            number,
        },
        v: 1,
    }
}

#[test]
fn approval_signing_rejects_unsafe_target_numbers() {
    let safe = approval_payload(12).signing_bytes().unwrap();
    assert!(safe.starts_with(APPROVAL_SIGNING_PREFIX));

    assert!(matches!(
        approval_payload(9_007_199_254_740_992).signing_bytes(),
        Err(CanonError::UnsafeInteger)
    ));
    assert!(matches!(
        approval_payload(9_007_199_254_740_993).signing_bytes(),
        Err(CanonError::UnsafeInteger)
    ));
}

#[test]
fn idempotency_key_rejects_unsafe_intent_integers() {
    let derive = |intent: &serde_json::Value| {
        IdempotencyKey::derive(
            &project_id(),
            &delivery_id(),
            ForgeActionKind::CreatePr,
            "pr:gantry/42-fix-pagination->main",
            intent,
            0,
        )
    };
    assert!(derive(&json!({"n": 42})).is_ok());
    assert!(matches!(
        derive(&json!({"n": 9_007_199_254_740_992u64})),
        Err(CanonError::UnsafeInteger)
    ));
    assert!(matches!(
        derive(&json!({"n": 9_007_199_254_740_993u64})),
        Err(CanonError::UnsafeInteger)
    ));
}

#[test]
fn timestamp_serialization_stays_inside_schema_range() {
    assert_eq!(
        serde_json::to_string(&Timestamp::from_millis(Timestamp::MIN_MILLIS)).unwrap(),
        "\"0000-01-01T00:00:00.000Z\""
    );
    assert_eq!(
        serde_json::to_string(&Timestamp::from_millis(Timestamp::MAX_MILLIS)).unwrap(),
        "\"9999-12-31T23:59:59.999Z\""
    );
    assert_eq!(
        serde_json::from_str::<Timestamp>("\"0000-01-01T00:00:00.000Z\"")
            .unwrap()
            .as_millis(),
        Timestamp::MIN_MILLIS
    );
    assert_eq!(
        serde_json::from_str::<Timestamp>("\"9999-12-31T23:59:59.999Z\"")
            .unwrap()
            .as_millis(),
        Timestamp::MAX_MILLIS
    );

    for ms in [
        Timestamp::MIN_MILLIS - 1,
        Timestamp::MAX_MILLIS + 1,
        i64::MIN,
        i64::MAX,
    ] {
        assert!(
            serde_json::to_string(&Timestamp::from_millis(ms)).is_err(),
            "{ms} must not serialize outside the schema range"
        );
        assert!(!Timestamp::from_millis(ms).is_representable());
    }

    assert!(Timestamp::from_millis(Timestamp::MAX_MILLIS)
        .checked_add_secs(1)
        .is_none());
    assert!(Timestamp::from_millis(i64::MAX).to_date().year() <= 9999);
    assert!(Timestamp::from_millis(i64::MIN).to_date().year() >= 0);
}

#[test]
fn date_rejects_years_outside_four_digits() {
    assert_eq!(
        Date::from_ymd(10_000, 1, 1),
        Err(gantry_core::time::TimeError::InvalidDate)
    );
    assert!(Date::from_ymd(-1, 1, 1).is_err());

    let first = Date::from_ymd(0, 1, 1).unwrap();
    assert_eq!(first.to_string(), "0000-01-01");
    assert_eq!(serde_json::to_string(&first).unwrap(), "\"0000-01-01\"");
    assert_eq!(
        serde_json::from_str::<Date>("\"0000-01-01\"").unwrap(),
        first
    );

    let last = Date::from_ymd(9999, 12, 31).unwrap();
    assert_eq!(serde_json::to_string(&last).unwrap(), "\"9999-12-31\"");

    assert!(Date::from_ymd(2000, 2, 29).is_ok());
    assert!(Date::from_ymd(1900, 2, 29).is_err());
}
