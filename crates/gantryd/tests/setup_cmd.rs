//! 生产 `github.setup.begin/status` 的组件测试（控制通道命令）。

use std::sync::Arc;
use std::time::Duration;

use gantry_core::secret::{Consumer, SecretName};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::secrets::MemorySecretStore;
use gantry_forge::setup::{SetupDeps, SetupService};
use gantry_pki::directory::StoreDirectory;
use gantry_store::Store;
use gantry_store::StoreDeps;
use serde_json::json;

use gantryd::commands::{ProductionCommands, StoreGithubSettingsWriter};
use gantryd::control::CommandSet;

fn name(value: &str) -> SecretName {
    SecretName::parse(value).expect("secret name")
}

async fn commands(with_setup: bool) -> ProductionCommands {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(7)),
    ));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    })
    .expect("store");
    let directory = Arc::new(StoreDirectory::new(store.clone(), clock.clone()));
    let commands = ProductionCommands::new(store.clone(), directory);
    if !with_setup {
        return commands;
    }
    let secrets = MemorySecretStore::new(Timestamp::from_millis(1_700_000_000_000));
    secrets.allow(Consumer::Forge);
    let setup = Arc::new(SetupService::new(SetupDeps {
        secrets: Arc::new(secrets.scoped(Consumer::Forge).expect("forge secrets")),
        private_key: name("github_app_private_key"),
        webhook_secret: name("github_webhook_secret"),
        settings: Arc::new(StoreGithubSettingsWriter::new(store)),
        clock,
        entropy: Arc::new(SeqEntropy::new(3)),
        api_base: "http://127.0.0.1:1".to_owned(),
        http_timeout: Duration::from_secs(1),
    }));
    commands.with_setup(setup)
}

#[tokio::test]
async fn begin_and_status_in_bootstrap_period() {
    let commands = commands(true).await;
    let begin = commands
        .handle("github.setup.begin", &json!({}))
        .await
        .expect("begin");
    let begin = serde_json::to_value(begin).expect("begin json");
    assert_eq!(begin["ok"], true, "{begin}");
    let state = begin["result"]["state"].as_str().expect("state").to_owned();

    let status = commands
        .handle("github.setup.status", &json!({"state": state}))
        .await
        .expect("status");
    let status = serde_json::to_value(status).expect("status json");
    assert_eq!(status["result"]["status"], "pending");
}

#[tokio::test]
async fn commands_report_not_configured_without_setup_service() {
    let commands = commands(false).await;
    let response = commands
        .handle("github.setup.begin", &json!({}))
        .await
        .expect("begin");
    let response = serde_json::to_value(response).expect("json");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "not_configured");
}
