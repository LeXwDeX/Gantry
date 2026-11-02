//! `assembly::build_models` 的本机回环集成测试（#105 models 片）。
//!
//! 只验证独立构造与真实 `ModelRouter` 路径：设置来自真实内存 store，模型请求经
//! `EgressStub` 回环数据面计量并写入真实 `budget.charged`。测试装配仍用
//! `FakeModelProvider`，`assemble_real` 仍拒绝启动；这组用例不代表真实模型 /
//! Egress 环境验收。
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use gantry_core::egress::{LedgerRef, UsageCaller};
use gantry_core::entities::{Actor, ActorKind, Role, SettingsResource, SettingsScope, Tier};
use gantry_core::events::{BudgetChargeSource, BudgetChargedData, EventType, NewEvent, Subject};
use gantry_core::ids::{DeliveryId, IdGen, ProjectId};
use gantry_core::level::Effort;
use gantry_core::ports::egress::EgressControl;
use gantry_core::ports::models::{CallId, UsageSource};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_fakes::egress::{EgressCall, EgressStub};
use gantry_models::prompt::TrustedText;
use gantry_models::{Attribution, DirectCall, EffortTable, ModelRouter, RouterClock, TokioClock};
use gantry_store::{Store, StoreDeps};
use serde_json::{json, Value};

use gantryd::assembly::build_models;

const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const START_MS: i64 = 1_793_000_000_000;
const CLASSIFY_SCHEMA: &str = "gantry.worker.output.classify/1";
const INTENT_SCHEMA: &str = "gantry.worker.output.intent/1";
const CLASSIFY_OK: &str = r#"{"kind":"classify","target":"event_line","text":"ok"}"#;
const INTENT_OK: &str = r#"{"kind":"intent","intent":"query","needs_code":false}"#;

fn project() -> ProjectId {
    PROJECT.parse().unwrap()
}

fn delivery() -> DeliveryId {
    DELIVERY.parse().unwrap()
}

/// 注入的能力表：模型名与测试设置一致，故意不含未配置 / 未知模型。
fn efforts() -> EffortTable {
    EffortTable::new()
        .with_model("utility-from-settings", [(Effort::Low, "low")])
        .with_model("planner-from-settings", [(Effort::High, "high")])
}

fn utility_settings() -> Value {
    json!({"provider": "anthropic", "model": "utility-from-settings"})
}

fn planner_settings() -> Value {
    json!({"provider": "anthropic", "model": "planner-from-settings"})
}

fn models_doc(utility: Value, planner: Value) -> Value {
    json!({
        "providers": {
            "anthropic": {
                "kind": "anthropic",
                "base_url": "https://api.anthropic.com",
                "credential": {"type": "api_key", "secret_ref": "model.anthropic.main"}
            }
        },
        "utility": utility,
        "planner": planner,
        "worker": {"provider": "anthropic", "model": "worker-model"}
    })
}

fn test_store() -> (Store, Arc<IdGen>, Arc<SeqEntropy>) {
    let clock = Arc::new(FakeClock::new(START_MS));
    let entropy = Arc::new(SeqEntropy::new(7));
    let ids = Arc::new(IdGen::new(clock.clone(), entropy.clone()));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    })
    .unwrap();
    (store, ids, entropy)
}

