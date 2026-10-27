//! 请求处理管线（本文 §7.1）：鉴权链、路由、限流、请求格式检查与公共响应头。
//! 对应本文 §12.2「鉴权链」「统一错误 / 校验」「契约一致性」中不依赖 TLS 的 M1 条目。

mod support;

use std::sync::Arc;

use bytes::Bytes;
use gantry_api::Admission;
use gantry_core::entities::{DevicePlatform, DeviceRole, DeviceStatus};
use http::Method;
use serde_json::json;
use support::*;

const CNV: &str = "cnv_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const DLV: &str = "dlv_01JB6Z3K7R9T2X5N8Q1V4W6YZM";
const KEY: &str = "01JB7P9Z3K5R8T2X6N1Q4V7WYA";

fn messages() -> String {
    format!("/v1/conversations/{CNV}/messages")
}

fn echo_harness() -> (Harness, Arc<Echo>) {
    let echo = Arc::new(Echo::default());
    (harness(Some(echo.clone())), echo)
}

#[tokio::test]
async fn unknown_and_unenabled_routes_are_404_problems() {
    let (h, echo) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Approver);
    h.devices.insert(d.clone());
    for path in ["/v1/nope", "/webhook", "/v1/pair", "/v1/results/abc", "/"] {
        let resp = send(&h, req(Method::GET, path, &d), "").await;
        assert_problem(&resp, 404, "not_found");
        assert!(header(&resp, "ratelimit-limit").is_some(), "{path}");
    }
    // `POST /v1/pair` 只在配对监听器上（api.md §1.3）
    let resp = send(&h, json_req(Method::POST, "/v1/pair", &d), "{}").await;
    assert_problem(&resp, 404, "not_found");
    // M2 路由在 M1 未启用：与未注册一样 404
    let resp = send(&h, req(Method::GET, "/v1/worktrees", &d), "").await;
    assert_problem(&resp, 404, "not_found");
    assert!(echo.ops().is_empty());
}

#[tokio::test]
async fn wrong_method_is_405_with_allow() {
    let (h, _) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Approver);
    h.devices.insert(d.clone());
    let resp = send(&h, req(Method::DELETE, &messages(), &d), "").await;
    assert_problem(&resp, 405, "method_not_allowed");
    assert_eq!(header(&resp, "allow"), Some("GET, POST"));
    // 无 CORS：OPTIONS 一律 405
    let resp = send(&h, req(Method::OPTIONS, "/v1/me", &d), "").await;
    assert_problem(&resp, 405, "method_not_allowed");
    assert_eq!(header(&resp, "allow"), Some("GET"));
}

#[tokio::test]
async fn literal_paths_win_over_parameters() {
    let (h, echo) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Approver);
    h.devices.insert(d.clone());
    for path in [
        "/v1/inbox/summary",
        "/v1/settings/catalog",
        "/v1/settings/status",
    ] {
        let resp = send(&h, req(Method::GET, path, &d), "").await;
        assert_eq!(resp.status(), 200, "{path}");
    }
    let resp = send(
        &h,
        json_req(Method::POST, "/v1/devices/self/renew", &d),
        "{}",
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        echo.ops(),
        [
            "getInboxSummary",
            "getSettingsCatalog",
            "getSettingsStatus",
            "renewDeviceCertificate"
        ]
    );
    // 字面路径存在但方法不对：405，而不是落到 `{inbox_id}`
    let resp = send(&h, json_req(Method::POST, "/v1/inbox/summary", &d), "{}").await;
    assert_problem(&resp, 405, "method_not_allowed");
}

