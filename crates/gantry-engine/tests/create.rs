//! 交付创建纯构造（engine.md §6 / §7.6 T1）。

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, Seq, Tier};
use gantry_core::events::EventType;
use gantry_core::ids::{DeliveryId, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::create::{budget_from_level, created_event, NewDelivery};

fn level_entry() -> LevelEntry {
    LevelEntry {
        graph_template: "low.single".to_owned(),
        budget: LevelBudget {
            tokens_by_tier: TierBudget {
                utility: 50_000,
                planner: 100_000,
                worker: 1_000_000,
            },
            nodes: 8,
            rounds: 0,
            wall_time_s: 3600,
            concurrency: 1,
            forge_writes: 20,
            forge_writes_per_hour: 30,
        },
    }
}

fn delivery() -> NewDelivery {
    NewDelivery {
        id: "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
            .parse::<DeliveryId>()
            .unwrap(),
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA"
            .parse::<ProjectId>()
            .unwrap(),
        title: "fix login timeout".to_owned(),
        user_text: "修复登录超时".to_owned(),
        level_cap: Level::Low,
        level_effective: Level::Low,
        origin: DeliveryOrigin::Conversation,
        source_message_id: "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD"
            .parse::<MessageId>()
            .unwrap(),
        issues: vec![],
        budget: budget_from_level(&level_entry()),
        config_rev: Seq::new(7).unwrap(),
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
    }
}

#[test]
fn budget_maps_every_level_field() {
    let budget = budget_from_level(&level_entry());
    assert_eq!(budget.template, "low.single");
    assert_eq!(budget.limits.nodes, 8);
    assert_eq!(budget.limits.rounds, 0);
    assert_eq!(budget.limits.wall_time, 3600);
    assert_eq!(budget.limits.concurrency, 1);
    assert_eq!(budget.limits.forge_writes, 20);
    assert_eq!(budget.limits.tokens_by_tier[&Tier::Utility], 50_000);
    assert_eq!(budget.limits.tokens_by_tier[&Tier::Planner], 100_000);
    assert_eq!(budget.limits.tokens_by_tier[&Tier::Worker], 1_000_000);
    assert!(budget.limits.tokens.is_none());
}

#[test]
fn delivery_created_event_is_valid_and_idempotent() {
    let event = created_event(delivery()).expect("valid delivery");
    assert_eq!(event.ty, EventType::DeliveryCreated);
    assert_eq!(event.subject.as_str(), "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB");
    assert_eq!(event.data["level_cap"], "Low");
    assert_eq!(event.data["level_effective"], "Low");
    assert_eq!(event.data["budget"]["template"], "low.single");
    assert_eq!(
        event.data["source_message_id"],
        "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD"
    );
    assert_eq!(
        event.dedup_key.as_deref(),
        Some("delivery.created:msg_01JB7N3R5T8X2V6M9Q4K1W7YZD")
    );
}

#[test]
fn none_cap_is_rejected() {
    let mut delivery = delivery();
    delivery.level_cap = Level::None;
    delivery.level_effective = Level::None;
    assert!(created_event(delivery).is_err());
}

#[test]
fn effective_above_cap_is_rejected() {
    let mut delivery = delivery();
    delivery.level_effective = Level::High;
    assert!(created_event(delivery).is_err());
}

#[test]
fn template_name_is_fixed_from_the_level_entry() {
    let mut entry = level_entry();
    entry.graph_template = "low.single".to_owned();
    assert_eq!(budget_from_level(&entry).template, "low.single");

    entry.graph_template = "medium.plan_build_verify".to_owned();
    assert_eq!(
        budget_from_level(&entry).template,
        "medium.plan_build_verify"
    );
}
