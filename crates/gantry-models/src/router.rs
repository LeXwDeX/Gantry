//! `ModelRouter`：`resolve` 与 `invoke_direct`（models.md §6、§7.3、§7.5、§7.6）。
//!
//! 一次直接调用：校验角色与 schema → 取视图快照 → 会话台账检查 → 解析模型 →
//! 推理深度映射（不支持即报错）→ 组装 prompt → 经 Egress 回环反向代理发送
//! （重试 / 退避 / 期限 / 并发闸 / 令牌轮换）→ 输出校验 → 关闭调用并按终结证明
//! 记账（[`crate::accounting`]）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use gantry_core::entities::{Actor, ActorKind, Role, Seq, Tier, TokenUsage};
use gantry_core::events::ServedBy;
use gantry_core::ids::{ConversationId, DeliveryId, Entropy, IdGen, ProjectId};
use gantry_core::level::Level;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::{EgressControl, SvcTokenGrant};
use gantry_core::ports::engine::ModuleStatus;
use gantry_core::ports::models::{
    CallId, EgressRoute, LedgerId, Material, ModelProvider, ProviderError, ProviderRequest,
    StopReason, UsageSource,
};
use gantry_core::time::{Date, Timestamp};
use gantry_store::Store;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::accounting::{self, Settlement};
use crate::error::ModelsError;
use crate::output::DirectRole;
use crate::prompt::{build_prompt, TrustedText};
use crate::settings::ViewCache;
use crate::view::{ModelsView, Purpose, ResolveQuery, ResolvedModel};

/// 每个 provider 的并发闸（§7.8 内部常量）。
pub const PROVIDER_MAX_INFLIGHT: usize = 8;
/// 计量与自报偏离超过该比例记审计警告（§7.7）。
pub const DIVERGENCE_THRESHOLD: f64 = 0.10;

/// 调用归属；经 `X-Gantry-Ledger` 头告诉 Egress（P-74）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attribution {
    Attempt {
        delivery_id: DeliveryId,
        node_id: gantry_core::ids::NodeId,
        attempt_id: gantry_core::ids::AttemptId,
    },
    Aux {
        delivery_id: DeliveryId,
        purpose: String,
    },
    /// 记入该会话当日的会话台账（P-58）。
    Conversation {
        conversation_id: ConversationId,
        purpose: String,
    },
    /// 无项目 / 交付 / 会话的全局调用：不建台账、不写 `budget.charged`
    /// （event-data §2.2.6.1）。
    Unattributed { purpose: String },
}

#[derive(Clone, Debug)]
pub struct DirectCall {
    /// 调用方生成；贯穿 Egress 计量与日志。重做时换新 id。
    pub call_id: CallId,
    pub project_id: ProjectId,
    pub role: Role,
    pub attribution: Attribution,
    pub level: Option<Level>,
    pub critical: bool,
    pub instructions: TrustedText,
    pub materials: Vec<Material>,
    /// 如 `gantry.worker.output.classify/1`。
    pub schema: String,
    /// 只能收紧 `total_timeout_s`。
    pub deadline: Option<Duration>,
}

