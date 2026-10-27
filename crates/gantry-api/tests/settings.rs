//! settings #30 #31（M1 只开放 models）#35 #36（本文 §12.2「settings」「分页 / 幂等 /
//! 并发控制」中的 settings 条目），对接真实 policy + pki。

mod support;

use bytes::Bytes;
use gantry_core::entities::{DevicePlatform, Seq};
use gantry_core::events::Subject;
use gantry_store::EventFilter;
use http::Method;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{json, Value};
use support::pki_env::{Paired, PkiEnv};
use support::{assert_problem, body, header, validate};

const SETTINGS: &str = include_str!("../../../fixtures/api/settings.json");
const PROBE: &str = "probe-secret-7f3a9c-DO-NOT-LEAK";

async fn send(env: &PkiEnv, head: gantry_api::RequestHead, body: &str) -> gantry_api::ApiResponse {
    env.core.handle(head, Bytes::from(body.to_owned())).await
}

/// fixtures 里的 models 文档，凭据带本次写入的明文值。
fn models_with_secret() -> Value {
    let mut doc: Value =
        serde_json::from_str::<Value>(SETTINGS).unwrap()["global"]["models"]["value"].clone();
    let cred = doc["providers"]["anthropic"]["credential"]
        .as_object_mut()
        .unwrap();
    cred.remove("present");
    cred.insert("value".into(), json!(PROBE));
    doc
}

fn models_without_secret() -> Value {
    let mut doc = models_with_secret();
    doc["providers"]["anthropic"]["credential"]
        .as_object_mut()
        .unwrap()
        .remove("value");
    doc
}

fn put(d: &Paired, path: &str, etag: Option<&str>) -> gantry_api::RequestHead {
    let head = d
        .req(Method::PUT, path)
        .header("content-type", "application/json");
    match etag {
        Some(e) => head.header("if-match", e),
        None => head,
    }
}

async fn sign_approval(
    env: &PkiEnv,
    approver: &Paired,
    op: &Paired,
    signing: &Value,
    key: &str,
) -> String {
    let resp = send(
        env,
        op.req(Method::POST, "/v1/approvals")
            .header("content-type", "application/json")
            .header("idempotency-key", key),
        &signing.to_string(),
    )
    .await;
    assert_eq!(resp.status(), 201, "{:?}", body(&resp));
    let id = body(&resp)["id"].as_str().unwrap().to_owned();
    let resp = send(
        env,
        approver.req(Method::GET, &format!("/v1/approvals/{id}/challenge")),
        "",
    )
    .await;
    let c = body(&resp);
    let payload: gantry_core::canon::ApprovalPayload =
        serde_json::from_value(c["payload"].clone()).unwrap();
    let sig: Signature =
        SigningKey::from(approver.keys.approval.clone()).sign(&payload.signing_bytes().unwrap());
    use base64::Engine as _;
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes());
    let grant = json!({ "nonce": c["payload"]["nonce"], "signature": sig }).to_string();
    let resp = send(
        env,
        approver
            .req(Method::POST, &format!("/v1/approvals/{id}/grant"))
            .header("content-type", "application/json")
            .header("idempotency-key", &format!("{key}-g")),
        &grant,
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    id
}

async fn all_events(env: &PkiEnv) -> String {
    let page = env
        .store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 200)
        .await
        .unwrap();
    serde_json::to_string(&page.events).unwrap()
}

#[tokio::test]
async fn reads_etags_catalog_and_status() {
    let env = PkiEnv::new().await;
    let op = env.pair(DevicePlatform::Macos, "a").await;
    for r in [
        "models", "levels", "policies", "guardian", "sandbox", "egress", "notify", "github",
    ] {
        let resp = send(&env, op.req(Method::GET, &format!("/v1/settings/{r}")), "").await;
        assert_eq!(resp.status(), 200, "{r}");
        validate("GlobalSettingsResponse", &body(&resp));
        assert_eq!(header(&resp, "etag"), Some("\"0\""));
    }
    assert_problem(
        &send(&env, op.req(Method::GET, "/v1/settings/nope"), "").await,
        404,
        "not_found",
    );
    let resp = send(
        &env,
        op.req(Method::GET, "/v1/settings/models")
            .header("if-none-match", "\"0\""),
        "",
    )
    .await;
    assert_eq!(resp.status(), 304);
    assert!(resp.body().as_bytes().is_empty());
    let resp = send(&env, op.req(Method::GET, "/v1/settings/catalog"), "").await;
    validate("SettingsCatalog", &body(&resp));
    let resp = send(&env, op.req(Method::GET, "/v1/settings/status"), "").await;
    validate("SettingsStatus", &body(&resp));
    // M1 只开放 models 写入
    let resp = send(
        &env,
        put(&op, "/v1/settings/levels", Some("\"0\"")),
        r#"{"value":{}}"#,
    )
    .await;
    assert_problem(&resp, 404, "not_found");
}

