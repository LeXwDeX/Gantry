//! #2 `GET /v1/me`（api.md §4.2）：内置处理程序、OpenAPI schema 与不泄露审批公钥。

mod support;

use gantry_core::entities::{DevicePlatform, DeviceRole};
use http::Method;
use support::*;

#[tokio::test]
async fn me_matches_openapi_and_hides_the_approval_pubkey() {
    let h = harness(None);
    for (n, platform, profile) in [
        (1, DevicePlatform::Macos, "desktop"),
        (2, DevicePlatform::Windows, "desktop"),
        (3, DevicePlatform::Ios, "mobile"),
    ] {
        let d = device(n, platform, DeviceRole::Operator);
        h.devices.insert(d.clone());
        let resp = send(&h, req(Method::GET, "/v1/me", &d), "").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            header(&resp, "content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(header(&resp, "x-event-seq"), Some("0"));
        let b = body(&resp);
        validate("MeResponse", &b);
        assert_eq!(b["profile"], profile);
        assert_eq!(b["device"]["id"], d.id.to_string());
        assert!(b["device"].get("approval_pubkey").is_none());
        assert_eq!(b["server"]["api_version"], "v1");
        assert_eq!(b["server"]["log_epoch"], 1);
        assert_eq!(b["server"]["time"], "2026-10-26T07:33:20.000Z");
        // not_before = T0-10d, not_after = T0+20d → renew_after = T0+10d
        assert_eq!(b["certificate"]["not_after"], "2026-11-15T07:33:20.000Z");
        assert_eq!(b["certificate"]["renew_after"], "2026-11-05T07:33:20.000Z");
    }
}

#[tokio::test]
async fn unwired_m1_routes_are_404_for_now() {
    let h = harness(None);
    let d = device(1, DevicePlatform::Macos, DeviceRole::Approver);
    h.devices.insert(d.clone());
    let resp = send(&h, req(Method::GET, "/v1/projects", &d), "").await;
    assert_problem(&resp, 404, "not_found");
}
