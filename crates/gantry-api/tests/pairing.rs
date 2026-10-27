//! 配对端点 #1（api.md §1.3，本文 §12.2「配对端到端」），对接真实 pki。

mod support;

use std::net::IpAddr;

use bytes::Bytes;
use gantry_api::pairing::PairingListener;
use http::Method;
use serde_json::{json, Value};
use support::pki_env::{Keys, PkiEnv};
use support::{assert_problem, body, validate};

fn listener(env: &PkiEnv) -> PairingListener {
    PairingListener::new(
        env.pairing.clone(),
        env.store.clone(),
        env.clock.clone(),
        std::sync::Arc::new(gantry_core::ids::IdGen::new(
            env.clock.clone(),
            std::sync::Arc::new(gantry_core::testing::SeqEntropy::new(5)),
        )),
    )
}

async fn token(env: &PkiEnv) -> String {
    env.pairing
        .open_session(0)
        .await
        .unwrap()
        .token
        .expose_secret()
        .to_owned()
}

fn request(token: &str, keys: &Keys, platform: &str, with_key: bool) -> Value {
    let mut req = json!({
        "token": token,
        "csr": keys.csr(),
        "device": { "name": "Lex 的 MacBook", "platform": platform },
    });
    if with_key {
        req["approval_pubkey"] = json!(gantry_pki::csr::encode_approval_pubkey(
            &keys.approval_public()
        ));
    }
    req
}

async fn pair(l: &PairingListener, ip: &str, body: &Value) -> gantry_api::ApiResponse {
    l.handle(
        ip.parse::<IpAddr>().unwrap(),
        &Method::POST,
        "/v1/pair",
        Some("application/json"),
        Bytes::from(body.to_string()),
    )
    .await
}

#[tokio::test]
async fn pairing_issues_a_chain_and_first_desktop_is_approver() {
    let env = PkiEnv::new().await;
    let l = listener(&env);
    let keys = Keys::new();
    let resp = pair(
        &l,
        "10.0.0.1",
        &request(&token(&env).await, &keys, "macos", true),
    )
    .await;
    assert_eq!(resp.status(), 201, "{:?}", body(&resp));
    let b = body(&resp);
    validate("PairResponse", &b);
    assert_eq!(b["certificate_chain"].as_array().unwrap().len(), 3);
    assert_eq!(b["device"]["role"], "approver");
    assert!(b["device"].get("approval_pubkey").is_none());
    assert_eq!(
        b["device"]["approval_pubkey_fpr"],
        keys.approval_public().fingerprint().to_string()
    );
    let keys2 = Keys::new();
    let resp = pair(
        &l,
        "10.0.0.1",
        &request(&token(&env).await, &keys2, "windows", true),
    )
    .await;
    assert_eq!(body(&resp)["device"]["role"], "operator");
}

#[tokio::test]
async fn invalid_requests_and_tokens() {
    let env = PkiEnv::new().await;
    let l = listener(&env);
    // 桌面档案缺 approval_pubkey → 422
    let resp = pair(
        &l,
        "10.0.0.2",
        &request(&token(&env).await, &Keys::new(), "macos", false),
    )
    .await;
    assert_problem(&resp, 422, "validation_failed");
    // 无效 / 已用令牌 → 401，除 request_id 外逐字节相同
    let bogus = pair(
        &l,
        "10.0.0.3",
        &request("not-a-real-token", &Keys::new(), "macos", true),
    )
    .await;
    let t = token(&env).await;
    assert_eq!(
        pair(&l, "10.0.0.3", &request(&t, &Keys::new(), "macos", true))
            .await
            .status(),
        201
    );
    let used = pair(&l, "10.0.0.3", &request(&t, &Keys::new(), "macos", true)).await;
    let strip = |v: Value| {
        let mut v = v;
        v.as_object_mut().unwrap().remove("request_id");
        v
    };
    assert_eq!(
        strip(assert_problem(&bogus, 401, "pair_token_invalid")),
        strip(assert_problem(&used, 401, "pair_token_invalid"))
    );
    // 只路由 POST /v1/pair
    let resp = l
        .handle(
            "10.0.0.4".parse().unwrap(),
            &Method::GET,
            "/v1/me",
            None,
            Bytes::new(),
        )
        .await;
    assert_problem(&resp, 404, "not_found");
    let resp = l
        .handle(
            "10.0.0.4".parse().unwrap(),
            &Method::GET,
            "/v1/pair",
            None,
            Bytes::new(),
        )
        .await;
    assert_problem(&resp, 405, "method_not_allowed");
    // 非 JSON → 415；超过 16 KiB → 413
    let resp = l
        .handle(
            "10.0.0.5".parse().unwrap(),
            &Method::POST,
            "/v1/pair",
            Some("text/plain"),
            Bytes::from("x"),
        )
        .await;
    assert_problem(&resp, 415, "unsupported_media_type");
    let resp = l
        .handle(
            "10.0.0.5".parse().unwrap(),
            &Method::POST,
            "/v1/pair",
            Some("application/json"),
            Bytes::from(vec![b' '; 16 * 1024 + 1]),
        )
        .await;
    assert_problem(&resp, 413, "payload_too_large");
}

#[tokio::test]
async fn pairing_is_rate_limited_per_source_ip() {
    let env = PkiEnv::new().await;
    let l = listener(&env);
    let req = request("bogus-token", &Keys::new(), "macos", true);
    for _ in 0..5 {
        assert_eq!(pair(&l, "10.0.0.9", &req).await.status(), 401);
    }
    let resp = pair(&l, "10.0.0.9", &req).await;
    assert_problem(&resp, 429, "rate_limited");
    // 同一 /64 的 IPv6 共享额度；其他来源不受影响
    for i in 0..5 {
        pair(&l, &format!("2001:db8::{i}"), &req).await;
    }
    assert_problem(&pair(&l, "2001:db8::ff", &req).await, 429, "rate_limited");
    assert_eq!(pair(&l, "10.0.0.10", &req).await.status(), 401);
}