#[tokio::test]
async fn preconditions_dry_run_signature_and_role() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    let body_with = json!({ "value": models_with_secret() }).to_string();
    // If-Match：缺 → 428；旧 → 412 且头带当前 ETag
    assert_problem(
        &send(
            &env,
            put(&approver, "/v1/settings/models", None),
            &body_with,
        )
        .await,
        428,
        "precondition_required",
    );
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"5\"")),
        &body_with,
    )
    .await;
    assert_problem(&resp, 412, "precondition_failed");
    assert_eq!(header(&resp, "etag"), Some("\"0\""));
    // dry_run：不落库、不发事件，缺签名头也返回 signing
    let before = env.store.events().head().await.unwrap();
    let resp = send(
        &env,
        put(&op, "/v1/settings/models?dry_run=true", Some("\"0\"")),
        &body_with,
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    let dry = body(&resp);
    validate("DryRunResponse", &dry);
    assert_eq!(dry["valid"], true);
    assert_eq!(dry["required_role"], "approver");
    assert_eq!(dry["requires_signature"], true);
    assert_eq!(dry["signing"]["action"], "settings_update");
    assert_eq!(dry["signing"]["target"]["base_rev"], 0);
    assert_eq!(env.store.events().head().await.unwrap(), before);
    // 带 secret value、不带审批：operator 先 403 forbidden_role（角色先于签名），approver 403 signature_required
    let resp = send(
        &env,
        put(&op, "/v1/settings/models", Some("\"0\"")),
        &body_with,
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 403, "forbidden_role")["required_role"],
        "approver"
    );
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")),
        &body_with,
    )
    .await;
    let b = assert_problem(&resp, 403, "signature_required");
    assert_eq!(b["signing"], dry["signing"]);
    // 语义校验：null 清空 → secret_clear_in_use；未带值且不存在 → secret_ref_missing（指针相对 value）
    let mut cleared = models_without_secret();
    cleared["providers"]["anthropic"]["credential"]["value"] = Value::Null;
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")),
        &json!({"value": cleared}).to_string(),
    )
    .await;
    let errs = assert_problem(&resp, 422, "validation_failed")["errors"].clone();
    assert!(
        errs.as_array()
            .unwrap()
            .iter()
            .any(|e| e["code"] == "secret_clear_in_use"
                && e["pointer"] == "/providers/anthropic/credential/value"),
        "{errs}"
    );
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")),
        &json!({"value": models_without_secret()}).to_string(),
    )
    .await;
    let errs = assert_problem(&resp, 422, "validation_failed")["errors"].clone();
    assert!(
        errs.as_array()
            .unwrap()
            .iter()
            .any(|e| e["code"] == "secret_ref_missing"),
        "{errs}"
    );
}

