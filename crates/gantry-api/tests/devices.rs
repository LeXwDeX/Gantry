//! 设备路由 #37 #38 #47 与鉴权链，对接真实 `pki`（本文 §12.2「证书续期」「鉴权链」）。

mod support;

use bytes::Bytes;
use gantry_core::entities::{Actor, ActorKind, DevicePlatform, DeviceRole};
use gantry_pki::admin::DeviceAdmin;
use http::Method;
use serde_json::json;
use support::pki_env::{peer_of, Keys, Paired, PkiEnv};
use support::{assert_problem, body, header, validate};

async fn send(env: &PkiEnv, head: gantry_api::RequestHead, body: &str) -> gantry_api::ApiResponse {
    env.core.handle(head, Bytes::from(body.to_owned())).await
}

fn json_req(d: &Paired, method: Method, uri: &str) -> gantry_api::RequestHead {
    d.req(method, uri)
        .header("content-type", "application/json")
}

#[tokio::test]
async fn paired_devices_authenticate_and_first_desktop_is_approver() {
    let env = PkiEnv::new().await;
    let first = env.pair(DevicePlatform::Macos, "first").await;
    let second = env.pair(DevicePlatform::Windows, "second").await;
    let resp = send(&env, first.req(Method::GET, "/v1/me"), "").await;
    assert_eq!(resp.status(), 200);
    let b = body(&resp);
    validate("MeResponse", &b);
    assert_eq!(b["device"]["role"], "approver");
    assert_eq!(
        b["device"]["approval_pubkey_fpr"],
        first.keys.approval_public().fingerprint().to_string()
    );
    assert!(b["device"].get("approval_pubkey").is_none());
    let resp = send(&env, second.req(Method::GET, "/v1/me"), "").await;
    assert_eq!(body(&resp)["device"]["role"], "operator");
}

