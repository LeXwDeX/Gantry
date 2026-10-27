//! models.md §12.2：经 Egress、对账、无 key、输出校验、重试与期限、用量、
//! 会话台账、热加载、并发闸、角色约束、不降级。
//!
//! 模型调用走回环 HTTP 模型桩（`common::egress`），不读取任何真实密钥。
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::egress::{anthropic_body, ok, status, Action, ProxiedEgress, RecordingProxy};
use common::{global_models, project, put_settings, store, START_MS};
use gantry_core::entities::{Actor, ActorKind, Role, SettingsResource, TokenWeights};
use gantry_core::events::{BudgetChargedData, EventType, NewEvent, Subject};
use gantry_core::ids::{ConversationId, DeliveryId, Entropy, IdGen, Ulid};
use gantry_core::level::Effort;
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::EgressControl;
use gantry_core::ports::models::{Material, UsageSource};
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use gantry_core::worker::MaterialKind;
use gantry_fakes::egress::{default_config, EgressStub};
use gantry_models::prompt::TrustedText;
use gantry_models::{
    AnthropicProvider, Attribution, DirectCall, DirectOutcome, EffortTable, ModelRouter,
    ModelsError, RouterClock, RouterDeps, StoreSettings, UsageWarning, ViewCache,
    PROVIDER_MAX_INFLIGHT,
};
use gantry_store::Store;
use serde_json::{json, Value};

const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const CONVERSATION: &str = "cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ";
const CLASSIFY_OK: &str = r#"{"kind":"classify","target":"event_line","text":"ok"}"#;
const CLASSIFY_SCHEMA: &str = "gantry.worker.output.classify/1";

/// 单调时间 = 真实流逝 + 假睡眠累计；`sleep` 只记账不等待。
struct TestClock {
    start: std::time::Instant,
    offset: Mutex<Duration>,
    sleeps: Mutex<Vec<Duration>>,
}

impl TestClock {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            offset: Mutex::new(Duration::ZERO),
            sleeps: Mutex::new(Vec::new()),
        }
    }

    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().unwrap().clone()
    }
}

#[async_trait]
impl RouterClock for TestClock {
    fn elapsed(&self) -> Duration {
        self.start.elapsed() + *self.offset.lock().unwrap()
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_millis(START_MS)
    }

    async fn sleep(&self, duration: Duration) {
        *self.offset.lock().unwrap() += duration;
        self.sleeps.lock().unwrap().push(duration);
        tokio::task::yield_now().await;
    }
}

struct CountingEntropy(AtomicU64);

impl Entropy for CountingEntropy {
    fn fill(&self, buf: &mut [u8]) {
        let value = self.0.fetch_add(1, Ordering::SeqCst).to_le_bytes();
        for (index, byte) in buf.iter_mut().enumerate() {
            *byte = value[index % value.len()] ^ (index as u8);
        }
    }
}

struct Env {
    router: Arc<ModelRouter>,
    proxy: Arc<RecordingProxy>,
    egress: Arc<ProxiedEgress>,
    stub: Arc<EgressStub>,
    store: Store,
    clock: Arc<TestClock>,
    ids: Arc<IdGen>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.proxy.shutdown();
    }
}

fn efforts() -> EffortTable {
    EffortTable::new()
        .with_model("utility-model", [(Effort::Low, "low")])
        .with_model("utility-model-2", [(Effort::Low, "low")])
        .with_model("planner-model", [(Effort::High, "high")])
}