/// 追加一条 `settings.updated`；`value` 是全局 models 的完整存储文档。
async fn put_models(store: &Store, value: Value) {
    let base_rev = store
        .snapshot_sync()
        .unwrap()
        .settings_current_for(&SettingsScope::Global, None, SettingsResource::Models)
        .rev;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::parse("settings").unwrap(),
        project_id: None,
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data: json!({
            "scope": "global",
            "resource": "models",
            "schema_version": 1,
            "base_rev": base_rev.as_u64(),
            "value": value,
            "relaxes": [],
            "secret_changes": [],
        }),
        dedup_key: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// 通过有效夹具建立交付；`budget.charged` 只由记账路径写入。
async fn seed_delivery(store: &Store) {
    let cases: Value =
        serde_json::from_str(include_str!("../../gantry-core/fixtures/events/cases.json")).unwrap();
    let mut tx = store.begin_write().await.unwrap();
    tx.append(NewEvent {
        ty: EventType::DeliveryCreated,
        subject: Subject::parse(DELIVERY).unwrap(),
        project_id: Some(project()),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data: cases["valid"]["delivery.created"].clone(),
        dedup_key: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

struct Env {
    router: ModelRouter,
    stub: Arc<EgressStub>,
    store: Store,
    ids: Arc<IdGen>,
}

/// 用真实内存 store、真实 `build_models` 与回环 Egress 构造独立测试环境。
async fn env(utility: Value, planner: Value) -> Env {
    let (store, ids, entropy) = test_store();
    put_models(&store, models_doc(utility, planner)).await;
    seed_delivery(&store).await;
    let stub = Arc::new(EgressStub::with_default_config());
    let egress: Arc<dyn EgressControl> = stub.clone();
    let clock: Arc<dyn RouterClock> = Arc::new(TokioClock::default());
    let router = build_models(&store, egress, ids.clone(), entropy, clock, efforts());
    Env {
        router,
        stub,
        store,
        ids,
    }
}

fn classify_call(ids: &IdGen) -> DirectCall {
    DirectCall {
        call_id: ids.next_ulid().unwrap(),
        project_id: project(),
        role: Role::Classify,
        attribution: Attribution::Aux {
            delivery_id: delivery(),
            purpose: "classify".into(),
        },
        level: None,
        critical: false,
        instructions: TrustedText::from_static("Classify the CI event in one line."),
        materials: vec![],
        schema: CLASSIFY_SCHEMA.into(),
        deadline: None,
    }
}

fn classify_body() -> String {
    anthropic_body(CLASSIFY_OK)
}

fn intent_body() -> String {
    anthropic_body(INTENT_OK)
}

fn anthropic_body(text: &str) -> String {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0
        }
    })
    .to_string()
}

async fn charges(store: &Store) -> Vec<BudgetChargedData> {
    store
        .events()
        .by_subject(&Subject::parse(DELIVERY).unwrap())
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.ty == EventType::BudgetCharged)
        .map(|event| serde_json::from_value(event.data).unwrap())
        .collect()
}

fn assert_no_data_plane(stub: &EgressStub) {
    assert!(stub.usage_records().is_empty());
    assert!(!stub
        .calls()
        .iter()
        .any(|call| matches!(call, EgressCall::DataPlane { .. })));
}

/// 正向路径确实经过 Egress 的 svc 数据面，且只调用一次。
fn assert_svc_data_plane(stub: &EgressStub, call_id: CallId) {
    let calls = stub.calls();
    let data_plane: Vec<_> = calls
        .iter()
        .filter_map(|call| match call {
            EgressCall::DataPlane {
                kind,
                subject,
                provider,
                status,
            } => Some((*kind, subject.as_str(), provider.as_str(), *status)),
            _ => None,
        })
        .collect();
    assert_eq!(data_plane.len(), 1, "exactly one data-plane call");
    let (kind, subject, provider, status) = data_plane[0];
    assert_eq!(kind, "svc");
    assert_eq!(subject, call_id.to_string());
    assert_eq!(provider, "anthropic");
    assert_eq!(status, 200);
}

/// 失败闭合：没有数据面请求，也没有 `budget.charged`。
async fn assert_no_request_or_charge(stub: &EgressStub, store: &Store) {
    assert_no_data_plane(stub);
    assert!(charges(store).await.is_empty(), "no budget.charged");
}

