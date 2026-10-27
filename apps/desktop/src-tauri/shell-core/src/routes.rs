//! `api_request` 的路由表、参数校验与响应整形（desktop-shell.md §6.4、§7.1，api.md §3、§4.1）。
//!
//! - 48 条路由逐条分入"UI 可调 / 外壳专用 / 桌面不用"三类（[`ROUTES`]，OQ-S12 的临时处理：本模块自维护，
//!   由 `tests/routes.rs` 与 `schemas/openapi.yaml` 对账）。
//! - 参数与请求体按**编译时嵌入**的 `openapi.yaml` 校验：路径参数、查询参数逐个按其 schema，
//!   请求体按 `requestBody` 的 schema（`additionalProperties: false` 由契约给出）；不合格的请求不会发出。
//! - 响应只返回 `ApiResult`：`detail` 丢弃，响应头只留四项白名单。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Map, Value};

use crate::ipc::{ShellError, ShellErrorCode};

/// 编译时嵌入的契约；与服务端是同一份文件。
const OPENAPI: &str = include_str!("../../../../../schemas/openapi.yaml");
/// api.md §3.7：请求体上限。
pub const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteClass {
    /// 经 `api_request` 可调，值为路由 ID（`<资源>.<动作>`，与 `src/api/routes.ts` 共用）。
    Ui(&'static str),
    /// 只由外壳内部调用（配对、续期、审批签名链、受保护写入、事件流）。
    Shell,
    /// 桌面端不用（#48 推送令牌，P-77）。
    Unused,
}

#[derive(Clone, Copy, Debug)]
pub struct RouteSpec {
    /// api.md §4.1 路由总表编号。
    pub number: u8,
    pub operation_id: &'static str,
    pub class: RouteClass,
}

use RouteClass::{Shell, Ui, Unused};

/// §6.4：48 条路由，恰好各分入一类。
pub const ROUTES: [RouteSpec; 48] = [
    r(1, "pairDevice", Shell),
    r(2, "getMe", Ui("me.get")),
    r(3, "listProjects", Ui("projects.list")),
    r(4, "getProject", Ui("projects.get")),
    r(5, "listDeliveries", Ui("deliveries.list")),
    r(6, "getDelivery", Ui("deliveries.get")),
    r(7, "patchDelivery", Ui("deliveries.patch")),
    r(8, "abandonDelivery", Ui("deliveries.abandon")),
    r(9, "getDeliveryBudget", Ui("deliveries.budget")),
    r(10, "listConversations", Ui("conversations.list")),
    r(11, "listMessages", Ui("messages.list")),
    r(12, "postMessage", Ui("messages.post")),
    r(13, "listWorktrees", Ui("worktrees.list")),
    r(14, "getWorktree", Ui("worktrees.get")),
    r(15, "acquireWorktreeLock", Ui("worktrees.lock_put")),
    r(16, "releaseWorktreeLock", Ui("worktrees.lock_delete")),
    r(17, "listDeliveryWorkflows", Ui("workflows.list")),
    r(18, "getWorkflow", Ui("workflows.get")),
    r(19, "listNodeAttempts", Ui("attempts.list")),
    r(20, "getAttempt", Ui("attempts.get")),
    r(21, "listInbox", Ui("inbox.list")),
    r(22, "getInboxItem", Ui("inbox.get")),
    r(23, "dismissInboxItem", Ui("inbox.dismiss")),
    r(24, "listApprovals", Ui("approvals.list")),
    r(25, "getApproval", Ui("approvals.get")),
    r(26, "createApproval", Shell),
    r(27, "getApprovalChallenge", Shell),
    r(28, "grantApproval", Shell),
    r(29, "denyApproval", Ui("approvals.deny")),
    r(30, "getSettings", Ui("settings.get")),
    r(31, "putSettings", Shell),
    r(32, "getProjectSettings", Ui("project_settings.get")),
    r(33, "putProjectSettings", Shell),
    r(34, "deleteProjectSettings", Shell),
    r(35, "getSettingsCatalog", Ui("settings.catalog")),
    r(36, "getSettingsStatus", Ui("settings.status")),
    r(37, "listDevices", Ui("devices.list")),
    r(38, "renameDevice", Ui("devices.rename")),
    r(39, "setDeviceRole", Shell),
    r(40, "revokeDevice", Shell),
    r(41, "streamEvents", Shell),
    r(42, "chooseInboxOption", Ui("inbox.choose")),
    r(43, "pauseDelivery", Ui("deliveries.pause")),
    r(44, "resumeDelivery", Ui("deliveries.resume")),
    r(45, "retryNode", Ui("nodes.retry")),
    r(46, "getInboxSummary", Ui("inbox.summary")),
    r(47, "renewDeviceCertificate", Shell),
    r(48, "putPushToken", Unused),
];

const fn r(number: u8, operation_id: &'static str, class: RouteClass) -> RouteSpec {
    RouteSpec {
        number,
        operation_id,
        class,
    }
}

// ---- 契约 ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idempotency {
    Required,
    Optional,
    NotApplicable,
}

