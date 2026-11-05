//! `StoreGithubSettingsWriter` 的组件测试：`github.setup` 回调写 `github` 设置事件。

use std::sync::Arc;

use gantry_core::entities::Seq;
use gantry_core::events::EventType;
use gantry_core::secret::SecretName;
use gantry_core::testing::FakeClock;
use gantry_forge::setup::{GithubAppSettings, GithubSettingsWriter};
use gantry_store::{EventFilter, Store, StoreDeps};
use gantryd::commands::StoreGithubSettingsWriter;

fn store() -> Store {
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(gantry_core::testing::SeqEntropy::new(7)),
    ));
    Store::open_in_memory_sync(StoreDeps { clock, ids }).expect("store")
}

#[tokio::test]
async fn write_app_emits_github_settings() {
    let store = store();
    let writer = StoreGithubSettingsWriter::new(store.clone());
    writer
        .write_app(&GithubAppSettings {
            app_id: 42,
            slug: "gantry-test".to_owned(),
            private_key_ref: SecretName::parse("github.app.private_key").expect("secret"),
            webhook_secret_ref: SecretName::parse("github.app.webhook_secret").expect("secret"),
            commit_identity_name: "gantry-bot".to_owned(),
            commit_identity_email: "1+gantry-bot@users.noreply.github.com".to_owned(),
        })
        .await
        .expect("write app");

    let page = store
        .events()
        .scan(
            Seq::ZERO,
            &EventFilter {
                project_ids: None,
                include: vec![],
                exclude: vec![],
            },
            10,
        )
        .await
        .expect("events");
    let updated = page
        .events
        .iter()
        .find(|event| event.ty == EventType::SettingsUpdated)
        .expect("settings.updated");
    assert_eq!(updated.data["resource"], "github");
    assert_eq!(updated.data["value"]["app"]["app_id"], 42);
    assert_eq!(
        updated.data["value"]["app"]["private_key"]["secret_ref"],
        "github.app.private_key"
    );
    assert_eq!(
        updated.data["value"]["commit_identity"]["email"],
        "1+gantry-bot@users.noreply.github.com"
    );
}