#[tokio::test]
async fn signed_credential_write_is_two_phase_and_never_echoes_the_value() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    let doc_a = json!({ "value": models_with_secret() }).to_string();
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")),
        &doc_a,
    )
    .await;
    let signing = assert_problem(&resp, 403, "signature_required")["signing"].clone();
    let approval = sign_approval(&env, &approver, &op, &signing, "settings-approval-01").await;

    // 为内容 A 批准、提交内容 B → 409 digest_mismatch，未写入、未暂存生效
    let mut other = models_with_secret();
    other["retry"]["max_attempts"] = json!(2);
    let doc_b = json!({ "value": other }).to_string();
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")).header("gantry-approval", &approval),
        &doc_b,
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 409, "approval_mismatch")["reason"],
        "digest_mismatch"
    );
    let resp = send(&env, approver.req(Method::GET, "/v1/settings/models"), "").await;
    assert_eq!(body(&resp)["rev"], 0);
    // 两阶段：事务回滚 → 暂存项被 discard，从未 activate
    use gantry_fakes::egress::EgressCall;
    let calls = env.egress.calls();
    assert!(calls
        .iter()
        .any(|c| matches!(c, EgressCall::DiscardSecret { .. })));
    assert!(!calls
        .iter()
        .any(|c| matches!(c, EgressCall::ActivateSecret { .. })));
    // 旧 If-Match 优先于 approval_mismatch
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"9\"")).header("gantry-approval", &approval),
        &doc_b,
    )
    .await;
    assert_problem(&resp, 412, "precondition_failed");

    // approver + 有效审批 → 200；ETag = rev = X-Event-Seq
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")).header("gantry-approval", &approval),
        &doc_a,
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    let b = body(&resp);
    validate("GlobalSettingsResponse", &b);
    let rev = b["rev"].as_u64().unwrap();
    assert_eq!(header(&resp, "etag").unwrap(), format!("\"{rev}\""));
    assert_eq!(header(&resp, "x-event-seq").unwrap(), rev.to_string());
    assert_eq!(
        b["value"]["providers"]["anthropic"]["credential"]["present"],
        true
    );
    // settings.updated：secret_changes 只有 {ref, op, version}
    let events = env
        .store
        .events()
        .by_subject(&Subject::settings())
        .await
        .unwrap();
    let data = &events.last().unwrap().data;
    assert_eq!(data["secret_changes"][0]["ref"], "model.anthropic.main");
    assert_eq!(data["secret_changes"][0]["op"], "set");
    assert!(data["secret_changes"][0]["version"].as_u64().unwrap() >= 1);
    // 同一审批第二次使用 → consumed
    let resp = send(
        &env,
        put(
            &approver,
            "/v1/settings/models",
            Some(&format!("\"{rev}\"")),
        )
        .header("gantry-approval", &approval),
        &doc_a,
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 409, "approval_mismatch")["reason"],
        "consumed"
    );
    // 探针：响应、事件日志里都没有明文
    let get = send(&env, op.req(Method::GET, "/v1/settings/models"), "").await;
    for raw in [
        String::from_utf8(get.body().as_bytes().to_vec()).unwrap(),
        all_events(&env).await,
    ] {
        assert!(!raw.contains(PROBE), "secret value leaked: {raw}");
    }
    // operator 原样重写（无放宽、无 secret 写入）→ 无需签名
    env.settings_ctx
        .existing
        .lock()
        .unwrap()
        .insert("model.anthropic.main".into());
    let resp = send(
        &env,
        put(&op, "/v1/settings/models", Some(&format!("\"{rev}\""))),
        &json!({"value": models_without_secret()}).to_string(),
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    // 并发保存：后到者 412
    let resp = send(
        &env,
        put(&op, "/v1/settings/models", Some(&format!("\"{rev}\""))),
        &json!({"value": models_without_secret()}).to_string(),
    )
    .await;
    assert_problem(&resp, 412, "precondition_failed");
}

#[tokio::test]
async fn review_fixes_dry_run_status_and_multi_secret_stage() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    // dry_run：secret 拆分错误也按试运行契约返回 valid:false
    let mut cleared = models_without_secret();
    cleared["providers"]["anthropic"]["credential"]["value"] = Value::Null;
    let resp = send(
        &env,
        put(&op, "/v1/settings/models?dry_run=true", Some("\"0\"")),
        &json!({"value": cleared}).to_string(),
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    let dry = body(&resp);
    validate("DryRunResponse", &dry);
    assert_eq!(dry["valid"], false);
    assert!(dry["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["code"] == "secret_clear_in_use"));

    // 状态聚合失败：显式 unhealthy，不是空对象
    *env.settings_ctx.status_fails.lock().unwrap() = true;
    let resp = send(&env, op.req(Method::GET, "/v1/settings/status"), "").await;
    let status = body(&resp);
    validate("SettingsStatus", &status);
    assert_eq!(status["modules"]["models"]["health"], "unhealthy");

    // 一次写两个 provider 的凭据：每个暂存用独立 request_id，都能生效
    let mut doc = models_with_secret();
    let mut second = doc["providers"]["anthropic"].clone();
    second["credential"]["secret_ref"] = json!("model.anthropic.backup");
    second["credential"]["value"] = json!("probe-second-secret-value");
    doc["providers"]["anthropic-backup"] = second;
    let body_two = json!({ "value": doc }).to_string();
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")),
        &body_two,
    )
    .await;
    let signing = assert_problem(&resp, 403, "signature_required")["signing"].clone();
    let approval = sign_approval(&env, &approver, &op, &signing, "settings-two-secret").await;
    let resp = send(
        &env,
        put(&approver, "/v1/settings/models", Some("\"0\"")).header("gantry-approval", &approval),
        &body_two,
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    let events = env
        .store
        .events()
        .by_subject(&Subject::settings())
        .await
        .unwrap();
    assert_eq!(
        events.last().unwrap().data["secret_changes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let b = body(&resp);
    assert_eq!(
        b["value"]["providers"]["anthropic-backup"]["credential"]["present"],
        true
    );
}
