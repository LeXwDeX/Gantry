//! 登记表测试：`docs/modules/egress.md` §12.A（令牌与归属、重启）、§12.C（计量与封顶、
//! `svc_token` 与调用级关闭）、§12.F（配置）、§12.G（密钥两段式写入）中不依赖网络的部分；
//! `docs/interfaces/egress-call-terminal.md` §7。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::egress::{
    records_digest, ConfigView, EgressHealth, IssueJobTokenRequest, IssueSvcTokenRequest, IssuedTo,
    JobTokenState, LedgerRef, UsageCaller,
};
use gantry_core::entities::{Actor, ActorKind, Seq, Tier, TokenUsage, TokenWeights};
use gantry_core::ids::{AttemptId, Ulid};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::secrets::{SecretSink, SecretValue};
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_egress::db::Db;
use gantry_egress::host::Host;
use gantry_egress::registry::{
    Caller, ControlError, Denial, DeniedKind, Endpoints, Listener, Metered, Registry,
    RegistryOptions, SvcCallMeta, UpstreamClass,
};
use gantry_egress::token::OsEntropy;
use gantry_fakes::secrets::MemorySecretStore;
use serde_json::json;

const T0: i64 = 1_793_000_000_000;

struct Harness {
    clock: Arc<FakeClock>,
    secrets: Arc<dyn SecretSink>,
    registry: Registry,
    path: Option<PathBuf>,
}

fn endpoints() -> Endpoints {
    Endpoints {
        sandbox_model_base: "http://10.89.0.1:8081".into(),
        connect_proxy_authority: "10.89.0.1:3128".into(),
        loopback_model_base: "http://127.0.0.1:8082".into(),
    }
}

fn options() -> RegistryOptions {
    let mut options = RegistryOptions::new(endpoints());
    options.drain_timeout = Duration::from_secs(2);
    options
}

fn secrets_for(clock: &FakeClock) -> Arc<dyn SecretSink> {
    let store = MemorySecretStore::new(clock.now_timestamp());
    store.allow(Consumer::Egress);
    Arc::new(store.scoped(Consumer::Egress).expect("egress scope"))
}

impl Harness {
    fn new() -> Self {
        let clock = Arc::new(FakeClock::new(T0));
        let secrets = secrets_for(&clock);
        let registry = Registry::open(
            Db::open_in_memory().unwrap(),
            clock.clone(),
            Arc::new(SeqEntropy::new(7)),
            secrets.clone(),
            options(),
        )
        .unwrap();
        Self {
            clock,
            secrets,
            registry,
            path: None,
        }
    }