/// 用量相关的警告（不是错误）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageWarning {
    /// Egress 计量取不到，退回提供方自报（OQ-3）。
    UsageUnavailable { reason: String },
    /// 已关闭但证明不完整：不结算、不零判定。
    NotSettled { reason: String },
    /// 计量与自报偏离超过阈值（审计警告）。
    Divergence { metered: u64, reported: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectOutcome {
    /// 已通过 schema 校验；其中的文本字段仍是不可信数据。
    pub output: Value,
    pub served_by: ServedBy,
    pub usage: TokenUsage,
    pub usage_source: UsageSource,
    pub weighted_tokens: u64,
    pub tries: u32,
    pub fell_back: bool,
    /// 写入的 `budget.charged` 序号；未记账（未归属、计量缺失）为 `None`。
    pub charged_seq: Option<Seq>,
    pub warnings: Vec<UsageWarning>,
}

/// 驱动退避与期限的时钟；测试注入假时钟。
#[async_trait]
pub trait RouterClock: Send + Sync {
    /// 单调时间。
    fn elapsed(&self) -> Duration;
    /// 墙钟（会话台账的 UTC 日）。
    fn now(&self) -> Timestamp;
    async fn sleep(&self, duration: Duration);
}

/// 生产时钟。
pub struct TokioClock {
    start: tokio::time::Instant,
}

impl Default for TokioClock {
    fn default() -> Self {
        Self {
            start: tokio::time::Instant::now(),
        }
    }
}

#[async_trait]
impl RouterClock for TokioClock {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    fn now(&self) -> Timestamp {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default();
        Timestamp::from_millis(ms)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// 构造依赖。
pub struct RouterDeps {
    pub views: ViewCache,
    pub providers: Vec<Arc<dyn ModelProvider>>,
    pub egress: Arc<dyn EgressControl>,
    pub store: Store,
    pub entropy: Arc<dyn Entropy>,
    pub ids: Arc<IdGen>,
    pub clock: Arc<dyn RouterClock>,
}

pub struct ModelRouter {
    views: ViewCache,
    providers: Vec<Arc<dyn ModelProvider>>,
    egress: Arc<dyn EgressControl>,
    store: Store,
    entropy: Arc<dyn Entropy>,
    ids: Arc<IdGen>,
    clock: Arc<dyn RouterClock>,
    svc: tokio::sync::Mutex<Option<SvcTokenGrant>>,
    gates: Mutex<HashMap<String, Arc<Semaphore>>>,
    inflight: Mutex<HashSet<CallId>>,
}

/// 发送阶段的结果（记账前）。
struct Sent {
    outcome: Result<Value, ModelsError>,
    tries: u32,
    reported: TokenUsage,
    /// 是否有请求可能已被 Egress 接收（决定是否需要关闭调用）。
    sent_any: bool,
}

impl ModelRouter {
    pub fn new(deps: RouterDeps) -> Self {
        Self {
            views: deps.views,
            providers: deps.providers,
            egress: deps.egress,
            store: deps.store,
            entropy: deps.entropy,
            ids: deps.ids,
            clock: deps.clock,
            svc: tokio::sync::Mutex::new(None),
            gates: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
        }
    }

    /// 解析模型与推理深度；纯读配置，不发请求。
    pub async fn resolve(&self, q: &ResolveQuery) -> Result<ResolvedModel, ModelsError> {
        self.views.view(Some(&q.project_id)).await?.resolve(q)
    }

    /// `settings/status` 的 `modules.models`。
    pub fn status(&self) -> ModuleStatus {
        self.views.status()
    }

    /// 直接调用：只接受 `intent` / `admission` / `summarize` / `classify`。
    pub async fn invoke_direct(&self, call: DirectCall) -> Result<DirectOutcome, ModelsError> {
        let role = DirectRole::for_role(&call.role)?;
        role.check_schema(&call.role, &call.schema)?;
        let view = self.views.view(Some(&call.project_id)).await?;
        let resolved = view.resolve(&ResolveQuery {
            project_id: call.project_id,
            tier: role.tier,
            level: call.level,
            critical: call.critical,
            purpose: Purpose::Direct,
        })?;
        let provider = self.provider(&resolved)?;
        let effort = provider
            .map_effort(&resolved.model, resolved.reasoning_effort)
            .map_err(|error| ModelsError::EffortUnsupported {
                model: error.model,
                effort: error.effort,
            })?;
        let day = self.clock.now().to_date();
        let ledger = ledger_id(&call.attribution, day);
        if let Attribution::Conversation {
            conversation_id, ..
        } = &call.attribution
        {
            let used = accounting::conversation_used(&self.store, conversation_id, day).await?;
            if used >= view.daily_tokens() {
                return Err(ModelsError::BudgetExhausted {
                    used,
                    cap: view.daily_tokens(),
                });
            }
        }
        let prompt = build_prompt(&call.instructions, &call.materials, self.entropy.as_ref())?;

        // 调用方丢弃 future 即取消；守卫保证取消后该调用不再被对账跳过。
        let _inflight = InflightGuard::new(&self.inflight, call.call_id);
        let sent = self
            .send(
                &call,
                &view,
                &resolved,
                provider.as_ref(),
                &role,
                &ledger,
                effort,
                &prompt,
            )
            .await;
        let settled = if sent.sent_any {
            self.settle(&call, role.tier, day).await
        } else {
            Ok(Settlement::nothing())
        };

        let output = sent.outcome?;
        let settlement = settled?;
        let mut warnings = settlement.warnings;
        let (usage, usage_source, weighted_tokens) = match settlement.metered {
            Some((usage, weighted)) => {
                if let Some(warning) = divergence(&usage, &sent.reported) {
                    warnings.push(warning);
                }
                (usage, UsageSource::Metered, weighted)
            }
            None => (
                sent.reported,
                UsageSource::ProviderReported,
                view.token_weights().weighted(&sent.reported),
            ),
        };
        Ok(DirectOutcome {
            output,
            served_by: ServedBy {
                provider: resolved.provider_id.clone(),
                model: resolved.model.clone(),
                effort: Some(resolved.reasoning_effort),
            },
            usage,
            usage_source,
            weighted_tokens,
            tries: sent.tries,
            fell_back: false,
            charged_seq: settlement.charged_seq,
            warnings,
        })
    }

    /// 对账（P-74）：按被吊销令牌的全部用量记录，补记缺失的 `budget.charged`。
    /// 以 `source.call_id` 去重；进行中的调用跳过。返回补记条数。
    pub async fn reconcile_svc_tokens(
        &self,
        ids: &[gantry_core::egress::SvcTokenId],
    ) -> Result<usize, ModelsError> {
        let mut added = 0;
        for id in ids {
            let skip = self.lock_inflight().clone();
            added += accounting::reconcile_token(
                &self.store,
                self.egress.as_ref(),
                id,
                &skip,
                &self.ctx()?,
            )
            .await?;
        }
        Ok(added)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        call: &DirectCall,
        view: &ModelsView,
        resolved: &ResolvedModel,
        provider: &dyn ModelProvider,
        role: &DirectRole,
        ledger: &LedgerId,
        effort: gantry_core::ports::models::ProviderEffort,
        prompt: &crate::prompt::BuiltPrompt,
    ) -> Sent {
        let retry = &view.models().retry;
        let start = self.clock.elapsed();
        let mut total = Duration::from_secs(resolved.total_timeout_s.unwrap_or(u64::MAX / 1000));
        if let Some(deadline) = call.deadline {
            total = total.min(deadline);
        }
        let request_timeout = Duration::from_secs(resolved.request_timeout_s);
        let gate = self.gate(&resolved.provider_id);
        let schema = role.json_schema();

        let mut sent = Sent {
            outcome: Err(ModelsError::Timeout),
            tries: 0,
            reported: TokenUsage::default(),
            sent_any: false,
        };
        let mut attempts: u64 = 0;
        let mut rotated = false;
        let mut feedback: Option<String> = None;

        loop {
            let remaining = match total.checked_sub(self.clock.elapsed().saturating_sub(start)) {
                Some(left) if !left.is_zero() => left,
                _ => {
                    sent.outcome = Err(ModelsError::Timeout);
                    return sent;
                }
            };
            let grant = match self.svc_grant().await {
                Ok(grant) => grant,
                Err(error) => {
                    sent.outcome = Err(error);
                    return sent;
                }
            };
            let Some(base_url) = grant.model_base_urls.get(&resolved.provider_id) else {
                sent.outcome = Err(ModelsError::Unavailable {
                    reason: format!("egress has no route for provider {}", resolved.provider_id),
                });
                return sent;
            };
            let route = EgressRoute {
                base_url: base_url.to_owned(),
                tier: role.tier,
                call_id: call.call_id,
                ledger: ledger.clone(),
            };
            let system = match &feedback {
                Some(code) => format!(
                    "{}\n\nYour previous reply was rejected ({code}). Reply again with one JSON object matching the schema.",
                    prompt.system
                ),
                None => prompt.system.clone(),
            };
            let req = ProviderRequest {
                model: resolved.model.clone(),
                system,
                input: prompt.input.clone(),
                effort: effort.clone(),
                output_schema: Some(schema.clone()),
                max_output_tokens: role.max_output_tokens,
                request_timeout: request_timeout.min(remaining),
                call_id: call.call_id,
            };

            // 排队时间计入期限。
            let permit = match tokio::time::timeout(remaining, gate.clone().acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                _ => {
                    sent.outcome = Err(ModelsError::Timeout);
                    return sent;
                }
            };
            let left = total.saturating_sub(self.clock.elapsed().saturating_sub(start));
            if left.is_zero() {
                sent.outcome = Err(ModelsError::Timeout);
                return sent;
            }
            let req = ProviderRequest {
                request_timeout: req.request_timeout.min(left),
                ..req
            };
            sent.tries += 1;
            sent.sent_any = true;
            let result = tokio::time::timeout(req.request_timeout, provider.complete(req, &route))
                .await
                .unwrap_or(Err(ProviderError::Timeout));
            drop(permit);

            let mut retry_after = None;
            let last_failure: String;
            match result {
                Ok(response) => {
                    add_usage(&mut sent.reported, &response.usage_reported);
                    match response.stop {
                        StopReason::EndTurn => match role.validate(&response.text) {
                            Ok(output) => {
                                sent.outcome = Ok(output);
                                return sent;
                            }
                            Err(error) => {
                                feedback = Some(error.code());
                                attempts += 1;
                                if attempts >= retry.max_attempts {
                                    sent.outcome = Err(ModelsError::SchemaInvalid {
                                        last_error: error.code(),
                                    });
                                    return sent;
                                }
                                // 输出无效不是上游故障，不退避。
                                continue;
                            }
                        },
                        StopReason::MaxTokens => {
                            sent.outcome = Err(ModelsError::OutputTruncated);
                            return sent;
                        }
                        StopReason::Refusal { category } => {
                            sent.outcome = Err(ModelsError::ContentRefused { category });
                            return sent;
                        }
                        StopReason::Other(reason) => {
                            last_failure = format!("stop_reason {reason}");
                        }
                    }
                }
                Err(ProviderError::EgressTokenInvalid) => {
                    // 令牌被吊销或过期：换新令牌重试一次，不计入 max_attempts。
                    if rotated {
                        sent.outcome = Err(ModelsError::Unavailable {
                            reason: "egress rejected the rotated svc_token".into(),
                        });
                        return sent;
                    }
                    rotated = true;
                    if let Err(error) = self.rotate_svc(&grant).await {
                        sent.outcome = Err(error);
                        return sent;
                    }
                    continue;
                }
                Err(ProviderError::InvalidRequest { message }) => {
                    let status = message
                        .strip_prefix("status ")
                        .and_then(|code| code.parse().ok())
                        .unwrap_or(400);
                    sent.outcome = Err(ModelsError::InvalidRequest { status });
                    return sent;
                }
                Err(ProviderError::ContentRefused { category }) => {
                    sent.outcome = Err(ModelsError::ContentRefused { category });
                    return sent;
                }
                Err(ProviderError::OutputTruncated) => {
                    sent.outcome = Err(ModelsError::OutputTruncated);
                    return sent;
                }
                Err(ProviderError::UnsupportedEffort(error)) => {
                    sent.outcome = Err(ModelsError::EffortUnsupported {
                        model: error.model,
                        effort: error.effort,
                    });
                    return sent;
                }
                Err(ProviderError::ModelNotFound) => {
                    // 模型不可用：不在当前候选上重试（M2 起换下一个降级候选）。
                    sent.outcome = Err(ModelsError::Unavailable {
                        reason: "model_not_found".into(),
                    });
                    return sent;
                }
                Err(ProviderError::RateLimited { retry_after: after }) => {
                    retry_after = after;
                    last_failure = "rate_limited".into();
                }
                Err(ProviderError::Timeout) => {
                    last_failure = "timeout".into();
                }
                Err(ProviderError::Unavailable { message })
                | Err(ProviderError::Protocol { message }) => {
                    last_failure = message;
                }
            }

            attempts += 1;
            if attempts >= retry.max_attempts {
                sent.outcome = Err(ModelsError::Unavailable {
                    reason: last_failure,
                });
                return sent;
            }
            let backoff = self.backoff(retry.backoff_ms, attempts, retry_after);
            let elapsed = self.clock.elapsed().saturating_sub(start);
            if elapsed + backoff >= total {
                sent.outcome = Err(ModelsError::Timeout);
                return sent;
            }
            self.clock.sleep(backoff).await;
        }
    }

    /// 关闭调用并按终结证明记账；不可结算时退回自报并给出警告。
    async fn settle(
        &self,
        call: &DirectCall,
        tier: Tier,
        day: Date,
    ) -> Result<Settlement, ModelsError> {
        let svc_id = match self.svc.lock().await.as_ref() {
            Some(grant) => grant.svc_token_id.clone(),
            None => return Ok(Settlement::nothing()),
        };
        let charge = accounting::Charge {
            project_id: call.project_id,
            call_id: call.call_id,
            ledger: accounting::ledger_ref(&call.attribution, day),
            tier,
        };
        accounting::settle_call(
            &self.store,
            self.egress.as_ref(),
            &svc_id,
            &charge,
            &self.ctx()?,
        )
        .await
    }

    fn provider(&self, resolved: &ResolvedModel) -> Result<Arc<dyn ModelProvider>, ModelsError> {
        self.providers
            .iter()
            .find(|provider| provider.kind() == resolved.provider_kind)
            .cloned()
            .ok_or_else(|| ModelsError::NotConfigured {
                tier: resolved.tier.as_str(),
                reason: format!(
                    "no provider implementation for {:?}",
                    resolved.provider_kind
                ),
            })
    }

    fn gate(&self, provider_id: &str) -> Arc<Semaphore> {
        self.gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(provider_id.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(PROVIDER_MAX_INFLIGHT)))
            .clone()
    }

    fn lock_inflight(&self) -> std::sync::MutexGuard<'_, HashSet<CallId>> {
        self.inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 指数退避加抖动（0–25%）；`retry-after` 更长时取它。
    fn backoff(&self, base_ms: u64, attempt: u64, retry_after: Option<Duration>) -> Duration {
        let exp = base_ms.saturating_mul(1u64 << (attempt - 1).min(16));
        let mut byte = [0u8; 1];
        self.entropy.fill(&mut byte);
        let jitter = exp.saturating_mul(u64::from(byte[0])) / (255 * 4);
        let backoff = Duration::from_millis(exp.saturating_add(jitter));
        retry_after.map_or(backoff, |after| after.max(backoff))
    }

    fn ctx(&self) -> Result<ActorCtx, ModelsError> {
        let request_id = self
            .ids
            .next_ulid()
            .map_err(|error| ModelsError::Internal {
                reason: format!("id generation: {error}"),
            })?;
        Ok(ActorCtx {
            actor: Actor::new(ActorKind::System, "models"),
            device: None,
            role: None,
            client_profile: None,
            request_id,
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        })
    }

    /// 当前进程的 `svc_token`；首次取得时对上一个进程留下的令牌对账。
    async fn svc_grant(&self) -> Result<SvcTokenGrant, ModelsError> {
        let mut guard = self.svc.lock().await;
        if let Some(grant) = guard.as_ref() {
            return Ok(grant.clone());
        }
        let grant = self
            .egress
            .service_token(&self.ctx()?)
            .await
            .map_err(egress_error)?;
        *guard = Some(grant.clone());
        drop(guard);
        self.reconcile_svc_tokens(&grant.revoked_previous).await?;
        Ok(grant)
    }

    /// 主动轮换 `svc_token`（到期前 1 h，P-74），并对被吊销的令牌对账。
    pub async fn rotate_service_token(&self) -> Result<(), ModelsError> {
        let mut guard = self.svc.lock().await;
        let grant = self
            .egress
            .rotate_service_token(&self.ctx()?)
            .await
            .map_err(egress_error)?;
        *guard = Some(grant.clone());
        drop(guard);
        self.reconcile_svc_tokens(&grant.revoked_previous).await?;
        Ok(())
    }

    /// `403 token_invalid` 后换新令牌；并发调用只轮换一次。
    async fn rotate_svc(&self, stale: &SvcTokenGrant) -> Result<(), ModelsError> {
        let mut guard = self.svc.lock().await;
        if let Some(current) = guard.as_ref() {
            if current.svc_token_id != stale.svc_token_id {
                return Ok(());
            }
        }
        let grant = self
            .egress
            .rotate_service_token(&self.ctx()?)
            .await
            .map_err(egress_error)?;
        *guard = Some(grant.clone());
        drop(guard);
        self.reconcile_svc_tokens(&grant.revoked_previous).await?;
        Ok(())
    }
}

/// 进行中调用的登记；离开作用域（含取消）时移除。
struct InflightGuard<'a> {
    set: &'a Mutex<HashSet<CallId>>,
    call_id: CallId,
}

impl<'a> InflightGuard<'a> {
    fn new(set: &'a Mutex<HashSet<CallId>>, call_id: CallId) -> Self {
        set.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call_id);
        Self { set, call_id }
    }
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.call_id);
    }
}

