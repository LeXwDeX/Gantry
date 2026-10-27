//! 登记表：令牌、用量、调用闸门、配置视图与暂存密钥的单一事实源
//! （`docs/modules/egress.md` §7.2、§7.9；`docs/interfaces/egress-control.md` §3–§4；
//! `docs/interfaces/egress-call-terminal.md`）。
//!
//! 控制面服务与数据面监听器都只经本模块读写状态。每次状态变化先写 `egress.db`
//! （一个 SQLite 事务），成功后才改内存；写失败则拒绝该操作（失败闭合）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use gantry_core::canon::Sha256Digest;
use gantry_core::egress::{
    CallTerminal, CloseCallResponse, ConfigView, ConnectProxyUrl, ControlProtocol, DeniedCounters,
    EgressHealth, EndpointsResponse, FinalUsage, IssueJobTokenRequest, IssueJobTokenResponse,
    IssueSvcTokenRequest, IssueSvcTokenResponse, IssuedTo, JobTokenState, LedgerRef, ModelBaseUrls,
    ProblemError, RevokeSvcTokenResponse, SandboxTier, StagedId, StagedSecretItem, StatusResponse,
    SvcTokenId, UpstreamErrors, UsageCaller, UsagePage, UsageRecord,
};
use gantry_core::entities::{Tier, TokenUsage, TokenWeights};
use gantry_core::ids::{AttemptId, Clock, Entropy, IdGen, Ulid};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::secrets::{Expect, SecretSink, SecretValue, SecretsError};
use gantry_core::secret::SecretName;
use gantry_core::time::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::db::{Db, DbError, Table};
use crate::host::{AllowPattern, Host};
use crate::review::{review, ProviderUpstream, ReviewedConfig};
use crate::token::{mint_job_token, mint_svc_token, TokenHash};

/// 吊销后自动遗忘最终用量的时长（egress-control §4.2，24 h [提议]）。
pub const FINAL_USAGE_TTL_S: u64 = 86_400;
/// 用量记录保留期（egress-control §4.5，7 天 [提议]）。
pub const USAGE_RETENTION_S: u64 = 7 * 86_400;
/// 暂存密钥的激活期限（egress-control §4.4，10 分钟 [提议]）。
pub const STAGED_TTL_S: u64 = 600;
/// `job_token` 硬上限 = 签发时刻 + `wall_time_s` + 600 s（egress-control §3.1 [提议]）。
pub const JOB_TOKEN_GRACE_S: u64 = 600;
/// `svc_token` 默认与允许的有效期（egress-control §4.3 [提议]）。
pub const SVC_TTL_DEFAULT_S: u64 = 86_400;
pub const SVC_TTL_RANGE_S: (u64, u64) = (3_600, 604_800);
/// 用量拉取的默认与最大页长（egress-control §4.5）。
pub const USAGE_PAGE_DEFAULT: u32 = 200;
pub const USAGE_PAGE_MAX: u32 = 1_000;
/// 暂存值上限（egress-control §4.4）。
pub const SECRET_VALUE_MAX: usize = 16 * 1024;

/// 引导配置给出的静态端点（不含令牌）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// 例如 `http://10.89.0.1:8081`。
    pub sandbox_model_base: String,
    /// `CONNECT` 代理的 `host:port`，例如 `10.89.0.1:3128`。
    pub connect_proxy_authority: String,
    /// 例如 `http://127.0.0.1:8082`。
    pub loopback_model_base: String,
}

#[derive(Clone, Debug)]
pub struct RegistryOptions {
    pub endpoints: Endpoints,
    /// "只能从沙盒网段使用"能否强制（P-73）；不能时 `status.detail` 标 `source_check: unverified`。
    pub source_check_enforced: bool,
    /// 关闭调用、吊销 job 时等待在途请求结束的上限；超时返回 `unavailable`，调用方重试。
    pub drain_timeout: Duration,
    pub final_usage_ttl_s: u64,
    pub usage_retention_s: u64,
    pub staged_ttl_s: u64,
}

impl RegistryOptions {
    pub fn new(endpoints: Endpoints) -> Self {
        Self {
            endpoints,
            source_check_enforced: false,
            drain_timeout: Duration::from_secs(30),
            final_usage_ttl_s: FINAL_USAGE_TTL_S,
            usage_retention_s: USAGE_RETENTION_S,
            staged_ttl_s: STAGED_TTL_S,
        }
    }
}

/// 控制面错误：HTTP 状态与稳定的 `code`（egress-control §6）。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("not found")]
    NotFound,
    #[error("binding mismatch")]
    BindingMismatch,
    #[error("job revoked")]
    JobRevoked,
    #[error("stale config rev")]
    StaleRev,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("staged secret expired")]
    StagedExpired,
    #[error("usage cursor expired")]
    CursorExpired,
    #[error("validation failed")]
    Validation(Vec<ProblemError>),
    #[error("credential missing: {0}")]
    CredentialMissing(String),
    #[error("config invalid")]
    ConfigInvalid(Vec<ProblemError>),
    #[error("no egress config has been applied")]
    NotConfigured,
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl ControlError {
    pub fn status(&self) -> u16 {
        match self {
            Self::InvalidRequest(_) => 400,
            Self::NotFound => 404,
            Self::BindingMismatch | Self::JobRevoked | Self::StaleRev | Self::Conflict(_) => 409,
            Self::StagedExpired | Self::CursorExpired => 410,
            Self::Validation(_) | Self::CredentialMissing(_) | Self::ConfigInvalid(_) => 422,
            Self::NotConfigured | Self::Unavailable(_) => 503,
            Self::Internal(_) => 500,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::NotFound => "not_found",
            Self::BindingMismatch => "binding_mismatch",
            Self::JobRevoked => "job_revoked",
            Self::StaleRev => "stale_rev",
            Self::Conflict(_) => "conflict",
            Self::StagedExpired => "staged_expired",
            Self::CursorExpired => "cursor_expired",
            Self::Validation(_) => "validation_failed",
            Self::CredentialMissing(_) => "credential_missing",
            Self::ConfigInvalid(_) => "config_invalid",
            Self::NotConfigured => "not_configured",
            Self::Unavailable(_) => "unavailable",
            Self::Internal(_) => "internal",
        }
    }

    pub fn errors(&self) -> &[ProblemError] {
        match self {
            Self::Validation(errors) | Self::ConfigInvalid(errors) => errors,
            _ => &[],
        }
    }

    fn validation(pointer: &str, message: impl Into<String>) -> Self {
        Self::Validation(vec![ProblemError {
            pointer: pointer.to_owned(),
            message: message.into(),
        }])
    }
}

impl From<DbError> for ControlError {
    fn from(error: DbError) -> Self {
        Self::Unavailable(format!("storage: {error}"))
    }
}

/// 数据面拒绝。`rule` 是审计规则 id；对外响应只看 `error_type`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    /// 未知 / 已吊销 / 过期 / 用错监听器 / 调用已关闭：同一响应（egress-control §5）。
    TokenInvalid { rule: &'static str },
    /// provider 不是令牌绑定的、或不存在。
    ProviderNotAllowed,
    /// 目标不在有效分组。
    NotAllowedHost,
    /// 加权用量达到 `tokens_total`。
    BudgetExhausted,
    /// 状态无法持久化等内部原因：失败闭合。
    Unavailable,
}

impl Denial {
    pub fn status(self) -> u16 {
        match self {
            Self::Unavailable => 503,
            _ => 403,
        }
    }