/// 由 openapi 解析出的一条操作。
pub struct Operation {
    pub method: &'static str,
    pub template: String,
    pub idempotency: Idempotency,
    path_params: HashMap<String, Arc<jsonschema::Validator>>,
    query_params: HashMap<String, (Arc<jsonschema::Validator>, bool)>,
    body: Option<(Arc<jsonschema::Validator>, bool)>,
}

pub struct Contract {
    operations: HashMap<String, Operation>,
}

impl Contract {
    /// 进程内只解析一次。
    pub fn get() -> &'static Contract {
        static CONTRACT: OnceLock<Contract> = OnceLock::new();
        CONTRACT.get_or_init(|| Contract::parse(OPENAPI).expect("嵌入的 openapi.yaml 可解析"))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let doc: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let components = doc["components"].clone();
        let compile = |schema: &Value| -> Result<Arc<jsonschema::Validator>, String> {
            let root = json!({ "allOf": [schema], "components": components });
            jsonschema::validator_for(&root)
                .map(Arc::new)
                .map_err(|e| e.to_string())
        };
        let mut operations = HashMap::new();
        let paths = doc["paths"].as_object().ok_or("缺少 paths")?;
        for (template, item) in paths {
            for (method, op) in item.as_object().ok_or("path 项不是对象")? {
                let method: &'static str = match method.as_str() {
                    "get" => "GET",
                    "post" => "POST",
                    "put" => "PUT",
                    "patch" => "PATCH",
                    "delete" => "DELETE",
                    _ => continue,
                };
                let id = op["operationId"].as_str().ok_or("缺少 operationId")?;
                let idempotency = match op["x-gantry-idempotency"].as_str() {
                    Some("required") => Idempotency::Required,
                    Some("optional") => Idempotency::Optional,
                    _ => Idempotency::NotApplicable,
                };
                let mut path_params = HashMap::new();
                let mut query_params = HashMap::new();
                for p in op["parameters"].as_array().into_iter().flatten() {
                    let name = p["name"].as_str().ok_or("参数缺少 name")?.to_string();
                    let validator = compile(&p["schema"])?;
                    let is_array = p["schema"]["type"] == "array";
                    match p["in"].as_str() {
                        Some("path") => {
                            path_params.insert(name, validator);
                        }
                        Some("query") => {
                            query_params.insert(name, (validator, is_array));
                        }
                        _ => {}
                    }
                }
                let body = match op.get("requestBody") {
                    Some(rb) => Some((
                        compile(&rb["content"]["application/json"]["schema"])?,
                        rb["required"].as_bool().unwrap_or(false),
                    )),
                    None => None,
                };
                operations.insert(
                    id.to_string(),
                    Operation {
                        method,
                        template: template.clone(),
                        idempotency,
                        path_params,
                        query_params,
                        body,
                    },
                );
            }
        }
        Ok(Self { operations })
    }

    pub fn operation(&self, operation_id: &str) -> Option<&Operation> {
        self.operations.get(operation_id)
    }

    pub fn operation_ids(&self) -> impl Iterator<Item = &str> {
        self.operations.keys().map(String::as_str)
    }

    pub fn query_keys(&self, operation_id: &str) -> Vec<&str> {
        self.operations
            .get(operation_id)
            .map(|op| op.query_params.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

// ---- 请求 ----

/// 校验通过、可由外壳发出的请求。头由外壳设置：`Content-Type` / `Accept`，以及可选的 `Idempotency-Key`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedRequest {
    pub operation_id: &'static str,
    pub method: &'static str,
    /// 路径加查询串，例如 `/v1/deliveries?project_id=prj_…&stage=plan`。
    pub path_and_query: String,
    pub body: Option<Vec<u8>>,
    pub idempotency_key: Option<String>,
}

/// UI 可调路由的规格；外壳专用与桌面不用的路由对 UI 而言不存在。
pub fn ui_route(route_id: &str) -> Option<&'static RouteSpec> {
    ROUTES
        .iter()
        .find(|r| matches!(r.class, Ui(id) if id == route_id))
}

/// `api_request` 的全部本地校验（§7.1）。任一项不合格 → 错误，请求不会到达服务端。
pub fn prepare(
    route_id: &str,
    path: &str,
    query: Option<&Value>,
    body: Option<&Value>,
    idempotency_key: Option<&str>,
) -> Result<PreparedRequest, ShellError> {
    let spec = ui_route(route_id)
        .ok_or_else(|| ShellError::new(ShellErrorCode::UnknownRoute, "未知路由"))?;
    let op = Contract::get()
        .operation(spec.operation_id)
        .expect("路由表与契约对账过");

    check_path(op, path)?;
    let query_string = encode_query(op, query)?;
    let body = check_body(op, body)?;
    let idempotency_key = check_idempotency(op, idempotency_key)?;

    let mut path_and_query = path.to_string();
    if !query_string.is_empty() {
        path_and_query.push('?');
        path_and_query.push_str(&query_string);
    }
    Ok(PreparedRequest {
        operation_id: spec.operation_id,
        method: op.method,
        path_and_query,
        body,
        idempotency_key,
    })
}