fn egress_error(error: gantry_core::ports::egress::EgressError) -> ModelsError {
    ModelsError::Unavailable {
        reason: format!("egress control: {}", redact(&error.to_string())),
    }
}

/// 错误文本里不出现令牌形状的片段。
fn redact(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            if word.contains("gst1_") || word.contains("gjt1_") || word.contains("/s/") {
                "***"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn ledger_id(attribution: &Attribution, day: Date) -> LedgerId {
    match attribution {
        Attribution::Attempt { delivery_id, .. } | Attribution::Aux { delivery_id, .. } => {
            LedgerId::new(delivery_id.to_string())
        }
        Attribution::Conversation {
            conversation_id, ..
        } => LedgerId::new(format!("{conversation_id}:{day}")),
        Attribution::Unattributed { .. } => LedgerId::unattributed(),
    }
}

fn add_usage(total: &mut TokenUsage, more: &TokenUsage) {
    total.input_tokens += more.input_tokens;
    total.output_tokens += more.output_tokens;
    total.cache_read_tokens += more.cache_read_tokens;
    total.cache_write_tokens += more.cache_write_tokens;
    total.requests += more.requests;
}

fn token_sum(usage: &TokenUsage) -> u64 {
    usage.input_tokens + usage.output_tokens + usage.cache_read_tokens + usage.cache_write_tokens
}

fn divergence(metered: &TokenUsage, reported: &TokenUsage) -> Option<UsageWarning> {
    let metered = token_sum(metered);
    let reported = token_sum(reported);
    let base = metered.max(1) as f64;
    let diff = (metered as f64 - reported as f64).abs();
    (diff / base > DIVERGENCE_THRESHOLD).then_some(UsageWarning::Divergence { metered, reported })
}