    /// 错误体的 `error.type`（modules/egress.md §6.4）。
    pub fn error_type(self) -> &'static str {
        match self {
            Self::TokenInvalid { .. } => "token_invalid",
            Self::ProviderNotAllowed => "provider_not_allowed",
            Self::NotAllowedHost => "not_allowed_host",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Unavailable => "upstream_unavailable",
        }
    }

    pub fn rule(self) -> &'static str {
        match self {
            Self::TokenInvalid { rule } => rule,
            Self::ProviderNotAllowed => "provider.not_allowed",
            Self::NotAllowedHost => "profile.not_in_groups",
            Self::BudgetExhausted => "cap.tokens_exhausted",
            Self::Unavailable => "internal.unavailable",
        }
    }
}

/// 令牌出现的监听器。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listener {
    /// 沙盒侧（模型 `/j/…`、`CONNECT`）：只认 `job_token`。
    Sandbox,
    /// 回环（`/s/…`）：只认 `svc_token`。
    Loopback,
}

/// 回环请求头声明的归属（P-74）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvcCallMeta {
    pub call_id: Ulid,
    /// `None` = `X-Gantry-Ledger: unattributed`。
    pub ledger: Option<LedgerRef>,
    pub tier: Tier,
}

/// 已识别的调用方（令牌有效）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    Job { job_id: AttemptId },
    Svc { svc_token_id: SvcTokenId },
}

/// 拒绝计数的类别（`DeniedCounters`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeniedKind {
    NotAllowedHost,
    PathNotAllowed,
}

/// 上游结果类别，用于 `upstream_errors`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamClass {
    Ok,
    Status429,
    Status5xx,
    ConnError,
}

/// 一次请求的计量结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metered {
    pub usage: TokenUsage,
    /// 用量不可解析 / 流被截断 / 中断：数字是估计或下界。
    pub incomplete: bool,
    pub upstream: UpstreamClass,
}

// ---------------------------------------------------------------------------
// 持久化行
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JobRow {
    request: IssueJobTokenRequest,
    /// 当前有效令牌的哈希；吊销后为 `None`。
    active_hash: Option<Sha256Digest>,
    /// 签发过、已被重签替换的令牌哈希（拒绝时审计为 `token.revoked`）。
    retired_hashes: Vec<Sha256Digest>,
    issued_at: Timestamp,
    expires_at: Timestamp,
    usage: TokenUsage,
    weighted_tokens: u64,
    capped: bool,
    incomplete: bool,
    upstream_errors: UpstreamErrors,
    consecutive_errors: u64,
    denied: DeniedCounters,
    revoked_at: Option<Timestamp>,
    final_usage: Option<FinalUsage>,
}

impl JobRow {
    fn snapshot(&self) -> FinalUsage {
        if let Some(final_usage) = &self.final_usage {
            return final_usage.clone();
        }
        FinalUsage {
            job_id: self.request.job_id,
            state: JobTokenState::Active,
            revoked_at: None,
            usage: self.usage,
            weighted_tokens: self.weighted_tokens,
            capped: self.capped,
            incomplete: self.incomplete,
            upstream_errors: self.upstream_errors.clone(),
            denied: self.denied.clone(),
        }
    }

