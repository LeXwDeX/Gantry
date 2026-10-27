//! `EgressStub`：实现控制面端口，并起一个回环 HTTP 假数据面。
//!
//! 覆盖签发 / 重签 / 吊销 / 遗忘 `job_token`、进程级 `svc_token` 轮换、用量
//! 拉取、调用级关闭与终结证明（`egress-call-terminal.md`）、配置视图、按 request_id
//! 幂等的 secret 暂存。数据面接受
//! `/j/<job_token>/<provider>/…` 与 `/s/<svc_token>/<provider>/…`，拒绝错误、
//! 已撤销或过期令牌。它不是 Egress 的真实实现，也不做 DNS、TLS 或上游转发。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use gantry_core::egress::{
    CallTerminal, CloseCallResponse, ConfigApplyResponse, ConfigView, ConnectProxyUrl,
    DeniedCounters, FinalUsage, IssueSvcTokenResponse, JobToken, JobTokenState, LedgerRef,
    ModelBaseUrls, StagedId, StagedSecretItem, SvcToken, SvcTokenId, UpstreamErrors, UsageCaller,
    UsagePage, UsageRecord,
};
use gantry_core::entities::{Seq, Tier, TokenUsage, TokenWeights};
use gantry_core::ids::{AttemptId, DeliveryId, Ulid};
use gantry_core::ports::context::{ActorCtx, Health};
use gantry_core::ports::egress::{
    CallClosed, CallStatement, CallUsage, EgressConfigView, EgressControl, EgressEndpoints,
    EgressError, EgressStatus, InjectionId, IssueJobToken, JobTokenGrant, JobUsage, ProviderId,
    SvcTokenGrant, UsageCursor,
};
use gantry_core::ports::models::{CallId, UsageSource};
use gantry_core::ports::sandbox::GroupName;
use gantry_core::ports::secrets::SecretVersion;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::time::Timestamp;
use serde_json::Value;

use crate::github::{discard_pending_input, HttpRequest, ReadOutcome};

const JOB_TOKEN_TTL: Duration = Duration::from_secs(3600);
const SVC_TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);
const STAGED_TTL: Duration = Duration::from_secs(600);
const DEFAULT_RESPONSE: &str = concat!(
    r#"{"id":"msg_fake","type":"message","role":"assistant","content":"#,
    r#"[{"type":"text","text":"fake egress response"}]}"#
);

/// EgressStub 的调用记录；不含任何令牌原文或 secret 值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressCall {
    Endpoints,
    IssueJobToken {
        job_id: AttemptId,
        provider: ProviderId,
        ctx: ActorCtx,
    },
    JobUsage {
        job_id: AttemptId,
    },
    RevokeJobToken {
        job_id: AttemptId,
        ctx: ActorCtx,
    },
    ForgetJob {
        job_id: AttemptId,
        ctx: ActorCtx,
    },
    ServiceToken {
        ctx: ActorCtx,
    },
    RotateServiceToken {
        ctx: ActorCtx,
    },
    ApplyConfig {
        rev: u64,
        ctx: ActorCtx,
    },
    SecretPresent {
        refs: Vec<SecretName>,
    },
    StageSecret {
        request_id: Ulid,
        secret_ref: SecretName,
        ctx: ActorCtx,
    },
    ActivateSecret {
        staged_id: StagedId,
        ctx: ActorCtx,
    },
    DiscardSecret {
        staged_id: StagedId,
        ctx: ActorCtx,
    },
    StagedSecrets,
    CallUsage {
        call_id: CallId,
    },
    UsageSince {
        cursor: UsageCursor,
        limit: u32,
    },
    SvcTokenUsage {
        svc_token_id: SvcTokenId,
    },
    CloseCall {
        svc_token_id: SvcTokenId,
        call_id: CallId,
        ctx: ActorCtx,
    },
    CallTerminal {
        call_id: CallId,
    },
    Status,
    DataPlane {
        kind: &'static str,
        subject: String,
        provider: String,
        status: u16,
    },
    DataPlaneRejected {
        kind: &'static str,
        status: u16,
        code: String,
    },
}

#[derive(Clone, Debug)]
pub struct StubFailure {
    pub status: u16,
    pub body: String,
    pub retry_after_s: Option<u64>,
}

#[derive(Debug)]
struct StageEntry {
    consumer: Consumer,
    staged_id: StagedId,
    secret_ref: SecretName,
    version: u64,
    value: String,
    activated: bool,
    discarded: bool,
    staged_at: Timestamp,
    expires_at: Instant,
}

struct JobEntry {
    request: IssueJobToken,
    ledger: LedgerRef,
    active_token: Option<String>,
    expires_at_instant: Instant,
    revoked_at: Option<Timestamp>,
    final_usage: Option<FinalUsage>,
    /// 签发时的权重修订；该令牌的用量记录都带它（`UsageRecord.weight_rev`）。
    weight_rev: u64,
    usage: TokenUsage,
    weighted_tokens: u64,
    capped: bool,
    incomplete: bool,
    upstream_errors: UpstreamErrors,
    denied: DeniedCounters,
}

struct SvcEntry {
    id: SvcTokenId,
    token: String,
    expires_at_instant: Instant,
}

