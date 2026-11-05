//! 工作流 / 节点 / 尝试读路由（#17–#20）使用 store 的图投影。

mod support;

use gantry_core::entities::{DevicePlatform, DeviceRole};
use gantry_core::events::EventType;
use gantry_core::ids::ProjectId;
use http::Method;
use serde_json::json;
use support::*;

const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DELIVERY: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const WORKFLOW: &str = "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZF";
const BUILD: &str = "nod_01JB7M7V4X2N9R5T8Q3K6W1YZG";
const VERIFY: &str = "nod_01JB7M7W5X3N9R6T8Q2K4V1YZH";
const ATTEMPT: &str = "att_01JB7M9X5N3R8T2Q6K1V4W7YZH";
const WORKFLOW2: &str = "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZG";
const WORKFLOW3: &str = "wfl_01JB7M6T3X9V2N5R8Q1K4W7YZH";
const DELIVERY2: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC";

#[tokio::test]
async fn workflow_node_and_attempt_reads_use_store_projection() {
    let h = harness(None);
    let d = device(22, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(d.clone());
    let project = PROJECT.parse::<ProjectId>().unwrap();
    doubles::record(
        &h.store,
        EventType::WorkflowCreated,
        WORKFLOW,
        Some(project),
        json!({
            "stage": "implement",
            "template": "medium.implement",
            "graph_rev": 1,
            "delivery_id": DELIVERY,
            "nodes": [
                {"node_id": BUILD, "key": "build", "role": "build", "depends_on": [], "required": true, "critical": false},
                {"node_id": VERIFY, "key": "verify", "role": "verify", "depends_on": ["build"], "required": true, "critical": false}
            ]
        }),
        None,
    )
    .await;
    doubles::record(
        &h.store,
        EventType::AttemptStarted,
        ATTEMPT,
        Some(project),
        json!({
            "attempt_id": ATTEMPT, "node_id": BUILD, "n": 1,
            "provider": "anthropic", "model": "claude", "reasoning_effort": "medium",
            "config_rev": 1,
            "base_sha": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
            "tokens_total": 1000, "wall_time_s": 600
        }),
        None,
    )
    .await;
    doubles::record(
        &h.store,
        EventType::AttemptFinished,
        ATTEMPT,
        Some(project),
        json!({
            "status": "succeeded",
            "result_ref": "sha256:5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9",
            "result_tree": "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
            "result_commit": "d7d018f867d6c5b78e82906386420d45a4d84486",
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_tokens": 0, "cache_write_tokens": 0, "requests": 1}
        }),
        None,
    )
    .await;
    for (workflow, delivery) in [(WORKFLOW2, DELIVERY), (WORKFLOW3, DELIVERY2)] {
        doubles::record(
            &h.store,
            EventType::WorkflowCreated,
            workflow,
            Some(project),
            json!({
                "stage": "implement",
                "template": "medium.implement",
                "graph_rev": 1,
                "delivery_id": delivery,
                "nodes": [
                    {"node_id": "nod_01JB7M7V4X2N9R5T8Q3K6W1YZJ", "key": "build", "role": "build", "depends_on": [], "required": true, "critical": false}
                ]
            }),
            None,
        )
        .await;
    }
    doubles::record(
        &h.store,
        EventType::WorkflowRevised,
        WORKFLOW,
        Some(project),
        json!({
            "graph_rev": 2, "reason": "review", "round": 1,
            "added": [{"node_id": "nod_01JB7M7X6Y4P9S7T8R3K5V2YZJ", "key": "fix", "role": "build", "depends_on": ["verify"], "required": true, "critical": false}],
            "superseded": [VERIFY]
        }),
        None,
    )
    .await;

    let base = format!("/v1/deliveries/{DELIVERY}/workflows");
    let resp = send(&h, req(Method::GET, &base, &d), "").await;
    assert_eq!(resp.status().as_u16(), 200, "{:?}", body(&resp));
    let page = body(&resp);
    // 默认 id desc：较新的 WORKFLOW2 在前。
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["items"][0]["id"], json!(WORKFLOW2));
    assert!(page["items"][0].get("nodes").is_none());

    // 游标绑定交付单元：把 DELIVERY 的游标用于 DELIVERY2 → 400 invalid_cursor。
    let resp = send(
        &h,
        req(
            Method::GET,
            &format!("/v1/deliveries/{DELIVERY}/workflows?limit=1"),
            &d,
        ),
        "",
    )
    .await;
    let cursor = body(&resp)["next_cursor"].as_str().unwrap().to_owned();
    let resp = send(
        &h,
        req(
            Method::GET,
            &format!("/v1/deliveries/{DELIVERY2}/workflows?limit=1&cursor={cursor}"),
            &d,
        ),
        "",
    )
    .await;
    assert_problem(&resp, 400, "invalid_cursor");

    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/workflows/{WORKFLOW}"), &d),
        "",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "{:?}", body(&resp));
    let workflow = body(&resp);
    assert_eq!(workflow["graph_rev"], json!(2));
    assert_eq!(workflow["nodes"].as_array().unwrap().len(), 2);

    let resp = send(
        &h,
        req(
            Method::GET,
            &format!("/v1/workflows/{WORKFLOW}?include_superseded=true"),
            &d,
        ),
        "",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "{:?}", body(&resp));
    assert_eq!(body(&resp)["nodes"].as_array().unwrap().len(), 3);

    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/nodes/{BUILD}/attempts"), &d),
        "",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "{:?}", body(&resp));
    let page = body(&resp);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], json!(ATTEMPT));

    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/attempts/{ATTEMPT}"), &d),
        "",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "{:?}", body(&resp));
    assert_eq!(body(&resp)["n"], json!(1));
    assert_eq!(body(&resp)["usage"]["input_tokens"], json!(10));

    let resp = send(
        &h,
        req(
            Method::GET,
            "/v1/workflows/wfl_01JB7M6T3X9V2N5R8Q1K4W7YZK",
            &d,
        ),
        "",
    )
    .await;
    assert_problem(&resp, 404, "not_found");
}