#[tokio::test(flavor = "multi_thread")]
async fn build_models_classify_charges_the_real_delivery_ledger() {
    let env = env(utility_settings(), planner_settings()).await;
    env.stub.set_data_plane_response(classify_body());
    let call = classify_call(&env.ids);
    let call_id = call.call_id;
    let outcome = env.router.invoke_direct(call).await.unwrap();

    assert_eq!(outcome.output["kind"], "classify");
    assert_eq!(outcome.served_by.model, "utility-from-settings");
    assert_eq!(outcome.served_by.effort, Some(Effort::Low));
    assert_eq!(outcome.usage_source, UsageSource::Metered);
    assert!(
        outcome.charged_seq.is_some(),
        "budget.charged must be written"
    );
    assert!(env.router.status().applied_rev.as_u64() > 0);
    assert_svc_data_plane(&env.stub, call_id);

    let records = env.stub.usage_records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].model, "utility-from-settings");
    assert_eq!(records[0].tier, Tier::Utility);
    assert_eq!(
        records[0].ledger,
        Some(LedgerRef::Delivery { id: delivery() })
    );
    assert_eq!(outcome.usage, records[0].usage);
    assert_eq!(outcome.weighted_tokens, records[0].weighted_tokens);
    match &records[0].caller {
        UsageCaller::Svc {
            call_id: recorded, ..
        } => assert_eq!(*recorded, call_id),
        other => panic!("expected svc usage, got {other:?}"),
    }

    let charges = charges(&env.store).await;
    assert_eq!(charges.len(), 1);
    let charge = &charges[0];
    assert_eq!(charge.ledger, LedgerRef::Delivery { id: delivery() });
    assert_eq!(charge.tier, Some(Tier::Utility));
    assert_eq!(charge.delta, outcome.weighted_tokens);
    assert_eq!(charge.used_after, outcome.weighted_tokens);
    assert!(matches!(
        charge.source,
        BudgetChargeSource::DirectCall { call_id: id } if id == call_id
    ));
    assert_eq!(charge.usage, Some(outcome.usage));
}

#[tokio::test(flavor = "multi_thread")]
async fn build_models_planner_uses_planner_tier_and_effort() {
    let env = env(utility_settings(), planner_settings()).await;
    env.stub.set_data_plane_response(intent_body());
    let mut call = classify_call(&env.ids);
    call.role = Role::Intent;
    call.schema = INTENT_SCHEMA.into();
    let call_id = call.call_id;
    let outcome = env.router.invoke_direct(call).await.unwrap();

    assert_eq!(outcome.output["intent"], "query");
    assert_eq!(outcome.served_by.model, "planner-from-settings");
    assert_eq!(outcome.served_by.effort, Some(Effort::High));
    assert_svc_data_plane(&env.stub, call_id);
    let records = env.stub.usage_records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].tier, Tier::Planner);
    assert_eq!(
        records[0].ledger,
        Some(LedgerRef::Delivery { id: delivery() })
    );
    let charges = charges(&env.store).await;
    assert_eq!(charges.len(), 1);
    assert_eq!(charges[0].tier, Some(Tier::Planner));
}

#[tokio::test(flavor = "multi_thread")]
async fn build_models_fails_closed_before_sending_for_unknown_effort_or_model() {
    // 未知 effort：设置为 medium，注入的能力表只有 low。
    let unknown_effort = env(
        json!({
            "provider": "anthropic",
            "model": "utility-from-settings",
            "reasoning_effort": "medium"
        }),
        planner_settings(),
    )
    .await;
    let error = unknown_effort
        .router
        .invoke_direct(classify_call(&unknown_effort.ids))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effort_unsupported");
    assert_no_request_or_charge(&unknown_effort.stub, &unknown_effort.store).await;

    // 未知模型：模型不在注入的能力表中。
    let unknown_model = env(
        json!({"provider": "anthropic", "model": "model-not-in-effort-table"}),
        planner_settings(),
    )
    .await;
    let error = unknown_model
        .router
        .invoke_direct(classify_call(&unknown_model.ids))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effort_unsupported");
    assert_no_request_or_charge(&unknown_model.stub, &unknown_model.store).await;

    // 未配置模型：视图解析失败，不产生请求。
    let missing_model = env(json!({"provider": "anthropic"}), planner_settings()).await;
    let error = missing_model
        .router
        .invoke_direct(classify_call(&missing_model.ids))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "not_configured");
    assert_no_request_or_charge(&missing_model.stub, &missing_model.store).await;
}