async fn seed_ledgers(store: &Store) {
    let cases: Value =
        serde_json::from_str(include_str!("../../gantry-core/fixtures/events/cases.json")).unwrap();
    let mut tx = store.begin_write().await.unwrap();
    for (ty, subject) in [
        ("delivery.created", DELIVERY),
        ("conversation.created", CONVERSATION),
    ] {
        tx.append(NewEvent {
            ty: ty.parse().unwrap(),
            subject: Subject::parse(subject).unwrap(),
            project_id: Some(project()),
            actor: Actor::new(ActorKind::Engine, "engine"),
            causation_id: None,
            correlation_id: None,
            data: cases["valid"][ty].clone(),
            dedup_key: None,
        })
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

async fn env_with(models: Value) -> Env {
    let (store, fake_clock) = store();
    put_settings(&store, None, SettingsResource::Models, models).await;
    seed_ledgers(&store).await;
    let stub = Arc::new(EgressStub::with_default_config());
    let upstream = stub
        .data_plane_url()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    let proxy = RecordingProxy::start(upstream);
    proxy.set_default(ok(CLASSIFY_OK));
    let egress = Arc::new(ProxiedEgress::new(stub.clone(), &proxy));
    let entropy = Arc::new(CountingEntropy(AtomicU64::new(1)));
    let ids = Arc::new(IdGen::new(fake_clock, entropy.clone()));
    let clock = Arc::new(TestClock::new());
    let router = Arc::new(ModelRouter::new(RouterDeps {
        views: ViewCache::new(Arc::new(StoreSettings::new(store.clone()))),
        providers: vec![Arc::new(AnthropicProvider::new(efforts()))],
        egress: egress.clone(),
        store: store.clone(),
        entropy,
        ids: ids.clone(),
        clock: clock.clone(),
    }));
    Env {
        router,
        proxy,
        egress,
        stub,
        store,
        clock,
        ids,
    }
}

async fn env() -> Env {
    env_with(global_models()).await
}

fn delivery() -> DeliveryId {
    DELIVERY.parse().unwrap()
}

fn conversation() -> ConversationId {
    CONVERSATION.parse().unwrap()
}

impl Env {
    fn call(&self, attribution: Attribution) -> DirectCall {
        DirectCall {
            call_id: self.ids.next_ulid().unwrap(),
            project_id: project(),
            role: Role::Classify,
            attribution,
            level: None,
            critical: false,
            instructions: TrustedText::from_static("Summarize the CI event in one line."),
            materials: vec![Material {
                id: "mat-1".into(),
                kind: MaterialKind::CiLog,
                source: "acme/widgets#5".into(),
                text: UntrustedText::new("error: build failed".into(), "ci".into(), false),
            }],
            schema: CLASSIFY_SCHEMA.into(),
            deadline: None,
        }
    }

    fn aux(&self) -> DirectCall {
        self.call(Attribution::Aux {
            delivery_id: delivery(),
            purpose: "ci_summary".into(),
        })
    }

    async fn charges(&self) -> Vec<(String, BudgetChargedData)> {
        let mut out = Vec::new();
        for subject in [DELIVERY, CONVERSATION] {
            for event in self
                .store
                .events()
                .by_subject(&Subject::parse(subject).unwrap())
                .await
                .unwrap()
            {
                if event.ty == EventType::BudgetCharged {
                    out.push((
                        event.subject.to_string(),
                        serde_json::from_value(event.data).unwrap(),
                    ));
                }
            }
        }
        out
    }

    fn svc_token(&self) -> String {
        let path = &self.proxy.recorded()[0].path;
        path.split('/').nth(2).unwrap().to_owned()
    }
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(1, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

// ---------------------------------------------------------------------------
// 经 Egress
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn requests_go_only_to_egress_loopback_with_svc_token_and_ledger_headers() {
    let env = env().await;
    let calls = [
        env.call(Attribution::Attempt {
            delivery_id: delivery(),
            node_id: "nod_01JBX7A1B2C3D4E5F6G7H8J9K0".parse().unwrap(),
            attempt_id: "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD".parse().unwrap(),
        }),
        env.call(Attribution::Conversation {
            conversation_id: conversation(),
            purpose: "status_answer".into(),
        }),
        env.call(Attribution::Unattributed {
            purpose: "self_check".into(),
        }),
    ];
    let ids: Vec<_> = calls.iter().map(|call| call.call_id).collect();
    let mut outcomes = Vec::new();
    for call in calls {
        outcomes.push(env.router.invoke_direct(call).await.unwrap());
    }

    let recorded = env.proxy.recorded();
    assert_eq!(recorded.len(), 3);
    let token = env.svc_token();
    assert!(token.starts_with("gst1_"));
    let day = Timestamp::from_millis(START_MS).to_date();
    let ledgers = [
        DELIVERY.to_owned(),
        format!("{CONVERSATION}:{day}"),
        "unattributed".to_owned(),
    ];
    for ((request, ledger), call_id) in recorded.iter().zip(&ledgers).zip(&ids) {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, format!("/s/{token}/anthropic/v1/messages"));
        assert_eq!(request.headers["x-gantry-ledger"], *ledger);
        assert_eq!(request.headers["x-gantry-tier"], "utility");
        assert_eq!(request.headers["x-gantry-call-id"], call_id.to_string());
        assert_eq!(request.headers["anthropic-version"], "2026-10-21");
        for banned in ["x-api-key", "authorization", "anthropic-api-key"] {
            assert!(!request.headers.contains_key(banned), "{banned}");
        }
        let body = request.json();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        for sampling in ["temperature", "top_p", "top_k"] {
            assert!(body.get(sampling).is_none());
        }
        assert_eq!(body["model"], "utility-model");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        // 请求只到达回环代理：Host 是 127.0.0.1，从不是提供方主机。
        assert!(request.headers["host"].starts_with("127.0.0.1:"));
    }
    // EgressStub 数据面逐请求计量了三次。
    assert_eq!(env.stub.usage_records().len(), 3);

    // 交付与会话各记一条；未归属调用不记账（event-data §2.2.6.1）。
    let charges = env.charges().await;
    assert_eq!(charges.len(), 2);
    assert!(outcomes[0].charged_seq.is_some());
    assert!(outcomes[1].charged_seq.is_some());
    assert_eq!(outcomes[2].charged_seq, None);
    assert_eq!(outcomes[2].usage_source, UsageSource::Metered);
}

#[tokio::test(flavor = "multi_thread")]
async fn intent_uses_planner_tier_model_and_effort() {
    let env = env().await;
    env.proxy.push(ok(
        r#"{"kind":"intent","intent":"query","needs_code":false}"#,
    ));
    let mut call = env.call(Attribution::Conversation {
        conversation_id: conversation(),
        purpose: "intent".into(),
    });
    call.role = Role::Intent;
    call.schema = "gantry.worker.output.intent/1".into();
    let outcome = env.router.invoke_direct(call).await.unwrap();
    assert_eq!(outcome.output["intent"], "query");
    assert_eq!(outcome.served_by.model, "planner-model");
    assert_eq!(outcome.served_by.effort, Some(Effort::High));
    let request = &env.proxy.recorded()[0];
    assert_eq!(request.headers["x-gantry-tier"], "planner");
    assert_eq!(request.json()["output_config"]["effort"], "high");
    let charges = env.charges().await;
    assert_eq!(
        charges[0].1.tier,
        Some(gantry_core::entities::Tier::Planner)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_svc_token_is_rotated_once_and_the_call_succeeds() {
    let env = env().await;
    env.router.invoke_direct(env.aux()).await.unwrap();
    let first_token = env.svc_token();
    // 另一处轮换使路由持有的令牌失效（模拟过期 / 吊销）。
    env.stub.rotate_service_token(&ctx()).await.unwrap();

    let outcome = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(outcome.tries, 2, "rejected try + retry on the new token");
    let recorded = env.proxy.recorded();
    assert_eq!(recorded.len(), 3);
    assert!(recorded[1].path.contains(&first_token));
    assert!(!recorded[2].path.contains(&first_token));
    assert!(env.stub.calls().iter().any(|call| matches!(
        call,
        gantry_fakes::egress::EgressCall::RotateServiceToken { .. }
    )));
    assert_eq!(env.charges().await.len(), 2);
}

// ---------------------------------------------------------------------------
// 对账
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn reconciliation_backfills_a_lost_charge_exactly_once() {
    let env = env().await;
    env.router.invoke_direct(env.aux()).await.unwrap();
    env.router.invoke_direct(env.aux()).await.unwrap();
    // 第三次：Egress 已计量，但调用方在响应前放弃（模拟崩溃），没有记账。
    env.proxy.push(Action::Forward {
        body: anthropic_body(CLASSIFY_OK, "end_turn", (1, 1, 0, 0)),
        delay: Duration::from_millis(1500),
    });
    let third = env.aux();
    let third_id = third.call_id;
    let cancelled =
        tokio::time::timeout(Duration::from_millis(300), env.router.invoke_direct(third)).await;
    assert!(cancelled.is_err());
    assert_eq!(env.stub.usage_records().len(), 3);
    assert_eq!(env.charges().await.len(), 2);

    let svc_id = match &env.stub.usage_records()[0].caller {
        gantry_core::egress::UsageCaller::Svc { svc_token_id, .. } => svc_token_id.clone(),
        other => panic!("unexpected caller {other:?}"),
    };
    env.router.rotate_service_token().await.unwrap();
    let charges = env.charges().await;
    assert_eq!(charges.len(), 3);
    assert!(charges.iter().any(|(_, data)| matches!(
        data.source,
        gantry_core::events::BudgetChargeSource::DirectCall { call_id } if call_id == third_id
    )));

    // 重复对账不重复补记。
    assert_eq!(env.router.reconcile_svc_tokens(&[svc_id]).await.unwrap(), 0);
    assert_eq!(env.charges().await.len(), 3);
}

// ---------------------------------------------------------------------------
// 无 key
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn tokens_and_auth_never_appear_in_outcomes_or_errors() {
    let env = env().await;
    let outcome = env.router.invoke_direct(env.aux()).await.unwrap();
    let token = env.svc_token();
    env.proxy.push(status(400));
    let error = env.router.invoke_direct(env.aux()).await.unwrap_err();
    assert!(matches!(error, ModelsError::InvalidRequest { status: 400 }));
    for text in [
        format!("{outcome:?}"),
        format!("{error:?}"),
        error.to_string(),
        format!("{:?}", env.router.status()),
    ] {
        assert!(!text.contains(&token), "{text}");
        assert!(!text.contains("gst1_"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("x-api-key"), "{text}");
        // 提示词与材料正文不进入结果与错误。
        assert!(!text.contains("error: build failed"), "{text}");
    }
}

// ---------------------------------------------------------------------------
// 输出校验
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn invalid_output_is_retried_with_error_code_only() {
    let env = env().await;
    env.proxy.push(ok("garbage-xyz"));
    env.proxy.push(ok(CLASSIFY_OK));
    let outcome = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(outcome.tries, 2);
    let recorded = env.proxy.recorded();
    let retry_system = recorded[1].json()["system"].as_str().unwrap().to_owned();
    assert!(retry_system.contains("invalid_json"));
    assert!(!retry_system.contains("garbage-xyz"));
    assert!(!recorded[1].json()["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("garbage-xyz"));
    // 输出无效不是上游故障，不退避。
    assert!(env.clock.sleeps().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn exhausting_attempts_on_invalid_output_is_schema_invalid() {
    let env = env().await;
    env.proxy.set_default(ok(
        r#"{"kind":"classify","target":"ci_classification","text":"x"}"#,
    ));
    let error = env.router.invoke_direct(env.aux()).await.unwrap_err();
    match error {
        ModelsError::SchemaInvalid { last_error } => {
            assert!(last_error.starts_with("inconsistent:"), "{last_error}")
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(env.proxy.recorded().len(), 3);
    // 失败的调用仍按 Egress 计量记账。
    assert_eq!(env.charges().await.len(), 1);
}

// ---------------------------------------------------------------------------
// 重试与期限
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn transient_failures_retry_with_exponential_backoff() {
    let env = env().await;
    env.proxy.push(status(503));
    env.proxy.push(Action::Respond {
        status: 429,
        body: "{}".into(),
        headers: vec![("retry-after".into(), "0".into())],
        delay: Duration::ZERO,
    });
    env.proxy.push(ok(CLASSIFY_OK));
    let outcome = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(outcome.tries, 3);
    let sleeps = env.clock.sleeps();
    assert_eq!(sleeps.len(), 2);
    // backoff_ms 默认 1000：第 n 次重试等待 1000·2^(n-1) 毫秒，加 0–25% 抖动。
    assert!(sleeps[0] >= Duration::from_millis(1000) && sleeps[0] <= Duration::from_millis(1250));
    assert!(sleeps[1] >= Duration::from_millis(2000) && sleeps[1] <= Duration::from_millis(2500));
}

#[tokio::test(flavor = "multi_thread")]
async fn exhausted_transient_failures_are_unavailable() {
    let env = env().await;
    env.proxy.set_default(status(503));
    let error = env.router.invoke_direct(env.aux()).await.unwrap_err();
    assert_eq!(error.code(), "unavailable");
    assert_eq!(env.proxy.recorded().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_retryable_outcomes_send_exactly_one_request() {
    let cases: Vec<(Action, &str)> = vec![
        (status(400), "invalid_request"),
        (status(404), "unavailable"),
        (
            Action::Forward {
                body: json!({
                    "content": [],
                    "stop_reason": "refusal",
                    "stop_details": {"type": "refusal", "category": "cyber"},
                    "usage": {"input_tokens": 5, "output_tokens": 0}
                })
                .to_string(),
                delay: Duration::ZERO,
            },
            "content_refused",
        ),
        (
            Action::Forward {
                body: anthropic_body("{\"kind\":", "max_tokens", (5, 5, 0, 0)),
                delay: Duration::ZERO,
            },
            "output_truncated",
        ),
    ];
    for (action, code) in cases {
        let env = env().await;
        env.proxy.push(action);
        let error = env.router.invoke_direct(env.aux()).await.unwrap_err();
        assert_eq!(error.code(), code);
        if let ModelsError::ContentRefused { category } = &error {
            assert_eq!(category.as_deref(), Some("cyber"));
        }
        assert_eq!(env.proxy.recorded().len(), 1, "{code}");
        assert!(env.clock.sleeps().is_empty(), "{code}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn total_timeout_stops_retrying_immediately() {
    let mut models = global_models();
    models["retry"] = json!({"max_attempts": 5, "backoff_ms": 60000});
    let env = env_with(models).await;
    env.proxy.set_default(status(503));
    let error = env.router.invoke_direct(env.aux()).await.unwrap_err();
    assert_eq!(error, ModelsError::Timeout);
    // utility total_timeout_s = 120：第二次失败后的 120 s 退避越过期限，不再发送。
    assert_eq!(env.proxy.recorded().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_deadline_only_tightens() {
    let env = env().await;
    env.proxy.set_default(status(503));
    let mut call = env.aux();
    call.deadline = Some(Duration::from_millis(1500));
    let error = env.router.invoke_direct(call).await.unwrap_err();
    assert_eq!(error, ModelsError::Timeout);
    assert_eq!(env.proxy.recorded().len(), 2);

    // 比 total_timeout_s 更长的 deadline 不放宽：仍按 max_attempts 收尾。
    let env = self::env().await;
    env.proxy.set_default(status(503));
    let mut call = env.aux();
    call.deadline = Some(Duration::from_secs(100_000));
    let error = env.router.invoke_direct(call).await.unwrap_err();
    assert_eq!(error.code(), "unavailable");
    assert_eq!(env.proxy.recorded().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_responses_time_out_per_request() {
    let mut models = global_models();
    models["utility"]["request_timeout_s"] = json!(1);
    models["utility"]["total_timeout_s"] = json!(2);
    let env = env_with(models).await;
    env.proxy.set_default(Action::Forward {
        body: anthropic_body(CLASSIFY_OK, "end_turn", (1, 1, 0, 0)),
        delay: Duration::from_secs(3),
    });
    let started = std::time::Instant::now();
    let error = env.router.invoke_direct(env.aux()).await.unwrap_err();
    assert_eq!(error, ModelsError::Timeout);
    assert!(started.elapsed() < Duration::from_millis(2500));
}

// ---------------------------------------------------------------------------
// 用量
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn metered_usage_wins_over_provider_reported() {
    let env = env().await;
    let outcome = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(outcome.usage_source, UsageSource::Metered);
    let records = env.stub.usage_records();
    assert_eq!(records.len(), 1);
    assert_eq!(outcome.usage, records[0].usage);
    assert_eq!(outcome.weighted_tokens, records[0].weighted_tokens);
    // 桩按字节计量，与提供方自报（100 / 20）偏离超过 10%：审计警告。
    assert!(outcome
        .warnings
        .iter()
        .any(|warning| matches!(warning, UsageWarning::Divergence { .. })));

    let charges = env.charges().await;
    let (subject, data) = &charges[0];
    assert_eq!(subject, DELIVERY);
    assert_eq!(data.delta, outcome.weighted_tokens);
    assert_eq!(data.used_after, outcome.weighted_tokens);
    assert_eq!(data.usage, Some(outcome.usage));
    // 四类 token 与 requests 分别记录。
    assert_eq!(outcome.usage.requests, 1);
    assert!(outcome.usage.input_tokens > 0 && outcome.usage.output_tokens > 0);

    // 同一台账同一 tier 的 used_after 累加。
    let second = env.router.invoke_direct(env.aux()).await.unwrap();
    let charges = env.charges().await;
    assert_eq!(
        charges[1].1.used_after,
        outcome.weighted_tokens + second.weighted_tokens
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_token_weights_apply_to_the_next_call() {
    let env = env().await;
    let before = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(
        before.weighted_tokens,
        TokenWeights::default().weighted(&before.usage)
    );

    let weights = TokenWeights {
        input: 2.0,
        output: 10.0,
        cache_read: 0.5,
        cache_write: 3.0,
    };
    put_settings(
        &env.store,
        None,
        SettingsResource::Levels,
        json!({"token_weights": {"input": 2.0, "output": 10.0, "cache_read": 0.5, "cache_write": 3.0}}),
    )
    .await;
    // engine 把 levels.token_weights 推给 Egress（同一组权重，P-70）。
    let mut view = default_config(2);
    view.token_weights = weights;
    env.egress.apply_config(view, &ctx()).await.unwrap();

    let after = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(after.weighted_tokens, weights.weighted(&after.usage));
    assert_ne!(
        after.weighted_tokens,
        TokenWeights::default().weighted(&after.usage)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_metering_falls_back_to_provider_reported_with_warning() {
    let env = env().await;
    env.egress.fail_terminal.store(true, Ordering::SeqCst);
    let outcome = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(outcome.usage_source, UsageSource::ProviderReported);
    assert_eq!(outcome.usage.input_tokens, 100);
    assert_eq!(outcome.usage.output_tokens, 20);
    assert_eq!(
        outcome.weighted_tokens,
        TokenWeights::default().weighted(&outcome.usage)
    );
    assert!(matches!(
        outcome.warnings.as_slice(),
        [UsageWarning::UsageUnavailable { .. }]
    ));
    assert_eq!(outcome.charged_seq, None);
    assert!(env.charges().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn incomplete_terminal_is_not_settled() {
    let env = env().await;
    let call = env.aux();
    let call_id = call.call_id;
    // 中断标记只能在关闭前打上：先让 Egress 计量，再在关闭前置位。
    env.proxy.push(Action::Forward {
        body: anthropic_body(CLASSIFY_OK, "end_turn", (100, 20, 0, 0)),
        delay: Duration::from_millis(200),
    });
    let router = env.router.clone();
    let task = tokio::spawn(async move { router.invoke_direct(call).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    env.stub.interrupt_call_records(&call_id);
    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome.usage_source, UsageSource::ProviderReported);
    assert!(matches!(
        outcome.warnings.as_slice(),
        [UsageWarning::NotSettled { .. }]
    ));
    assert!(env.charges().await.is_empty());
}

// ---------------------------------------------------------------------------
// 会话台账
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn conversation_ledger_charges_and_enforces_the_daily_cap() {
    let env = env().await;
    put_settings(
        &env.store,
        None,
        SettingsResource::Levels,
        json!({"none": {"daily_tokens": 1}}),
    )
    .await;
    let conversation_call = || {
        env.call(Attribution::Conversation {
            conversation_id: conversation(),
            purpose: "status_answer".into(),
        })
    };
    let first = env.router.invoke_direct(conversation_call()).await.unwrap();
    assert!(first.weighted_tokens >= 1);
    let charges = env.charges().await;
    assert_eq!(charges.len(), 1);
    let (subject, data) = &charges[0];
    assert_eq!(subject, CONVERSATION);
    let ledger = serde_json::to_value(&data.ledger).unwrap();
    assert_eq!(ledger["subject"], "conversation");
    assert_eq!(ledger["id"], CONVERSATION);
    assert_eq!(
        ledger["day"],
        Timestamp::from_millis(START_MS).to_date().to_string()
    );
    assert!(ledger.get("delivery_id").is_none());

    let sent_before = env.proxy.recorded().len();
    let error = env
        .router
        .invoke_direct(conversation_call())
        .await
        .unwrap_err();
    assert!(matches!(error, ModelsError::BudgetExhausted { cap: 1, .. }));
    assert_eq!(
        env.proxy.recorded().len(),
        sent_before,
        "no request after cap"
    );

    // 交付台账不受会话上限影响；未归属调用不追加任何记账事件。
    env.router.invoke_direct(env.aux()).await.unwrap();
    env.router
        .invoke_direct(env.call(Attribution::Unattributed {
            purpose: "x".into(),
        }))
        .await
        .unwrap();
    assert_eq!(env.charges().await.len(), 2);
}

// ---------------------------------------------------------------------------
// 热加载
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_call_keeps_its_snapshot_and_next_call_uses_new_config() {
    let env = env().await;
    env.proxy.push(Action::Forward {
        body: anthropic_body(CLASSIFY_OK, "end_turn", (1, 1, 0, 0)),
        delay: Duration::from_millis(300),
    });
    let router = env.router.clone();
    let call = env.aux();
    let task = tokio::spawn(async move { router.invoke_direct(call).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut models = global_models();
    models["utility"]["model"] = json!("utility-model-2");
    put_settings(&env.store, None, SettingsResource::Models, models).await;

    let old = task.await.unwrap().unwrap();
    assert_eq!(old.served_by.model, "utility-model");
    let new = env.router.invoke_direct(env.aux()).await.unwrap();
    assert_eq!(new.served_by.model, "utility-model-2");
    let recorded = env.proxy.recorded();
    assert_eq!(recorded[0].json()["model"], "utility-model");
    assert_eq!(recorded[1].json()["model"], "utility-model-2");
    assert!(env.router.status().applied_rev.as_u64() > 0);
}

// ---------------------------------------------------------------------------
// 并发闸
// ---------------------------------------------------------------------------

async fn run_concurrent(
    env: &Env,
    count: usize,
    deadline: Option<Duration>,
) -> Vec<Result<DirectOutcome, ModelsError>> {
    let mut tasks = Vec::new();
    for _ in 0..count {
        let router = env.router.clone();
        let mut call = env.aux();
        call.deadline = deadline;
        tasks.push(tokio::spawn(
            async move { router.invoke_direct(call).await },
        ));
    }
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }
    results
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_gate_caps_in_flight_requests() {
    let env = env().await;
    env.proxy.set_default(Action::Forward {
        body: anthropic_body(CLASSIFY_OK, "end_turn", (1, 1, 0, 0)),
        delay: Duration::from_millis(300),
    });
    let results = run_concurrent(&env, PROVIDER_MAX_INFLIGHT + 4, None).await;
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(env.proxy.max_concurrency(), PROVIDER_MAX_INFLIGHT);
}

#[tokio::test(flavor = "multi_thread")]
async fn queueing_at_the_gate_counts_toward_the_deadline() {
    let env = env().await;
    env.proxy.set_default(Action::Forward {
        body: anthropic_body(CLASSIFY_OK, "end_turn", (1, 1, 0, 0)),
        delay: Duration::from_millis(1000),
    });
    let results = run_concurrent(
        &env,
        PROVIDER_MAX_INFLIGHT + 4,
        Some(Duration::from_millis(1500)),
    )
    .await;
    let ok = results.iter().filter(|result| result.is_ok()).count();
    let timed_out = results
        .iter()
        .filter(|result| matches!(result, Err(ModelsError::Timeout)))
        .count();
    assert_eq!(ok, PROVIDER_MAX_INFLIGHT);
    assert_eq!(timed_out, 4);
    assert!(env.proxy.max_concurrency() <= PROVIDER_MAX_INFLIGHT);
}

// ---------------------------------------------------------------------------
// 角色约束与不降级
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_roles_and_foreign_schemas_are_rejected_before_sending() {
    let env = env().await;
    let mut call = env.aux();
    call.role = Role::Build;
    assert_eq!(
        env.router.invoke_direct(call).await.unwrap_err().code(),
        "invalid_role"
    );
    let mut call = env.aux();
    call.schema = "gantry.worker.output.intent/1".into();
    assert!(matches!(
        env.router.invoke_direct(call).await.unwrap_err(),
        ModelsError::InvalidSchema { .. }
    ));
    assert!(env.proxy.recorded().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_effort_fails_closed_without_downgrade() {
    let mut models = global_models();
    models["utility"]["reasoning_effort"] = json!("medium");
    let env = env_with(models).await;
    let error = env.router.invoke_direct(env.aux()).await.unwrap_err();
    assert_eq!(
        error,
        ModelsError::EffortUnsupported {
            model: "utility-model".into(),
            effort: Effort::Medium,
        }
    );
    assert!(env.proxy.recorded().is_empty());
}