struct State {
    addr: SocketAddr,
    config: Option<ConfigView>,
    applied_rev: Option<u64>,
    health: Health,
    detail: Option<String>,
    jobs: BTreeMap<AttemptId, JobEntry>,
    job_by_token: HashMap<String, AttemptId>,
    svc: Option<SvcEntry>,
    /// 签发过的全部 `svc_token_id`（含已吊销）；调用级关闭的路径 token 只需签发过。
    svc_issued: BTreeSet<SvcTokenId>,
    /// 已关闭调用的墓碑与证明；替身没有真实 inflight，关闭即从 accepting 直达 closed。
    closed_calls: BTreeMap<CallId, CallTerminal>,
    /// 已过保留期的调用（明细已删，证明 `incomplete = true`）。
    expired_calls: BTreeSet<CallId>,
    usage: Vec<UsageRecord>,
    next_seq: u64,
    calls: Vec<EgressCall>,
    staged: BTreeMap<Ulid, StageEntry>,
    staged_by_id: BTreeMap<StagedId, Ulid>,
    active_secrets: BTreeMap<SecretName, (u64, String)>,
    secret_versions: BTreeMap<SecretName, u64>,
    counter: u64,
    response_body: String,
    job_ttl: Duration,
    svc_ttl: Duration,
    staged_ttl: Duration,
}

impl State {
    fn next_counter(&mut self) -> u64 {
        self.counter = self.counter.saturating_add(1);
        self.counter
    }

    fn record_job_call(&mut self, call: EgressCall) {
        self.calls.push(call);
    }
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(1_700_000_000_000);
    Timestamp::from_millis(ms)
}

fn token_text(prefix: &str, counter: u64, width: usize) -> String {
    format!("{prefix}{counter:0width$}")
}

fn job_model_url(state: &State, token: &str, provider: &str) -> String {
    format!(
        "http://127.0.0.1:{}/j/{token}/{provider}",
        state.addr.port()
    )
}

fn svc_model_url(state: &State, token: &str, provider: &str) -> String {
    format!(
        "http://127.0.0.1:{}/s/{token}/{provider}",
        state.addr.port()
    )
}

fn job_weights(request: &IssueJobToken) -> TokenWeights {
    request.weights
}

fn request_ledger(request: &IssueJobToken, issued_at: Timestamp) -> LedgerRef {
    match (request.delivery_id, request.conversation_id) {
        (Some(id), None) => LedgerRef::Delivery { id },
        (None, Some(id)) => LedgerRef::Conversation {
            id,
            day: issued_at.to_date(),
        },
        _ => unreachable!("IssueJobToken::validate rejects this shape"),
    }
}

fn job_tier(request: &IssueJobToken) -> Tier {
    match request.tier {
        gantry_core::egress::SandboxTier::Planner => Tier::Planner,
        gantry_core::egress::SandboxTier::Worker => Tier::Worker,
    }
}

fn final_usage(entry: &JobEntry) -> FinalUsage {
    if let Some(usage) = &entry.final_usage {
        return usage.clone();
    }
    FinalUsage {
        job_id: entry.request.job_id,
        state: if entry.revoked_at.is_some() {
            JobTokenState::Revoked
        } else {
            JobTokenState::Active
        },
        revoked_at: entry.revoked_at,
        usage: entry.usage,
        weighted_tokens: entry.weighted_tokens,
        capped: entry.capped,
        incomplete: entry.incomplete,
        upstream_errors: entry.upstream_errors.clone(),
        denied: entry.denied.clone(),
    }
}

fn job_usage(entry: &JobEntry) -> JobUsage {
    JobUsage {
        usage: entry.usage,
        weighted_tokens: entry.weighted_tokens,
        capped: entry.capped,
        incomplete: entry.incomplete,
        upstream_errors: entry.upstream_errors.clone(),
        denied: entry.denied.clone(),
    }
}

fn not_configured() -> EgressError {
    EgressError::Rejected {
        code: "not_configured".into(),
        message: "no egress config has been applied".into(),
    }
}

fn validation(pointer: &str, message: impl Into<String>) -> EgressError {
    EgressError::Rejected {
        code: "validation_failed".into(),
        message: format!("{pointer}: {}", message.into()),
    }
}

