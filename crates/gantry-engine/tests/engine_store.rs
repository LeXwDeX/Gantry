//! `Engine::create_delivery` 的存储集成测试（内存 Store）。

use std::sync::Arc;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{
    Actor, ActorKind, DeliveryOrigin, InboxKind, InboxPayload, IssueRef, IssueState, MissingAspect,
    Ref, Seq, Severity,
};
use gantry_core::events::EventType;
use gantry_core::ids::IdGen;
use gantry_core::ids::{DeliveryId, MessageId, ProjectId, Ulid};
use gantry_core::level::Level;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::inbox::{DedupKey, RaiseRequest, SafeSummary};
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use gantry_engine::create::{budget_from_level, NewDelivery};
use gantry_engine::engine::{Engine, EngineError};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{EventFilter, Store, StoreDeps};

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
        config_rev: Seq::new(1).unwrap(),
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
    }
}

async fn engine() -> Engine {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let inbox = Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store.clone()));
    Engine::new(store, inbox)
}

fn empty_filter() -> EventFilter {
    EventFilter {
        project_ids: None,
        include: Vec::new(),
        exclude: Vec::new(),
    }
}

#[tokio::test]
async fn create_delivery_appends_one_event() {
    let engine = engine().await;
    let id = engine.create_delivery(delivery()).await.expect("created");
    assert_eq!(id.to_string(), "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB");

    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].ty, EventType::DeliveryCreated);
    assert_eq!(page.events[0].subject.as_str(), id.to_string());
    assert_eq!(page.events[0].data["level_cap"], "Low");
}

#[tokio::test]
async fn same_source_message_is_idempotent() {
    let engine = engine().await;
    engine.create_delivery(delivery()).await.unwrap();
    engine.create_delivery(delivery()).await.unwrap();
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
}

#[tokio::test]
async fn invalid_delivery_is_not_appended() {
    let engine = engine().await;
    let mut delivery = delivery();
    delivery.level_cap = Level::None;
    delivery.level_effective = Level::None;
    assert!(engine.create_delivery(delivery).await.is_err());
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert!(page.events.is_empty());
}

#[tokio::test]
async fn default_medium_is_not_configured_and_writes_nothing() {
    let engine = engine().await;
    let mut delivery = delivery();
    delivery.level_cap = Level::Medium;
    delivery.level_effective = Level::Medium;
    delivery.budget.template = "medium.plan_build_verify".to_owned();
    assert!(matches!(
        engine.create_delivery(delivery).await,
        Err(EngineError::NotConfigured)
    ));
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert!(page.events.is_empty());
}

#[tokio::test]
async fn medium_cap_with_low_template_is_refused() {
    let engine = engine().await;
    let mut delivery = delivery();
    delivery.level_cap = Level::Medium;
    delivery.level_effective = Level::Medium;
    // 模板名本身已登记，但档位与模板不匹配；入口不能据此放行。
    assert!(matches!(
        engine.create_delivery(delivery).await,
        Err(EngineError::NotConfigured)
    ));
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert!(page.events.is_empty());
}

#[tokio::test]
async fn low_cap_with_unregistered_template_is_refused() {
    let engine = engine().await;
    let mut delivery = delivery();
    delivery.budget.template = "low.unregistered".to_owned();
    assert!(matches!(
        engine.create_delivery(delivery).await,
        Err(EngineError::NotConfigured)
    ));
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert!(page.events.is_empty());
}

#[tokio::test]
async fn low_delivery_starts_at_low_effective_regardless_of_input() {
    let engine = engine().await;
    let mut delivery = delivery();
    delivery.level_effective = Level::High;
    engine.create_delivery(delivery).await.unwrap();
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events[0].data["level_effective"], "Low");
}

#[tokio::test]
async fn unsupported_high_is_not_configured_and_writes_nothing() {
    let engine = engine().await;
    let mut delivery = delivery();
    delivery.level_cap = Level::High;
    delivery.level_effective = Level::High;
    delivery.budget.template = "high.multi_review".to_owned();
    assert!(matches!(
        engine.create_delivery(delivery).await,
        Err(EngineError::NotConfigured)
    ));
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert!(page.events.is_empty());
}

