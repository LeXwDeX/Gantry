//! 审批与签名（本文 §12.2「审批与签名」的 M1 条目），对接真实 `policy` 与 `pki`。

mod support;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bytes::Bytes;
use gantry_core::canon::{ApprovalPayload, GitOid};
use gantry_core::entities::{Actor, ActorKind, Approval, DevicePlatform, ForgeActionKind};
use gantry_core::ids::DeliveryId;
use gantry_core::ports::{ActionDecision, ActionFacts, ActionRequest, ActorCtx, PolicyPort};
use gantry_core::testing::builders;
use http::Method;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{json, Value};
use support::pki_env::{Paired, PkiEnv};
use support::{assert_problem, body, header, validate};

const KEY: &str = "01JB7P9Z3K5R8T2X6N1Q4V7WYA";

async fn send(env: &PkiEnv, head: gantry_api::RequestHead, body: &str) -> gantry_api::ApiResponse {
    env.core.handle(head, Bytes::from(body.to_owned())).await
}

fn post(d: &Paired, uri: &str, key: Option<&str>) -> gantry_api::RequestHead {
    let head = d
        .req(Method::POST, uri)
        .header("content-type", "application/json");
    match key {
        Some(k) => head.header("idempotency-key", k),
        None => head,
    }
}

fn sign(key: &SigningKey, payload: &Value) -> String {
    let payload: ApprovalPayload = serde_json::from_value(payload.clone()).unwrap();
    let signature: Signature = key.sign(&payload.signing_bytes().unwrap());
    URL_SAFE_NO_PAD.encode(signature.to_bytes())
}

fn approval_key(d: &Paired) -> SigningKey {
    SigningKey::from(d.keys.approval.clone())
}

/// mTLS 客户端私钥（rcgen 生成）也转成 p256 签名钥，用于"拿错钥签名"的用例。
fn mtls_key(d: &Paired) -> SigningKey {
    use p256::pkcs8::DecodePrivateKey as _;
    SigningKey::from_pkcs8_der(&d.keys.client.serialize_der()).unwrap()
}