fn validate_config(view: &ConfigView) -> Result<(), EgressError> {
    if !view.profiles.contains_key(&view.default_profile) {
        return Err(EgressError::InvalidConfig {
            pointer: "/default_profile".into(),
            message: "default profile does not exist".into(),
        });
    }
    for (name, profile) in &view.profiles {
        for group in &profile.groups {
            if !view.groups.contains_key(group) {
                return Err(EgressError::InvalidConfig {
                    pointer: format!("/profiles/{name}/groups"),
                    message: format!("group {group} does not exist"),
                });
            }
        }
    }
    for (id, provider) in &view.providers {
        if host_of(&provider.base_url) == Some("api.github.com") {
            return Err(EgressError::InvalidConfig {
                pointer: format!("/providers/{id}/base_url"),
                message: "api.github.com is not an allowed model host".into(),
            });
        }
    }
    Ok(())
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split('/').next()?;
    let host = authority.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

fn validate_issue(state: &State, request: &IssueJobToken) -> Result<(), EgressError> {
    let config = state.config.as_ref().ok_or_else(not_configured)?;
    request.validate()?;
    if !config.providers.contains_key(request.provider.as_str()) {
        return Err(validation(
            "/provider",
            format!("unknown provider {}", request.provider),
        ));
    }
    let Some(profile) = config.profiles.get(request.profile.as_str()) else {
        return Err(validation(
            "/profile",
            format!("unknown profile {}", request.profile),
        ));
    };
    for group in &request.groups {
        if !profile.groups.iter().any(|name| name == group.as_str()) {
            return Err(validation(
                "/groups",
                format!("group {group} is not in profile {}", request.profile),
            ));
        }
    }
    Ok(())
}

fn effective_groups(state: &State, request: &IssueJobToken) -> Vec<GroupName> {
    let Some(config) = &state.config else {
        return Vec::new();
    };
    let Some(profile) = config.profiles.get(request.profile.as_str()) else {
        return Vec::new();
    };
    request
        .groups
        .iter()
        .filter(|group| profile.groups.iter().any(|name| name == group.as_str()))
        .cloned()
        .collect()
}

fn make_grant(
    state: &mut State,
    request: &IssueJobToken,
    reissued: bool,
) -> Result<JobTokenGrant, EgressError> {
    let counter = state.next_counter();
    let token_text = token_text("gjt1_", counter, 43);
    let token = JobToken::parse(&token_text).map_err(|error| EgressError::Rejected {
        code: "internal".into(),
        message: error,
    })?;
    let issued_at = now();
    let expires_at = issued_at
        .checked_add_secs(state.job_ttl.as_secs())
        .unwrap_or(issued_at);
    let expires_at_instant = Instant::now() + state.job_ttl;
    let provider = request.provider.as_str().to_owned();
    let mut urls = BTreeMap::new();
    urls.insert(
        provider.clone(),
        job_model_url(state, &token_text, &provider),
    );
    let connect = format!("http://job:{token_text}@127.0.0.1:{}", state.addr.port());

    let effective = effective_groups(state, request);
    let job_id = request.job_id;
    {
        let entry = state.jobs.get_mut(&job_id).expect("job entry exists");
        entry.active_token = Some(token_text.clone());
        entry.expires_at_instant = expires_at_instant;
    }
    state.job_by_token.insert(token_text, job_id);

    Ok(JobTokenGrant {
        job_token: token,
        model_base_urls: ModelBaseUrls::new(urls),
        connect_proxy_url: ConnectProxyUrl::new(connect),
        registry_routes: BTreeMap::<InjectionId, String>::new(),
        effective_groups: effective,
        issued_at,
        expires_at,
        reissued,
    })
}

/// 一个可直接用于测试的配置视图。
pub fn default_config(rev: u64) -> ConfigView {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic".into(),
        gantry_core::egress::ProviderConfig {
            kind: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            secret_ref: SecretName::parse("model.anthropic.main").expect("valid secret ref"),
        },
    );
    let mut groups = BTreeMap::new();
    groups.insert(
        "model_api".into(),
        gantry_core::egress::EgressGroup {
            hosts: vec!["api.anthropic.com".into()],
        },
    );
    groups.insert(
        "npm".into(),
        gantry_core::egress::EgressGroup {
            hosts: vec!["registry.npmjs.org".into()],
        },
    );
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "default".into(),
        gantry_core::egress::EgressProfile {
            groups: vec!["model_api".into(), "npm".into()],
        },
    );
    ConfigView {
        rev,
        providers,
        groups,
        profiles,
        default_profile: "default".into(),
        injection: BTreeMap::new(),
        token_weights: TokenWeights::default(),
    }
}

/// M0 的 Egress 替身。
pub struct EgressStub {
    state: Arc<Mutex<State>>,
    data_plane: Option<DataPlane>,
}

