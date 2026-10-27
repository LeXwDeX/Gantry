//! 与 `EgressStub` 的共享一致性测试（`docs/interfaces/egress-control.md` §8、
//! `docs/interfaces/egress-call-terminal.md` §7）：同一组用例在端口 `EgressControl` 层分别跑
//! `EgressStub` 与真实 Egress（登记表 + `egress.sock` 控制面 + 数据面监听器，经
//! `EgressClient` 调用），断言两者行为一致。

#![cfg(unix)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::egress::{IssuedTo, SandboxTier};
use gantry_core::entities::{Seq, TokenWeights};
use gantry_core::ids::{AttemptId, Ulid};
use gantry_core::ports::context::Health;
use gantry_core::ports::egress::{
    EgressConfigView, EgressControl, EgressError, IssueJobToken, ProviderId, StageSecret,
    UsageCursor,
};
use gantry_core::ports::sandbox::{GroupName, ProfileName};
use gantry_core::secret::{SecretName, SecretString};
use gantry_egress::audit::MemoryAudit;
use gantry_egress::client::EgressClient;
use gantry_egress::control::ControlServer;
use gantry_egress::db::Db;
use gantry_egress::model_proxy::{Limits, ModelProxy};
use gantry_egress::netdeny::DenyNets;
use gantry_egress::registry::{Endpoints, Listener, Registry, RegistryOptions};
use gantry_egress::upstream::{roots_from_pem, tls_config, Connector};
use gantry_fakes::egress::EgressStub;
use gantry_fakes::secrets::MemorySecretStore;
use gantry_fakes::test_ca::TestCA;
use support::*;
use tokio::net::{TcpListener, UnixListener};

/// 被测对象：端口实现 + 一个能用签发的 base URL 发模型请求的数据面。
#[async_trait]
trait Subject: Send + Sync {
    fn control(&self) -> &dyn EgressControl;
    /// 提供方主机（配置视图里的 `base_url` 与 `model_api` 分组）。
    fn provider_host(&self) -> &str;
}

struct Stub(EgressStub);

#[async_trait]
impl Subject for Stub {
    fn control(&self) -> &dyn EgressControl {
        &self.0
    }
    fn provider_host(&self) -> &str {
        "localhost"
    }
}