async fn challenge(env: &PkiEnv, d: &Paired, id: &str) -> Value {
    let resp = send(
        env,
        d.req(Method::GET, &format!("/v1/approvals/{id}/challenge")),
        "",
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    assert_eq!(header(&resp, "cache-control"), Some("no-store"));
    let b = body(&resp);
    validate("ApprovalChallenge", &b);
    b
}

async fn grant(
    env: &PkiEnv,
    d: &Paired,
    id: &str,
    nonce: &str,
    sig: &str,
    key: &str,
) -> gantry_api::ApiResponse {
    let payload = json!({ "nonce": nonce, "signature": sig }).to_string();
    send(
        env,
        post(d, &format!("/v1/approvals/{id}/grant"), Some(key)),
        &payload,
    )
    .await
}

/// 发起内部动作审批（吊销目标设备）。
async fn create_revoke(env: &PkiEnv, by: &Paired, target: &Paired, key: &str) -> Value {
    let digest = gantry_policy::signing::request_digest(&json!({})).unwrap();
    let payload = json!({
        "action": "device_revoke",
        "target": { "kind": "device", "id": target.response.device.id },
        "subject_digest": digest,
    });
    let resp = send(
        env,
        post(by, "/v1/approvals", Some(key)),
        &payload.to_string(),
    )
    .await;
    assert_eq!(resp.status(), 201, "{:?}", body(&resp));
    let b = body(&resp);
    validate("Approval", &b);
    b
}

async fn approve(env: &PkiEnv, approver: &Paired, id: &str, key: &str) {
    let c = challenge(env, approver, id).await;
    let sig = sign(&approval_key(approver), &c["payload"]);
    let resp = grant(
        env,
        approver,
        id,
        c["payload"]["nonce"].as_str().unwrap(),
        &sig,
        key,
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
}

#[tokio::test]
async fn internal_approval_full_chain_then_signed_revoke() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    let victim = env.pair(DevicePlatform::Windows, "victim").await;
    let approval = create_revoke(&env, &op, &victim, KEY).await;
    let id = approval["id"].as_str().unwrap().to_owned();
    assert!(approval["delivery_id"].is_null());
    assert_eq!(approval["status"], "pending");

    // 同一设备有效期内重复 challenge 得同一份
    let c1 = challenge(&env, &approver, &id).await;
    let c2 = challenge(&env, &approver, &id).await;
    assert_eq!(c1, c2);
    // 篡改 nonce → 409 challenge_invalid；用 mTLS 私钥签名 → 403 signature_invalid（P-47）
    let sig = sign(&approval_key(&approver), &c1["payload"]);
    let bad_nonce = "A".repeat(43);
    let resp = grant(
        &env,
        &approver,
        &id,
        &bad_nonce,
        &sig,
        "grant-key-bad-nonce01",
    )
    .await;
    assert_problem(&resp, 409, "challenge_invalid");
    let nonce = c1["payload"]["nonce"].as_str().unwrap().to_owned();
    let wrong = sign(&mtls_key(&approver), &c1["payload"]);
    let resp = grant(&env, &approver, &id, &nonce, &wrong, "grant-key-mtls-key01").await;
    assert_problem(&resp, 403, "signature_invalid");
    // 正确签名 → 200 granted；同键重试重放首次 200
    let resp = grant(&env, &approver, &id, &nonce, &sig, "grant-key-good-00001").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body(&resp)["status"], "granted");
    let replay = grant(&env, &approver, &id, &nonce, &sig, "grant-key-good-00001").await;
    assert_eq!(replay.body().as_bytes(), resp.body().as_bytes());
    assert_eq!(header(&replay, "idempotent-replayed"), Some("true"));
    // 换键重发已用 nonce → challenge_invalid(used) 或 not_pending
    let resp = grant(&env, &approver, &id, &nonce, &sig, "grant-key-reused-001").await;
    assert_eq!(resp.status(), 409);

    // 不带 Gantry-Approval → 403 signature_required（附 signing）
    let path = format!("/v1/devices/{}/revoke", victim.response.device.id);
    let resp = send(&env, post(&approver, &path, None), "{}").await;
    let b = assert_problem(&resp, 403, "signature_required");
    assert_eq!(b["signing"]["action"], "device_revoke");
    assert_eq!(b["signing"]["target"]["kind"], "device");
    // operator 带审批也不行（路由要求 approver）
    let resp = send(
        &env,
        post(&op, &path, None).header("gantry-approval", &id),
        "{}",
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 403, "forbidden_role")["required_role"],
        "approver"
    );
    // approver 带审批 → 200 revoked；同一审批第二次 → 已吊销原样 200（先于签名）
    let resp = send(
        &env,
        post(&approver, &path, None).header("gantry-approval", &id),
        "{}",
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    assert_eq!(body(&resp)["status"], "revoked");
    assert_problem(
        &send(&env, victim.req(Method::GET, "/v1/me"), "").await,
        401,
        "device_revoked",
    );
    let again = send(&env, post(&approver, &path, None), "{}").await;
    assert_eq!(again.status(), 200);
    // 审批已消费
    let resp = send(
        &env,
        approver.req(Method::GET, &format!("/v1/approvals/{id}")),
        "",
    )
    .await;
    assert_eq!(body(&resp)["status"], "consumed");
}

