//! `api_request` 路由表（desktop-shell.md §12"IPC 白名单与参数"）。

use std::collections::BTreeSet;
use std::path::PathBuf;

use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::routes::{prepare, shape_response, Contract, RouteClass, ROUTES};
use serde_json::{json, Value};

const PRJ: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const DLV: &str = "dlv_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const CNV: &str = "cnv_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const APR: &str = "apr_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const KEY: &str = "idem_0123456789abcdef";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..")
}

#[test]
fn every_openapi_route_is_classified_exactly_once() {
    let text = std::fs::read_to_string(repo_root().join("schemas/openapi.yaml")).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    let mut in_doc = Vec::new();
    for item in doc["paths"].as_object().unwrap().values() {
        for (method, op) in item.as_object().unwrap() {
            if ["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                in_doc.push(op["operationId"].as_str().unwrap().to_string());
            }
        }
    }
    let table: BTreeSet<&str> = ROUTES.iter().map(|r| r.operation_id).collect();
    assert_eq!(
        in_doc.len(),
        48,
        "路由总数变了：新增路由必须先在 ROUTES 里分类"
    );
    assert_eq!(table.len(), 48, "ROUTES 里有重复的 operationId");
    assert_eq!(
        table,
        in_doc.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        "ROUTES 与 openapi 不一致（缺失或多余）"
    );
    for (i, r) in ROUTES.iter().enumerate() {
        assert_eq!(r.number as usize, i + 1);
    }
    let count = |f: fn(&RouteClass) -> bool| ROUTES.iter().filter(|r| f(&r.class)).count();
    assert_eq!(count(|c| matches!(c, RouteClass::Ui(_))), 36);
    assert_eq!(count(|c| matches!(c, RouteClass::Shell)), 11);
    assert_eq!(count(|c| matches!(c, RouteClass::Unused)), 1);
    // 嵌入的契约与仓库里的文件是同一份。
    let embedded: BTreeSet<&str> = Contract::get().operation_ids().collect();
    assert_eq!(embedded, in_doc.iter().map(String::as_str).collect());
}