    fn hashes(&self) -> impl Iterator<Item = Sha256Digest> + '_ {
        self.active_hash
            .iter()
            .copied()
            .chain(self.retired_hashes.iter().copied())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SvcRow {
    id: SvcTokenId,
    hash: Sha256Digest,
    issued_to: IssuedTo,
    issued_at: Timestamp,
    expires_at: Timestamp,
    revoked_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CallPhase {
    Draining,
    Closed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CallRow {
    phase: CallPhase,
    terminal: Option<CallTerminal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum InflightOwner {
    Job {
        job_id: AttemptId,
    },
    Svc {
        svc_token_id: SvcTokenId,
        call_id: Ulid,
        ledger: Option<LedgerRef>,
        tier: Tier,
        weights: TokenWeights,
        weight_rev: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InflightRow {
    owner: InflightOwner,
    provider: String,
    model: String,
    started_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StagedState {
    Staged,
    Activated,
    Discarded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StagedRow {
    staged_id: StagedId,
    request_id: Ulid,
    secret_ref: SecretName,
    version: u64,
    /// 值的 SHA-256，只用于判断同一 `request_id` 的重放是否同值；不是值本身。
    value_digest: Sha256Digest,
    staged_at: Timestamp,
    expires_at: Timestamp,
    state: StagedState,
    activated_at: Option<Timestamp>,
}

const META_CONFIG: &str = "config";
const META_NEXT_SEQ: &str = "next_seq";
const META_PRUNED_THROUGH: &str = "pruned_through";

// ---------------------------------------------------------------------------
// 内存状态
// ---------------------------------------------------------------------------

struct State {
    db: Db,
    config: Option<ReviewedConfig>,
    last_apply_error: Option<String>,
    jobs: BTreeMap<AttemptId, JobRow>,
    job_by_hash: HashMap<Sha256Digest, AttemptId>,
    svcs: BTreeMap<SvcTokenId, SvcRow>,
    svc_by_hash: HashMap<Sha256Digest, SvcTokenId>,
    usage: Vec<UsageRecord>,
    next_seq: u64,
    pruned_through: u64,
    calls: BTreeMap<Ulid, CallRow>,
    inflight: BTreeMap<u64, InflightRow>,
    next_inflight: u64,
    staged: BTreeMap<StagedId, StagedRow>,
    audit_failed: bool,
}

struct Inner {
    state: Mutex<State>,
    drained: Notify,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    ids: IdGen,
    secrets: Arc<dyn SecretSink>,
    options: RegistryOptions,
}

/// 登记表句柄；克隆共享同一份状态。
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

/// 一次已接受的数据面请求。必须用 [`Registry::finish`] 收尾；若被丢弃（任务被取消），
/// 按中断处理：记一条零用量的 `incomplete` 记录，并使该 job 用量 `incomplete`。
pub struct Admitted {
    registry: Registry,
    id: u64,
    done: bool,
    pub caller: Caller,
    pub provider: ProviderUpstream,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.registry.complete(
                self.id,
                Metered {
                    usage: TokenUsage::default(),
                    incomplete: true,
                    upstream: UpstreamClass::ConnError,
                },
            );
        }
    }
}

impl Registry {
    /// 打开登记表并恢复：载入令牌、用量、墓碑与最后一次已应用的视图；遗留的在途登记
    /// 使对应 job `incomplete`，对应调用记一条中断记录（绝不静默丢失）。
    pub fn open(
        db: Db,
        clock: Arc<dyn Clock>,
        entropy: Arc<dyn Entropy>,
        secrets: Arc<dyn SecretSink>,
        options: RegistryOptions,
    ) -> Result<Self, ControlError> {
        let config = match db.meta::<ConfigView>(META_CONFIG)? {
            Some(view) => Some(review(view).map_err(|errors| {
                ControlError::Internal(format!("persisted config no longer reviews: {errors:?}"))
            })?),
            None => None,
        };
        let jobs: BTreeMap<AttemptId, JobRow> = db
            .load::<JobRow>(Table::Jobs)?
            .into_iter()
            .map(|(_, row)| (row.request.job_id, row))
            .collect();
        let svcs: BTreeMap<SvcTokenId, SvcRow> = db
            .load::<SvcRow>(Table::SvcTokens)?
            .into_iter()
            .map(|(_, row)| (row.id.clone(), row))
            .collect();
        let calls: BTreeMap<Ulid, CallRow> = db
            .load::<CallRow>(Table::Calls)?
            .into_iter()
            .filter_map(|(key, row)| key.parse::<Ulid>().ok().map(|id| (id, row)))
            .collect();
        let staged: BTreeMap<StagedId, StagedRow> = db
            .load::<StagedRow>(Table::Staged)?
            .into_iter()
            .map(|(_, row)| (row.staged_id.clone(), row))
            .collect();
        let leftovers = db.load::<InflightRow>(Table::Inflight)?;
        let usage = db.load_usage()?;
        let next_seq = db
            .meta::<u64>(META_NEXT_SEQ)?
            .unwrap_or(0)
            .max(usage.last().map_or(0, |record| record.seq));
        let pruned_through = db.meta::<u64>(META_PRUNED_THROUGH)?.unwrap_or(0);

        let mut job_by_hash = HashMap::new();
        for row in jobs.values() {
            for hash in row.hashes() {
                job_by_hash.insert(hash, row.request.job_id);
            }
        }
        let svc_by_hash = svcs
            .values()
            .map(|row| (row.hash, row.id.clone()))
            .collect();

        let ids = IdGen::new(clock.clone(), entropy.clone());
        let registry = Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    db,
                    config,
                    last_apply_error: None,
                    jobs,
                    job_by_hash,
                    svcs,
                    svc_by_hash,
                    usage,
                    next_seq,
                    pruned_through,
                    calls,
                    inflight: BTreeMap::new(),
                    next_inflight: 0,
                    staged,
                    audit_failed: false,
                }),
                drained: Notify::new(),
                clock,
                entropy,
                ids,
                secrets,
                options,
            }),
        };
        registry.recover_inflight(leftovers)?;
        Ok(registry)
    }

    fn recover_inflight(&self, leftovers: Vec<(String, InflightRow)>) -> Result<(), ControlError> {
        if leftovers.is_empty() {
            return Ok(());
        }
        let now = self.now();
        let mut state = self.lock();
        for (_, row) in leftovers {
            let metered = Metered {
                usage: TokenUsage::default(),
                incomplete: true,
                upstream: UpstreamClass::ConnError,
            };
            record_completion(&mut state, &row, metered, now)?;
        }
        state.db.tx(|tx| tx.clear(Table::Inflight))?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now(&self) -> Timestamp {
        self.inner.clock.now()
    }

    // -----------------------------------------------------------------------
    // 4.6 配置视图
    // -----------------------------------------------------------------------

    pub fn apply_config(&self, view: ConfigView) -> Result<u64, ControlError> {
        let mut state = self.lock();
        if let Some(applied) = state.config.as_ref().map(|c| c.view.rev) {
            if view.rev < applied {
                return Err(ControlError::StaleRev);
            }
            if view.rev == applied {
                return Ok(applied);
            }
        }
        match review(view) {
            Ok(reviewed) => {
                state.db.tx(|tx| tx.put_meta(META_CONFIG, &reviewed.view))?;
                let rev = reviewed.view.rev;
                state.config = Some(reviewed);
                state.last_apply_error = None;
                Ok(rev)
            }
            Err(errors) => {
                state.last_apply_error = Some(format!(
                    "config rejected: {}",
                    errors
                        .iter()
                        .map(|e| format!("{} {}", e.pointer, e.message))
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
                Err(ControlError::ConfigInvalid(errors))
            }
        }
    }

    /// 当前已应用的视图（数据面取 provider、分组、权重）。
    pub fn config(&self) -> Option<ReviewedConfig> {
        self.lock().config.clone()
    }

    // -----------------------------------------------------------------------
    // 4.1 / 4.2 job_token
    // -----------------------------------------------------------------------

    pub fn issue_job(
        &self,
        req: IssueJobTokenRequest,
    ) -> Result<IssueJobTokenResponse, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        sweep(&mut state, now, &self.inner.options)?;
        let config = state.config.clone().ok_or(ControlError::NotConfigured)?;
        validate_issue(&config, &req)?;
        let provider = config
            .view
            .providers
            .get(&req.provider)
            .expect("validated provider");
        if !self.secret_present(&provider.secret_ref)? {
            return Err(ControlError::CredentialMissing(
                provider.secret_ref.to_string(),
            ));
        }
        let expires_at = now
            .checked_add_secs(req.wall_time_s.saturating_add(JOB_TOKEN_GRACE_S))
            .ok_or_else(|| ControlError::validation("/wall_time_s", "out of range"))?;
        let (token, hash) = mint_job_token(self.inner.entropy.as_ref());
        let hash = hash.digest();

        let (row, reissued) = match state.jobs.get(&req.job_id) {
            Some(existing) => {
                if existing.revoked_at.is_some() {
                    return Err(ControlError::JobRevoked);
                }
                if !same_binding(&existing.request, &req) {
                    return Err(ControlError::BindingMismatch);
                }
                let mut row = existing.clone();
                if let Some(old) = row.active_hash.take() {
                    row.retired_hashes.push(old);
                }
                row.active_hash = Some(hash);
                row.issued_at = now;
                row.expires_at = expires_at;
                (row, true)
            }
            None => (
                JobRow {
                    request: req.clone(),
                    active_hash: Some(hash),
                    retired_hashes: Vec::new(),
                    issued_at: now,
                    expires_at,
                    usage: TokenUsage::default(),
                    weighted_tokens: 0,
                    capped: false,
                    incomplete: false,
                    upstream_errors: UpstreamErrors::default(),
                    consecutive_errors: 0,
                    denied: DeniedCounters::default(),
                    revoked_at: None,
                    final_usage: None,
                },
                false,
            ),
        };
        state
            .db
            .tx(|tx| tx.put(Table::Jobs, &req.job_id.to_string(), &row))?;
        state.job_by_hash.insert(hash, req.job_id);
        let effective_groups = effective_groups(&config, &row.request);
        state.jobs.insert(req.job_id, row);

        let endpoints = &self.inner.options.endpoints;
        let secret = token.expose_secret();
        let mut urls = BTreeMap::new();
        urls.insert(
            req.provider.clone(),
            format!(
                "{}/j/{secret}/{}",
                endpoints.sandbox_model_base, req.provider
            ),
        );
        Ok(IssueJobTokenResponse {
            job_id: req.job_id,
            model_base_urls: ModelBaseUrls::new(urls),
            connect_proxy_url: ConnectProxyUrl::new(format!(
                "http://job:{secret}@{}",
                endpoints.connect_proxy_authority
            )),
            job_token: token,
            effective_groups,
            issued_at: now,
            expires_at,
            reissued,
        })
    }

    /// `GET …/usage`：当前快照（`state: active`），已吊销则返回最终用量。
    pub fn job_usage(&self, job_id: &AttemptId) -> Result<FinalUsage, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        sweep(&mut state, now, &self.inner.options)?;
        state
            .jobs
            .get(job_id)
            .map(JobRow::snapshot)
            .ok_or(ControlError::NotFound)
    }

    /// 原子地"吊销并取最终用量"：先关门（新请求一律 `token_invalid`），等该 job 的在途请求
    /// 结束并计入，再定格最终用量。重复调用返回同一份，直到遗忘。
    pub async fn revoke_job(&self, job_id: &AttemptId) -> Result<FinalUsage, ControlError> {
        {
            let now = self.now();
            let mut state = self.lock();
            sweep(&mut state, now, &self.inner.options)?;
            let row = state.jobs.get(job_id).ok_or(ControlError::NotFound)?;
            if let Some(final_usage) = &row.final_usage {
                return Ok(final_usage.clone());
            }
            if row.revoked_at.is_none() {
                let mut row = row.clone();
                row.revoked_at = Some(now);
                if let Some(old) = row.active_hash.take() {
                    row.retired_hashes.push(old);
                }
                state
                    .db
                    .tx(|tx| tx.put(Table::Jobs, &job_id.to_string(), &row))?;
                state.jobs.insert(*job_id, row);
            }
        }
        self.wait_drained(
            |row| matches!(&row.owner, InflightOwner::Job { job_id: id } if id == job_id),
        )
        .await?;
        let mut state = self.lock();
        let row = state.jobs.get(job_id).ok_or(ControlError::NotFound)?;
        if let Some(final_usage) = &row.final_usage {
            return Ok(final_usage.clone());
        }
        let mut row = row.clone();
        let mut final_usage = row.snapshot();
        final_usage.state = JobTokenState::Revoked;
        final_usage.revoked_at = row.revoked_at;
        row.final_usage = Some(final_usage.clone());
        state
            .db
            .tx(|tx| tx.put(Table::Jobs, &job_id.to_string(), &row))?;
        state.jobs.insert(*job_id, row);
        Ok(final_usage)
    }

    pub fn forget_job(&self, job_id: &AttemptId) -> Result<(), ControlError> {
        let mut state = self.lock();
        forget(&mut state, job_id)
    }

    // -----------------------------------------------------------------------
    // 4.3 svc_token 与调用级关闭
    // -----------------------------------------------------------------------

    pub fn issue_svc(
        &self,
        req: IssueSvcTokenRequest,
    ) -> Result<IssueSvcTokenResponse, ControlError> {
        let ttl = req.ttl_s.unwrap_or(SVC_TTL_DEFAULT_S);
        if !(SVC_TTL_RANGE_S.0..=SVC_TTL_RANGE_S.1).contains(&ttl) {
            return Err(ControlError::validation(
                "/ttl_s",
                format!(
                    "must be within {}..={}",
                    SVC_TTL_RANGE_S.0, SVC_TTL_RANGE_S.1
                ),
            ));
        }
        let now = self.now();
        let mut state = self.lock();
        let config = state.config.clone().ok_or(ControlError::NotConfigured)?;
        let id = SvcTokenId::parse(&format!("svt_{}", self.next_ulid()?))
            .map_err(ControlError::Internal)?;
        let (token, hash) = mint_svc_token(self.inner.entropy.as_ref());
        let expires_at = now
            .checked_add_secs(ttl)
            .ok_or_else(|| ControlError::validation("/ttl_s", "out of range"))?;
        let mut revoked = Vec::new();
        for row in state.svcs.values() {
            if row.revoked_at.is_none() {
                let mut row = row.clone();
                row.revoked_at = Some(now);
                revoked.push(row);
            }
        }
        let row = SvcRow {
            id: id.clone(),
            hash: hash.digest(),
            issued_to: req.issued_to,
            issued_at: now,
            expires_at,
            revoked_at: None,
        };
        state.db.tx(|tx| {
            for old in &revoked {
                tx.put(Table::SvcTokens, old.id.as_str(), old)?;
            }
            tx.put(Table::SvcTokens, id.as_str(), &row)
        })?;
        let revoked_previous = revoked.iter().map(|row| row.id.clone()).collect();
        for old in revoked {
            state.svcs.insert(old.id.clone(), old);
        }
        state.svc_by_hash.insert(row.hash, id.clone());
        state.svcs.insert(id.clone(), row);

        let secret = token.expose_secret();
        let base = &self.inner.options.endpoints.loopback_model_base;
        let urls = config
            .view
            .providers
            .keys()
            .map(|provider| (provider.clone(), format!("{base}/s/{secret}/{provider}")))
            .collect();
        Ok(IssueSvcTokenResponse {
            svc_token_id: id,
            svc_token: token,
            model_base_urls: ModelBaseUrls::new(urls),
            expires_at,
            revoked_previous,
        })
    }

    /// 整令牌吊销；已吊销原样返回。
    pub fn revoke_svc(&self, id: &SvcTokenId) -> Result<RevokeSvcTokenResponse, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        let row = state.svcs.get(id).ok_or(ControlError::NotFound)?.clone();
        let revoked_at = match row.revoked_at {
            Some(at) => at,
            None => {
                let mut row = row;
                row.revoked_at = Some(now);
                state
                    .db
                    .tx(|tx| tx.put(Table::SvcTokens, id.as_str(), &row))?;
                state.svcs.insert(id.clone(), row);
                now
            }
        };
        Ok(RevokeSvcTokenResponse {
            svc_token_id: id.clone(),
            state: JobTokenState::Revoked,
            revoked_at,
        })
    }

    /// 按全局 `call_id` 关闭一次调用（egress-call-terminal.md §2）。
    pub async fn close_call(
        &self,
        svc_token_id: &SvcTokenId,
        call_id: Ulid,
    ) -> Result<CloseCallResponse, ControlError> {
        {
            let now = self.now();
            let mut state = self.lock();
            if !state.svcs.contains_key(svc_token_id) {
                return Err(ControlError::NotFound);
            }
            if let Some(terminal) = stored_terminal(&state, call_id, now, &self.inner.options) {
                return Ok(CloseCallResponse { call_id, terminal });
            }
            // 关联校验：已接受请求（在途或已完成）里有路径 token 的，或全局零接收。
            let mut accepted = BTreeSet::new();
            for record in &state.usage {
                if let UsageCaller::Svc {
                    svc_token_id: id,
                    call_id: recorded,
                } = &record.caller
                {
                    if *recorded == call_id {
                        accepted.insert(id.clone());
                    }
                }
            }
            for row in state.inflight.values() {
                if let InflightOwner::Svc {
                    svc_token_id: id,
                    call_id: recorded,
                    ..
                } = &row.owner
                {
                    if *recorded == call_id {
                        accepted.insert(id.clone());
                    }
                }
            }
            if !accepted.is_empty() && !accepted.contains(svc_token_id) {
                return Err(ControlError::Conflict(
                    "the call's accepted requests belong to other svc tokens".into(),
                ));
            }
            if !state.calls.contains_key(&call_id) {
                let row = CallRow {
                    phase: CallPhase::Draining,
                    terminal: None,
                };
                state
                    .db
                    .tx(|tx| tx.put(Table::Calls, &call_id.to_string(), &row))?;
                state.calls.insert(call_id, row);
            }
        }
        self.wait_drained(
            |row| matches!(&row.owner, InflightOwner::Svc { call_id: id, .. } if *id == call_id),
        )
        .await?;
        let now = self.now();
        let mut state = self.lock();
        if let Some(terminal) = stored_terminal(&state, call_id, now, &self.inner.options) {
            return Ok(CloseCallResponse { call_id, terminal });
        }
        let records = call_records(&state.usage, call_id);
        let terminal = CallTerminal::from_records(&records, now)
            .map_err(|error| ControlError::Internal(error.to_string()))?;
        let row = CallRow {
            phase: CallPhase::Closed,
            terminal: Some(terminal.clone()),
        };
        state
            .db
            .tx(|tx| tx.put(Table::Calls, &call_id.to_string(), &row))?;
        state.calls.insert(call_id, row);
        Ok(CloseCallResponse { call_id, terminal })
    }

    // -----------------------------------------------------------------------
    // 4.5 用量
    // -----------------------------------------------------------------------

    /// `GET usage?after=&limit=`。
    pub fn usage_since(
        &self,
        after: Option<&str>,
        limit: Option<u32>,
    ) -> Result<UsagePage, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        sweep(&mut state, now, &self.inner.options)?;
        let after = resolve_cursor(&state, after)?;
        let limit = page_limit(limit)?;
        Ok(page(&state, state.usage.iter(), after, limit))
    }

