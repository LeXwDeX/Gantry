//! App Manifest 创建回调的验收：forge.md §12 A 组"`/setup/github/callback` 无会话 / `state`
//! 错误 / 过期 → 404；有效回调后 `secrets` 中出现 App 私钥与 webhook secret，
//! `github.commit_identity` 已写入，响应不含密钥"。全部对 `FakeGitHub`（A 级）。
//!
//! 设置落盘与审计事件归设置模块（`GithubSettingsWriter` 的实现方），这里用记录型替身核对
//! forge 交给它的内容。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::ids::IdGen;
use gantry_core::ports::SecretSource;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::github::FakeGitHub;
use gantry_fakes::secrets::MemorySecretStore;
use gantry_forge::setup::{
    GithubAppSettings, GithubSettingsWriter, SetupDeps, SetupError, SetupService, SetupStatus,
};
use gantry_forge::{module_spec, ForgeConfig, WebhookDeps, WebhookOptions, WebhookReceiver};
use gantry_store::{Store, StoreDeps};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MS: i64 = 1_700_000_000_000;
const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nSENTINEL-PEM\n-----END RSA PRIVATE KEY-----\n";
const HOOK_SECRET: &str = "sentinel-webhook-secret";

#[derive(Default)]
struct RecordingWriter(Mutex<Vec<GithubAppSettings>>);

impl GithubSettingsWriter for RecordingWriter {
    fn write_app(&self, settings: &GithubAppSettings) -> Result<(), String> {
        self.0.lock().unwrap().push(settings.clone());
        Ok(())
    }
}

fn name(value: &str) -> SecretName {
    SecretName::parse(value).unwrap()
}

struct Harness {
    github: FakeGitHub,
    clock: Arc<FakeClock>,
    secrets: MemorySecretStore,
    writer: Arc<RecordingWriter>,
    setup: Arc<SetupService>,
    receiver: WebhookReceiver,
}

async fn harness() -> Harness {
    let github = FakeGitHub::start();
    let clock = Arc::new(FakeClock::new(MS));
    let secrets = MemorySecretStore::new(Timestamp::from_millis(MS));
    secrets.allow(Consumer::Forge);
    let writer = Arc::new(RecordingWriter::default());
    let setup = Arc::new(SetupService::new(SetupDeps {
        secrets: Arc::new(secrets.scoped(Consumer::Forge).unwrap()),
        private_key: name("github_app_private_key"),
        webhook_secret: name("github_webhook_secret"),
        settings: writer.clone(),
        clock: clock.clone(),
        entropy: Arc::new(SeqEntropy::new(3)),
        api_base: github.base_url(),
        http_timeout: Duration::from_secs(5),
    }));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(
        StoreDeps {
            clock: clock.clone(),
            ids,
        },
        &[module_spec()],
    )
    .await
    .unwrap();
    let receiver = WebhookReceiver::bind(WebhookDeps {
        store,
        config: Arc::new(ForgeConfig::default()),
        secrets: Arc::new(secrets.reader(Consumer::Forge).unwrap()),
        secret: name("github_webhook_secret"),
        previous_secret: None,
        app_id: 0,
        clock: clock.clone(),
        options: WebhookOptions {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..WebhookOptions::default()
        },
        setup: Some(setup.clone()),
    })
    .await
    .unwrap();
    github.set_manifest_conversion(
        "code-ok",
        json!({
            "id": 424242,
            "slug": "gantry-hooks",
            "html_url": "https://github.com/apps/gantry-hooks",
            "pem": PEM,
            "webhook_secret": HOOK_SECRET,
            "client_secret": "sentinel-client-secret",
            "permissions": {"contents": "write", "pull_requests": "write", "issues": "write"},
            "events": ["pull_request", "push"]
        }),
    );
    github.set_user("gantry-hooks[bot]", 9001);
    Harness {
        github,
        clock,
        secrets,
        writer,
        setup,
        receiver,
    }
}

/// 浏览器式 GET，返回 (状态码, 原始响应)。
async fn get(receiver: &WebhookReceiver, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(receiver.local_addr())
        .await
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, response)
}

fn stored(h: &Harness, secret: &str) -> Option<Vec<u8>> {
    h.secrets
        .reader(Consumer::Forge)
        .unwrap()
        .get(&name(secret))
        .unwrap()
        .map(|secret| secret.value.expose().to_vec())
}

