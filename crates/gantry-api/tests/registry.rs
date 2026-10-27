//! 契约一致性：注册表与 `schemas/openapi.yaml`、api.md §4.1 逐行相等（本文 §12.2）。

mod support;

use std::collections::{BTreeMap, BTreeSet};

use gantry_api::query::{params_for, QueryType};
use gantry_api::registry::{IdemPolicy, MobileAccess, RateClass, RoleReq, ROUTES};
use gantry_api::ProblemCode;
use gantry_core::entities::DeviceRole;
use serde_json::Value;

fn openapi() -> Value {
    serde_json::from_str(support::OPENAPI).unwrap()
}

fn resolve<'a>(doc: &'a Value, v: &'a Value) -> &'a Value {
    match v.get("$ref").and_then(Value::as_str) {
        Some(r) => {
            let mut cur = doc;
            for part in r.trim_start_matches("#/").split('/') {
                cur = &cur[part];
            }
            resolve(doc, cur)
        }
        None => v,
    }
}

fn operations(doc: &Value) -> Vec<(String, String, &Value, Vec<&Value>)> {
    let mut out = Vec::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        let shared: Vec<&Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        for (method, op) in item.as_object().unwrap() {
            if method == "parameters" {
                continue;
            }
            let mut params = shared.clone();
            if let Some(own) = op.get("parameters").and_then(Value::as_array) {
                params.extend(own.iter());
            }
            out.push((method.to_uppercase(), path.clone(), op, params));
        }
    }
    out
}

fn role_str(r: RoleReq) -> &'static str {
    match r {
        RoleReq::PairToken => "none",
        RoleReq::SettingsDynamic => "settings_dynamic",
        RoleReq::Fixed(DeviceRole::Viewer) => "viewer",
        RoleReq::Fixed(DeviceRole::Operator) => "operator",
        RoleReq::Fixed(DeviceRole::Approver) => "approver",
    }
}

fn mobile_json(m: MobileAccess) -> Value {
    match m {
        MobileAccess::No => Value::Bool(false),
        MobileAccess::Yes => Value::Bool(true),
        MobileAccess::YesChooseLimited => Value::String("limited".into()),
    }
}

fn idem_str(i: IdemPolicy) -> &'static str {
    match i {
        IdemPolicy::Required => "required",
        IdemPolicy::Optional => "optional",
        IdemPolicy::NotApplicable => "not_applicable",
    }
}

#[test]
fn registry_matches_openapi_row_by_row() {
    let doc = openapi();
    let ops = operations(&doc);
    assert_eq!(ops.len(), 48);
    assert_eq!(ROUTES.len(), 48);
    let mut seen = BTreeSet::new();
    for (method, path, op, _) in &ops {
        let route = ROUTES
            .iter()
            .find(|r| r.method == method && r.pattern == path)
            .unwrap_or_else(|| panic!("OpenAPI has {method} {path} but the registry does not"));
        assert!(seen.insert(route.id), "duplicate registry row {}", route.id);
        let ctx = format!("#{} {method} {path}", route.id);
        assert_eq!(op["operationId"], route.operation_id, "{ctx}");
        assert_eq!(op["x-gantry-role"], role_str(route.min_role), "{ctx}");
        assert_eq!(op["x-gantry-mobile"], mobile_json(route.mobile), "{ctx}");
        assert_eq!(op["x-gantry-idempotency"], idem_str(route.idem), "{ctx}");
        assert_eq!(op["x-gantry-rate"], route.rate.as_str(), "{ctx}");
        match op.get("requestBody") {
            Some(rb) => {
                assert!(route.has_body(), "{ctx}: OpenAPI has a request body");
                assert_eq!(rb["x-gantry-body-limit"], route.body_limit, "{ctx}");
                let schema = &rb["content"]["application/json"]["schema"]["$ref"];
                assert_eq!(
                    schema == "#/components/schemas/EmptyRequest",
                    route.empty_body(),
                    "{ctx}"
                );
            }
            None => assert!(!route.has_body(), "{ctx}: OpenAPI has no request body"),
        }
    }
    // 反方向：注册表的每一行都在 OpenAPI 里（序号 1..=48 连续）。
    assert_eq!(seen, (1..=48).collect::<BTreeSet<u8>>());
    for (i, route) in ROUTES.iter().enumerate() {
        assert_eq!(
            route.id as usize,
            i + 1,
            "registry is ordered by contract id"
        );
    }
}

#[test]
fn mobile_subset_is_exactly_the_contract_13_routes() {
    let mobile: Vec<u8> = ROUTES
        .iter()
        .filter(|r| r.mobile.allowed())
        .map(|r| r.id)
        .collect();
    assert_eq!(mobile, [2, 3, 4, 10, 11, 12, 21, 22, 41, 42, 46, 47, 48]);
}

