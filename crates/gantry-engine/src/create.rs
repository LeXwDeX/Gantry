//! 交付创建事件（engine.md §6 / §7.6 T1）。
//!
//! 只做纯构造：调用方提供 settings 已求值的 `LevelEntry` 与引用快照；
//! 本模块不读取存储、不调用外部系统。

use std::collections::BTreeMap;

use gantry_core::entities::{Actor, BudgetLimits, DeliveryOrigin, IssueRef, Seq, Tier};
use gantry_core::events::{
    DeliveryBudget, DeliveryCreatedData, EventType, NewEvent, SchemaError, Subject,
};
use gantry_core::ids::{DeliveryId, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::resource::LevelEntry;

pub struct NewDelivery {
    pub id: DeliveryId,
    pub project_id: ProjectId,
    pub title: String,
    pub level_cap: Level,
    pub level_effective: Level,
    pub origin: DeliveryOrigin,
    pub source_message_id: MessageId,
    /// 用户亲手输入的原文（可信，engine.md §6）；记入 `delivery.created` 供重放（#70 B）。
    pub user_text: String,
    pub issues: Vec<IssueRef>,
    pub budget: DeliveryBudget,
    pub config_rev: Seq,
    pub actor: Actor,
}

/// `levels.<level>.budget` → `DeliveryBudget`；台账模板取模板族名（P-57）。
pub fn budget_from_level(entry: &LevelEntry) -> DeliveryBudget {
    let budget = &entry.budget;
    let tokens = BTreeMap::from([
        (Tier::Utility, budget.tokens_by_tier.utility),
        (Tier::Planner, budget.tokens_by_tier.planner),
        (Tier::Worker, budget.tokens_by_tier.worker),
    ]);
    DeliveryBudget {
        template: entry.graph_template.clone(),
        limits: BudgetLimits::delivery(
            tokens,
            budget.nodes,
            budget.rounds,
            budget.wall_time_s,
            budget.concurrency,
            budget.forge_writes,
        ),
    }
}

/// 构造 `delivery.created` 并执行契约校验。
pub fn created_event(delivery: NewDelivery) -> Result<NewEvent, SchemaError> {
    let delivery_text = delivery.id.to_string();
    let subject = Subject::parse(&delivery_text)
        .map_err(|error| SchemaError::shape("/subject", error.to_string()))?;
    let dedup_key = format!("delivery.created:{}", delivery.source_message_id);
    let data = DeliveryCreatedData {
        title: delivery.title,
        level_cap: delivery.level_cap,
        level_effective: delivery.level_effective,
        origin: delivery.origin,
        source_message_id: Some(delivery.source_message_id),
        user_text: delivery.user_text,
        issues: delivery.issues,
        budget: delivery.budget,
        config_rev: delivery.config_rev,
    };
    let event = NewEvent {
        ty: EventType::DeliveryCreated,
        subject,
        project_id: Some(delivery.project_id),
        actor: delivery.actor,
        causation_id: None,
        correlation_id: Some(delivery_text),
        data: serde_json::to_value(data)
            .map_err(|error| SchemaError::shape("/data", error.to_string()))?,
        dedup_key: Some(dedup_key),
    };
    event.validate()?;
    Ok(event)
}
