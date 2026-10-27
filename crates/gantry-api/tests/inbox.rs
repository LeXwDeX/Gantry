//! 收件箱读路由 #21 #22 #46（api.md §4.8，本文 §12.2「控制路由与收件箱计数」「分页」）。

mod support;

use gantry_core::entities::{Actor, ActorKind, DevicePlatform, DeviceRole, Severity};
use gantry_core::events::{EventType, NewEvent, Subject};
use gantry_core::ids::{InboxId, ProjectId};
use gantry_core::testing::builders;
use http::Method;
use serde_json::json;
use support::*;

fn project(n: u128) -> ProjectId {
    ProjectId::from_parts(T0 as u64, n).unwrap()
}

/// 经 store 追加 `inbox.raised`，由内置投影建行。
async fn raise(h: &Harness, n: u128, project_id: Option<ProjectId>, severity: Severity) -> InboxId {
    let mut item = builders::inbox_item();
    item.id = InboxId::from_parts(T0 as u64, 300 + n).unwrap();
    item.project_id = project_id;
    item.severity = severity;
    item.dedup_key = format!("test:{n}");
    let data = json!({ "item": item });
    let mut tx = h.store.begin_write().await.unwrap();
    tx.append(NewEvent {
        ty: EventType::InboxRaised,
        subject: Subject::parse(&item.id.to_string()).unwrap(),
        project_id,
        actor: Actor::new(ActorKind::System, "test"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    item.id
}

fn viewer(h: &Harness, platform: DevicePlatform) -> gantry_core::entities::Device {
    let d = device(1, platform, DeviceRole::Viewer);
    h.devices.insert(d.clone());
    d
}

#[tokio::test]
async fn summary_counts_open_items_by_project_and_is_not_swallowed() {
    let h = harness(None);
    let phone = viewer(&h, DevicePlatform::Ios);
    raise(&h, 1, Some(project(1)), Severity::Warn).await;
    raise(&h, 2, Some(project(1)), Severity::Blocker).await;
    raise(&h, 3, None, Severity::Info).await;
    let resp = send(&h, req(Method::GET, "/v1/inbox/summary", &phone), "").await;
    assert_eq!(resp.status(), 200);
    let b = body(&resp);
    validate("InboxSummary", &b);
    assert_eq!(b["total_open"], 3);
    let rows = b["by_project"].as_array().unwrap();
    assert!(rows
        .iter()
        .any(|r| r["project_id"].is_null() && r["open"] == 1));
    assert!(rows
        .iter()
        .any(|r| r["project_id"] == project(1).to_string()
            && r["open"] == 2
            && r["max_severity"] == "blocker"));
    assert_eq!(header(&resp, "x-event-seq"), Some("3"));
}

#[tokio::test]
async fn list_and_get_with_opaque_cursors() {
    let h = harness(None);
    let d = viewer(&h, DevicePlatform::Macos);
    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(raise(&h, n, Some(project(1)), Severity::Warn).await);
    }
    let resp = send(&h, req(Method::GET, "/v1/inbox?limit=2", &d), "").await;
    assert_eq!(resp.status(), 200);
    let b = body(&resp);
    validate("InboxList", &b);
    let first: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        first,
        [ids[4].to_string(), ids[3].to_string()],
        "default order desc"
    );
    let cursor = b["next_cursor"].as_str().unwrap().to_owned();
    assert!(!cursor.contains("inbox:"), "cursor is opaque");
    // 翻页期间插入新项：不重复、不遗漏
    raise(&h, 9, Some(project(1)), Severity::Warn).await;
    let mut seen = first.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut next = Some(cursor);
    while let Some(c) = next {
        let resp = send(
            &h,
            req(Method::GET, &format!("/v1/inbox?limit=2&cursor={c}"), &d),
            "",
        )
        .await;
        let b = body(&resp);
        seen.extend(
            b["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_owned()),
        );
        next = b["next_cursor"].as_str().map(str::to_owned);
    }
    let expected: Vec<String> = ids.iter().rev().map(|i| i.to_string()).collect();
    assert_eq!(seen, expected);

    // 篡改 / 跨方向游标 → 400 invalid_cursor
    let resp = send(&h, req(Method::GET, "/v1/inbox?limit=2", &d), "").await;
    let c = body(&resp)["next_cursor"].as_str().unwrap().to_owned();
    for bad in [format!("{c}x"), "AAAA".to_owned(), "!!".to_owned()] {
        let resp = send(
            &h,
            req(Method::GET, &format!("/v1/inbox?cursor={bad}"), &d),
            "",
        )
        .await;
        assert_problem(&resp, 400, "invalid_cursor");
    }
    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/inbox?order=asc&cursor={c}"), &d),
        "",
    )
    .await;
    assert_problem(&resp, 400, "invalid_cursor");
    assert_problem(
        &send(&h, req(Method::GET, "/v1/inbox?limit=201", &d), "").await,
        400,
        "invalid_request",
    );

    // 详情
    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/inbox/{}", ids[0]), &d),
        "",
    )
    .await;
    assert_eq!(resp.status(), 200);
    validate("InboxItem", &body(&resp));
    let missing = InboxId::from_parts(T0 as u64, 1).unwrap();
    assert_problem(
        &send(
            &h,
            req(Method::GET, &format!("/v1/inbox/{missing}"), &d),
            "",
        )
        .await,
        404,
        "not_found",
    );
}

#[tokio::test]
async fn list_filters_by_status_project_and_kind() {
    let h = harness(None);
    let d = viewer(&h, DevicePlatform::Macos);
    raise(&h, 1, Some(project(1)), Severity::Warn).await;
    raise(&h, 2, Some(project(2)), Severity::Warn).await;
    let resp = send(
        &h,
        req(
            Method::GET,
            &format!("/v1/inbox?project_id={}", project(2)),
            &d,
        ),
        "",
    )
    .await;
    let items = body(&resp)["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["project_id"], project(2).to_string());
    let resp = send(&h, req(Method::GET, "/v1/inbox?status=resolved", &d), "").await;
    assert!(body(&resp)["items"].as_array().unwrap().is_empty());
    let kind = serde_json::to_value(builders::inbox_item().kind).unwrap();
    let other = if kind == json!("escalation") {
        "spec_needed"
    } else {
        "escalation"
    };
    let resp = send(
        &h,
        req(Method::GET, &format!("/v1/inbox?kind={other}"), &d),
        "",
    )
    .await;
    assert!(body(&resp)["items"].as_array().unwrap().is_empty());
}