    fn on_disk(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gantry-egress-{name}-{}-{}.db",
            std::process::id(),
            T0
        ));
        let _ = std::fs::remove_file(&path);
        let clock = Arc::new(FakeClock::new(T0));
        let secrets = secrets_for(&clock);
        let registry = Registry::open(
            Db::open(&path).unwrap(),
            clock.clone(),
            Arc::new(OsEntropy),
            secrets.clone(),
            options(),
        )
        .unwrap();
        Self {
            clock,
            secrets,
            registry,
            path: Some(path),
        }
    }

    /// 模拟进程崩溃后重启：同一个数据库文件、同一个密钥库，新的内存状态。
    fn restart(mut self) -> Self {
        let path = self.path.take().expect("on-disk harness");
        let clock = self.clock.clone();
        let secrets = self.secrets.clone();
        drop(self);
        let registry = Registry::open(
            Db::open(&path).unwrap(),
            clock.clone(),
            Arc::new(OsEntropy),
            secrets.clone(),
            options(),
        )
        .unwrap();
        Self {
            clock,
            secrets,
            registry,
            path: Some(path),
        }
    }

    fn configured() -> Self {
        let harness = Self::new();
        harness.setup();
        harness
    }

    fn setup(&self) {
        self.registry.apply_config(view(7)).unwrap();
        self.put_secret("model.anthropic.main", "sk-gantry-sentinel-0001");
    }

    fn put_secret(&self, name: &str, value: &str) {
        let staged = self
            .registry
            .stage_secret(
                Ulid::from_parts(T0 as u64, u128::from(value.len() as u32)).unwrap(),
                SecretName::parse(name).unwrap(),
                SecretValue::from_bytes(value.as_bytes().to_vec()),
                &ctx(),
            )
            .unwrap();
        self.registry
            .activate_secret(&staged.staged_id, &ctx())
            .unwrap();
    }

    fn issue(&self, job: u128) -> (String, IssueJobTokenRequest) {
        let req = job_request(job);
        let grant = self.registry.issue_job(req.clone()).unwrap();
        (grant.job_token.expose_secret().to_owned(), req)
    }

    fn svc(&self) -> (String, gantry_core::egress::SvcTokenId) {
        let grant = self
            .registry
            .issue_svc(IssueSvcTokenRequest {
                issued_to: IssuedTo {
                    process: "gantryd".into(),
                    pid: 4182,
                    started_at: self.clock.now_timestamp(),
                },
                ttl_s: None,
            })
            .unwrap();
        (
            grant.svc_token.expose_secret().to_owned(),
            grant.svc_token_id,
        )
    }

    /// 发一次 job 请求并按给定用量收尾。
    fn job_request(&self, token: &str, usage: TokenUsage) -> Result<(), Denial> {
        let caller = self.registry.authenticate(Listener::Sandbox, token)?;
        let admitted = self
            .registry
            .admit(&caller, "anthropic", "example-worker-model", None)?;
        self.registry
            .finish(admitted, ok(usage))
            .expect("usage committed");
        Ok(())
    }

    fn svc_request(&self, token: &str, call: Ulid, usage: TokenUsage) -> Result<(), Denial> {
        let caller = self.registry.authenticate(Listener::Loopback, token)?;
        let admitted = self.registry.admit(
            &caller,
            "anthropic",
            "example-utility-model",
            Some(&meta(call)),
        )?;
        self.registry
            .finish(admitted, ok(usage))
            .expect("usage committed");
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(T0 as u64, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn view(rev: u64) -> ConfigView {
    serde_json::from_value(json!({
        "rev": rev,
        "providers": {
            "anthropic": {
                "kind": "anthropic",
                "base_url": "https://api.anthropic.com",
                "secret_ref": "model.anthropic.main"
            }
        },
        "groups": {
            "model_api": { "hosts": ["api.anthropic.com"] },
            "npm": { "hosts": ["registry.npmjs.org"] },
            "crates": { "hosts": ["crates.io", "*.crates.io"] }
        },
        "profiles": { "default": { "groups": ["model_api", "npm", "crates"] } },
        "default_profile": "default",
        "injection": {},
        "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 }
    }))
    .unwrap()
}

fn attempt(n: u128) -> AttemptId {
    AttemptId::from_parts(T0 as u64, n).unwrap()
}

fn job_request(n: u128) -> IssueJobTokenRequest {
    serde_json::from_value(json!({
        "job_id": attempt(n).to_string(),
        "sandbox_id": "sbx_01JBX4R2M6P8Q1S3T5V7W9Y0ZA",
        "project_id": "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA",
        "ledger": { "subject": "delivery", "id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD" },
        "tier": "worker",
        "provider": "anthropic",
        "profile": "default",
        "groups": ["model_api", "npm", "crates"],
        "tokens_total": 1000,
        "token_weights": { "input": 1.0, "output": 1.0, "cache_read": 1.0, "cache_write": 1.0 },
        "wall_time_s": 1800,
        "config_rev": 7
    }))
    .unwrap()
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage::new(input, output, 0, 0, 1)
}

fn ok(usage: TokenUsage) -> Metered {
    Metered {
        usage,
        incomplete: false,
        upstream: UpstreamClass::Ok,
    }
}

fn meta(call: Ulid) -> SvcCallMeta {
    SvcCallMeta {
        call_id: call,
        ledger: Some(LedgerRef::Delivery {
            id: "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD".parse().unwrap(),
        }),
        tier: Tier::Utility,
    }
}

fn call(n: u128) -> Ulid {
    Ulid::from_parts(T0 as u64 + 1, n).unwrap()
}

fn token_invalid(result: Result<impl Sized, Denial>) -> &'static str {
    match result {
        Err(Denial::TokenInvalid { rule }) => rule,
        Err(other) => panic!("expected token_invalid, got {other:?}"),
        Ok(_) => panic!("expected token_invalid, got success"),
    }
}

// ---------------------------------------------------------------------------
// 配置（§12.F）
// ---------------------------------------------------------------------------

#[test]
fn without_config_everything_is_refused_and_unhealthy() {
    let harness = Harness::new();
    assert_eq!(
        harness.registry.issue_job(job_request(1)),
        Err(ControlError::NotConfigured)
    );
    let status = harness.registry.status().unwrap();
    assert_eq!(status.health, EgressHealth::Unhealthy);
    assert_eq!(status.applied_rev, None);
    assert!(status.detail.unwrap().contains("no config applied"));
    assert_eq!(
        token_invalid(harness.registry.authenticate(
            Listener::Sandbox,
            "gjt1_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        )),
        "token.unknown"
    );

    harness.setup();
    let status = harness.registry.status().unwrap();
    assert_eq!(status.health, EgressHealth::Ok);
    assert_eq!(status.applied_rev, Some(7));
    assert_eq!(status.detail.as_deref(), Some("source_check: unverified"));
}

#[test]
fn invalid_view_is_rejected_old_view_kept_and_health_degraded() {
    let harness = Harness::configured();
    let mut bad = serde_json::to_value(view(8)).unwrap();
    bad["groups"]["npm"]["hosts"] = json!(["api.github.com"]);
    let bad: ConfigView = serde_json::from_value(bad).unwrap();
    let error = harness.registry.apply_config(bad).unwrap_err();
    assert_eq!(error.code(), "config_invalid");
    assert_eq!(error.errors()[0].pointer, "/groups/npm/hosts/0");

    let status = harness.registry.status().unwrap();
    assert_eq!(status.applied_rev, Some(7));
    assert_eq!(status.health, EgressHealth::Degraded);
    assert!(status.detail.unwrap().contains("/groups/npm/hosts/0"));

    // 旧视图继续生效；下一次合法视图恢复 ok。
    assert!(harness.registry.issue_job(job_request(1)).is_ok());
    assert_eq!(harness.registry.apply_config(view(9)), Ok(9));
    assert_eq!(harness.registry.status().unwrap().health, EgressHealth::Ok);
}

#[test]
fn config_rev_is_monotonic_and_idempotent() {
    let harness = Harness::configured();
    assert_eq!(harness.registry.apply_config(view(7)), Ok(7));
    assert_eq!(
        harness.registry.apply_config(view(6)),
        Err(ControlError::StaleRev)
    );
    assert_eq!(ControlError::StaleRev.status(), 409);
    assert_eq!(harness.registry.apply_config(view(8)), Ok(8));
}

#[test]
fn tightening_applies_to_existing_jobs_widening_only_to_new_ones() {
    let harness = Harness::configured();
    let (_, req) = harness.issue(1);
    let npm = Host::parse("registry.npmjs.org").unwrap();
    assert!(harness.registry.host_allowed(&req.job_id, &npm));

    // 收紧：档案去掉 npm，存量 job 立即失去 npm。
    let mut narrowed = serde_json::to_value(view(8)).unwrap();
    narrowed["profiles"]["default"]["groups"] = json!(["model_api", "crates"]);
    harness
        .registry
        .apply_config(serde_json::from_value(narrowed).unwrap())
        .unwrap();
    assert!(!harness.registry.host_allowed(&req.job_id, &npm));

    // 放宽：新增分组 pypi 进档案，存量 job 的快照里没有它。
    let mut widened = serde_json::to_value(view(9)).unwrap();
    widened["groups"]["pypi"] = json!({ "hosts": ["pypi.org"] });
    widened["profiles"]["default"]["groups"] = json!(["model_api", "npm", "crates", "pypi"]);
    harness
        .registry
        .apply_config(serde_json::from_value(widened).unwrap())
        .unwrap();
    let pypi = Host::parse("pypi.org").unwrap();
    assert!(!harness.registry.host_allowed(&req.job_id, &pypi));
    assert!(harness.registry.host_allowed(&req.job_id, &npm));
    let crates = Host::parse("index.crates.io").unwrap();
    assert!(harness.registry.host_allowed(&req.job_id, &crates));
}

// ---------------------------------------------------------------------------
// job_token（§12.A）
// ---------------------------------------------------------------------------

#[test]
fn issue_requires_an_activated_provider_credential() {
    let harness = Harness::new();
    harness.registry.apply_config(view(7)).unwrap();
    let error = harness.registry.issue_job(job_request(1)).unwrap_err();
    assert_eq!(error.code(), "credential_missing");
    let status = harness.registry.status().unwrap();
    assert_eq!(status.health, EgressHealth::Degraded);
    assert_eq!(
        status.unresolved_secret_refs,
        vec![SecretName::parse("model.anthropic.main").unwrap()]
    );
}

#[test]
fn issue_validates_profile_groups_and_provider() {
    let harness = Harness::configured();
    let mut req = job_request(1);
    req.groups.push("pypi".into());
    req.provider = "openai".into();
    let error = harness.registry.issue_job(req).unwrap_err();
    assert_eq!(error.code(), "validation_failed");
    let pointers: Vec<_> = error.errors().iter().map(|e| e.pointer.as_str()).collect();
    assert_eq!(pointers, vec!["/provider", "/groups/3"]);
}

#[test]
fn grant_carries_token_bound_urls_and_effective_groups() {
    let harness = Harness::configured();
    let grant = harness.registry.issue_job(job_request(1)).unwrap();
    let token = grant.job_token.expose_secret();
    assert!(token.starts_with("gjt1_"));
    assert_eq!(
        grant.model_base_urls.get("anthropic"),
        Some(format!("http://10.89.0.1:8081/j/{token}/anthropic").as_str())
    );
    assert_eq!(
        grant.connect_proxy_url.expose_secret(),
        format!("http://job:{token}@10.89.0.1:3128")
    );
    assert_eq!(grant.effective_groups, vec!["model_api", "npm", "crates"]);
    assert_eq!(
        grant.expires_at.as_millis() - grant.issued_at.as_millis(),
        (1800 + 600) * 1000
    );
    assert!(!grant.reissued);
}

#[test]
fn reissue_with_same_binding_rotates_the_token_and_keeps_usage() {
    let harness = Harness::configured();
    let (old, req) = harness.issue(1);
    harness.job_request(&old, usage(100, 10)).unwrap();

    let mut replay = req.clone();
    replay.config_rev = Seq::from_wire(8).unwrap();
    let grant = harness.registry.issue_job(replay).unwrap();
    assert!(grant.reissued);
    let new = grant.job_token.expose_secret().to_owned();
    assert_ne!(old, new);
    assert_eq!(
        token_invalid(harness.registry.authenticate(Listener::Sandbox, &old)),
        "token.revoked"
    );
    harness.job_request(&new, usage(1, 1)).unwrap();
    let snapshot = harness.registry.job_usage(&req.job_id).unwrap();
    assert_eq!(snapshot.usage, TokenUsage::new(101, 11, 0, 0, 2));

    let mut other = req.clone();
    other.tokens_total += 1;
    assert_eq!(
        harness.registry.issue_job(other),
        Err(ControlError::BindingMismatch)
    );
}

#[tokio::test]
async fn revoke_is_final_idempotent_and_forget_then_not_found() {
    let harness = Harness::configured();
    let (token, req) = harness.issue(1);
    harness.job_request(&token, usage(10, 2)).unwrap();

    let first = harness.registry.revoke_job(&req.job_id).await.unwrap();
    assert_eq!(first.state, JobTokenState::Revoked);
    assert_eq!(first.revoked_at, Some(harness.clock.now_timestamp()));
    assert_eq!(first.usage, TokenUsage::new(10, 2, 0, 0, 1));
    assert_eq!(
        token_invalid(harness.registry.authenticate(Listener::Sandbox, &token)),
        "token.revoked"
    );
    harness.clock.advance_secs(5);
    assert_eq!(
        harness.registry.revoke_job(&req.job_id).await.unwrap(),
        first
    );
    assert_eq!(harness.registry.job_usage(&req.job_id).unwrap(), first);
    assert_eq!(
        harness.registry.issue_job(req.clone()),
        Err(ControlError::JobRevoked)
    );

    harness.registry.forget_job(&req.job_id).unwrap();
    harness.registry.forget_job(&req.job_id).unwrap();
    assert_eq!(
        harness.registry.revoke_job(&req.job_id).await,
        Err(ControlError::NotFound)
    );
    assert_eq!(
        harness.registry.job_usage(&req.job_id),
        Err(ControlError::NotFound)
    );
}

#[tokio::test]
async fn revoked_jobs_are_forgotten_after_the_ttl() {
    let harness = Harness::configured();
    let (_, req) = harness.issue(1);
    harness.registry.revoke_job(&req.job_id).await.unwrap();
    harness.clock.advance_secs(86_400 - 1);
    assert!(harness.registry.job_usage(&req.job_id).is_ok());
    harness.clock.advance_secs(1);
    assert_eq!(
        harness.registry.job_usage(&req.job_id),
        Err(ControlError::NotFound)
    );
}

#[test]
fn tokens_expire_at_the_hard_limit() {
    let harness = Harness::configured();
    let (token, _) = harness.issue(1);
    harness.clock.advance_secs(1800 + 600 - 1);
    assert!(harness
        .registry
        .authenticate(Listener::Sandbox, &token)
        .is_ok());
    harness.clock.advance_secs(1);
    assert_eq!(
        token_invalid(harness.registry.authenticate(Listener::Sandbox, &token)),
        "token.expired"
    );
}

#[tokio::test]
async fn revoke_waits_for_in_flight_requests_and_counts_them() {
    let harness = Harness::configured();
    let (token, req) = harness.issue(1);
    let caller = harness
        .registry
        .authenticate(Listener::Sandbox, &token)
        .unwrap();
    let admitted = harness
        .registry
        .admit(&caller, "anthropic", "m", None)
        .unwrap();

    let registry = harness.registry.clone();
    let job_id = req.job_id;
    let revoke = tokio::spawn(async move { registry.revoke_job(&job_id).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!revoke.is_finished());
    // 吊销开始后，新请求立即被拒。
    assert_eq!(
        token_invalid(harness.registry.authenticate(Listener::Sandbox, &token)),
        "token.revoked"
    );
    harness.registry.finish(admitted, ok(usage(7, 3))).unwrap();
    let final_usage = revoke.await.unwrap().unwrap();
    assert_eq!(final_usage.usage, TokenUsage::new(7, 3, 0, 0, 1));
}

#[test]
fn jobs_are_metered_independently_and_capped_on_weighted_usage() {
    let harness = Harness::configured();
    let (a, a_req) = harness.issue(1);
    let mut b_req = job_request(2);
    b_req.tokens_total = 100;
    b_req.token_weights = TokenWeights {
        input: 1.0,
        output: 5.0,
        cache_read: 0.1,
        cache_write: 1.25,
    };
    let b = harness
        .registry
        .issue_job(b_req.clone())
        .unwrap()
        .job_token
        .expose_secret()
        .to_owned();

    // b：加权 = 10×1 + 17×5 + 40×0.1 = 99（= tokens_total − 1）→ 下一个请求放行。
    harness
        .job_request(&b, TokenUsage::new(10, 17, 40, 0, 1))
        .unwrap();
    assert_eq!(
        harness
            .registry
            .job_usage(&b_req.job_id)
            .unwrap()
            .weighted_tokens,
        99
    );
    harness
        .job_request(&b, TokenUsage::new(1, 0, 0, 0, 1))
        .unwrap();
    // 达到 tokens_total 后，后续请求 403 budget_exhausted、capped。
    assert_eq!(
        harness.job_request(&b, usage(1, 0)),
        Err(Denial::BudgetExhausted)
    );
    let b_usage = harness.registry.job_usage(&b_req.job_id).unwrap();
    assert!(b_usage.capped);
    assert_eq!(b_usage.weighted_tokens, 100);
    assert_eq!(b_usage.denied.budget_exhausted, 1);

    // a 不受 b 影响。
    harness.job_request(&a, usage(5, 5)).unwrap();
    let a_usage = harness.registry.job_usage(&a_req.job_id).unwrap();
    assert_eq!(a_usage.usage, TokenUsage::new(5, 5, 0, 0, 1));
    assert!(!a_usage.capped);
}

#[test]
fn provider_must_match_the_binding() {
    let harness = Harness::configured();
    let (token, _) = harness.issue(1);
    let caller = harness
        .registry
        .authenticate(Listener::Sandbox, &token)
        .unwrap();
    assert!(matches!(
        harness.registry.admit(&caller, "other", "m", None),
        Err(Denial::ProviderNotAllowed)
    ));
}

#[test]
fn tokens_only_work_on_their_own_listener() {
    let harness = Harness::configured();
    let (job, _) = harness.issue(1);
    let (svc, _) = harness.svc();
    assert_eq!(
        token_invalid(harness.registry.authenticate(Listener::Loopback, &job)),
        "token.wrong_listener"
    );
    assert_eq!(
        token_invalid(harness.registry.authenticate(Listener::Sandbox, &svc)),
        "token.wrong_listener"
    );
}

#[test]
fn upstream_errors_and_denials_are_summarised() {
    let harness = Harness::configured();
    let (token, req) = harness.issue(1);
    let caller = harness
        .registry
        .authenticate(Listener::Sandbox, &token)
        .unwrap();
    for class in [
        UpstreamClass::Status429,
        UpstreamClass::Status5xx,
        UpstreamClass::Ok,
        UpstreamClass::ConnError,
        UpstreamClass::Status5xx,
        UpstreamClass::Status5xx,
    ] {
        let admitted = harness
            .registry
            .admit(&caller, "anthropic", "m", None)
            .unwrap();
        harness
            .registry
            .finish(
                admitted,
                Metered {
                    usage: TokenUsage::default(),
                    incomplete: false,
                    upstream: class,
                },
            )
            .unwrap();
    }
    harness
        .registry
        .note_denied(&caller, DeniedKind::PathNotAllowed)
        .unwrap();
    harness
        .registry
        .note_denied(&caller, DeniedKind::NotAllowedHost)
        .unwrap();
    let snapshot = harness.registry.job_usage(&req.job_id).unwrap();
    assert_eq!(snapshot.upstream_errors.total_429, 1);
    assert_eq!(snapshot.upstream_errors.total_5xx, 3);
    assert_eq!(snapshot.upstream_errors.total_conn_errors, 1);
    assert_eq!(snapshot.upstream_errors.max_consecutive, 3);
    assert_eq!(snapshot.denied.path_not_allowed, 1);
    assert_eq!(snapshot.denied.not_allowed_host, 1);
}

// ---------------------------------------------------------------------------
// svc_token、用量拉取与调用级关闭（§12.C；egress-call-terminal §7）
// ---------------------------------------------------------------------------

#[test]
fn issuing_a_svc_token_revokes_the_previous_one() {
    let harness = Harness::configured();
    let (first, first_id) = harness.svc();
    harness.svc_request(&first, call(1), usage(3, 4)).unwrap();
    let grant = harness
        .registry
        .issue_svc(IssueSvcTokenRequest {
            issued_to: IssuedTo {
                process: "gantryd".into(),
                pid: 1,
                started_at: harness.clock.now_timestamp(),
            },
            ttl_s: Some(3600),
        })
        .unwrap();
    assert_eq!(grant.revoked_previous, vec![first_id.clone()]);
    let second = grant.svc_token.expose_secret();
    assert_eq!(
        grant.model_base_urls.get("anthropic"),
        Some(format!("http://127.0.0.1:8082/s/{second}/anthropic").as_str())
    );
    assert_eq!(
        token_invalid(harness.registry.authenticate(Listener::Loopback, &first)),
        "token.revoked"
    );
    let page = harness
        .registry
        .svc_token_usage(&first_id, None, None)
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(harness.registry.status().unwrap().active_svc_tokens, 1);

    let error = harness
        .registry
        .issue_svc(IssueSvcTokenRequest {
            issued_to: IssuedTo {
                process: "gantryd".into(),
                pid: 1,
                started_at: harness.clock.now_timestamp(),
            },
            ttl_s: Some(60),
        })
        .unwrap_err();
    assert_eq!(error.code(), "validation_failed");
}

#[test]
fn records_carry_attribution_and_weight_revision() {
    let harness = Harness::configured();
    let (job, req) = harness.issue(1);
    harness.job_request(&job, usage(10, 2)).unwrap();
    harness.registry.apply_config(view(9)).unwrap();
    let (svc, svc_id) = harness.svc();
    harness.svc_request(&svc, call(1), usage(100, 10)).unwrap();

    let page = harness.registry.usage_since(None, None).unwrap();
    let [job_record, svc_record] = page.items.as_slice() else {
        panic!("two records");
    };
    assert_eq!(job_record.caller, UsageCaller::Job { job_id: req.job_id });
    assert_eq!(
        job_record.weight_rev, 7,
        "job records use the issuing config_rev"
    );
    assert_eq!(job_record.weighted_tokens, 12);
    assert_eq!(job_record.tier, Tier::Worker);
    assert_eq!(job_record.project_id, Some(req.project_id));
    assert_eq!(
        svc_record.caller,
        UsageCaller::Svc {
            svc_token_id: svc_id,
            call_id: call(1)
        }
    );
    assert_eq!(svc_record.weight_rev, 9, "svc records use the applied rev");
    assert_eq!(svc_record.weighted_tokens, 150);
    assert_eq!(svc_record.tier, Tier::Utility);
    assert_eq!(svc_record.ledger, meta(call(1)).ledger);
    assert_eq!(svc_record.project_id, None);
    assert_eq!(page.next_cursor, svc_record.seq.to_string());
}

#[test]
fn usage_pages_have_no_gaps_or_duplicates() {
    let harness = Harness::configured();
    let (svc, _) = harness.svc();
    for n in 0..25 {
        harness.svc_request(&svc, call(n), usage(1, 1)).unwrap();
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = harness
            .registry
            .usage_since(cursor.as_deref(), Some(7))
            .unwrap();
        if page.items.is_empty() {
            assert_eq!(cursor.as_deref(), Some(page.next_cursor.as_str()));
            break;
        }
        seen.extend(page.items.iter().map(|record| record.seq));
        cursor = Some(page.next_cursor);
    }
    assert_eq!(seen, (1..=25).collect::<Vec<_>>());
    // 重复拉同一游标得到同一批。
    let a = harness.registry.usage_since(Some("10"), Some(5)).unwrap();
    let b = harness.registry.usage_since(Some("10"), Some(5)).unwrap();
    assert_eq!(a, b);
    assert_eq!(
        harness
            .registry
            .usage_since(Some("x"), None)
            .unwrap_err()
            .code(),
        "invalid_request"
    );
}

#[test]
fn cursors_older_than_the_retention_window_expire() {
    let harness = Harness::configured();
    let (svc, _) = harness.svc();
    harness.svc_request(&svc, call(1), usage(1, 1)).unwrap();
    harness.clock.advance_secs(7 * 86_400);
    let (svc, _) = harness.svc();
    harness.svc_request(&svc, call(2), usage(1, 1)).unwrap();
    assert_eq!(
        harness.registry.usage_since(Some("0"), None),
        Err(ControlError::CursorExpired)
    );
    let page = harness.registry.usage_since(None, None).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].seq, 2);
}

#[tokio::test]
async fn closing_a_call_fences_it_and_returns_a_verifiable_terminal() {
    let harness = Harness::configured();
    let (svc, svc_id) = harness.svc();
    harness
        .svc_request(&svc, call(1), usage(3201, 410))
        .unwrap();
    harness.svc_request(&svc, call(1), usage(1, 1)).unwrap();

    assert!(harness
        .registry
        .call_statement(call(1))
        .unwrap()
        .terminal
        .is_none());
    let closed = harness.registry.close_call(&svc_id, call(1)).await.unwrap();
    let terminal = closed.terminal.clone();
    assert_eq!(terminal.record_count, 2);
    assert_eq!(terminal.svc_token_ids, vec![svc_id.clone()]);
    assert!(!terminal.incomplete);

    // 关闭后同一 call_id 的请求一律 token_invalid，无新记录。
    assert_eq!(
        token_invalid(harness.svc_request(&svc, call(1), usage(1, 1))),
        "call.closed"
    );
    let statement = harness.registry.call_statement(call(1)).unwrap();
    assert_eq!(statement.items.len(), 2);
    assert_eq!(statement.terminal.as_ref(), Some(&terminal));
    terminal.verify(call(1), &statement.items).unwrap();

    // 幂等：重复关闭返回同一份证明，读取不改变状态。
    harness.clock.advance_secs(10);
    assert_eq!(
        harness.registry.close_call(&svc_id, call(1)).await.unwrap(),
        closed
    );
    assert_eq!(harness.registry.call_statement(call(1)).unwrap(), statement);
}

#[tokio::test]
async fn zero_receipt_calls_close_with_the_empty_digest() {
    let harness = Harness::configured();
    let (_, svc_id) = harness.svc();
    let closed = harness.registry.close_call(&svc_id, call(9)).await.unwrap();
    assert_eq!(closed.terminal.record_count, 0);
    assert_eq!(closed.terminal.last_seq, 0);
    assert!(closed.terminal.svc_token_ids.is_empty());
    assert_eq!(closed.terminal.records_digest, records_digest(&[]).unwrap());
}

#[tokio::test]
async fn closing_requires_an_associated_token() {
    let harness = Harness::configured();
    let (a, a_id) = harness.svc();
    harness.svc_request(&a, call(1), usage(1, 1)).unwrap();
    let (_, b_id) = harness.svc(); // 签发 b 同时吊销 a
    assert!(matches!(
        harness.registry.close_call(&b_id, call(1)).await,
        Err(ControlError::Conflict(_))
    ));
    // 已吊销的 a 仍可关闭它的旧调用（崩溃恢复）。
    let closed = harness.registry.close_call(&a_id, call(1)).await.unwrap();
    assert_eq!(closed.terminal.svc_token_ids, vec![a_id]);

    let unknown = "svt_01JBX5A2C4E6G8J1L3N5Q7S9UW".parse().unwrap();
    assert_eq!(
        harness.registry.close_call(&unknown, call(2)).await,
        Err(ControlError::NotFound)
    );
}

#[tokio::test]
async fn closing_waits_for_in_flight_requests() {
    let harness = Harness::configured();
    let (svc, svc_id) = harness.svc();
    let caller = harness
        .registry
        .authenticate(Listener::Loopback, &svc)
        .unwrap();
    let admitted = harness
        .registry
        .admit(&caller, "anthropic", "m", Some(&meta(call(1))))
        .unwrap();

    let registry = harness.registry.clone();
    let close = tokio::spawn(async move { registry.close_call(&svc_id, call(1)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!close.is_finished());
    // draining：新请求被拒。
    assert_eq!(
        token_invalid(harness.svc_request(&svc, call(1), usage(1, 1))),
        "call.closed"
    );
    harness.registry.finish(admitted, ok(usage(5, 5))).unwrap();
    let closed = close.await.unwrap().unwrap();
    assert_eq!(closed.terminal.record_count, 1);
    assert_eq!(closed.terminal.usage, TokenUsage::new(5, 5, 0, 0, 1));
}

#[tokio::test]
async fn interrupted_requests_make_the_terminal_incomplete() {
    let harness = Harness::configured();
    let (svc, svc_id) = harness.svc();
    let caller = harness
        .registry
        .authenticate(Listener::Loopback, &svc)
        .unwrap();
    let admitted = harness
        .registry
        .admit(&caller, "anthropic", "m", Some(&meta(call(1))))
        .unwrap();
    drop(admitted); // 任务被取消
    let closed = harness.registry.close_call(&svc_id, call(1)).await.unwrap();
    assert!(closed.terminal.incomplete);
    assert_eq!(closed.terminal.record_count, 1);
    let statement = harness.registry.call_statement(call(1)).unwrap();
    closed.terminal.verify(call(1), &statement.items).unwrap();
}

#[tokio::test]
async fn terminals_past_retention_stay_closed_but_incomplete() {
    let harness = Harness::configured();
    let (svc, svc_id) = harness.svc();
    harness.svc_request(&svc, call(1), usage(1, 1)).unwrap();
    let closed = harness.registry.close_call(&svc_id, call(1)).await.unwrap();
    harness.clock.advance_secs(7 * 86_400);
    let statement = harness.registry.call_statement(call(1)).unwrap();
    assert!(statement.items.is_empty());
    let terminal = statement.terminal.unwrap();
    assert!(terminal.incomplete);
    assert_eq!(terminal.records_digest, closed.terminal.records_digest);
    // 墓碑永久保留：同一 call_id 仍不能被重新接收（用新进程的令牌也不行）。
    let (fresh, _) = harness.svc();
    assert_eq!(
        token_invalid(harness.svc_request(&fresh, call(1), usage(1, 1))),
        "call.closed"
    );
    assert!(
        harness
            .registry
            .close_call(&svc_id, call(1))
            .await
            .unwrap()
            .terminal
            .incomplete
    );
}

// ---------------------------------------------------------------------------
// 密钥（§12.G）
// ---------------------------------------------------------------------------

#[test]
fn secrets_are_staged_then_activated_or_discarded() {
    let harness = Harness::new();
    harness.registry.apply_config(view(7)).unwrap();
    let name = SecretName::parse("model.anthropic.main").unwrap();
    let request = Ulid::from_parts(T0 as u64, 42).unwrap();
    let value = || SecretValue::from_bytes(b"sk-gantry-sentinel-xyz".to_vec());

    let staged = harness
        .registry
        .stage_secret(request, name.clone(), value(), &ctx())
        .unwrap();
    assert!(
        !harness
            .registry
            .secrets_present(std::slice::from_ref(&name))
            .unwrap()[&name]
    );
    assert_eq!(harness.registry.staged_secrets(&ctx()).unwrap().len(), 1);
    // 同一 request_id 同值重放 → 同一 staged_id；不同值 → conflict。
    assert_eq!(
        harness
            .registry
            .stage_secret(request, name.clone(), value(), &ctx())
            .unwrap(),
        staged
    );
    assert!(matches!(
        harness.registry.stage_secret(
            request,
            name.clone(),
            SecretValue::from_bytes(b"other".to_vec()),
            &ctx()
        ),
        Err(ControlError::Conflict(_))
    ));

    harness
        .registry
        .activate_secret(&staged.staged_id, &ctx())
        .unwrap();
    assert!(
        harness
            .registry
            .secrets_present(std::slice::from_ref(&name))
            .unwrap()[&name]
    );
    assert!(harness.registry.staged_secrets(&ctx()).unwrap().is_empty());
    // 已激活的再激活原样成功（同一激活时刻）。
    let first = harness
        .registry
        .activate_secret(&staged.staged_id, &ctx())
        .unwrap();
    harness.clock.advance_secs(5);
    assert_eq!(
        harness
            .registry
            .activate_secret(&staged.staged_id, &ctx())
            .unwrap(),
        first
    );

    // 被当前视图引用的 secret 不能回收。
    assert!(matches!(
        harness.registry.delete_secret(&name, &ctx()),
        Err(ControlError::Conflict(_))
    ));
    let unused = SecretName::parse("registry.npm.corp").unwrap();
    harness.registry.delete_secret(&unused, &ctx()).unwrap();

    // 丢弃：暂存值从不可见。
    let discarded = harness
        .registry
        .stage_secret(
            Ulid::from_parts(T0 as u64, 43).unwrap(),
            unused.clone(),
            value(),
            &ctx(),
        )
        .unwrap();
    harness
        .registry
        .discard_secret(&discarded.staged_id, &ctx())
        .unwrap();
    harness
        .registry
        .discard_secret(&discarded.staged_id, &ctx())
        .unwrap();
    assert!(
        !harness
            .registry
            .secrets_present(std::slice::from_ref(&unused))
            .unwrap()[&unused]
    );
    assert!(harness.secrets.staged().unwrap().is_empty());
}

#[test]
fn staged_secrets_expire() {
    let harness = Harness::new();
    let name = SecretName::parse("model.anthropic.main").unwrap();
    let staged = harness
        .registry
        .stage_secret(
            Ulid::from_parts(T0 as u64, 1).unwrap(),
            name.clone(),
            SecretValue::from_bytes(b"v".to_vec()),
            &ctx(),
        )
        .unwrap();
    assert_eq!(
        staged.expires_at.as_millis() - staged.staged_at.as_millis(),
        600_000
    );
    harness.clock.advance_secs(600);
    assert_eq!(
        harness.registry.activate_secret(&staged.staged_id, &ctx()),
        Err(ControlError::StagedExpired)
    );
    assert!(harness.registry.staged_secrets(&ctx()).unwrap().is_empty());
    assert!(
        !harness
            .registry
            .secrets_present(std::slice::from_ref(&name))
            .unwrap()[&name]
    );

    let empty = harness.registry.stage_secret(
        Ulid::from_parts(T0 as u64, 2).unwrap(),
        name,
        SecretValue::from_bytes(Vec::new()),
        &ctx(),
    );
    assert_eq!(empty.unwrap_err().code(), "validation_failed");
}

// ---------------------------------------------------------------------------
// 持久化与重启（§12.A）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_restores_tokens_usage_caps_and_marks_in_flight_jobs_incomplete() {
    let harness = Harness::on_disk("restart");
    harness.setup();
    let mut capped_req = job_request(2);
    capped_req.tokens_total = 10;
    let capped = harness
        .registry
        .issue_job(capped_req.clone())
        .unwrap()
        .job_token
        .expose_secret()
        .to_owned();
    harness.job_request(&capped, usage(10, 0)).unwrap();
    assert_eq!(
        harness.job_request(&capped, usage(1, 0)),
        Err(Denial::BudgetExhausted)
    );

    let (token, req) = harness.issue(1);
    harness.job_request(&token, usage(3, 3)).unwrap();
    let (svc, svc_id) = harness.svc();
    harness.svc_request(&svc, call(1), usage(1, 1)).unwrap();
    // 崩溃时有一个在途请求：既不完成也不触发 Drop。
    let caller = harness
        .registry
        .authenticate(Listener::Sandbox, &token)
        .unwrap();
    std::mem::forget(
        harness
            .registry
            .admit(&caller, "anthropic", "m", None)
            .unwrap(),
    );

    let harness = harness.restart();
    let restored = harness.registry.job_usage(&req.job_id).unwrap();
    assert!(restored.incomplete, "in-flight at crash → incomplete");
    assert_eq!(restored.usage.input_tokens, 3);
    let capped_usage = harness.registry.job_usage(&capped_req.job_id).unwrap();
    assert!(capped_usage.capped);
    assert_eq!(
        harness.job_request(&capped, usage(1, 0)),
        Err(Denial::BudgetExhausted)
    );

    // 令牌继续有效，归属不变；applied_rev 不回退。
    assert_eq!(
        harness.registry.authenticate(Listener::Sandbox, &token),
        Ok(Caller::Job { job_id: req.job_id })
    );
    assert_eq!(
        harness.registry.authenticate(Listener::Loopback, &svc),
        Ok(Caller::Svc {
            svc_token_id: svc_id
        })
    );
    assert_eq!(harness.registry.status().unwrap().applied_rev, Some(7));
    let seqs: Vec<u64> = harness
        .registry
        .usage_since(None, None)
        .unwrap()
        .items
        .iter()
        .map(|record| record.seq)
        .collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4],
        "the interrupted request leaves a record"
    );
    harness.job_request(&token, usage(1, 1)).unwrap();
    assert_eq!(
        harness.registry.usage_since(Some("4"), None).unwrap().items[0].seq,
        5
    );
}

#[test]
fn the_database_holds_only_token_hashes() {
    let harness = Harness::on_disk("hashes");
    harness.setup();
    let (job, _) = harness.issue(1);
    let (svc, _) = harness.svc();
    harness.job_request(&job, usage(1, 1)).unwrap();
    let path = harness.path.clone().unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    if let Ok(wal) = std::fs::read(format!("{}-wal", path.display())) {
        bytes.extend(wal);
    }
    let text = String::from_utf8_lossy(&bytes);
    for needle in [&job[5..], &svc[5..], "sk-gantry-sentinel"] {
        assert!(
            !text.contains(needle),
            "egress.db must not contain {needle}"
        );
    }
}