#[tokio::test]
async fn list_devices_paginates_without_approval_keys() {
    let env = PkiEnv::new().await;
    let a = env.pair(DevicePlatform::Macos, "a").await;
    let b = env.pair(DevicePlatform::Ios, "b").await;
    let c = env.pair(DevicePlatform::Windows, "c").await;
    let resp = send(&env, a.req(Method::GET, "/v1/devices?limit=2"), "").await;
    assert_eq!(resp.status(), 200);
    let page = body(&resp);
    validate("DeviceList", &page);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert!(page["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d.get("approval_pubkey").is_none()));
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let resp = send(
        &env,
        a.req(Method::GET, &format!("/v1/devices?limit=2&cursor={cursor}")),
        "",
    )
    .await;
    let rest = body(&resp);
    let mut ids: Vec<String> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(rest["items"].as_array().unwrap())
        .map(|d| d["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    let mut expected: Vec<String> = [&a, &b, &c]
        .iter()
        .map(|d| d.response.device.id.to_string())
        .collect();
    expected.sort();
    assert_eq!(ids, expected);
    assert!(rest["next_cursor"].is_null());
    let resp = send(
        &env,
        a.req(Method::GET, &format!("/v1/devices?cursor={cursor}x")),
        "",
    )
    .await;
    assert_problem(&resp, 400, "invalid_cursor");
    // 手机档案不能管理设备
    assert_problem(
        &send(&env, b.req(Method::GET, "/v1/devices"), "").await,
        403,
        "forbidden_profile",
    );
}

#[tokio::test]
async fn rename_goes_through_pki_and_validates_the_name() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "a").await;
    let op = env.pair(DevicePlatform::Macos, "b").await;
    let target = approver.response.device.id;
    let path = format!("/v1/devices/{target}");
    let resp = send(
        &env,
        json_req(&op, Method::PATCH, &path),
        r#"{"name":"工作站"}"#,
    )
    .await;
    assert_eq!(resp.status(), 200);
    let b = body(&resp);
    validate("Device", &b);
    assert_eq!(b["name"], "工作站");
    let head = env.store.events().head().await.unwrap();
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        head.as_u64().to_string()
    );
    // 名称为空 → 422（契约 schema）；带别的字段 → unknown_field
    let resp = send(&env, json_req(&op, Method::PATCH, &path), r#"{"name":""}"#).await;
    assert_eq!(
        assert_problem(&resp, 422, "validation_failed")["errors"][0]["pointer"],
        "/name"
    );
    let resp = send(
        &env,
        json_req(&op, Method::PATCH, &path),
        r#"{"name":"x","role":"approver"}"#,
    )
    .await;
    assert_eq!(
        assert_problem(&resp, 422, "validation_failed")["errors"][0]["code"],
        "unknown_field"
    );
    // 设备不存在 → 404
    let missing = gantry_core::ids::DeviceId::from_parts(1, 1).unwrap();
    let resp = send(
        &env,
        json_req(&op, Method::PATCH, &format!("/v1/devices/{missing}")),
        r#"{"name":"x"}"#,
    )
    .await;
    assert_problem(&resp, 404, "not_found");
}

#[tokio::test]
async fn renew_issues_a_new_chain_and_both_certs_authenticate() {
    let env = PkiEnv::new().await;
    let d = env.pair(DevicePlatform::Macos, "a").await;
    let phone = env.pair(DevicePlatform::Ios, "phone").await;
    let new_keys = Keys::new();
    let payload = json!({ "csr": new_keys.csr() }).to_string();
    let resp = send(
        &env,
        json_req(&d, Method::POST, "/v1/devices/self/renew"),
        &payload,
    )
    .await;
    assert_eq!(resp.status(), 200, "{:?}", body(&resp));
    let b = body(&resp);
    validate("DeviceRenewResponse", &b);
    let chain: Vec<&str> = b["certificate_chain"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[2], d.response.certificate_chain[2], "root unchanged");
    // 新证书立即可用，旧证书到期前仍可用；审批公钥指纹不变
    let new_peer = peer_of(chain[0]);
    assert_ne!(new_peer.serial, d.peer.serial);
    let me_new = send(
        &env,
        gantry_api::RequestHead::new(Method::GET, "/v1/me", Some(new_peer)),
        "",
    )
    .await;
    assert_eq!(me_new.status(), 200);
    assert_eq!(
        body(&me_new)["device"]["approval_pubkey_fpr"],
        d.keys.approval_public().fingerprint().to_string()
    );
    assert_eq!(
        send(&env, d.req(Method::GET, "/v1/me"), "").await.status(),
        200
    );
    // 手机档案、viewer 起均可续期
    let payload = json!({ "csr": Keys::new().csr() }).to_string();
    let resp = send(
        &env,
        json_req(&phone, Method::POST, "/v1/devices/self/renew"),
        &payload,
    )
    .await;
    assert_eq!(resp.status(), 200);
    // 非法 CSR → 422 /csr
    let resp = send(
        &env,
        json_req(&d, Method::POST, "/v1/devices/self/renew"),
        r#"{"csr":"not a csr"}"#,
    )
    .await;
    let errors = assert_problem(&resp, 422, "validation_failed")["errors"].clone();
    assert_eq!(errors[0]["pointer"], "/csr");
}

#[tokio::test]
async fn revoked_devices_get_401_and_roles_apply_immediately() {
    let env = PkiEnv::new().await;
    let approver = env.pair(DevicePlatform::Macos, "a").await;
    let op = env.pair(DevicePlatform::Macos, "b").await;
    let host = Actor::new(ActorKind::System, "host");
    // 改角色立即生效（同一"连接"的下一个请求）
    let mut tx = env.store.begin_write().await.unwrap();
    env.admin
        .set_role(&mut tx, &op.response.device.id, DeviceRole::Viewer, &host)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let path = format!("/v1/devices/{}", approver.response.device.id);
    let resp = send(&env, json_req(&op, Method::PATCH, &path), r#"{"name":"x"}"#).await;
    assert_eq!(
        assert_problem(&resp, 403, "forbidden_role")["required_role"],
        "operator"
    );
    // 吊销后下一个请求 401
    let mut tx = env.store.begin_write().await.unwrap();
    env.admin
        .revoke(&mut tx, &op.response.device.id, &host)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_problem(
        &send(&env, op.req(Method::GET, "/v1/me"), "").await,
        401,
        "device_revoked",
    );
    assert_eq!(
        send(&env, approver.req(Method::GET, "/v1/me"), "")
            .await
            .status(),
        200
    );
}