impl EgressStub {
    pub fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake egress data plane");
        listener
            .set_nonblocking(true)
            .expect("set fake egress listener nonblocking");
        let addr = listener.local_addr().expect("fake egress local addr");
        let state = Arc::new(Mutex::new(State {
            addr,
            config: None,
            applied_rev: None,
            health: Health::Degraded,
            detail: Some("no config applied".into()),
            jobs: BTreeMap::new(),
            job_by_token: HashMap::new(),
            svc: None,
            svc_issued: BTreeSet::new(),
            closed_calls: BTreeMap::new(),
            expired_calls: BTreeSet::new(),
            usage: Vec::new(),
            next_seq: 0,
            calls: Vec::new(),
            staged: BTreeMap::new(),
            staged_by_id: BTreeMap::new(),
            active_secrets: BTreeMap::new(),
            secret_versions: BTreeMap::new(),
            counter: 0,
            response_body: DEFAULT_RESPONSE.into(),
            job_ttl: JOB_TOKEN_TTL,
            svc_ttl: SVC_TOKEN_TTL,
            staged_ttl: STAGED_TTL,
        }));
        let data_plane = DataPlane::start(listener, state.clone());
        Self {
            state,
            data_plane: Some(data_plane),
        }
    }

    /// 起一个已应用默认配置的实例。
    pub fn with_default_config() -> Self {
        let stub = Self::new();
        {
            let mut state = stub.lock();
            state.config = Some(default_config(1));
            state.applied_rev = Some(1);
            state.health = Health::Ok;
            state.detail = None;
        }
        stub
    }

    pub fn data_plane_url(&self) -> String {
        let state = self.lock();
        format!("http://127.0.0.1:{}", state.addr.port())
    }

    pub fn set_data_plane_response(&self, body: impl Into<String>) {
        self.lock().response_body = body.into();
    }

    pub fn set_job_token_ttl(&self, ttl: Duration) {
        self.lock().job_ttl = ttl;
    }

    pub fn set_svc_token_ttl(&self, ttl: Duration) {
        self.lock().svc_ttl = ttl;
    }

    pub fn calls(&self) -> Vec<EgressCall> {
        self.lock().calls.clone()
    }

    pub fn usage_records(&self) -> Vec<UsageRecord> {
        self.lock().usage.clone()
    }

    /// 测试钩子：模拟该调用的用量记录过了保留期（明细删除，墓碑保留）。
    pub fn expire_call_records(&self, call_id: &CallId) {
        let mut state = self.lock();
        state
            .usage
            .retain(|record| !is_call_record(record, call_id));
        state.expired_calls.insert(*call_id);
    }

    /// 测试钩子：把该调用已有的用量记录标为中断（`incomplete = true`）。只影响此后的关闭。
    pub fn interrupt_call_records(&self, call_id: &CallId) {
        let mut state = self.lock();
        for record in state.usage.iter_mut() {
            if is_call_record(record, call_id) {
                record.incomplete = true;
            }
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(data_plane) = self.data_plane.take() {
            data_plane.stop();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for EgressStub {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EgressStub {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[async_trait]
impl EgressControl for EgressStub {
    async fn endpoints(&self) -> Result<EgressEndpoints, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::Endpoints);
        Ok(EgressEndpoints {
            connect_proxy_host: format!("127.0.0.1:{}", state.addr.port()),
            add_host: None,
        })
    }

    async fn issue_job_token(
        &self,
        req: IssueJobToken,
        ctx: &ActorCtx,
    ) -> Result<JobTokenGrant, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::IssueJobToken {
            job_id: req.job_id,
            provider: req.provider.clone(),
            ctx: ctx.clone(),
        });
        validate_issue(&state, &req)?;

        let existing = state.jobs.get(&req.job_id);
        if let Some(entry) = existing {
            if entry.revoked_at.is_some() {
                return Err(EgressError::Conflict {
                    message: "job_revoked".into(),
                });
            }
            if entry.request != req {
                return Err(EgressError::Conflict {
                    message: "binding_mismatch".into(),
                });
            }
            if let Some(old) = entry.active_token.clone() {
                state.job_by_token.remove(&old);
            }
            return make_grant(&mut state, &req, true);
        }

        let issued_at = now();
        let job_ttl = state.job_ttl;
        // 权重快照所属的配置修订由调用方随签发请求给出（#94 拍板）。
        let weight_rev = req.config_rev.as_u64();
        state.jobs.insert(
            req.job_id,
            JobEntry {
                request: req.clone(),
                ledger: request_ledger(&req, issued_at),
                active_token: None,
                expires_at_instant: Instant::now() + job_ttl,
                revoked_at: None,
                final_usage: None,
                weight_rev,
                usage: TokenUsage::default(),
                weighted_tokens: 0,
                capped: false,
                incomplete: false,
                upstream_errors: UpstreamErrors::default(),
                denied: DeniedCounters::default(),
            },
        );
        make_grant(&mut state, &req, false)
    }

    async fn job_usage(&self, job_id: &AttemptId) -> Result<JobUsage, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::JobUsage { job_id: *job_id });
        state
            .jobs
            .get(job_id)
            .map(job_usage)
            .ok_or(EgressError::NotFound)
    }

    async fn revoke_job_token(
        &self,
        job_id: &AttemptId,
        ctx: &ActorCtx,
    ) -> Result<FinalUsage, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::RevokeJobToken {
            job_id: *job_id,
            ctx: ctx.clone(),
        });
        let Some(entry) = state.jobs.get_mut(job_id) else {
            return Err(EgressError::NotFound);
        };
        if let Some(final_usage) = &entry.final_usage {
            return Ok(final_usage.clone());
        }
        entry.revoked_at = Some(now());
        let token = entry.active_token.take();
        let usage = final_usage(entry);
        entry.final_usage = Some(usage.clone());
        if let Some(token) = token {
            state.job_by_token.remove(&token);
        }
        Ok(usage)
    }

    async fn forget_job(&self, job_id: &AttemptId, ctx: &ActorCtx) -> Result<(), EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::ForgetJob {
            job_id: *job_id,
            ctx: ctx.clone(),
        });
        if let Some(entry) = state.jobs.remove(job_id) {
            if let Some(token) = entry.active_token {
                state.job_by_token.remove(&token);
            }
        }
        Ok(())
    }

    async fn service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::ServiceToken { ctx: ctx.clone() });
        issue_svc(&mut state)
    }

    async fn rotate_service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::RotateServiceToken { ctx: ctx.clone() });
        issue_svc(&mut state)
    }

    async fn apply_config(
        &self,
        view: EgressConfigView,
        ctx: &ActorCtx,
    ) -> Result<gantry_core::ports::egress::AppliedConfig, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::ApplyConfig {
            rev: view.rev,
            ctx: ctx.clone(),
        });
        if let Err(error) = validate_config(&view) {
            state.health = Health::Degraded;
            state.detail = Some(error.to_string());
            return Err(error);
        }
        if let Some(applied) = state.applied_rev {
            if view.rev < applied {
                return Err(EgressError::Conflict {
                    message: "stale_rev".into(),
                });
            }
            if view.rev == applied {
                return Ok(ConfigApplyResponse {
                    applied_rev: applied,
                });
            }
        }
        state.applied_rev = Some(view.rev);
        state.config = Some(view);
        state.health = Health::Ok;
        state.detail = None;
        Ok(ConfigApplyResponse {
            applied_rev: state.applied_rev.expect("just set"),
        })
    }

    async fn secret_present(
        &self,
        refs: &[SecretName],
    ) -> Result<BTreeMap<SecretName, bool>, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::SecretPresent {
            refs: refs.to_vec(),
        });
        Ok(refs
            .iter()
            .map(|name| (name.clone(), state.active_secrets.contains_key(name)))
            .collect())
    }

    async fn stage_secret(
        &self,
        req: gantry_core::ports::egress::StageSecret,
        ctx: &ActorCtx,
    ) -> Result<gantry_core::ports::egress::StagedSecret, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::StageSecret {
            request_id: ctx.request_id,
            secret_ref: req.secret_ref.clone(),
            ctx: ctx.clone(),
        });
        let value = req.value.expose_secret().to_owned();
        if value.is_empty() || value.len() > 16 * 1024 {
            return Err(validation("/value", "secret value must be 1..=16 KiB"));
        }
        if let Some(existing) = state.staged.get(&ctx.request_id) {
            if existing.secret_ref == req.secret_ref && existing.value == value {
                return Ok(gantry_core::ports::egress::StagedSecret {
                    consumer: existing.consumer,
                    staged_id: existing.staged_id.clone(),
                    secret_ref: existing.secret_ref.clone(),
                    version: SecretVersion::new(existing.version).expect("version is positive"),
                });
            }
            return Err(EgressError::Conflict {
                message: "stage_request_conflict".into(),
            });
        }
        let counter = state.next_counter();
        let version = state
            .secret_versions
            .get(&req.secret_ref)
            .copied()
            .unwrap_or(0)
            + 1;
        state
            .secret_versions
            .insert(req.secret_ref.clone(), version);
        let staged_id = StagedId::parse(&token_text("stg_", counter, 26)).map_err(|error| {
            EgressError::Rejected {
                code: "internal".into(),
                message: error,
            }
        })?;
        let staged_at = now();
        let staged_ttl = state.staged_ttl;
        state.staged.insert(
            ctx.request_id,
            StageEntry {
                consumer: Consumer::Egress,
                staged_id: staged_id.clone(),
                secret_ref: req.secret_ref.clone(),
                version,
                value,
                activated: false,
                discarded: false,
                staged_at,
                expires_at: Instant::now() + staged_ttl,
            },
        );
        state.staged_by_id.insert(staged_id.clone(), ctx.request_id);
        Ok(gantry_core::ports::egress::StagedSecret {
            consumer: Consumer::Egress,
            staged_id,
            secret_ref: req.secret_ref,
            version: SecretVersion::new(version).expect("version is positive"),
        })
    }

    async fn activate_secret(
        &self,
        staged: &gantry_core::ports::egress::StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<SecretVersion, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::ActivateSecret {
            staged_id: staged.staged_id.clone(),
            ctx: ctx.clone(),
        });
        let Some(request_id) = state.staged_by_id.get(&staged.staged_id).copied() else {
            return Err(EgressError::NotFound);
        };
        let Some(entry) = state.staged.get_mut(&request_id) else {
            return Err(EgressError::NotFound);
        };
        if entry.discarded {
            return Err(EgressError::Rejected {
                code: "staged_discarded".into(),
                message: "staged secret was discarded".into(),
            });
        }
        if !entry.activated && Instant::now() >= entry.expires_at {
            return Err(EgressError::Rejected {
                code: "staged_expired".into(),
                message: "staged secret expired".into(),
            });
        }
        entry.activated = true;
        let version = entry.version;
        let secret_ref = entry.secret_ref.clone();
        let value = entry.value.clone();
        state.active_secrets.insert(secret_ref, (version, value));
        Ok(SecretVersion::new(version).expect("version is positive"))
    }

    async fn discard_secret(
        &self,
        staged: &gantry_core::ports::egress::StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<(), EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::DiscardSecret {
            staged_id: staged.staged_id.clone(),
            ctx: ctx.clone(),
        });
        if let Some(request_id) = state.staged_by_id.get(&staged.staged_id).copied() {
            if let Some(entry) = state.staged.get_mut(&request_id) {
                if !entry.activated {
                    entry.discarded = true;
                }
            }
        }
        Ok(())
    }

    async fn staged_secrets(&self) -> Result<Vec<StagedSecretItem>, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::StagedSecrets);
        let now_ts = now();
        Ok(state
            .staged
            .values()
            .filter(|entry| !entry.activated && !entry.discarded)
            .map(|entry| StagedSecretItem {
                staged_id: entry.staged_id.clone(),
                secret_ref: entry.secret_ref.clone(),
                version: entry.version,
                staged_at: entry.staged_at,
                age_s: (now_ts.as_millis() - entry.staged_at.as_millis()).max(0) as u64 / 1000,
            })
            .collect())
    }

    async fn call_usage(&self, call_id: &CallId) -> Result<CallUsage, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::CallUsage { call_id: *call_id });
        let mut usage = TokenUsage::default();
        let mut weighted_tokens = 0u64;
        let mut found = false;
        for record in &state.usage {
            if let UsageCaller::Svc {
                call_id: recorded, ..
            } = &record.caller
            {
                if recorded == call_id {
                    found = true;
                    usage.input_tokens += record.usage.input_tokens;
                    usage.output_tokens += record.usage.output_tokens;
                    usage.cache_read_tokens += record.usage.cache_read_tokens;
                    usage.cache_write_tokens += record.usage.cache_write_tokens;
                    usage.requests += record.usage.requests;
                    weighted_tokens += record.weighted_tokens;
                }
            }
        }
        if !found {
            return Err(EgressError::NotFound);
        }
        Ok(CallUsage {
            call_id: *call_id,
            usage,
            weighted_tokens,
            usage_source: UsageSource::Metered,
            incomplete: false,
        })
    }

    async fn usage_since(&self, cursor: UsageCursor, limit: u32) -> Result<UsagePage, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::UsageSince {
            cursor: cursor.clone(),
            limit,
        });
        Ok(page_usage(&state.usage, &cursor, limit))
    }

    async fn svc_token_usage(
        &self,
        id: &SvcTokenId,
        cursor: Option<UsageCursor>,
    ) -> Result<UsagePage, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::SvcTokenUsage {
            svc_token_id: id.clone(),
        });
        let filtered: Vec<UsageRecord> = state
            .usage
            .iter()
            .filter(|record| {
                matches!(
                    &record.caller,
                    UsageCaller::Svc { svc_token_id, .. } if svc_token_id == id
                )
            })
            .cloned()
            .collect();
        Ok(page_usage(
            &filtered,
            &cursor.unwrap_or_else(|| UsageCursor::new("0")),
            1000,
        ))
    }

    async fn close_call(
        &self,
        svc_token_id: &SvcTokenId,
        call_id: &CallId,
        ctx: &ActorCtx,
    ) -> Result<CallClosed, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::CloseCall {
            svc_token_id: svc_token_id.clone(),
            call_id: *call_id,
            ctx: ctx.clone(),
        });
        if !state.svc_issued.contains(svc_token_id) {
            return Err(EgressError::NotFound);
        }
        if let Some(terminal) = stored_terminal(&state, call_id) {
            return Ok(CloseCallResponse {
                call_id: *call_id,
                terminal,
            });
        }
        let records = call_records(&state.usage, call_id);
        let associated = records.iter().any(|record| {
            matches!(
                &record.caller,
                UsageCaller::Svc { svc_token_id: id, .. } if id == svc_token_id
            )
        });
        if !records.is_empty() && !associated {
            return Err(EgressError::Conflict {
                message: "conflict: call has accepted requests only on other svc tokens".into(),
            });
        }
        let terminal =
            CallTerminal::from_records(&records, now()).map_err(|error| EgressError::Rejected {
                code: "internal".into(),
                message: error.to_string(),
            })?;
        state.closed_calls.insert(*call_id, terminal.clone());
        Ok(CloseCallResponse {
            call_id: *call_id,
            terminal,
        })
    }

    async fn call_terminal(&self, call_id: &CallId) -> Result<CallStatement, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::CallTerminal { call_id: *call_id });
        Ok(CallStatement {
            call_id: *call_id,
            items: call_records(&state.usage, call_id),
            terminal: stored_terminal(&state, call_id),
        })
    }

    async fn status(&self) -> Result<EgressStatus, EgressError> {
        let mut state = self.lock();
        state.record_job_call(EgressCall::Status);
        let unresolved_secret_refs: Vec<SecretName> = state
            .config
            .as_ref()
            .map(|config| {
                config
                    .providers
                    .values()
                    .map(|provider| provider.secret_ref.clone())
                    .filter(|name| !state.active_secrets.contains_key(name))
                    .collect()
            })
            .unwrap_or_default();
        let active_job_tokens = state
            .jobs
            .values()
            .filter(|entry| entry.active_token.is_some())
            .count() as u32;
        let applied_rev = state
            .applied_rev
            .and_then(|rev| Seq::from_wire(rev).ok())
            .unwrap_or(Seq::ZERO);
        Ok(EgressStatus {
            applied_rev,
            health: state.health,
            detail: state.detail.clone(),
            unresolved_secret_refs,
            active_job_tokens,
        })
    }
}

