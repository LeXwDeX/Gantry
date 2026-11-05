//! 生产 `settings.bootstrap_set` 的组件测试（控制通道命令，不经进程）。

use std::sync::Arc;

use gantry_core::events::EventType;
use gantry_core::testing::FakeClock;
use gantry_pki::directory::StoreDirectory;
use gantry_store::{Store, StoreDeps};
use serde_json::json;

use gantryd::commands::ProductionCommands;
use gantryd::control::CommandSet;

async fn commands() -> (ProductionCommands, Store) {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(gantry_core::testing::SeqEntropy::new(7)),
    ));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    })
    .expect("store");
    let directory = Arc::new(StoreDirectory::new(store.clone(), clock));
    (ProductionCommands::new(store.clone(), directory), store)
}

#[tokio::test]
async fn writes_sandbox_base_image_during_bootstrap() {
    let (commands, store) = commands().await;
    let response = commands
        .handle(
            "settings.bootstrap_set",
            &json!({
                "resource": "sandbox",
                "values": {"base_image": "registry.example/gantry/worker@sha256:0000000000000000000000000000000000000000000000000000000000000000"}
            }),
        )
        .await
        .expect("known command");
    let response = serde_json::to_value(response).expect("response");
    assert_eq!(response["ok"], true, "{response}");

    let events = store
        .events()
        .scan(
            gantry_core::entities::Seq::ZERO,
            &gantry_store::EventFilter {
                project_ids: None,
                include: vec![],
                exclude: vec![],
            },
            10,
        )
        .await
        .expect("events");
    let updated = events
        .events
        .iter()
        .find(|event| event.ty == EventType::SettingsUpdated)
        .expect("settings.updated");
    assert_eq!(updated.data["resource"], "sandbox");
    assert_eq!(updated.data["base_rev"], 0);
    assert_eq!(
        updated.data["value"]["base_image"],
        "registry.example/gantry/worker@sha256:0000000000000000000000000000000000000000000000000000000000000000"
    );
}

#[tokio::test]
async fn rejects_keys_outside_the_bootstrap_whitelist() {
    let (commands, store) = commands().await;
    let response = commands
        .handle(
            "settings.bootstrap_set",
            &json!({"resource": "sandbox", "values": {"idle_timeout_s": "10"}}),
        )
        .await
        .expect("known command");
    let response = serde_json::to_value(response).expect("response");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "not_writable");

    let head = store.events().head().await.expect("head");
    assert_eq!(head, gantry_core::entities::Seq::ZERO);
}