#[tokio::test]
async fn event_time_comes_from_the_store_clock() {
    let engine = engine().await;
    engine.create_delivery(delivery()).await.unwrap();
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events[0].ts, Timestamp::from_millis(1_700_000_000_000));
}

/// 同一条消息重复创建：不追加第二条事件，返回第一次创建的交付单元（engine.md §6）。
#[tokio::test]
async fn duplicate_message_returns_the_existing_delivery() {
    let engine = engine().await;
    let first = engine.create_delivery(delivery()).await.expect("created");
    let mut again = delivery();
    again.id = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC"
        .parse::<DeliveryId>()
        .unwrap();
    let second = engine.create_delivery(again).await.expect("deduped");
    assert_eq!(second, first);
    let page = engine
        .store()
        .events()
        .scan(Seq::ZERO, &empty_filter(), 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
}

fn issue(number: u64) -> IssueRef {
    IssueRef {
        repo: RepoSlug::parse("acme/web").unwrap(),
        number,
        node_id: format!("I_{number}"),
        title: UntrustedText::new("fix login".into(), "github".into(), false),
        state: IssueState::Open,
        state_reason: None,
        synced_at: Timestamp::from_millis(1_700_000_000_000),
    }
}

/// #105 A7：`locate_deliveries` 按 refs 命中绑定且未终结的交付单元。
#[tokio::test]
async fn locate_deliveries_matches_bound_issue_refs() {
    let engine = engine().await;
    let mut created = delivery();
    created.issues = vec![issue(42)];
    let project = created.project_id;
    let id = engine.create_delivery(created).await.unwrap();

    assert_eq!(
        engine.locate_deliveries(&project, &[]).await.unwrap(),
        vec![]
    );

    let found = engine
        .locate_deliveries(
            &project,
            &[Ref::Issue {
                repo: RepoSlug::parse("acme/web").unwrap(),
                number: 42,
            }],
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    assert_eq!(found[0].project_id, project);
    assert_eq!(found[0].level_cap, Level::Low);
    assert_eq!(found[0].stage, gantry_core::entities::Stage::Spec);
    assert_eq!(found[0].open_spec_needed, None);

    assert!(engine
        .locate_deliveries(
            &project,
            &[Ref::Issue {
                repo: RepoSlug::parse("acme/web").unwrap(),
                number: 43,
            }],
        )
        .await
        .unwrap()
        .is_empty());

    assert_eq!(
        engine
            .locate_deliveries(&project, &[Ref::Delivery { id }])
            .await
            .unwrap()
            .len(),
        1
    );
}

/// #105 A7：未决的 `spec_needed` 随定位结果返回。
#[tokio::test]
async fn locate_deliveries_reports_open_spec_needed() {
    let engine = engine().await;
    let mut created = delivery();
    created.issues = vec![issue(42)];
    let project = created.project_id;
    let id = engine.create_delivery(created).await.unwrap();
    let ctx = ActorCtx {
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(1_700_000_000_000, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    };
    let item = engine
        .raise_inbox(
            RaiseRequest {
                project_id: Some(project),
                delivery_id: Some(id),
                kind: InboxKind::SpecNeeded,
                severity: Severity::Blocker,
                summary: SafeSummary::new("需要规格"),
                payload: InboxPayload::SpecNeeded {
                    missing: vec![MissingAspect {
                        aspect: "goals".to_owned(),
                        question: "目标是什么？".to_owned(),
                    }],
                },
                dedup_key: DedupKey::new("spec:test"),
                supersedes: Vec::new(),
                actor: ctx.actor.clone(),
                causation_id: None,
            },
            &ctx,
        )
        .await
        .unwrap();
    let found = engine
        .locate_deliveries(
            &project,
            &[Ref::Issue {
                repo: RepoSlug::parse("acme/web").unwrap(),
                number: 42,
            }],
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].open_spec_needed, Some(item.id));
}
