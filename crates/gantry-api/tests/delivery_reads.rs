//! 交付单元读路由（#5 #6）使用 store 投影。

mod support;

use gantry_core::entities::{DevicePlatform, DeviceRole};
use gantry_core::events::EventType;
use gantry_core::ids::{DeliveryId, ProjectId, WorktreeId};
use gantry_core::testing::builders;
use http::Method;
use serde_json::json;
use support::*;

#[tokio::test]
async fn delivery_reads_use_store_projection() {
    let h = harness(None);
    let d = device(21, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(d.clone());

    let mut project = builders::project();
    project.id = ProjectId::from_parts(T0 as u64, 21).unwrap();
    doubles::record(
        &h.store,
        EventType::ProjectAdded,
        &project.id.to_string(),
        Some(project.id),
        serde_json::to_value(&project).unwrap(),
        None,
    )
    .await;

    let delivery = DeliveryId::from_parts(T0 as u64, 2100).unwrap();
    let mut data = doubles::fixture_data("delivery.created");
    data["title"] = json!("fix login");
    doubles::record(
        &h.store,
        EventType::DeliveryCreated,
        &delivery.to_string(),
        Some(project.id),
        data,
        None,
    )
    .await;

    let worktree = WorktreeId::from_parts(T0 as u64, 2101).unwrap();
    let mut data = doubles::fixture_data("worktree.created");
    data["worktree_id"] = json!(worktree);
    data["project_id"] = json!(project.id);
    data["delivery_id"] = json!(delivery);
    doubles::record(
        &h.store,
        EventType::WorktreeCreated,
        &worktree.to_string(),
        Some(project.id),
        data,
        None,
    )
    .await;

    let resp = send(&h, req(Method::GET, "/v1/deliveries", &d), "").await;
    assert_eq!(resp.status().as_u16(), 200, "{:?}", body(&resp));
    let page = body(&resp);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let item = &page["items"][0];
    assert_eq!(item["id"], json!(delivery));
    assert_eq!(item["project_id"], json!(project.id));
    assert_eq!(item["worktree_id"], json!(worktree));
    assert!(item["pr"].is_null());
    assert!(item["parent_id"].is_null());
    assert_eq!(item["depends_on"], json!([]));
    assert!(header(&resp, "x-event-seq").is_some());

    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/deliveries/{delivery}"), &d),
        "",
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "{:?}", body(&resp));
    assert_eq!(body(&resp)["id"], json!(delivery));

    let unknown = DeliveryId::from_parts(T0 as u64, 2199).unwrap();
    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/deliveries/{unknown}"), &d),
        "",
    )
    .await;
    assert_problem(&resp, 404, "not_found");

    let resp = send(&h, req(Method::GET, "/v1/deliveries?stage=nope", &d), "").await;
    assert_problem(&resp, 400, "invalid_request");
}