#[tokio::test]
async fn unknown_or_revoked_device_is_401() {
    let (h, echo) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Approver);
    // 未在册
    let resp = send(&h, req(Method::GET, "/v1/me", &d), "").await;
    assert_problem(&resp, 401, "device_revoked");
    assert!(header(&resp, "ratelimit-limit").is_none());
    // 长连接途中被吊销：下一个请求即 401
    h.devices.insert(d.clone());
    assert_eq!(
        send(&h, req(Method::GET, "/v1/me", &d), "").await.status(),
        200
    );
    h.devices
        .update(&d.cert_serial, |d| d.status = DeviceStatus::Revoked);
    let resp = send(&h, req(Method::GET, "/v1/me", &d), "").await;
    assert_problem(&resp, 401, "device_revoked");
    // 没有客户端证书（传输层不应发生，失败即关闭）
    let resp = send(
        &h,
        gantry_api::RequestHead::new(Method::GET, "/v1/me", None),
        "",
    )
    .await;
    assert_problem(&resp, 401, "device_revoked");
    assert_eq!(echo.ops(), ["getMe"]);
}

#[tokio::test]
async fn directory_failure_is_503() {
    let (h, _) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Approver);
    h.devices.insert(d.clone());
    *h.devices.fail.lock().unwrap() = true;
    let resp = send(&h, req(Method::GET, "/v1/me", &d), "").await;
    assert_problem(&resp, 503, "unavailable");
    assert!(header(&resp, "retry-after").is_some());
}

#[tokio::test]
async fn role_is_checked_before_profile() {
    let (h, echo) = echo_harness();
    let viewer = device(1, DevicePlatform::Macos, DeviceRole::Viewer);
    let ios_approver = device(2, DevicePlatform::Ios, DeviceRole::Approver);
    let ios_viewer = device(3, DevicePlatform::Ios, DeviceRole::Viewer);
    for d in [&viewer, &ios_approver, &ios_viewer] {
        h.devices.insert(d.clone());
    }
    let body = r#"{"text":"hi"}"#;
    let resp = send(
        &h,
        json_req(Method::POST, &messages(), &viewer).header("idempotency-key", KEY),
        body,
    )
    .await;
    let b = assert_problem(&resp, 403, "forbidden_role");
    assert_eq!(b["required_role"], "operator");

    for path in [
        "/v1/deliveries".to_owned(),
        "/v1/settings/models".to_owned(),
        "/v1/approvals".to_owned(),
        "/v1/approvals/apr_01JB6Z3K7R9T2X5N8Q1V4W6YZM/challenge".to_owned(),
    ] {
        let resp = send(&h, req(Method::GET, &path, &ios_approver), "").await;
        assert_problem(&resp, 403, "forbidden_profile");
    }
    // 先角色后档案：手机 viewer 发消息是 forbidden_role
    let resp = send(
        &h,
        json_req(Method::POST, &messages(), &ios_viewer).header("idempotency-key", KEY),
        body,
    )
    .await;
    let b = assert_problem(&resp, 403, "forbidden_role");
    assert_eq!(b["required_role"], "operator");
    // 手机子集内的路由可用
    assert_eq!(
        send(&h, req(Method::GET, "/v1/inbox/summary", &ios_viewer), "")
            .await
            .status(),
        200
    );
    assert_eq!(echo.ops(), ["getInboxSummary"]);
}

#[tokio::test]
async fn role_changes_apply_to_the_next_request_and_are_read_once_per_request() {
    let (h, _) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(d.clone());
    let head = || {
        json_req(Method::POST, &format!("/v1/deliveries/{DLV}/pause"), &d)
            .header("idempotency-key", KEY)
    };
    assert_problem(&send(&h, head(), "{}").await, 403, "forbidden_role");
    h.devices
        .update(&d.cert_serial, |d| d.role = DeviceRole::Operator);
    assert_eq!(send(&h, head(), "{}").await.status(), 200);
    h.devices
        .update(&d.cert_serial, |d| d.role = DeviceRole::Viewer);
    assert_problem(&send(&h, head(), "{}").await, 403, "forbidden_role");
    assert_eq!(
        h.devices.lookups(),
        3,
        "exactly one by_cert_serial per request"
    );
}