struct Real {
    client: EgressClient,
    _ca: TestCA,
    _mock: MockUpstream,
    _dir: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[async_trait]
impl Subject for Real {
    fn control(&self) -> &dyn EgressControl {
        &self.client
    }
    fn provider_host(&self) -> &str {
        "localhost"
    }
}

async fn real(name: &str) -> Real {
    use std::os::unix::fs::MetadataExt;
    let dir = std::path::PathBuf::from(format!("/tmp/gc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("egress.sock");
    let ca = TestCA::new().expect("openssl test CA");
    let mock = MockUpstream::start(&ca).await;
    let clock = Arc::new(gantry_core::testing::FakeClock::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    ));
    let store = MemorySecretStore::new(clock.now_timestamp());
    store.allow(gantry_core::secret::Consumer::Egress);
    let secrets: Arc<dyn gantry_core::ports::secrets::SecretSink> =
        Arc::new(store.scoped(gantry_core::secret::Consumer::Egress).unwrap());
    let sandbox = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let loopback = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = Registry::open(
        Db::open_in_memory().unwrap(),
        clock.clone(),
        Arc::new(gantry_egress::token::OsEntropy),
        secrets.clone(),
        RegistryOptions::new(Endpoints {
            sandbox_model_base: format!("http://{}", sandbox.local_addr().unwrap()),
            connect_proxy_authority: "127.0.0.1:3128".into(),
            loopback_model_base: format!("http://{}", loopback.local_addr().unwrap()),
        }),
    )
    .unwrap();
    let resolver = Arc::new(FakeResolver::default());
    resolver.set("localhost", vec![vec![PUBLIC_IP]]);
    let connector = Connector::new(
        resolver,
        Arc::new(MapDialer::new(mock.addr)),
        DenyNets::default(),
        Duration::from_secs(5),
    );
    let audit = Arc::new(MemoryAudit::new());
    let proxy = ModelProxy::new(
        registry.clone(),
        secrets,
        audit.clone(),
        connector,
        tls_config(roots_from_pem(ca.root_cert_pem()).unwrap()),
        Limits::default(),
        None,
    );
    tokio::spawn(proxy.clone().serve(sandbox, Listener::Sandbox));
    tokio::spawn(proxy.serve(loopback, Listener::Loopback));
    let uid = std::fs::metadata(&dir).unwrap().uid();
    tokio::spawn(
        ControlServer::new(registry, audit, uid).serve(UnixListener::bind(&socket).unwrap()),
    );
    let client = EgressClient::new(
        &socket,
        clock.clone(),
        IssuedTo {
            process: "gantryd".into(),
            pid: std::process::id(),
            started_at: clock.now_timestamp(),
        },
    );
    Real {
        client,
        _ca: ca,
        _mock: mock,
        _dir: TempDir(dir),
    }
}

fn view(rev: u64, host: &str) -> EgressConfigView {
    support::view(rev, host)
}

fn job(n: u128) -> IssueJobToken {
    IssueJobToken {
        job_id: AttemptId::from_parts(T0 as u64, n).unwrap(),
        sandbox_id: "sbx_01JBX4R2M6P8Q1S3T5V7W9Y0ZA".parse().unwrap(),
        project_id: "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA".parse().unwrap(),
        delivery_id: Some("dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD".parse().unwrap()),
        conversation_id: None,
        tier: SandboxTier::Worker,
        provider: ProviderId::new("anthropic"),
        profile: ProfileName::new("default"),
        groups: vec![GroupName::new("model_api"), GroupName::new("npm")],
        tokens_total: 1_000_000,
        weights: TokenWeights::default(),
        wall_time_s: 1800,
        config_rev: Seq::from_wire(7).unwrap(),
    }
}

/// 应用配置并写入 provider 的凭据。
async fn configure(subject: &dyn Subject) {
    let control = subject.control();
    control
        .apply_config(view(7, subject.provider_host()), &ctx())
        .await
        .unwrap();
    let staged = control
        .stage_secret(
            StageSecret {
                secret_ref: SecretName::parse("model.anthropic.main").unwrap(),
                value: SecretString::new(SENTINEL.into()),
            },
            &ctx(),
        )
        .await
        .unwrap();
    control.activate_secret(&staged, &ctx()).await.unwrap();
}

async fn model_call(base: &str, headers: &[(&str, &str)]) -> u16 {
    let url = base.strip_prefix("http://").unwrap();
    let (authority, path) = url.split_once('/').unwrap();
    let mut all = vec![("content-type", "application/json")];
    all.extend_from_slice(headers);
    send(
        authority.parse().unwrap(),
        "POST",
        &format!("/{path}/v1/messages"),
        &all,
        MESSAGES_BODY,
    )
    .await
    .status
}

fn conflict_code(result: Result<impl std::fmt::Debug, EgressError>) -> String {
    match result {
        Err(EgressError::Conflict { message }) => message,
        other => panic!("expected Conflict, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 用例：同一函数分别作用于两个实现
// ---------------------------------------------------------------------------

async fn unconfigured_issue_is_refused(subject: &dyn Subject) {
    match subject.control().issue_job_token(job(1), &ctx()).await {
        Err(EgressError::Rejected { code, .. }) => assert_eq!(code, "not_configured"),
        other => panic!("expected not_configured, got {other:?}"),
    }
}

async fn config_revisions(subject: &dyn Subject) {
    configure(subject).await;
    let control = subject.control();
    assert_eq!(
        control
            .apply_config(view(7, subject.provider_host()), &ctx())
            .await
            .unwrap()
            .applied_rev,
        7
    );
    assert_eq!(
        conflict_code(
            control
                .apply_config(view(6, subject.provider_host()), &ctx())
                .await
        ),
        "stale_rev"
    );
    let mut bad = serde_json::to_value(view(8, subject.provider_host())).unwrap();
    bad["providers"]["anthropic"]["base_url"] = serde_json::json!("https://api.github.com");
    bad["groups"]["model_api"]["hosts"] = serde_json::json!(["localhost"]);
    let bad: EgressConfigView = serde_json::from_value(bad).unwrap();
    assert!(matches!(
        control.apply_config(bad, &ctx()).await,
        Err(EgressError::InvalidConfig { .. })
    ));
    let status = control.status().await.unwrap();
    assert_eq!(status.health, Health::Degraded);
    assert_eq!(status.applied_rev, Seq::from_wire(7).unwrap());
}

async fn job_token_lifecycle(subject: &dyn Subject) {
    configure(subject).await;
    let control = subject.control();
    let grant = control.issue_job_token(job(1), &ctx()).await.unwrap();
    assert!(!grant.reissued);
    let base = grant.model_base_urls.get("anthropic").unwrap().to_owned();
    assert_eq!(model_call(&base, &[]).await, 200);
    let usage = control.job_usage(&job(1).job_id).await.unwrap();
    assert_eq!(usage.usage.requests, 1);

    // 同绑定重签：新令牌可用、旧令牌失效、用量不清零。
    let again = control.issue_job_token(job(1), &ctx()).await.unwrap();
    assert!(again.reissued);
    assert_eq!(model_call(&base, &[]).await, 403);
    let base = again.model_base_urls.get("anthropic").unwrap().to_owned();
    assert_eq!(model_call(&base, &[]).await, 200);
    assert_eq!(
        control
            .job_usage(&job(1).job_id)
            .await
            .unwrap()
            .usage
            .requests,
        2
    );

    let mut other = job(1);
    other.tokens_total += 1;
    assert_eq!(
        conflict_code(control.issue_job_token(other, &ctx()).await),
        "binding_mismatch"
    );

    let revoked = control
        .revoke_job_token(&job(1).job_id, &ctx())
        .await
        .unwrap();
    assert_eq!(revoked.usage.requests, 2);
    assert_eq!(model_call(&base, &[]).await, 403);
    assert_eq!(
        control
            .revoke_job_token(&job(1).job_id, &ctx())
            .await
            .unwrap(),
        revoked
    );
    assert_eq!(
        conflict_code(control.issue_job_token(job(1), &ctx()).await),
        "job_revoked"
    );
    control.forget_job(&job(1).job_id, &ctx()).await.unwrap();
    assert_eq!(
        control.revoke_job_token(&job(1).job_id, &ctx()).await,
        Err(EgressError::NotFound)
    );
}

async fn svc_tokens_and_usage(subject: &dyn Subject) {
    configure(subject).await;
    let control = subject.control();
    let first = control.rotate_service_token(&ctx()).await.unwrap();
    let base = first.model_base_urls.get("anthropic").unwrap().to_owned();
    let call: Ulid = "01JBX5D2F4H6K8M0P2R4T6V8XZ".parse().unwrap();
    let call_text = call.to_string();
    let headers = [
        ("x-gantry-ledger", "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"),
        ("x-gantry-tier", "utility"),
        ("x-gantry-call-id", call_text.as_str()),
    ];
    assert_eq!(
        model_call(&base, &headers[..2]).await,
        400,
        "missing call id"
    );
    assert_eq!(model_call(&base, &headers).await, 200);
    let page = control
        .usage_since(UsageCursor::new("0"), 100)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].tier, gantry_core::entities::Tier::Utility);

    let second = control.rotate_service_token(&ctx()).await.unwrap();
    assert_eq!(second.revoked_previous, vec![first.svc_token_id.clone()]);
    assert_eq!(model_call(&base, &headers).await, 403);
    let old = control
        .svc_token_usage(&first.svc_token_id, None)
        .await
        .unwrap();
    assert_eq!(old.items.len(), 1);
    let usage = control.call_usage(&call).await.unwrap();
    assert_eq!(usage.usage.requests, 1);

    // 调用级关闭：另一个 token 关不了；有关联的旧 token 可以；之后同一 call_id 被拒。
    assert!(matches!(
        control
            .close_call(&second.svc_token_id, &call, &ctx())
            .await,
        Err(EgressError::Conflict { .. })
    ));
    let closed = control
        .close_call(&first.svc_token_id, &call, &ctx())
        .await
        .unwrap();
    assert_eq!(closed.terminal.record_count, 1);
    let second_base = second.model_base_urls.get("anthropic").unwrap().to_owned();
    assert_eq!(model_call(&second_base, &headers).await, 403);
    assert_eq!(
        control
            .close_call(&first.svc_token_id, &call, &ctx())
            .await
            .unwrap(),
        closed
    );
    let statement = control.call_terminal(&call).await.unwrap();
    assert!(statement.settleable_terminal().is_some());
    closed.terminal.verify(call, &statement.items).unwrap();

    // 零接收。
    let empty: Ulid = "01JBX5D2F4H6K8M0P2R4T6V8Y0".parse().unwrap();
    assert!(control
        .call_terminal(&empty)
        .await
        .unwrap()
        .terminal
        .is_none());
    let zero = control
        .close_call(&second.svc_token_id, &empty, &ctx())
        .await
        .unwrap();
    assert_eq!(zero.terminal.record_count, 0);
    assert_eq!(
        zero.terminal.records_digest,
        gantry_core::egress::records_digest(&[]).unwrap()
    );
}

async fn secrets_write_only(subject: &dyn Subject) {
    configure(subject).await;
    let control = subject.control();
    let name = SecretName::parse("registry.npm.corp").unwrap();
    let present = control
        .secret_present(std::slice::from_ref(&name))
        .await
        .unwrap();
    assert_eq!(present, BTreeMap::from([(name.clone(), false)]));
    let mut stage_ctx = ctx();
    stage_ctx.request_id = Ulid::from_parts(T0 as u64, 77).unwrap();
    let staged = control
        .stage_secret(
            StageSecret {
                secret_ref: name.clone(),
                value: SecretString::new("v1".into()),
            },
            &stage_ctx,
        )
        .await
        .unwrap();
    assert!(
        !control
            .secret_present(std::slice::from_ref(&name))
            .await
            .unwrap()[&name]
    );
    assert_eq!(control.staged_secrets().await.unwrap().len(), 1);
    control.discard_secret(&staged, &ctx()).await.unwrap();
    assert!(control.staged_secrets().await.unwrap().is_empty());
    assert!(
        !control
            .secret_present(std::slice::from_ref(&name))
            .await
            .unwrap()[&name]
    );
}

macro_rules! conformance {
    ($name:ident, $case:ident) => {
        mod $name {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn stub() {
                let stub = Stub(EgressStub::new());
                $case(&stub).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn real_egress() {
                let real = real(stringify!($name)).await;
                $case(&real).await;
            }
        }
    };
}

conformance!(unconfigured, unconfigured_issue_is_refused);
conformance!(config, config_revisions);
conformance!(jobs, job_token_lifecycle);
conformance!(svc, svc_tokens_and_usage);
conformance!(secrets, secrets_write_only);
