//! `build_conversation` 的装配测试：真实 `Engine` + 真实 `Conversation`，
//! 只把模型端口换成拒绝调用的桩，验证空 store 上 `run_once` 不产生事件。

use std::sync::Arc;

use async_trait::async_trait;
use gantry_conversation::ports::ModelsPort;
use gantry_core::entities::Seq;
use gantry_core::ids::{IdGen, ProjectId};
use gantry_core::settings::resource::{LevelBudget, LevelEntry, TierBudget};
use gantry_engine::engine::Engine;
use gantry_models::{DirectCall, DirectOutcome, ModelsError};
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Store, StoreDeps};

struct NoModels;

#[async_trait]
impl ModelsPort for NoModels {
    async fn configured(&self, _project: &ProjectId) -> Result<bool, ModelsError> {
        Ok(false)
    }

    async fn invoke_direct(&self, _call: DirectCall) -> Result<DirectOutcome, ModelsError> {
        Err(ModelsError::NotConfigured {
            tier: "utility",
            reason: "test stub".to_owned(),
        })
    }
}

fn low() -> LevelEntry {
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

#[tokio::test]
async fn build_conversation_wires_empty_store() {
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
    let engine = Arc::new(Engine::new(store.clone(), inbox));

    let conversation = gantryd::assembly::build_conversation(
        &store,
        engine,
        ids,
        Arc::new(NoModels),
        low(),
        Seq::new(1).expect("config rev"),
    );

    assert_eq!(conversation.run_once().await.expect("run"), 0);
}