fn is_call_record(record: &UsageRecord, call_id: &CallId) -> bool {
    matches!(&record.caller, UsageCaller::Svc { call_id: recorded, .. } if recorded == call_id)
}

fn call_records(records: &[UsageRecord], call_id: &CallId) -> Vec<UsageRecord> {
    records
        .iter()
        .filter(|record| is_call_record(record, call_id))
        .cloned()
        .collect()
}

/// 已关闭调用的证明；过了保留期仍返回同一份，只把 `incomplete` 置真（§5）。
fn stored_terminal(state: &State, call_id: &CallId) -> Option<CallTerminal> {
    let mut terminal = state.closed_calls.get(call_id)?.clone();
    if state.expired_calls.contains(call_id) {
        terminal.incomplete = true;
    }
    Some(terminal)
}

fn issue_svc(state: &mut State) -> Result<SvcTokenGrant, EgressError> {
    if state.config.is_none() {
        return Err(not_configured());
    }
    let mut revoked_previous = Vec::new();
    if let Some(old) = state.svc.take() {
        revoked_previous.push(old.id);
    }
    let counter = state.next_counter();
    let svc_token_text = token_text("gst1_", counter, 43);
    let id_text = token_text("svt_", counter, 26);
    let svc_token = SvcToken::parse(&svc_token_text).map_err(|error| EgressError::Rejected {
        code: "internal".into(),
        message: error,
    })?;
    let id = SvcTokenId::parse(&id_text).map_err(|error| EgressError::Rejected {
        code: "internal".into(),
        message: error,
    })?;
    let expires_at = now()
        .checked_add_secs(state.svc_ttl.as_secs())
        .unwrap_or_else(now);
    let mut urls = BTreeMap::new();
    if let Some(config) = &state.config {
        for provider in config.providers.keys() {
            urls.insert(
                provider.clone(),
                svc_model_url(state, &svc_token_text, provider),
            );
        }
    }
    let svc_expires_at = Instant::now() + state.svc_ttl;
    state.svc_issued.insert(id.clone());
    state.svc = Some(SvcEntry {
        id: id.clone(),
        token: svc_token_text,
        expires_at_instant: svc_expires_at,
    });
    Ok(IssueSvcTokenResponse {
        svc_token_id: id,
        svc_token,
        model_base_urls: ModelBaseUrls::new(urls),
        expires_at,
        revoked_previous,
    })
}