/// I6：路由集合是白名单；没有任何接收代码 / 命令 / 补丁 / 脚本的路由。
/// 新增路由必须同步契约并更新本快照。
#[test]
fn route_whitelist_snapshot() {
    let snapshot: Vec<String> = ROUTES
        .iter()
        .map(|r| format!("{} {}", r.method, r.pattern))
        .collect();
    let expected = [
        "POST /v1/pair",
        "GET /v1/me",
        "GET /v1/projects",
        "GET /v1/projects/{project_id}",
        "GET /v1/deliveries",
        "GET /v1/deliveries/{delivery_id}",
        "PATCH /v1/deliveries/{delivery_id}",
        "POST /v1/deliveries/{delivery_id}/abandon",
        "GET /v1/deliveries/{delivery_id}/budget",
        "GET /v1/projects/{project_id}/conversations",
        "GET /v1/conversations/{conversation_id}/messages",
        "POST /v1/conversations/{conversation_id}/messages",
        "GET /v1/worktrees",
        "GET /v1/worktrees/{worktree_id}",
        "PUT /v1/worktrees/{worktree_id}/lock",
        "DELETE /v1/worktrees/{worktree_id}/lock",
        "GET /v1/deliveries/{delivery_id}/workflows",
        "GET /v1/workflows/{workflow_id}",
        "GET /v1/nodes/{node_id}/attempts",
        "GET /v1/attempts/{attempt_id}",
        "GET /v1/inbox",
        "GET /v1/inbox/{inbox_id}",
        "POST /v1/inbox/{inbox_id}/dismiss",
        "GET /v1/approvals",
        "GET /v1/approvals/{approval_id}",
        "POST /v1/approvals",
        "GET /v1/approvals/{approval_id}/challenge",
        "POST /v1/approvals/{approval_id}/grant",
        "POST /v1/approvals/{approval_id}/deny",
        "GET /v1/settings/{resource}",
        "PUT /v1/settings/{resource}",
        "GET /v1/projects/{project_id}/settings/{resource}",
        "PUT /v1/projects/{project_id}/settings/{resource}",
        "DELETE /v1/projects/{project_id}/settings/{resource}",
        "GET /v1/settings/catalog",
        "GET /v1/settings/status",
        "GET /v1/devices",
        "PATCH /v1/devices/{device_id}",
        "PUT /v1/devices/{device_id}/role",
        "POST /v1/devices/{device_id}/revoke",
        "GET /v1/events",
        "POST /v1/inbox/{inbox_id}/choose",
        "POST /v1/deliveries/{delivery_id}/pause",
        "POST /v1/deliveries/{delivery_id}/resume",
        "POST /v1/nodes/{node_id}/retry",
        "GET /v1/inbox/summary",
        "POST /v1/devices/self/renew",
        "PUT /v1/devices/self/push-token",
    ];
    assert_eq!(snapshot, expected);
    for banned in [
        "exec", "command", "patch", "script", "run", "shell", "webhook",
    ] {
        assert!(
            snapshot
                .iter()
                .all(|s| !s.to_lowercase().contains(&format!("/{banned}"))),
            "route accepting {banned} is not allowed (I6)"
        );
    }
}

#[test]
fn rate_and_idempotency_columns_follow_the_contract() {
    let required: Vec<u8> = ROUTES
        .iter()
        .filter(|r| r.idem == IdemPolicy::Required)
        .map(|r| r.id)
        .collect();
    assert_eq!(required, [12, 26, 28, 29, 42, 43, 44, 45]);
    let optional: Vec<u8> = ROUTES
        .iter()
        .filter(|r| r.idem == IdemPolicy::Optional)
        .map(|r| r.id)
        .collect();
    assert_eq!(optional, [8, 23, 40, 47]);
    let sign: Vec<u8> = ROUTES
        .iter()
        .filter(|r| r.rate == RateClass::Sign)
        .map(|r| r.id)
        .collect();
    assert_eq!(sign, [26, 27]);
}

#[test]
fn problem_codes_match_openapi_enum() {
    let doc = openapi();
    let codes: Vec<&str> = doc["components"]["schemas"]["Problem"]["properties"]["code"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let ours: Vec<&str> = ProblemCode::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(ours, codes);
    assert_eq!(ours.len(), 30);
}

fn expected_type(schema: &Value) -> String {
    let schema_type = schema["type"].as_str().unwrap_or("");
    match schema_type {
        "integer" => format!(
            "int:{}:{}",
            schema["minimum"].as_u64().unwrap_or(0),
            schema["maximum"]
                .as_u64()
                .unwrap_or(gantry_api::query::MAX_SAFE_INT)
        ),
        "boolean" => "bool".into(),
        "array" => format!("list:{}", schema["items"]["enum"]),
        "string" if schema.get("enum").is_some() => format!("enum:{}", schema["enum"]),
        "string" if schema.get("pattern").is_some() => {
            let p = schema["pattern"].as_str().unwrap();
            format!("id:{}", &p[1..p.find('_').unwrap()])
        }
        _ => "str".into(),
    }
}

fn our_type(t: QueryType) -> String {
    match t {
        QueryType::Int { min, max } => format!("int:{min}:{max}"),
        QueryType::Bool => "bool".into(),
        QueryType::Str => "str".into(),
        QueryType::Id(p) => format!("id:{p}"),
        QueryType::Enum(v) => format!("enum:{}", serde_json::to_string(v).unwrap()),
        QueryType::EnumList(v) => format!("list:{}", serde_json::to_string(v).unwrap()),
    }
}

#[test]
fn query_params_match_openapi() {
    let doc = openapi();
    for (method, path, op, params) in operations(&doc) {
        let op_id = op["operationId"].as_str().unwrap();
        let mut theirs = BTreeMap::new();
        for p in params {
            let p = resolve(&doc, p);
            if p["in"] != "query" {
                continue;
            }
            let schema = resolve(&doc, &p["schema"]);
            let repeatable = p["x-gantry-repeatable"].as_bool().unwrap_or(false);
            theirs.insert(
                p["name"].as_str().unwrap().to_owned(),
                (expected_type(schema), repeatable),
            );
        }
        let ours: BTreeMap<String, (String, bool)> = params_for(op_id)
            .iter()
            .map(|p| (p.name.to_owned(), (our_type(p.ty), p.repeatable)))
            .collect();
        assert_eq!(ours, theirs, "{method} {path} ({op_id})");
    }
}