#[tokio::test]
async fn callback_without_session_is_404() {
    let h = harness().await;
    let (status, _) = get(&h.receiver, "/setup/github/callback?code=code-ok&state=x").await;
    assert_eq!(status, 404);
    assert!(stored(&h, "github_app_private_key").is_none());
    assert!(h.writer.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn begin_requires_bootstrap_and_is_single_session() {
    let h = harness().await;
    assert_eq!(
        h.setup.begin(Duration::from_secs(900), false),
        Err(SetupError::NotBootstrapPeriod)
    );
    let ticket = h.setup.begin(Duration::from_secs(900), true).unwrap();
    assert_eq!(ticket.state.len(), 64);
    assert_eq!(ticket.expires_at, Timestamp::from_millis(MS + 900_000));
    assert_eq!(
        h.setup.begin(Duration::from_secs(900), true),
        Err(SetupError::SetupInProgress)
    );
    assert_eq!(h.setup.status(&ticket.state), SetupStatus::Pending);
}

#[tokio::test]
async fn wrong_state_and_expired_session_are_404() {
    let h = harness().await;
    let ticket = h.setup.begin(Duration::from_secs(900), true).unwrap();
    let (status, _) = get(
        &h.receiver,
        "/setup/github/callback?code=code-ok&state=nope",
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(h.setup.status(&ticket.state), SetupStatus::Pending);

    h.clock.advance_ms(900_000);
    let path = format!("/setup/github/callback?code=code-ok&state={}", ticket.state);
    assert_eq!(get(&h.receiver, &path).await.0, 404);
    assert_eq!(h.setup.status(&ticket.state), SetupStatus::Expired);
    assert!(stored(&h, "github_app_private_key").is_none());
}

#[tokio::test]
async fn valid_callback_stores_credentials_and_writes_identity_without_leaking() {
    let h = harness().await;
    let ticket = h.setup.begin(Duration::from_secs(900), true).unwrap();
    let path = format!("/setup/github/callback?code=code-ok&state={}", ticket.state);
    let (status, response) = get(&h.receiver, &path).await;
    assert_eq!(status, 200);
    for secret in [PEM, HOOK_SECRET, "sentinel-client-secret", "SENTINEL-PEM"] {
        assert!(!response.contains(secret), "response leaks {secret}");
    }

    assert_eq!(
        stored(&h, "github_app_private_key").as_deref(),
        Some(PEM.as_bytes())
    );
    assert_eq!(
        stored(&h, "github_webhook_secret").as_deref(),
        Some(HOOK_SECRET.as_bytes())
    );
    let written = h.writer.0.lock().unwrap().clone();
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].app_id, 424242);
    assert_eq!(written[0].slug, "gantry-hooks");
    assert_eq!(written[0].commit_identity_name, "gantry-hooks[bot]");
    assert_eq!(
        written[0].commit_identity_email,
        "9001+gantry-hooks[bot]@users.noreply.github.com"
    );
    assert_eq!(written[0].private_key_ref, name("github_app_private_key"));

    // status 应答不含密钥字段。
    let SetupStatus::Completed { app } = h.setup.status(&ticket.state) else {
        panic!("completed");
    };
    assert_eq!(app["app_id"], json!(424242));
    let rendered = app.to_string();
    for key in ["pem", "webhook_secret", "client_secret"] {
        assert!(!rendered.contains(key));
    }

    // 会话已作废：同一 state 再来一次是 404，不再转换。
    assert_eq!(get(&h.receiver, &path).await.0, 404);
    assert_eq!(h.writer.0.lock().unwrap().len(), 1);
    // 转换只调用了一次。
    let conversions = h
        .github
        .recorded_calls()
        .iter()
        .filter(|call| call.path.ends_with("/conversions"))
        .count();
    assert_eq!(conversions, 1);
}

#[tokio::test]
async fn failed_conversion_reports_failure_without_storing() {
    let h = harness().await;
    let ticket = h.setup.begin(Duration::from_secs(900), true).unwrap();
    let path = format!(
        "/setup/github/callback?code=code-unknown&state={}",
        ticket.state
    );
    assert_eq!(get(&h.receiver, &path).await.0, 502);
    assert!(matches!(
        h.setup.status(&ticket.state),
        SetupStatus::Failed { ref code, .. } if code == "conversion_failed"
    ));
    assert!(stored(&h, "github_app_private_key").is_none());
}