#[tokio::test]
async fn role_change_checks_self_and_mobile_before_consuming() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    let phone = env.pair(DevicePlatform::Ios, "phone").await;
    // 改自己 → 409 self_role_change（不需要也不消费审批）
    let me = format!("/v1/devices/{}/role", approver.response.device.id);
    let put = |d: &Paired, path: &str| {
        d.req(Method::PUT, path)
            .header("content-type", "application/json")
    };
    let resp = send(&env, put(&approver, &me), r#"{"role":"viewer"}"#).await;
    assert_eq!(
        assert_problem(&resp, 409, "conflict")["reason"],
        "self_role_change"
    );
    // ios 设为 approver → 409 mobile_cannot_approve（P-76）
    let phone_path = format!("/v1/devices/{}/role", phone.response.device.id);
    let resp = send(&env, put(&approver, &phone_path), r#"{"role":"approver"}"#).await;
    assert_eq!(
        assert_problem(&resp, 409, "conflict")["reason"],
        "mobile_cannot_approve"
    );

    // 正常路径：发起审批（摘要对 {"role":"approver"}）→ 签名 → 带头重发
    let op_path = format!("/v1/devices/{}/role", op.response.device.id);
    let resp = send(&env, put(&approver, &op_path), r#"{"role":"approver"}"#).await;
    let signing = assert_problem(&resp, 403, "signature_required")["signing"].clone();
    let resp = send(
        &env,
        post(&op, "/v1/approvals", Some(KEY)),
        &signing.to_string(),
    )
    .await;
    assert_eq!(resp.status(), 201, "{:?}", body(&resp));
    let id = body(&resp)["id"].as_str().unwrap().to_owned();
    approve(&env, &approver, &id, "role-grant-key-0001").await;
    // 为内容 A 批准、提交内容 B → 409 approval_mismatch(digest_mismatch)，未写入
    let resp = send(
        &env,
        put(&approver, &op_path).header("gantry-approval", &id),
        r#"{"role":"viewer"}"#,
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 409, "approval_mismatch")["reason"],
        "digest_mismatch"
    );
    // 空白与键序不影响摘要
    let resp = send(
        &env,
        put(&approver, &op_path).header("gantry-approval", &id),
        r#"{ "role" : "approver" }"#,
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    assert_eq!(body(&resp)["role"], "approver");
    // 新角色对下一个请求立即生效
    let resp = send(&env, op.req(Method::GET, "/v1/me"), "").await;
    assert_eq!(body(&resp)["device"]["role"], "approver");
    // 同一审批第二次使用 → consumed
    let resp = send(
        &env,
        put(&approver, &op_path).header("gantry-approval", &id),
        r#"{"role":"approver"}"#,
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 409, "approval_mismatch")["reason"],
        "consumed"
    );
}

#[tokio::test]
async fn create_rejects_non_internal_actions_and_unknown_devices() {
    let env = PkiEnv::new().await;
    let _approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    let digest = gantry_policy::signing::request_digest(&json!({})).unwrap();
    for action in ["merge_pr", "force_push", "patch_apply_protected"] {
        let payload = json!({"action": action, "target": {"kind":"pr","repo":"acme/web","number":1}, "subject_digest": digest});
        let key = format!("create-{action}-00000");
        let resp = send(
            &env,
            post(&op, "/v1/approvals", Some(&key)),
            &payload.to_string(),
        )
        .await;
        assert_problem(&resp, 422, "validation_failed");
    }
    let missing = gantry_core::ids::DeviceId::from_parts(1, 1).unwrap();
    let payload = json!({"action": "device_revoke", "target": {"kind":"device","id": missing}, "subject_digest": digest});
    let resp = send(
        &env,
        post(&op, "/v1/approvals", Some("create-missing-dev01")),
        &payload.to_string(),
    )
    .await;
    assert_problem(&resp, 404, "not_found");
    // 缺幂等键
    let resp = send(&env, post(&op, "/v1/approvals", None), &payload.to_string()).await;
    assert_problem(&resp, 400, "idempotency_key_required");
}

#[tokio::test]
async fn deny_mobile_and_payload_rules() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    let phone = env.pair(DevicePlatform::Ios, "phone").await;
    let approval = create_revoke(&env, &op, &phone, KEY).await;
    let id = approval["id"].as_str().unwrap().to_owned();
    // grant 请求体带 payload → 422 unknown_field（客户端不能提交待签内容）
    let resp = send(
        &env,
        post(
            &approver,
            &format!("/v1/approvals/{id}/grant"),
            Some("grant-with-payload01"),
        ),
        &json!({"nonce": "A".repeat(43), "signature": "B".repeat(86), "payload": {}}).to_string(),
    )
    .await;
    let b = assert_problem(&resp, 422, "validation_failed");
    assert!(b["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["pointer"] == "/payload" && e["code"] == "unknown_field"));
    // 手机档案调审批路由：角色够的（viewer 起的读路由）→ 403 forbidden_profile；
    // 手机设备永远不是 approver（P-76），challenge 先在角色一步被拒（先角色后档案）
    let resp = send(
        &env,
        phone.req(Method::GET, &format!("/v1/approvals/{id}")),
        "",
    )
    .await;
    assert_problem(&resp, 403, "forbidden_profile");
    let resp = send(
        &env,
        phone.req(Method::GET, &format!("/v1/approvals/{id}/challenge")),
        "",
    )
    .await;
    assert_problem(&resp, 403, "forbidden_role");
    // deny：approver、不需要签名、幂等键必带
    let deny = format!("/v1/approvals/{id}/deny");
    assert_problem(
        &send(&env, post(&approver, &deny, None), "{}").await,
        400,
        "idempotency_key_required",
    );
    let resp = send(
        &env,
        post(&approver, &deny, Some("deny-key-0000000001")),
        r#"{"reason":"no"}"#,
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body(&resp)["status"], "denied");
    // 非 pending → 409 approval_not_pending（带 approval_status）
    let resp = send(
        &env,
        post(&approver, &deny, Some("deny-key-0000000002")),
        "{}",
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 409, "approval_not_pending")["approval_status"],
        "denied"
    );
    // 列表与过滤
    let resp = send(&env, op.req(Method::GET, "/v1/approvals?status=denied"), "").await;
    let list = body(&resp);
    validate("ApprovalList", &list);
    assert_eq!(list["items"][0]["id"], id.as_str());
}

/// PR 类审批：签名后 PR 有新推送 → grant 前强制对账发现 → 409 stale_head。
#[tokio::test]
async fn new_push_after_signing_makes_the_approval_stale() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let project = builders::project();
    let mut pr = builders::pull_request();
    let head_a = GitOid::parse(&"a".repeat(40)).unwrap();
    pr.head_sha = head_a;
    env.forge.insert_project(project.clone());
    env.forge
        .upsert_pr(project.id, "gantry/feature", pr.clone());
    let ctx = ActorCtx {
        actor: Actor::new(ActorKind::Engine, "engine"),
        device: None,
        role: None,
        client_profile: None,
        request_id: gantry_core::ids::Ulid::from_parts(0, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    };
    let request = ActionRequest {
        project_id: project.id,
        delivery_id: Some(DeliveryId::from_parts(1, 3).unwrap()),
        kind: ForgeActionKind::MergePr,
        facts: ActionFacts::MergePr {
            repo: pr.repo.clone(),
            number: pr.number,
            expected_head_sha: head_a,
        },
        requested_by: ctx.actor.clone(),
        epoch: 1,
    };
    let ActionDecision::NeedsApproval { spec, .. } =
        env.policy.decide_action(request, &ctx).await.unwrap()
    else {
        panic!("merge_pr needs approval");
    };
    let approval: Approval = env.policy.request_approval(spec, ctx).await.unwrap();
    let id = approval.id.to_string();
    let c = challenge(&env, &approver, &id).await;
    let sig = sign(&approval_key(&approver), &c["payload"]);
    // 签名后 PR 有新推送
    pr.head_sha = GitOid::parse(&"b".repeat(40)).unwrap();
    env.forge
        .upsert_pr(project.id, "gantry/feature", pr.clone());
    let resp = grant(
        &env,
        &approver,
        &id,
        c["payload"]["nonce"].as_str().unwrap(),
        &sig,
        "stale-grant-key-0001",
    )
    .await;
    let b = assert_problem(&resp, 409, "stale_head");
    assert_eq!(b["current_head_sha"], "b".repeat(40));
    assert_eq!(b["approval_status"], "invalidated");
    let resp = send(
        &env,
        approver.req(Method::GET, &format!("/v1/approvals/{id}")),
        "",
    )
    .await;
    assert_eq!(body(&resp)["status"], "invalidated");
}

#[tokio::test]
async fn review_fixes_deny_text_seq_and_create_recovery() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "approver").await;
    let op = env.pair(DevicePlatform::Macos, "operator").await;
    let victim = env.pair(DevicePlatform::Windows, "victim").await;

    // 拒绝备注含回车：边界 422 /reason，而不是存储层的 503
    let approval = create_revoke(&env, &op, &victim, KEY).await;
    let id = approval["id"].as_str().unwrap().to_owned();
    let deny = format!("/v1/approvals/{id}/deny");
    let payload = json!({"reason": "暂不批准\r\n需要补充测试"}).to_string();
    let resp = send(
        &env,
        post(&approver, &deny, Some("deny-cr-key-0000001")),
        &payload,
    )
    .await;
    let b = assert_problem(&resp, 422, "validation_failed");
    assert_eq!(b["errors"][0]["pointer"], "/reason");

    // 改角色响应的 X-Event-Seq 是本事务最后一个事件，不是提交后的全局 head
    let path = format!("/v1/devices/{}/role", op.response.device.id);
    let put = |d: &Paired| {
        d.req(Method::PUT, &path)
            .header("content-type", "application/json")
    };
    let signing = assert_problem(
        &send(&env, put(&approver), r#"{"role":"viewer"}"#).await,
        403,
        "signature_required",
    )["signing"]
        .clone();
    let created = send(
        &env,
        post(&op, "/v1/approvals", Some("role-approval-key-01")),
        &signing.to_string(),
    )
    .await;
    let role_id = body(&created)["id"].as_str().unwrap().to_owned();
    approve(&env, &approver, &role_id, "role-grant-key-00002").await;
    let resp = send(
        &env,
        put(&approver).header("gantry-approval", &role_id),
        r#"{"role":"viewer"}"#,
    )
    .await;
    assert_eq!(resp.status(), 200);
    let subject = gantry_core::events::Subject::parse(&op.response.device.id.to_string()).unwrap();
    let last = env
        .store
        .events()
        .by_subject(&subject)
        .await
        .unwrap()
        .last()
        .unwrap()
        .seq;
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        last.as_u64().to_string()
    );

    // 发起审批：业务已提交但幂等完成记录丢失 → 同键重试按 dedup_key 找回首次的审批
    let apr = gantry_core::ids::ApprovalId::from_parts(1, 77).unwrap();
    let dedup = format!("api:{}:recover-key-00000001", approver.response.device.id);
    let cases: Value =
        serde_json::from_str(include_str!("../../gantry-core/fixtures/events/cases.json")).unwrap();
    let mut tx = env.store.begin_write().await.unwrap();
    tx.append(gantry_core::events::NewEvent {
        ty: gantry_core::events::EventType::ApprovalRequested,
        subject: gantry_core::events::Subject::parse(&apr.to_string()).unwrap(),
        project_id: None,
        actor: Actor::new(ActorKind::Device, approver.response.device.id.to_string()),
        causation_id: None,
        correlation_id: None,
        data: cases["valid"]["approval.requested"].clone(),
        dedup_key: Some(dedup),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let digest = gantry_policy::signing::request_digest(&json!({})).unwrap();
    let payload = json!({"action": "device_revoke", "target": {"kind": "device", "id": victim.response.device.id}, "subject_digest": digest});
    let resp = send(
        &env,
        post(&approver, "/v1/approvals", Some("recover-key-00000001")),
        &payload.to_string(),
    )
    .await;
    assert_eq!(resp.status(), 201, "{:?}", body(&resp));
    assert_eq!(body(&resp)["id"], apr.to_string());
}
