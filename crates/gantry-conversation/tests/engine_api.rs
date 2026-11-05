//! `EngineApiAdapter` 的存储集成测试（#105 A7）：真实 `Engine` + 内存 Store。

use std::str::FromStr;
use std::sync::Arc;

use gantry_conversation::engine_api::EngineApiAdapter;
use gantry_conversation::ports::EngineApi;
use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, Ref, Seq, Stage};
use gantry_core::ids::{IdGen, ProjectId, Ulid};
use gantry_core::level::Level;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::engine::CreateDelivery;
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::engine::Engine;
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Store, StoreDeps};

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

fn actor_ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_str("01JB7N3R5T8X2V6M9Q4K1W7YZD").unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

async fn adapter() -> (EngineApiAdapter, ProjectId, Ref) {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(
        StoreDeps {
            clock,
            ids: ids.clone(),
        },
        &[],
    )
    .await
    .unwrap();
    let inbox = Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store.clone()));
    let engine = Arc::new(Engine::new(store, inbox));
    let project = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA"
        .parse::<ProjectId>()
        .unwrap();
    let reference = Ref::Issue {
        repo: RepoSlug::from_str("acme/widgets").unwrap(),
        number: 42,
    };
    let adapter =
        EngineApiAdapter::new(engine, ids, level_entry(), Seq::new(1).expect("config rev"));
    (adapter, project, reference)
}

fn command(project: ProjectId, reference: Ref) -> CreateDelivery {
    CreateDelivery {
        project_id: project,
        title: "fix login timeout".to_owned(),
        level_cap: Level::Low,
        refs: vec![reference],
        origin: DeliveryOrigin::Conversation,
        source_message_id: "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD".parse().unwrap(),
        user_text: "修复登录超时".to_owned(),
        ctx: actor_ctx(),
    }
}

#[tokio::test]
async fn create_then_locate_by_issue_ref() {
    let (adapter, project, reference) = adapter().await;
    let handle = adapter
        .create_delivery(command(project, reference.clone()))
        .await
        .expect("created");

    assert_eq!(handle.project_id, project);
    assert_eq!(handle.level_cap, Level::Low);
    assert_eq!(handle.stage, Stage::Spec);

    let located = adapter
        .locate_deliveries(&project, &[reference])
        .await
        .expect("located");
    assert_eq!(located, vec![handle]);
}

#[tokio::test]
async fn locate_other_project_returns_empty() {
    let (adapter, _project, reference) = adapter().await;
    adapter
        .create_delivery(command(
            "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap(),
            reference.clone(),
        ))
        .await
        .expect("created");

    let other = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZB".parse().unwrap();
    assert!(adapter
        .locate_deliveries(&other, &[reference])
        .await
        .expect("located")
        .is_empty());
}