#[test]
fn ui_route_ids_match_the_frontend() {
    let ts = std::fs::read_to_string(repo_root().join("apps/desktop/src/api/routes.ts")).unwrap();
    let start = ts.find("export type RouteId =").unwrap();
    let end = start + ts[start..].find(';').unwrap();
    let frontend: BTreeSet<String> = ts[start..end]
        .split('|')
        .filter_map(|part| part.split('"').nth(1).map(str::to_string))
        .collect();
    let shell: BTreeSet<String> = ROUTES
        .iter()
        .filter_map(|r| match r.class {
            RouteClass::Ui(id) => Some(id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(frontend, shell);
}

#[test]
fn valid_requests_are_prepared() {
    let req = prepare("me.get", "/v1/me", None, None, None).unwrap();
    assert_eq!((req.method, req.path_and_query.as_str()), ("GET", "/v1/me"));

    let req = prepare(
        "deliveries.list",
        "/v1/deliveries",
        Some(&json!({"project_id": PRJ, "stage": ["plan", "review"], "limit": 20, "active": true})),
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        req.path_and_query,
        format!("/v1/deliveries?active=true&limit=20&project_id={PRJ}&stage=plan&stage=review")
    );

    let req = prepare(
        "messages.post",
        &format!("/v1/conversations/{CNV}/messages"),
        None,
        Some(&json!({"text": "你好", "refs": []})),
        Some(KEY),
    )
    .unwrap();
    assert_eq!(req.method, "POST");
    assert_eq!(req.idempotency_key.as_deref(), Some(KEY));
    assert_eq!(
        serde_json::from_slice::<Value>(&req.body.unwrap()).unwrap()["text"],
        "你好"
    );

    // 可选幂等键：给不给都行。
    prepare(
        "deliveries.abandon",
        &format!("/v1/deliveries/{DLV}/abandon"),
        None,
        Some(&json!({})),
        None,
    )
    .ok();
}

fn code_and_field(
    result: Result<
        gantry_desktop_shell::routes::PreparedRequest,
        gantry_desktop_shell::ipc::ShellError,
    >,
) -> (ShellErrorCode, Option<String>) {
    let e = result.unwrap_err();
    (e.code, e.field)
}

#[test]
fn rejected_requests_never_get_prepared() {
    use ShellErrorCode::*;
    let bad = |f: &str| (BadArgument, Some(f.to_string()));
    let cases: Vec<(Result<_, _>, (ShellErrorCode, Option<String>))> = vec![
        // 未知路由。
        (
            prepare("projects.delete", "/v1/projects", None, None, None),
            (UnknownRoute, None),
        ),
        // 路径参数形态错误、路径与路由不符、路径穿越与编码。
        (
            prepare("projects.get", "/v1/projects/prj_bad", None, None, None),
            bad("path"),
        ),
        (
            prepare(
                "projects.get",
                &format!("/v1/projects/{DLV}"),
                None,
                None,
                None,
            ),
            bad("path"),
        ),
        (
            prepare("projects.get", "/v1/me", None, None, None),
            bad("path"),
        ),
        (
            prepare(
                "projects.get",
                &format!("/v1/projects/{PRJ}/../x"),
                None,
                None,
                None,
            ),
            bad("path"),
        ),
        (
            prepare(
                "projects.get",
                &format!("/v1/projects/{PRJ}%2F"),
                None,
                None,
                None,
            ),
            bad("path"),
        ),
        (
            prepare("projects.list", "/v1/projects?limit=1", None, None, None),
            bad("path"),
        ),
        (
            prepare("settings.get", "/v1/settings/catalog", None, None, None),
            bad("path"),
        ),
        // 未知查询参数、查询参数值不合 schema。
        (
            prepare(
                "projects.list",
                "/v1/projects",
                Some(&json!({"evil": 1})),
                None,
                None,
            ),
            bad("query.evil"),
        ),
        (
            prepare(
                "projects.list",
                "/v1/projects",
                Some(&json!({"limit": 1000})),
                None,
                None,
            ),
            bad("query.limit"),
        ),
        (
            prepare(
                "deliveries.list",
                "/v1/deliveries",
                Some(&json!({"stage": ["nope"]})),
                None,
                None,
            ),
            bad("query.stage"),
        ),
        (
            prepare(
                "projects.list",
                "/v1/projects",
                Some(&json!(["limit"])),
                None,
                None,
            ),
            bad("query"),
        ),
        // 请求体：schema 外字段、缺必填、不该有体、过大。
        (
            prepare(
                "messages.post",
                &format!("/v1/conversations/{CNV}/messages"),
                None,
                Some(&json!({"text": "x", "extra": 1})),
                Some(KEY),
            ),
            bad("body"),
        ),
        (
            prepare(
                "messages.post",
                &format!("/v1/conversations/{CNV}/messages"),
                None,
                None,
                Some(KEY),
            ),
            bad("body"),
        ),
        (
            prepare("me.get", "/v1/me", None, Some(&json!({})), None),
            bad("body"),
        ),
        (
            prepare(
                "messages.post",
                &format!("/v1/conversations/{CNV}/messages"),
                None,
                Some(&json!({"text": "x".repeat(70_000)})),
                Some(KEY),
            ),
            (TooLarge, None),
        ),
        // 幂等键：必带却缺失、形态错误、不该带。
        (
            prepare(
                "messages.post",
                &format!("/v1/conversations/{CNV}/messages"),
                None,
                Some(&json!({"text": "x"})),
                None,
            ),
            bad("idempotency_key"),
        ),
        (
            prepare(
                "messages.post",
                &format!("/v1/conversations/{CNV}/messages"),
                None,
                Some(&json!({"text": "x"})),
                Some("short"),
            ),
            bad("idempotency_key"),
        ),
        (
            prepare(
                "messages.post",
                &format!("/v1/conversations/{CNV}/messages"),
                None,
                Some(&json!({"text": "x"})),
                Some("has space 0123456789"),
            ),
            bad("idempotency_key"),
        ),
        (
            prepare("me.get", "/v1/me", None, None, Some(KEY)),
            bad("idempotency_key"),
        ),
    ];
    for (i, (result, want)) in cases.into_iter().enumerate() {
        assert_eq!(code_and_field(result), want, "case {i}");
    }
}

#[test]
fn shell_only_routes_are_unreachable_from_ui() {
    for id in [
        "pair",
        "pairDevice",
        "approvals.create",
        "approvals.challenge",
        "approvals.grant",
        "settings.put",
        "project_settings.put",
        "project_settings.delete",
        "devices.role",
        "devices.revoke",
        "events",
        "events.stream",
        "devices.renew",
        "devices.push_token",
    ] {
        assert_eq!(
            code_and_field(prepare(id, "/v1/me", None, None, None)).0,
            ShellErrorCode::UnknownRoute,
            "{id}"
        );
    }
    // 用 UI 路由 ID 夹带外壳专用路径也不行。
    for (id, path) in [
        ("approvals.get", format!("/v1/approvals/{APR}/challenge")),
        ("approvals.deny", format!("/v1/approvals/{APR}/grant")),
        (
            "settings.get",
            "/v1/settings/models/../../devices/self/renew".to_string(),
        ),
        ("devices.list", "/v1/events".to_string()),
    ] {
        assert_eq!(
            code_and_field(prepare(id, &path, None, None, Some(KEY))).0,
            ShellErrorCode::BadArgument,
            "{id} {path}"
        );
    }
}

#[test]
fn responses_are_shaped() {
    let headers = vec![
        ("ETag".to_string(), "\"rev-7\"".to_string()),
        ("X-Event-Seq".to_string(), "42".to_string()),
        ("Idempotent-Replayed".to_string(), "true".to_string()),
        ("X-Log-Epoch".to_string(), "3".to_string()),
        ("Set-Cookie".to_string(), "x=y".to_string()),
        ("X-Request-Id".to_string(), "req_1".to_string()),
    ];
    let ok = shape_response(200, &headers, br#"{"id":"prj_1"}"#);
    assert_eq!(
        ok,
        json!({"ok": true, "status": 200, "headers": {"etag": "\"rev-7\"", "x_event_seq": 42, "idempotent_replayed": true}, "body": {"id": "prj_1"}})
    );
    assert_eq!(shape_response(204, &[], b"")["body"], Value::Null);

    let problem = br#"{"type":"about:blank","code":"stale_head","title":"Stale","request_id":"req_9","detail":"internal path /var/lib/x","current_head_sha":"abc"}"#;
    let err = shape_response(409, &headers, problem);
    assert_eq!(err["ok"], false);
    assert_eq!(err["status"], 409);
    assert!(err["problem"].get("detail").is_none(), "detail 不得过 IPC");
    assert_eq!(err["problem"]["code"], "stale_head");
    assert_eq!(err["problem"]["current_head_sha"], "abc");
    assert!(err.get("headers").is_none());
    assert!(!err.to_string().contains("/var/lib"));

    let garbage = shape_response(502, &[], b"<html>bad gateway</html>");
    assert_eq!(garbage["problem"]["code"], "http_502");
}

#[test]
fn operation_lookup_for_logging() {
    use gantry_desktop_shell::routes::operation_for;
    assert_eq!(operation_for("GET", "/v1/me"), "getMe");
    assert_eq!(
        operation_for("PUT", "/v1/settings/models?dry_run=true"),
        "putSettings"
    );
    assert_eq!(operation_for("POST", "/v1/approvals"), "createApproval");
    assert_eq!(operation_for("GET", "/v1/approvals"), "listApprovals");
    assert_eq!(
        operation_for("POST", &format!("/v1/approvals/{APR}/grant")),
        "grantApproval"
    );
    assert_eq!(
        operation_for("POST", "/v1/devices/self/renew"),
        "renewDeviceCertificate"
    );
    assert_eq!(operation_for("DELETE", "/v1/me"), "other");
}