fn page_usage(records: &[UsageRecord], cursor: &UsageCursor, limit: u32) -> UsagePage {
    let after = cursor.as_str().parse::<u64>().unwrap_or(0);
    let limit = if limit == 0 { 200 } else { limit.min(1000) } as usize;
    let items: Vec<UsageRecord> = records
        .iter()
        .filter(|record| record.seq > after)
        .take(limit)
        .cloned()
        .collect();
    let next_cursor = items
        .last()
        .map(|record| record.seq.to_string())
        .or_else(|| records.last().map(|record| record.seq.to_string()))
        .unwrap_or_else(|| "0".into());
    UsagePage {
        items,
        next_cursor,
        terminal: None,
    }
}

// ---------------------------------------------------------------------------
// 假数据面
// ---------------------------------------------------------------------------

struct DataPlane {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DataPlane {
    fn start(listener: TcpListener, state: Arc<Mutex<State>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        let _ = handle_connection(stream, state);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for DataPlane {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct HttpResponse {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

impl HttpResponse {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            headers: Vec::new(),
        }
    }

    fn error(status: u16, code: &str) -> Self {
        Self::json(status, format!(r#"{{"error":"{code}"}}"#))
    }
}

fn handle_connection(mut stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    // 监听 socket 是非阻塞的；macOS 上 accept 出的连接会继承 O_NONBLOCK，
    // 首个 read 在请求尚未到达时返回 WouldBlock 并丢弃连接。这里恢复阻塞模式。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    match crate::github::read_http_request(&mut stream)? {
        ReadOutcome::Closed => Ok(()),
        ReadOutcome::Malformed => {
            let response = HttpResponse::error(400, "bad_request");
            write_response(&mut stream, &response)?;
            discard_pending_input(&mut stream);
            Ok(())
        }
        ReadOutcome::Request(request) => {
            let response = route(&request, &state);
            write_response(&mut stream, &response)
        }
    }
}

fn write_response(stream: &mut TcpStream, response: &HttpResponse) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()
}

fn route(request: &HttpRequest, state: &Arc<Mutex<State>>) -> HttpResponse {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let mut parts = path.trim_start_matches('/').split('/');
    match parts.next() {
        Some("j") => {
            let Some(token) = parts.next() else {
                return HttpResponse::error(404, "not_found");
            };
            let Some(provider) = parts.next() else {
                return HttpResponse::error(404, "not_found");
            };
            handle_job(request, token, provider, state)
        }
        Some("s") => {
            let Some(token) = parts.next() else {
                return HttpResponse::error(404, "not_found");
            };
            let Some(provider) = parts.next() else {
                return HttpResponse::error(404, "not_found");
            };
            handle_svc(request, token, provider, state)
        }
        _ => HttpResponse::error(404, "not_found"),
    }
}

fn handle_job(
    request: &HttpRequest,
    token: &str,
    provider: &str,
    shared: &Arc<Mutex<State>>,
) -> HttpResponse {
    if request.method != "POST" && request.method != "GET" {
        return HttpResponse::error(404, "not_found");
    }
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(job_id) = state.job_by_token.get(token).copied() else {
        state.calls.push(EgressCall::DataPlaneRejected {
            kind: "job",
            status: 403,
            code: "token_invalid".into(),
        });
        return HttpResponse::error(403, "token_invalid");
    };
    let body = state.response_body.clone();
    let request_usage = TokenUsage::new(request.body.len() as u64, body.len() as u64, 0, 0, 1);
    let model = model_from_body(&request.body);
    let (project_id, ledger, tier, weighted, weight_rev) = {
        let Some(entry) = state.jobs.get_mut(&job_id) else {
            return HttpResponse::error(403, "token_invalid");
        };
        if entry.revoked_at.is_some()
            || entry.active_token.as_deref() != Some(token)
            || Instant::now() >= entry.expires_at_instant
            || entry.request.provider.as_str() != provider
        {
            state.calls.push(EgressCall::DataPlaneRejected {
                kind: "job",
                status: 403,
                code: "token_invalid".into(),
            });
            return HttpResponse::error(403, "token_invalid");
        }
        if entry.capped {
            entry.denied.budget_exhausted += 1;
            state.calls.push(EgressCall::DataPlane {
                kind: "job",
                subject: job_id.to_string(),
                provider: provider.to_owned(),
                status: 403,
            });
            return HttpResponse::error(403, "budget_exhausted");
        }
        let weights = job_weights(&entry.request);
        let weighted = weights.weighted(&request_usage);
        entry.usage.input_tokens += request_usage.input_tokens;
        entry.usage.output_tokens += request_usage.output_tokens;
        entry.usage.cache_read_tokens += request_usage.cache_read_tokens;
        entry.usage.cache_write_tokens += request_usage.cache_write_tokens;
        entry.usage.requests += 1;
        entry.weighted_tokens = entry.weighted_tokens.saturating_add(weighted);
        if entry.request.tokens_total > 0 && entry.weighted_tokens >= entry.request.tokens_total {
            entry.capped = true;
        }
        (
            entry.request.project_id,
            entry.ledger.clone(),
            job_tier(&entry.request),
            weighted,
            entry.weight_rev,
        )
    };
    let record = UsageRecord {
        seq: next_usage_seq(&mut state),
        at: now(),
        caller: UsageCaller::Job { job_id },
        project_id: Some(project_id),
        ledger: Some(ledger),
        tier,
        provider: provider.to_owned(),
        model,
        usage: request_usage,
        weighted_tokens: weighted,
        weight_rev,
        incomplete: false,
    };
    state.usage.push(record);
    state.calls.push(EgressCall::DataPlane {
        kind: "job",
        subject: job_id.to_string(),
        provider: provider.to_owned(),
        status: 200,
    });
    HttpResponse::json(200, body)
}

fn model_from_body(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn next_usage_seq(state: &mut State) -> u64 {
    state.next_seq = state.next_seq.saturating_add(1);
    state.next_seq
}

fn handle_svc(
    request: &HttpRequest,
    token: &str,
    provider: &str,
    shared: &Arc<Mutex<State>>,
) -> HttpResponse {
    if request.method != "POST" && request.method != "GET" {
        return HttpResponse::error(404, "not_found");
    }
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(current_svc) = &state.svc else {
        state.calls.push(EgressCall::DataPlaneRejected {
            kind: "svc",
            status: 403,
            code: "token_invalid".into(),
        });
        return HttpResponse::error(403, "token_invalid");
    };
    if current_svc.token != token || Instant::now() >= current_svc.expires_at_instant {
        state.calls.push(EgressCall::DataPlaneRejected {
            kind: "svc",
            status: 403,
            code: "token_invalid".into(),
        });
        return HttpResponse::error(403, "token_invalid");
    }
    let svc_id = current_svc.id.clone();
    let Some(ledger_header) = request.headers.get("x-gantry-ledger") else {
        return HttpResponse::error(400, "ledger_header_invalid");
    };
    let Some(tier_header) = request.headers.get("x-gantry-tier") else {
        return HttpResponse::error(400, "ledger_header_invalid");
    };
    let Some(call_header) = request.headers.get("x-gantry-call-id") else {
        return HttpResponse::error(400, "ledger_header_invalid");
    };
    let Some(ledger) = parse_ledger(ledger_header) else {
        return HttpResponse::error(400, "ledger_header_invalid");
    };
    let Some(tier) = parse_tier(tier_header) else {
        return HttpResponse::error(400, "ledger_header_invalid");
    };
    let Ok(call_id) = call_header.parse::<CallId>() else {
        return HttpResponse::error(400, "ledger_header_invalid");
    };
    if state.closed_calls.contains_key(&call_id) {
        state.calls.push(EgressCall::DataPlaneRejected {
            kind: "svc",
            status: 403,
            code: "token_invalid".into(),
        });
        return HttpResponse::error(403, "token_invalid");
    }

    let body = state.response_body.clone();
    let request_usage = TokenUsage::new(request.body.len() as u64, body.len() as u64, 0, 0, 1);
    let weights = state
        .config
        .as_ref()
        .map(|config| config.token_weights)
        .unwrap_or_default();
    let weighted = weights.weighted(&request_usage);
    let weight_rev = state.applied_rev.unwrap_or(0);
    let record = UsageRecord {
        seq: next_usage_seq(&mut state),
        at: now(),
        caller: UsageCaller::Svc {
            svc_token_id: svc_id,
            call_id,
        },
        project_id: None,
        ledger,
        tier,
        provider: provider.to_owned(),
        model: model_from_body(&request.body),
        usage: request_usage,
        weighted_tokens: weighted,
        weight_rev,
        incomplete: false,
    };
    state.usage.push(record);
    state.calls.push(EgressCall::DataPlane {
        kind: "svc",
        subject: call_id.to_string(),
        provider: provider.to_owned(),
        status: 200,
    });
    HttpResponse::json(200, body)
}

fn parse_ledger(value: &str) -> Option<Option<LedgerRef>> {
    if value == "unattributed" {
        return Some(None);
    }
    if value.starts_with("dlv_") {
        return value
            .parse::<DeliveryId>()
            .ok()
            .map(|id| Some(LedgerRef::Delivery { id }));
    }
    let (conversation, day) = value.split_once(':')?;
    let id = conversation
        .parse::<gantry_core::ids::ConversationId>()
        .ok()?;
    let day = day.parse::<gantry_core::time::Date>().ok()?;
    Some(Some(LedgerRef::Conversation { id, day }))
}

fn parse_tier(value: &str) -> Option<Tier> {
    match value {
        "utility" => Some(Tier::Utility),
        "planner" => Some(Tier::Planner),
        _ => None,
    }
}