fn check_path(op: &Operation, path: &str) -> Result<(), ShellError> {
    let bad = || ShellError::bad_argument("path");
    if path.len() > 512 || path.contains(['?', '#', '%', '\\']) {
        return Err(bad());
    }
    let want: Vec<&str> = op.template.split('/').collect();
    let got: Vec<&str> = path.split('/').collect();
    if want.len() != got.len() {
        return Err(bad());
    }
    for (w, g) in want.iter().zip(&got) {
        match w.strip_prefix('{').and_then(|w| w.strip_suffix('}')) {
            Some(name) => {
                let validator = op.path_params.get(name).ok_or_else(bad)?;
                if !validator.is_valid(&Value::String(g.to_string())) {
                    return Err(bad());
                }
            }
            None if w == g => {}
            None => return Err(bad()),
        }
    }
    Ok(())
}

fn encode_query(op: &Operation, query: Option<&Value>) -> Result<String, ShellError> {
    let Some(query) = query else {
        return Ok(String::new());
    };
    let object = query
        .as_object()
        .ok_or_else(|| ShellError::bad_argument("query"))?;
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    // 按键排序，生成稳定的查询串。
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort();
    for key in keys {
        let value = &object[key];
        let field = || ShellError::bad_argument(format!("query.{key}"));
        let (validator, is_array) = op.query_params.get(key.as_str()).ok_or_else(field)?;
        if !validator.is_valid(value) {
            return Err(field());
        }
        let items: Vec<&Value> = if *is_array {
            value.as_array().ok_or_else(field)?.iter().collect()
        } else {
            vec![value]
        };
        for item in items {
            let text = match item {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return Err(field()),
            };
            out.append_pair(key, &text);
        }
    }
    Ok(out.finish())
}

fn check_body(op: &Operation, body: Option<&Value>) -> Result<Option<Vec<u8>>, ShellError> {
    match (&op.body, body) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(ShellError::bad_argument("body")),
        (Some((_, true)), None) => Err(ShellError::bad_argument("body")),
        (Some(_), None) => Ok(None),
        (Some((validator, _)), Some(value)) => {
            let bytes = serde_json::to_vec(value).map_err(|_| ShellError::bad_argument("body"))?;
            if bytes.len() > MAX_BODY_BYTES {
                return Err(ShellError::new(
                    ShellErrorCode::TooLarge,
                    "请求体超过 64 KiB",
                ));
            }
            if !validator.is_valid(value) {
                return Err(ShellError::bad_argument("body"));
            }
            Ok(Some(bytes))
        }
    }
}

fn check_idempotency(op: &Operation, key: Option<&str>) -> Result<Option<String>, ShellError> {
    let bad = || ShellError::bad_argument("idempotency_key");
    match (op.idempotency, key) {
        (Idempotency::Required, None) => Err(bad()),
        (Idempotency::NotApplicable, Some(_)) => Err(bad()),
        (_, None) => Ok(None),
        (_, Some(k)) => {
            let ok = (16..=64).contains(&k.len())
                && k.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            if ok {
                Ok(Some(k.to_string()))
            } else {
                Err(bad())
            }
        }
    }
}

// ---- 响应 ----

/// 只保留四个响应头（§7.1"响应整形"）。`X-Log-Epoch` 由外壳自己比对，不转给 UI。
pub fn shape_response(status: u16, headers: &[(String, String)], body: &[u8]) -> Value {
    if (200..300).contains(&status) {
        let mut h = Map::new();
        for (name, value) in headers {
            match name.to_ascii_lowercase().as_str() {
                "etag" => {
                    h.insert("etag".into(), Value::String(value.clone()));
                }
                "x-event-seq" => {
                    if let Ok(n) = value.trim().parse::<u64>() {
                        h.insert("x_event_seq".into(), json!(n));
                    }
                }
                "idempotent-replayed" => {
                    h.insert(
                        "idempotent_replayed".into(),
                        json!(value.trim().eq_ignore_ascii_case("true")),
                    );
                }
                "retry-after" => {
                    if let Ok(n) = value.trim().parse::<u64>() {
                        h.insert("retry_after".into(), json!(n));
                    }
                }
                _ => {}
            }
        }
        let body = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(body).unwrap_or(Value::Null)
        };
        return json!({ "ok": true, "status": status, "headers": h, "body": body });
    }
    let mut problem = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    // `detail` 只写日志、不过 IPC（api.md §1.5、§3.1）。
    problem.remove("detail");
    for key in ["code", "title", "request_id"] {
        if !problem.get(key).is_some_and(Value::is_string) {
            problem.insert(key.into(), Value::String(String::new()));
        }
    }
    if problem["code"] == "" {
        problem.insert("code".into(), Value::String(format!("http_{status}")));
    }
    json!({ "ok": false, "status": status, "problem": problem })
}

/// 网络层失败（无 HTTP 响应）。
pub fn network_failure(kind: &str) -> Value {
    json!({ "ok": false, "network": kind })
}