    /// `GET usage?svc_token_id=&after=`。
    pub fn svc_token_usage(
        &self,
        id: &SvcTokenId,
        after: Option<&str>,
        limit: Option<u32>,
    ) -> Result<UsagePage, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        sweep(&mut state, now, &self.inner.options)?;
        if !state.svcs.contains_key(id) {
            return Err(ControlError::NotFound);
        }
        let after = resolve_cursor(&state, after)?;
        let limit = page_limit(limit)?;
        let records = state.usage.iter().filter(|record| {
            matches!(&record.caller, UsageCaller::Svc { svc_token_id, .. } if svc_token_id == id)
        });
        Ok(page(&state, records, after, limit))
    }

    /// `GET usage?call_id=`：全部记录与终结证明（未关闭为 `None`）。只读。
    pub fn call_statement(&self, call_id: Ulid) -> Result<UsagePage, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        sweep(&mut state, now, &self.inner.options)?;
        let items = call_records(&state.usage, call_id);
        let next_cursor = items
            .last()
            .map(|record| record.seq.to_string())
            .unwrap_or_else(|| "0".into());
        Ok(UsagePage {
            items,
            next_cursor,
            terminal: stored_terminal(&state, call_id, now, &self.inner.options),
        })
    }

    // -----------------------------------------------------------------------
    // 4.4 密钥（只写不读）
    // -----------------------------------------------------------------------

    pub fn stage_secret(
        &self,
        request_id: Ulid,
        secret_ref: SecretName,
        value: SecretValue,
        ctx: &ActorCtx,
    ) -> Result<StagedRowView, ControlError> {
        let len = value.expose().len();
        if len == 0 || len > SECRET_VALUE_MAX {
            return Err(ControlError::validation(
                "/value",
                format!("must be 1..={SECRET_VALUE_MAX} bytes"),
            ));
        }
        let digest = gantry_core::canon::sha256(value.expose());
        let now = self.now();
        let mut state = self.lock();
        self.sweep_staged(&mut state, now, ctx)?;
        if let Some(existing) = state
            .staged
            .values()
            .find(|row| row.request_id == request_id)
        {
            return if existing.secret_ref == secret_ref && existing.value_digest == digest {
                Ok(StagedRowView::from(existing))
            } else {
                Err(ControlError::Conflict(
                    "request_id was replayed with different content".into(),
                ))
            };
        }
        let staged = self
            .inner
            .secrets
            .stage(&secret_ref, value, Expect::Any, ctx)
            .map_err(|error| match error {
                SecretsError::AlreadyStaged => {
                    ControlError::Conflict("another value of this secret is staged".into())
                }
                SecretsError::TooLarge { limit } => {
                    ControlError::validation("/value", format!("must be at most {limit} bytes"))
                }
                other => ControlError::Unavailable(format!("secrets: {other}")),
            })?;
        let staged_id = StagedId::parse(&format!("stg_{}", self.next_ulid()?))
            .map_err(ControlError::Internal)?;
        let row = StagedRow {
            staged_id: staged_id.clone(),
            request_id,
            secret_ref,
            version: staged.version.get(),
            value_digest: digest,
            staged_at: now,
            expires_at: now
                .checked_add_secs(self.inner.options.staged_ttl_s)
                .unwrap_or(now),
            state: StagedState::Staged,
            activated_at: None,
        };
        if let Err(error) = state
            .db
            .tx(|tx| tx.put(Table::Staged, staged_id.as_str(), &row))
        {
            let _ = self.inner.secrets.abort(&staged, ctx);
            return Err(error.into());
        }
        let view = StagedRowView::from(&row);
        state.staged.insert(staged_id, row);
        Ok(view)
    }

    pub fn activate_secret(
        &self,
        staged_id: &StagedId,
        ctx: &ActorCtx,
    ) -> Result<(SecretName, u64, Timestamp), ControlError> {
        let now = self.now();
        let mut state = self.lock();
        let row = state
            .staged
            .get(staged_id)
            .ok_or(ControlError::NotFound)?
            .clone();
        match row.state {
            StagedState::Activated => {
                return Ok((row.secret_ref, row.version, row.activated_at.unwrap_or(now)))
            }
            StagedState::Discarded => return Err(ControlError::StagedExpired),
            StagedState::Staged if now >= row.expires_at => {
                self.discard_row(&mut state, row, ctx)?;
                return Err(ControlError::StagedExpired);
            }
            StagedState::Staged => {}
        }
        let handle = self.staged_handle(&row);
        self.inner
            .secrets
            .commit(&handle, ctx)
            .map_err(|error| ControlError::Unavailable(format!("secrets: {error}")))?;
        let mut row = row;
        row.state = StagedState::Activated;
        row.activated_at = Some(now);
        state
            .db
            .tx(|tx| tx.put(Table::Staged, staged_id.as_str(), &row))?;
        let out = (row.secret_ref.clone(), row.version, now);
        state.staged.insert(staged_id.clone(), row);
        Ok(out)
    }

    pub fn discard_secret(&self, staged_id: &StagedId, ctx: &ActorCtx) -> Result<(), ControlError> {
        let mut state = self.lock();
        let Some(row) = state.staged.get(staged_id).cloned() else {
            return Err(ControlError::NotFound);
        };
        if row.state == StagedState::Staged {
            self.discard_row(&mut state, row, ctx)?;
        }
        Ok(())
    }

    pub fn staged_secrets(&self, ctx: &ActorCtx) -> Result<Vec<StagedSecretItem>, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        self.sweep_staged(&mut state, now, ctx)?;
        Ok(state
            .staged
            .values()
            .filter(|row| row.state == StagedState::Staged)
            .map(|row| StagedSecretItem {
                staged_id: row.staged_id.clone(),
                secret_ref: row.secret_ref.clone(),
                version: row.version,
                staged_at: row.staged_at,
                age_s: u64::try_from(now.as_millis() - row.staged_at.as_millis()).unwrap_or(0)
                    / 1000,
            })
            .collect())
    }

    pub fn secrets_present(
        &self,
        refs: &[SecretName],
    ) -> Result<BTreeMap<SecretName, bool>, ControlError> {
        refs.iter()
            .map(|name| Ok((name.clone(), self.secret_present(name)?)))
            .collect()
    }

    /// 回收不再被引用的 secret；被当前视图引用 → `conflict`。
    pub fn delete_secret(&self, name: &SecretName, ctx: &ActorCtx) -> Result<(), ControlError> {
        let state = self.lock();
        let referenced = state.config.as_ref().is_some_and(|config| {
            config
                .view
                .providers
                .values()
                .any(|provider| provider.secret_ref == *name)
        });
        if referenced {
            return Err(ControlError::Conflict(
                "the secret is referenced by the applied config".into(),
            ));
        }
        drop(state);
        self.inner
            .secrets
            .delete(name, Expect::Any, ctx)
            .map_err(|error| ControlError::Unavailable(format!("secrets: {error}")))?;
        Ok(())
    }

    fn secret_present(&self, name: &SecretName) -> Result<bool, ControlError> {
        self.inner
            .secrets
            .present(name)
            .map_err(|error| ControlError::Unavailable(format!("secrets: {error}")))
    }

    fn staged_handle(&self, row: &StagedRow) -> gantry_core::ports::secrets::StagedSecret {
        gantry_core::ports::secrets::StagedSecret {
            consumer: gantry_core::secret::Consumer::Egress,
            name: row.secret_ref.clone(),
            version: gantry_core::ports::secrets::SecretVersion::new(row.version)
                .expect("staged versions are positive"),
        }
    }

    fn discard_row(
        &self,
        state: &mut State,
        mut row: StagedRow,
        ctx: &ActorCtx,
    ) -> Result<(), ControlError> {
        match self.inner.secrets.abort(&self.staged_handle(&row), ctx) {
            Ok(()) | Err(SecretsError::NotStaged) | Err(SecretsError::NotFound) => {}
            Err(other) => return Err(ControlError::Unavailable(format!("secrets: {other}"))),
        }
        row.state = StagedState::Discarded;
        state
            .db
            .tx(|tx| tx.put(Table::Staged, row.staged_id.as_str(), &row))?;
        state.staged.insert(row.staged_id.clone(), row);
        Ok(())
    }

    fn sweep_staged(
        &self,
        state: &mut State,
        now: Timestamp,
        ctx: &ActorCtx,
    ) -> Result<(), ControlError> {
        let expired: Vec<StagedRow> = state
            .staged
            .values()
            .filter(|row| row.state == StagedState::Staged && now >= row.expires_at)
            .cloned()
            .collect();
        for row in expired {
            self.discard_row(state, row, ctx)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 4.7 状态与端点
    // -----------------------------------------------------------------------

    pub fn status(&self) -> Result<StatusResponse, ControlError> {
        let now = self.now();
        let mut state = self.lock();
        sweep(&mut state, now, &self.inner.options)?;
        let mut unresolved = Vec::new();
        if let Some(config) = &state.config {
            let refs: BTreeSet<&SecretName> = config
                .view
                .providers
                .values()
                .map(|provider| &provider.secret_ref)
                .collect();
            for name in refs {
                if !self.secret_present(name)? {
                    unresolved.push(name.clone());
                }
            }
        }
        let mut details = Vec::new();
        let health = if state.audit_failed {
            details.push("audit write failed".to_owned());
            EgressHealth::Unhealthy
        } else if state.config.is_none() {
            details.push("no config applied".to_owned());
            EgressHealth::Unhealthy
        } else if state.last_apply_error.is_some() || !unresolved.is_empty() {
            if let Some(error) = &state.last_apply_error {
                details.push(error.clone());
            }
            if !unresolved.is_empty() {
                details.push("unresolved secret refs".to_owned());
            }
            EgressHealth::Degraded
        } else {
            EgressHealth::Ok
        };
        if !self.inner.options.source_check_enforced {
            details.push("source_check: unverified".to_owned());
        }
        let active_jobs = state
            .jobs
            .values()
            .filter(|row| row.active_hash.is_some() && now < row.expires_at)
            .count() as u64;
        let active_svc_tokens = state
            .svcs
            .values()
            .filter(|row| row.revoked_at.is_none() && now < row.expires_at)
            .count() as u64;
        Ok(StatusResponse {
            protocol: ControlProtocol,
            applied_rev: state.config.as_ref().map(|config| config.view.rev),
            health,
            detail: (!details.is_empty()).then(|| details.join("; ")),
            unresolved_secret_refs: unresolved,
            active_jobs,
            active_svc_tokens,
            usage_head: (state.next_seq > 0).then(|| state.next_seq.to_string()),
        })
    }

    pub fn endpoints(&self) -> EndpointsResponse {
        let endpoints = &self.inner.options.endpoints;
        EndpointsResponse {
            sandbox_model_base: endpoints.sandbox_model_base.clone(),
            connect_proxy_url: format!("http://{}", endpoints.connect_proxy_authority),
            loopback_model_base: endpoints.loopback_model_base.clone(),
            registry_base: None,
        }
    }

    /// 审计写失败：失败闭合（拒绝新请求、`health = unhealthy`，OQ-E10）。
    pub fn set_audit_failed(&self, failed: bool) {
        self.lock().audit_failed = failed;
    }

    // -----------------------------------------------------------------------
    // 数据面
    // -----------------------------------------------------------------------

    /// 识别令牌（不改变状态）。四种无效情形同一拒绝，规则 id 只进审计。
    pub fn authenticate(&self, listener: Listener, token: &str) -> Result<Caller, Denial> {
        let now = self.now();
        let state = self.lock();
        if state.audit_failed {
            return Err(Denial::Unavailable);
        }
        let hash = TokenHash::of(token).digest();
        match listener {
            Listener::Sandbox => {
                if state.svc_by_hash.contains_key(&hash) {
                    return Err(Denial::TokenInvalid {
                        rule: "token.wrong_listener",
                    });
                }
                let job_id = state.job_by_hash.get(&hash).ok_or(Denial::TokenInvalid {
                    rule: "token.unknown",
                })?;
                let row = &state.jobs[job_id];
                job_token_valid(row, hash, now)?;
                Ok(Caller::Job { job_id: *job_id })
            }
            Listener::Loopback => {
                if state.job_by_hash.contains_key(&hash) {
                    return Err(Denial::TokenInvalid {
                        rule: "token.wrong_listener",
                    });
                }
                let id = state.svc_by_hash.get(&hash).ok_or(Denial::TokenInvalid {
                    rule: "token.unknown",
                })?;
                svc_token_valid(&state.svcs[id], now)?;
                Ok(Caller::Svc {
                    svc_token_id: id.clone(),
                })
            }
        }
    }

    /// job 的 tier（审计用）。
    pub fn job_tier(&self, job_id: &AttemptId) -> Option<Tier> {
        self.lock()
            .jobs
            .get(job_id)
            .map(|row| match row.request.tier {
                SandboxTier::Planner => Tier::Planner,
                SandboxTier::Worker => Tier::Worker,
            })
    }

    /// `CONNECT` 目标是否在该 job 的有效分组（签发快照 ∩ 当前配置）内。
    pub fn host_allowed(&self, job_id: &AttemptId, host: &Host) -> bool {
        let state = self.lock();
        let (Some(config), Some(row)) = (&state.config, state.jobs.get(job_id)) else {
            return false;
        };
        let allowed = effective_patterns(config, &row.request).any(|pattern| pattern.matches(host));
        allowed
    }

    /// 计入一次拒绝（`DeniedCounters`）。
    pub fn note_denied(&self, caller: &Caller, kind: DeniedKind) -> Result<(), Denial> {
        let Caller::Job { job_id } = caller else {
            return Ok(());
        };
        let mut state = self.lock();
        let Some(mut row) = state.jobs.get(job_id).cloned() else {
            return Ok(());
        };
        match kind {
            DeniedKind::NotAllowedHost => row.denied.not_allowed_host += 1,
            DeniedKind::PathNotAllowed => row.denied.path_not_allowed += 1,
        }
        persist_job(&mut state, row).map_err(|_| Denial::Unavailable)
    }

    /// 接受一个模型请求：再次校验令牌、provider 绑定、封顶与调用闸门，登记在途。
    /// 检查与登记在同一把锁内完成（与 `close_call` 的门禁同一串行点）。
    pub fn admit(
        &self,
        caller: &Caller,
        provider_id: &str,
        model: &str,
        svc: Option<&SvcCallMeta>,
    ) -> Result<Admitted, Denial> {
        let now = self.now();
        let mut state = self.lock();
        if state.audit_failed {
            return Err(Denial::Unavailable);
        }
        let config = state.config.clone().ok_or(Denial::Unavailable)?;
        let provider = config
            .provider(provider_id)
            .cloned()
            .ok_or(Denial::ProviderNotAllowed)?;
        let owner = match (caller, svc) {
            (Caller::Job { job_id }, None) => {
                let mut row = state
                    .jobs
                    .get(job_id)
                    .cloned()
                    .ok_or(Denial::TokenInvalid {
                        rule: "token.unknown",
                    })?;
                let hash = row.active_hash.ok_or(Denial::TokenInvalid {
                    rule: "token.revoked",
                })?;
                job_token_valid(&row, hash, now)?;
                if row.request.provider != provider_id {
                    return Err(Denial::ProviderNotAllowed);
                }
                // V11 的运行时再检：provider 主机属于该 job 的有效分组。
                if !effective_patterns(&config, &row.request).any(|p| p.matches(&provider.host)) {
                    return Err(Denial::NotAllowedHost);
                }
                if row.weighted_tokens >= row.request.tokens_total {
                    row.capped = true;
                    row.denied.budget_exhausted += 1;
                    persist_job(&mut state, row).map_err(|_| Denial::Unavailable)?;
                    return Err(Denial::BudgetExhausted);
                }
                InflightOwner::Job { job_id: *job_id }
            }
            (Caller::Svc { svc_token_id }, Some(meta)) => {
                let row = state.svcs.get(svc_token_id).ok_or(Denial::TokenInvalid {
                    rule: "token.unknown",
                })?;
                svc_token_valid(row, now)?;
                if state.calls.contains_key(&meta.call_id) {
                    return Err(Denial::TokenInvalid {
                        rule: "call.closed",
                    });
                }
                InflightOwner::Svc {
                    svc_token_id: svc_token_id.clone(),
                    call_id: meta.call_id,
                    ledger: meta.ledger.clone(),
                    tier: meta.tier,
                    weights: config.view.token_weights,
                    weight_rev: config.view.rev,
                }
            }
            _ => {
                return Err(Denial::TokenInvalid {
                    rule: "token.unknown",
                })
            }
        };
        let id = state.next_inflight;
        state.next_inflight += 1;
        let row = InflightRow {
            owner,
            provider: provider_id.to_owned(),
            model: model.to_owned(),
            started_at: now,
        };
        state
            .db
            .tx(|tx| tx.put(Table::Inflight, &id.to_string(), &row))
            .map_err(|_| Denial::Unavailable)?;
        state.inflight.insert(id, row);
        Ok(Admitted {
            registry: self.clone(),
            id,
            done: false,
            caller: caller.clone(),
            provider,
        })
    }

    /// 请求完成或中断：写一条用量记录、累加到 job，并删除在途登记（同一 SQLite 事务）。
    pub fn finish(
        &self,
        mut admitted: Admitted,
        metered: Metered,
    ) -> Result<UsageRecord, ControlError> {
        admitted.done = true;
        self.complete(admitted.id, metered)
    }

    fn complete(&self, id: u64, metered: Metered) -> Result<UsageRecord, ControlError> {
        let now = self.now();
        let record = {
            let mut state = self.lock();
            let row = state
                .inflight
                .get(&id)
                .cloned()
                .ok_or_else(|| ControlError::Internal("unknown inflight request".into()))?;
            let record = record_completion(&mut state, &row, metered, now)?;
            state
                .db
                .tx(|tx| tx.delete(Table::Inflight, &id.to_string()))?;
            state.inflight.remove(&id);
            record
        };
        self.inner.drained.notify_waiters();
        Ok(record)
    }

    async fn wait_drained(
        &self,
        matches: impl Fn(&InflightRow) -> bool,
    ) -> Result<(), ControlError> {
        let deadline = tokio::time::Instant::now() + self.inner.options.drain_timeout;
        loop {
            let notified = self.inner.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.lock().inflight.values().any(&matches) {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(ControlError::Unavailable(
                    "in-flight requests are still running; retry".into(),
                ));
            }
        }
    }

    fn next_ulid(&self) -> Result<Ulid, ControlError> {
        self.inner
            .ids
            .next_ulid()
            .map_err(|error| ControlError::Internal(error.code().into()))
    }
}

/// 暂存结果的线上视图（`StageSecretResponse` 的字段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedRowView {
    pub staged_id: StagedId,
    pub secret_ref: SecretName,
    pub version: u64,
    pub staged_at: Timestamp,
    pub expires_at: Timestamp,
}