#[tokio::test]
async fn rate_limit_runs_before_the_body_is_read() {
    let (h, echo) = echo_harness();
    let a = device(1, DevicePlatform::Macos, DeviceRole::Operator);
    let b = device(2, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(a.clone());
    h.devices.insert(b.clone());
    let head = |d| json_req(Method::POST, &messages(), d).header("idempotency-key", KEY);
    for _ in 0..30 {
        assert_eq!(send(&h, head(&a), "{}").await.status(), 200);
    }
    // 第 31 条：admit 就拒绝，不读请求体
    match h.core.admit(head(&a)).await {
        Admission::Respond(resp) => {
            assert_problem(&resp, 429, "rate_limited");
            assert_eq!(header(&resp, "retry-after"), Some("2"));
            assert_eq!(header(&resp, "ratelimit-limit"), Some("30"));
            assert_eq!(header(&resp, "ratelimit-remaining"), Some("0"));
        }
        Admission::Proceed(_) => panic!("rate limit must reject before the body"),
    }
    // 其他设备、其他类不受影响
    assert_eq!(send(&h, head(&b), "{}").await.status(), 200);
    assert_eq!(
        send(&h, req(Method::GET, "/v1/me", &a), "").await.status(),
        200
    );
    // 时间推进后恢复
    h.clock.advance_secs(2);
    assert_eq!(send(&h, head(&a), "{}").await.status(), 200);
    assert_eq!(echo.ops().len(), 33);
}

#[tokio::test]
async fn ratelimit_headers_on_early_errors_peek_the_read_bucket() {
    let (h, _) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(d.clone());
    for _ in 0..3 {
        let resp = send(&h, req(Method::GET, "/v1/nope", &d), "").await;
        assert_eq!(header(&resp, "ratelimit-remaining"), Some("600"));
    }
    let resp = send(&h, req(Method::GET, "/v1/me", &d), "").await;
    assert_eq!(header(&resp, "ratelimit-remaining"), Some("599"));
    assert_eq!(header(&resp, "ratelimit-limit"), Some("600"));
}

#[tokio::test]
async fn format_checks_follow_415_413_400_order() {
    let (h, echo) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(d.clone());
    let big = format!(r#"{{"text":"{}"}}"#, "x".repeat(70 * 1024));
    // 415 先于 413
    let resp = send(
        &h,
        req(Method::POST, &format!("{}?x=1", messages()), &d).header("content-type", "text/plain"),
        &big,
    )
    .await;
    assert_problem(&resp, 415, "unsupported_media_type");
    // 413 先于 400（未知查询参数、JSON 语法、缺幂等键）
    let resp = send(
        &h,
        json_req(Method::POST, &format!("{}?x=1", messages()), &d),
        &big,
    )
    .await;
    assert_problem(&resp, 413, "payload_too_large");
    // 400：未知查询参数
    let resp = send(
        &h,
        json_req(Method::POST, &format!("{}?x=1", messages()), &d),
        "{",
    )
    .await;
    assert_problem(&resp, 400, "invalid_request");
    // 400：JSON 语法
    let resp = send(
        &h,
        json_req(Method::POST, &messages(), &d).header("idempotency-key", KEY),
        "{",
    )
    .await;
    assert_problem(&resp, 400, "invalid_request");
    // 400：必带路由缺幂等键
    let resp = send(&h, json_req(Method::POST, &messages(), &d), "{}").await;
    assert_problem(&resp, 400, "idempotency_key_required");
    // 400：幂等键格式
    for bad in ["short", "has space in the key!!", &"k".repeat(65)] {
        let resp = send(
            &h,
            json_req(Method::POST, &messages(), &d).header("idempotency-key", bad),
            "{}",
        )
        .await;
        assert_problem(&resp, 400, "invalid_request");
    }
    // `application/json; charset=utf-8` 可接受
    let resp = send(
        &h,
        req(Method::POST, &messages(), &d)
            .header("content-type", "application/json; charset=utf-8")
            .header("idempotency-key", KEY),
        r#"{"text":"hi"}"#,
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(echo.ops(), ["postMessage"]);
}

#[tokio::test]
async fn path_and_query_parameters_follow_openapi_patterns() {
    let (h, echo) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(d.clone());
    for path in [
        "/v1/deliveries?project_id=oops",
        "/v1/projects/oops",
        "/v1/events?project_id=oops",
        "/v1/projects?limit=201",
        "/v1/projects?limit=1&limit=2",
        "/v1/projects?unknown=1",
        "/v1/deliveries?stage=nope",
    ] {
        let resp = send(&h, req(Method::GET, path, &d), "").await;
        assert_problem(&resp, 400, "invalid_request");
    }
    // 未知 settings resource 不是 400：交给处理程序（404）
    assert_eq!(
        send(&h, req(Method::GET, "/v1/settings/nope", &d), "")
            .await
            .status(),
        200
    );
    assert_eq!(echo.ops(), ["getSettings"]);
}

#[tokio::test]
async fn empty_request_bodies_default_to_an_empty_object() {
    let (h, echo) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(d.clone());
    // pause 的请求体是 EmptyRequest：空正文、无 Content-Type 也可
    let resp = send(
        &h,
        req(Method::POST, &format!("/v1/deliveries/{DLV}/pause"), &d)
            .header("idempotency-key", KEY),
        "",
    )
    .await;
    assert_eq!(resp.status(), 200);
    // 有正文却没有 Content-Type：415
    let resp = send(
        &h,
        req(Method::POST, &format!("/v1/deliveries/{DLV}/pause"), &d)
            .header("idempotency-key", KEY)
            .header("content-length", "2"),
        "{}",
    )
    .await;
    assert_problem(&resp, 415, "unsupported_media_type");
    // 其他路由的空正文：400
    let resp = send(
        &h,
        json_req(Method::POST, &messages(), &d).header("idempotency-key", KEY),
        "",
    )
    .await;
    assert_problem(&resp, 400, "invalid_request");
    let calls = echo.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, Some(json!({})));
    assert_eq!(calls[0].2.as_deref(), Some(KEY));
}

#[tokio::test]
async fn every_response_carries_common_headers_and_log_epoch() {
    let (h, _) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(d.clone());
    let ok = send(&h, req(Method::GET, "/v1/me", &d), "").await;
    let missing = send(&h, req(Method::GET, "/v1/nope", &d), "").await;
    for resp in [&ok, &missing] {
        assert_eq!(header(resp, "x-log-epoch"), Some("1"));
        assert_eq!(header(resp, "x-content-type-options"), Some("nosniff"));
        assert_eq!(header(resp, "cache-control"), Some("no-store"));
        assert!(header(resp, "server").is_none());
        assert!(header(resp, "x-request-id").is_some_and(gantry_api::registry::is_ulid));
    }
    assert_eq!(header(&ok, "x-event-seq"), Some("0"));
    assert!(header(&missing, "x-event-seq").is_none());
    // `restore_from` 后纪元加一（P-67）
    h.store.restore_epoch_bump();
    let resp = send(&h, req(Method::GET, "/v1/nope", &d), "").await;
    assert_eq!(header(&resp, "x-log-epoch"), Some("2"));
    let resp = send(
        &h,
        gantry_api::RequestHead::new(Method::GET, "/v1/me", None),
        "",
    )
    .await;
    assert_eq!(header(&resp, "x-log-epoch"), Some("2"));
}

#[tokio::test]
async fn admit_reports_route_body_limits() {
    let (h, _) = echo_harness();
    let d = device(1, DevicePlatform::Macos, DeviceRole::Approver);
    h.devices.insert(d.clone());
    let Admission::Proceed(a) = h
        .core
        .admit(json_req(Method::PUT, "/v1/settings/models", &d))
        .await
    else {
        panic!("settings PUT should be admitted");
    };
    assert_eq!(a.body_limit(), 256 * 1024);
    let Admission::Proceed(a) = h.core.admit(json_req(Method::POST, &messages(), &d)).await else {
        panic!("messages POST should be admitted");
    };
    assert_eq!(a.body_limit(), 64 * 1024);
    let resp = h
        .core
        .complete(*a, Bytes::from(vec![b' '; 64 * 1024 + 1]))
        .await;
    assert_problem(&resp, 413, "payload_too_large");
}