impl From<&StagedRow> for StagedRowView {
    fn from(row: &StagedRow) -> Self {
        Self {
            staged_id: row.staged_id.clone(),
            secret_ref: row.secret_ref.clone(),
            version: row.version,
            staged_at: row.staged_at,
            expires_at: row.expires_at,
        }
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn validate_issue(config: &ReviewedConfig, req: &IssueJobTokenRequest) -> Result<(), ControlError> {
    let mut errors = Vec::new();
    if !config.view.providers.contains_key(&req.provider) {
        errors.push(problem("/provider", "unknown provider"));
    }
    match config.view.profiles.get(&req.profile) {
        None => errors.push(problem("/profile", "unknown profile")),
        Some(profile) => {
            for (index, group) in req.groups.iter().enumerate() {
                if !profile.groups.contains(group) {
                    errors.push(problem(
                        &format!("/groups/{index}"),
                        "group is not in the profile",
                    ));
                }
            }
        }
    }
    let weights = [
        req.token_weights.input,
        req.token_weights.output,
        req.token_weights.cache_read,
        req.token_weights.cache_write,
    ];
    if !weights.iter().all(|w| w.is_finite() && *w > 0.0) {
        errors.push(problem(
            "/token_weights",
            "weights must be finite and positive",
        ));
    }
    if req.tokens_total == 0 {
        errors.push(problem("/tokens_total", "must be positive"));
    }
    if req.wall_time_s == 0 {
        errors.push(problem("/wall_time_s", "must be positive"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ControlError::Validation(errors))
    }
}

/// 绑定比较：`config_rev` 不属于绑定（egress-control §3.1 绑定列表），重放时可以不同。
fn same_binding(a: &IssueJobTokenRequest, b: &IssueJobTokenRequest) -> bool {
    let mut b = b.clone();
    b.config_rev = a.config_rev;
    *a == b
}

/// 有效分组 = 签发快照 ∩ 当前配置中该档案的分组。
fn effective_group_names<'a>(
    config: &'a ReviewedConfig,
    req: &'a IssueJobTokenRequest,
) -> impl Iterator<Item = &'a String> + 'a {
    let profile = config.view.profiles.get(&req.profile);
    req.groups.iter().filter(move |group| {
        profile.is_some_and(|profile| profile.groups.contains(group))
            && config.view.groups.contains_key(*group)
    })
}

fn effective_groups(config: &ReviewedConfig, req: &IssueJobTokenRequest) -> Vec<String> {
    effective_group_names(config, req).cloned().collect()
}

fn effective_patterns<'a>(
    config: &'a ReviewedConfig,
    req: &'a IssueJobTokenRequest,
) -> impl Iterator<Item = &'a AllowPattern> + 'a {
    effective_group_names(config, req)
        .filter_map(|group| config.group(group))
        .flatten()
}

fn job_token_valid(row: &JobRow, hash: Sha256Digest, now: Timestamp) -> Result<(), Denial> {
    if row.revoked_at.is_some() || row.active_hash != Some(hash) {
        return Err(Denial::TokenInvalid {
            rule: "token.revoked",
        });
    }
    if now >= row.expires_at {
        return Err(Denial::TokenInvalid {
            rule: "token.expired",
        });
    }
    Ok(())
}

fn svc_token_valid(row: &SvcRow, now: Timestamp) -> Result<(), Denial> {
    if row.revoked_at.is_some() {
        return Err(Denial::TokenInvalid {
            rule: "token.revoked",
        });
    }
    if now >= row.expires_at {
        return Err(Denial::TokenInvalid {
            rule: "token.expired",
        });
    }
    Ok(())
}

fn persist_job(state: &mut State, row: JobRow) -> Result<(), DbError> {
    let job_id = row.request.job_id;
    state
        .db
        .tx(|tx| tx.put(Table::Jobs, &job_id.to_string(), &row))?;
    state.jobs.insert(job_id, row);
    Ok(())
}

/// 写一条用量记录并（job 调用方）累加；不删除在途登记。
fn record_completion(
    state: &mut State,
    row: &InflightRow,
    metered: Metered,
    now: Timestamp,
) -> Result<UsageRecord, ControlError> {
    let seq = state.next_seq + 1;
    let (record, job) = match &row.owner {
        InflightOwner::Job { job_id } => {
            let Some(job) = state.jobs.get(job_id).cloned() else {
                // job 已被遗忘：用量无人可记，丢弃。
                return Err(ControlError::NotFound);
            };
            let weighted = job.request.token_weights.weighted(&metered.usage);
            let record = UsageRecord {
                seq,
                at: now,
                caller: UsageCaller::Job { job_id: *job_id },
                project_id: Some(job.request.project_id),
                ledger: Some(job.request.ledger.clone()),
                tier: match job.request.tier {
                    SandboxTier::Planner => Tier::Planner,
                    SandboxTier::Worker => Tier::Worker,
                },
                provider: row.provider.clone(),
                model: row.model.clone(),
                usage: metered.usage,
                weighted_tokens: weighted,
                weight_rev: job.request.config_rev.as_u64(),
                incomplete: metered.incomplete,
            };
            let mut job = job;
            add_usage(&mut job.usage, &metered.usage);
            job.weighted_tokens = job.weighted_tokens.saturating_add(weighted);
            job.incomplete |= metered.incomplete;
            note_upstream(&mut job, metered.upstream, now);
            (record, Some(job))
        }
        InflightOwner::Svc {
            svc_token_id,
            call_id,
            ledger,
            tier,
            weights,
            weight_rev,
        } => (
            UsageRecord {
                seq,
                at: now,
                caller: UsageCaller::Svc {
                    svc_token_id: svc_token_id.clone(),
                    call_id: *call_id,
                },
                project_id: None,
                ledger: ledger.clone(),
                tier: *tier,
                provider: row.provider.clone(),
                model: row.model.clone(),
                usage: metered.usage,
                weighted_tokens: weights.weighted(&metered.usage),
                weight_rev: *weight_rev,
                incomplete: metered.incomplete,
            },
            None,
        ),
    };
    state.db.tx(|tx| {
        tx.insert_usage(&record)?;
        tx.put_meta(META_NEXT_SEQ, &seq)?;
        if let Some(job) = &job {
            tx.put(Table::Jobs, &job.request.job_id.to_string(), job)?;
        }
        Ok(())
    })?;
    state.next_seq = seq;
    state.usage.push(record.clone());
    if let Some(job) = job {
        state.jobs.insert(job.request.job_id, job);
    }
    Ok(record)
}

fn add_usage(total: &mut TokenUsage, add: &TokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(add.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(add.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(add.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(add.cache_write_tokens);
    total.requests = total.requests.saturating_add(add.requests);
}

fn note_upstream(job: &mut JobRow, class: UpstreamClass, now: Timestamp) {
    let errors = &mut job.upstream_errors;
    match class {
        UpstreamClass::Ok => {
            job.consecutive_errors = 0;
            return;
        }
        UpstreamClass::Status429 => errors.total_429 += 1,
        UpstreamClass::Status5xx => errors.total_5xx += 1,
        UpstreamClass::ConnError => errors.total_conn_errors += 1,
    }
    job.consecutive_errors += 1;
    errors.max_consecutive = errors.max_consecutive.max(job.consecutive_errors);
    errors.last_at = Some(now);
}

fn call_records(records: &[UsageRecord], call_id: Ulid) -> Vec<UsageRecord> {
    records
        .iter()
        .filter(|record| {
            matches!(&record.caller, UsageCaller::Svc { call_id: id, .. } if *id == call_id)
        })
        .cloned()
        .collect()
}

/// 已关闭调用的证明；过了保留期仍返回同一份，`incomplete` 置真（egress-call-terminal §5）。
fn stored_terminal(
    state: &State,
    call_id: Ulid,
    now: Timestamp,
    options: &RegistryOptions,
) -> Option<CallTerminal> {
    let row = state.calls.get(&call_id)?;
    let mut terminal = row.terminal.clone()?;
    let expires = terminal
        .closed_at
        .checked_add_secs(options.usage_retention_s);
    if expires.is_none_or(|at| now >= at) {
        terminal.incomplete = true;
    }
    Some(terminal)
}

/// 不带游标从保留窗口起点读；游标早于保留窗口 → `410 cursor_expired`（egress-control §4.5）。
fn resolve_cursor(state: &State, after: Option<&str>) -> Result<u64, ControlError> {
    let Some(raw) = after else {
        return Ok(state.pruned_through);
    };
    let after = raw.parse::<u64>().map_err(|_| {
        ControlError::InvalidRequest("after must be a cursor from next_cursor".into())
    })?;
    if after < state.pruned_through {
        return Err(ControlError::CursorExpired);
    }
    Ok(after)
}

fn page_limit(limit: Option<u32>) -> Result<usize, ControlError> {
    match limit.unwrap_or(USAGE_PAGE_DEFAULT) {
        0 => Err(ControlError::InvalidRequest(
            "limit must be positive".into(),
        )),
        n => Ok(n.min(USAGE_PAGE_MAX) as usize),
    }
}

fn page<'a>(
    state: &State,
    records: impl Iterator<Item = &'a UsageRecord>,
    after: u64,
    limit: usize,
) -> UsagePage {
    let items: Vec<UsageRecord> = records
        .filter(|record| record.seq > after)
        .take(limit)
        .cloned()
        .collect();
    let next_cursor = items
        .last()
        .map(|record| record.seq)
        .unwrap_or(state.next_seq.max(after))
        .to_string();
    UsagePage {
        items,
        next_cursor,
        terminal: None,
    }
}

fn forget(state: &mut State, job_id: &AttemptId) -> Result<(), ControlError> {
    let Some(row) = state.jobs.get(job_id) else {
        return Ok(());
    };
    let hashes: Vec<Sha256Digest> = row.hashes().collect();
    state
        .db
        .tx(|tx| tx.delete(Table::Jobs, &job_id.to_string()))?;
    for hash in hashes {
        state.job_by_hash.remove(&hash);
    }
    state.jobs.remove(job_id);
    Ok(())
}

/// 惰性清理：吊销超过 TTL 的 job 自动遗忘；满保留期的用量记录删除（与证明过期同一边界）。
fn sweep(state: &mut State, now: Timestamp, options: &RegistryOptions) -> Result<(), ControlError> {
    let stale: Vec<AttemptId> = state
        .jobs
        .values()
        .filter(|row| {
            row.revoked_at
                .and_then(|at| at.checked_add_secs(options.final_usage_ttl_s))
                .is_some_and(|deadline| now >= deadline)
        })
        .map(|row| row.request.job_id)
        .collect();
    for job_id in stale {
        forget(state, &job_id)?;
    }
    let cutoff = now.as_millis() - (options.usage_retention_s as i64) * 1000;
    if state
        .usage
        .first()
        .is_some_and(|record| record.at.as_millis() <= cutoff)
    {
        let pruned = state.db.tx(|tx| {
            let pruned = tx.prune_usage(cutoff)?;
            if let Some(seq) = pruned {
                tx.put_meta(META_PRUNED_THROUGH, &seq)?;
            }
            Ok(pruned)
        })?;
        if let Some(seq) = pruned {
            state.pruned_through = state.pruned_through.max(seq);
        }
        state.usage.retain(|record| record.at.as_millis() > cutoff);
    }
    Ok(())
}

fn problem(pointer: &str, message: &str) -> ProblemError {
    ProblemError {
        pointer: pointer.to_owned(),
        message: message.to_owned(),
    }
}
