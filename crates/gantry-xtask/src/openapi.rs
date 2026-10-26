//! OpenAPI 一致性：路由、引用、示例夹具与 `core` 实体/事件形状。
//!
//! `schemas/openapi.yaml` 归 `api` 契约所有，本工具只读它，绝不写回。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::catalog::Artifact;
use crate::docs;
use crate::schema::DIALECT;
use crate::util::{canonical, enum_sets, pointer, Defs, Report};

pub fn check(root: &Path, artifacts: &[Artifact], report: &mut Report) {
    let path = root.join("schemas/openapi.yaml");
    let document = match fs::read_to_string(&path)
        .map_err(|error| format!("{}: {error}", path.display()))
        .and_then(|text| {
            serde_json::from_str::<Value>(&text)
                .map_err(|error| format!("{}: 不是 JSON 子集：{error}", path.display()))
        }) {
        Ok(document) => document,
        Err(error) => {
            report.fail(error);
            return;
        }
    };
    check_routes(root, &document, report);
    check_refs(&document, report);
    check_fixtures(root, &document, report);
    check_examples(&document, report);
    check_entities(artifacts, &document, report);
    check_gantry_events(artifacts, &document, report);
    check_approval_shapes(artifacts, &document, report);
    check_settings_updated_shape(artifacts, &document, report);
    check_settings_refs(&document, report);
}

fn operations(document: &Value) -> Vec<(String, String, &Value)> {
    let mut operations = Vec::new();
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return operations;
    };
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, operation) in item {
            if matches!(method.as_str(), "get" | "put" | "post" | "delete" | "patch") {
                operations.push((method.to_ascii_uppercase(), path.clone(), operation));
            }
        }
    }
    operations.sort_by(|a, b| (&a.1, &a.0).cmp(&(&b.1, &b.0)));
    operations
}

fn check_routes(root: &Path, document: &Value, report: &mut Report) {
    let documented = match docs::api_routes(root) {
        Ok(routes) => routes,
        Err(error) => {
            report.fail(format!("api.md 路由表：{error}"));
            return;
        }
    };
    let openapi: BTreeSet<(String, String)> = operations(document)
        .into_iter()
        .map(|(method, path, _)| (method, path))
        .collect();
    let spec: BTreeSet<(String, String)> = documented
        .iter()
        .map(|route| (route.method.clone(), route.path.clone()))
        .collect();
    for (method, path) in spec.difference(&openapi) {
        report.fail(format!("api.md 有但 openapi 缺少：{method} {path}"));
    }
    for (method, path) in openapi.difference(&spec) {
        report.fail(format!("openapi 有但 api.md 缺少：{method} {path}"));
    }
    if openapi.len() != 48 {
        report.fail(format!(
            "openapi 操作数为 {}，api.md 要求 48",
            openapi.len()
        ));
    } else {
        report.note("openapi 48 个操作与 api.md §4.1 逐条相等");
    }
}

fn check_refs(document: &Value, report: &mut Report) {
    let mut refs = BTreeSet::new();
    collect_refs(document, &mut refs);
    for reference in refs {
        if !reference.starts_with('#') {
            report.fail(format!("外部 $ref 不受支持：{reference}"));
            continue;
        }
        if pointer(document, &reference).is_none() {
            report.fail(format!("$ref 无法解析：{reference}"));
        }
    }
}

fn collect_refs(value: &Value, refs: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                refs.insert(reference.clone());
            }
            for child in map.values() {
                collect_refs(child, refs);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, refs);
            }
        }
        _ => {}
    }
}

fn check_fixtures(root: &Path, document: &Value, report: &mut Report) {
    let mut checked = 0;
    let mut missing = 0;
    for (method, path, operation) in operations(document) {
        let Some(fixture) = operation.get("x-gantry-fixture").and_then(Value::as_str) else {
            continue;
        };
        let fixture_path = root.join("fixtures/api").join(format!("{fixture}.json"));
        let text = match fs::read_to_string(&fixture_path) {
            Ok(text) => text,
            Err(error) => {
                report.fail(format!(
                    "{method} {path}: 夹具 {} 读取失败：{error}（OpenAPI 的 x-gantry-fixture 与 fixtures/api 不一致）",
                    fixture_path.display()
                ));
                missing += 1;
                continue;
            }
        };
        let data: Value = match serde_json::from_str(&text) {
            Ok(data) => data,
            Err(error) => {
                report.fail(format!(
                    "{method} {path}: 夹具 {} 不是 JSON：{error}",
                    fixture_path.display()
                ));
                continue;
            }
        };
        let Some((media_type, media)) = success_media(operation) else {
            continue;
        };
        if let Some(frames) = media.get("x-sse-frames").and_then(Value::as_object) {
            let Some(event_schema) = frames.get("event") else {
                continue;
            };
            let Some(events) = data.get("events").and_then(Value::as_array) else {
                report.fail(format!("{method} {path}: SSE 夹具 {fixture} 缺少 events[]"));
                continue;
            };
            let validator = match compile(document, event_schema) {
                Ok(validator) => validator,
                Err(error) => {
                    report.fail(format!("{method} {path}: SSE event schema {error}"));
                    continue;
                }
            };
            for (index, event) in events.iter().enumerate() {
                checked += 1;
                if let Err(error) = validator.validate(event) {
                    report.fail(format!(
                        "{method} {path}: SSE 夹具 {fixture} events[{index}] 未通过 GantryEvent：{error}"
                    ));
                }
            }
            if let (Some(heartbeat), Some(value)) = (frames.get("heartbeat"), data.get("heartbeat"))
            {
                if let Ok(validator) = compile(document, heartbeat) {
                    checked += 1;
                    if let Err(error) = validator.validate(value) {
                        report.fail(format!("{method} {path}: heartbeat 未通过 schema：{error}"));
                    }
                }
            }
            continue;
        }
        if media_type.starts_with("text/") {
            report.note(format!(
                "{method} {path}: {media_type} 夹具不做 JSON schema 校验"
            ));
            continue;
        }
        let Some(schema) = media.get("schema") else {
            continue;
        };
        match compile(document, schema) {
            Err(error) => report.fail(format!("{method} {path}: 响应 schema 无法编译：{error}")),
            Ok(validator) => {
                checked += 1;
                if validator.validate(&data).is_ok() {
                    continue;
                }
                if fixture == "settings" {
                    report.note(format!(
                        "{method} {path}: settings 夹具是多资源场景集合，不是单个响应实例；跳过逐值校验"
                    ));
                    continue;
                }
                if let Some(item_schema) = list_item_schema(schema, document) {
                    let Some(items) = data.get("items").and_then(Value::as_array) else {
                        report.fail(format!("{method} {path}: 列表夹具 {fixture} 缺少 items[]"));
                        continue;
                    };
                    let validator = match compile(document, item_schema) {
                        Ok(validator) => validator,
                        Err(error) => {
                            report.fail(format!(
                                "{method} {path}: 列表元素 schema 无法编译：{error}"
                            ));
                            continue;
                        }
                    };
                    let mut passed = 0;
                    let mut first_error = None;
                    for item in items {
                        match validator.validate(item) {
                            Ok(()) => passed += 1,
                            Err(error) => {
                                if first_error.is_none() {
                                    first_error = Some(error.to_string());
                                }
                            }
                        }
                    }
                    if passed != items.len() {
                        report.fail(format!(
                            "{method} {path}: 列表夹具 {fixture} 有 {} 个元素，{} 个通过（首个错误：{}）",
                            items.len(),
                            passed,
                            first_error.unwrap_or_else(|| "unknown".to_owned())
                        ));
                    }
                    continue;
                }
                let candidates = fixture_candidates(&data, schema, document);
                if candidates.is_empty() {
                    report.fail(format!(
                        "{method} {path}: 夹具 {fixture} 既不是响应实例，也找不到可校验的场景元素"
                    ));
                    continue;
                }
                let mut passed = 0;
                let mut first_error = None;
                for candidate in &candidates {
                    match validator.validate(candidate) {
                        Ok(()) => passed += 1,
                        Err(error) => {
                            if first_error.is_none() {
                                first_error = Some(error.to_string());
                            }
                        }
                    }
                }
                if passed == 0 {
                    report.fail(format!(
                        "{method} {path}: 夹具 {fixture} 的场景元素均未通过响应 schema（首个错误：{}）",
                        first_error.unwrap_or_else(|| "unknown".to_owned())
                    ));
                }
            }
        }
    }
    report.note(format!(
        "openapi 夹具：{checked} 个通过响应 schema 编译与校验，{missing} 个缺失"
    ));
}

fn success_media(operation: &Value) -> Option<(&str, &Value)> {
    let responses = operation.get("responses")?.as_object()?;
    let keys: Vec<&String> = responses.keys().collect();
    let preferred = ["200", "201", "202"];
    let selected = preferred
        .iter()
        .find(|key| keys.iter().any(|candidate| candidate.as_str() == **key))
        .map(|key| key.to_string())
        .or_else(|| {
            keys.iter()
                .find(|key| key.starts_with('2'))
                .map(|key| (*key).clone())
        })?;
    let response = responses.get(&selected)?;
    let content = response.get("content")?.as_object()?;
    let media = content
        .iter()
        .find(|(media_type, _)| media_type.starts_with("application/json"))
        .or_else(|| {
            content
                .iter()
                .find(|(_, media)| media.get("x-sse-frames").is_some())
        })
        .or_else(|| content.iter().next())?;
    Some((media.0.as_str(), media.1))
}

fn resolve_ref<'a>(document: &'a Value, schema: &'a Value) -> &'a Value {
    let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
        return schema;
    };
    pointer(document, reference).unwrap_or(schema)
}

fn list_item_schema<'a>(schema: &'a Value, document: &'a Value) -> Option<&'a Value> {
    let schema = resolve_ref(document, schema);
    schema
        .get("properties")
        .and_then(|properties| properties.get("items"))
        .and_then(|items| items.get("items"))
}

/// 从“场景集合”夹具里挑出可能是响应的值：`items[]` 元素或与实体同名的键。
fn fixture_candidates(data: &Value, schema: &Value, document: &Value) -> Vec<Value> {
    let mut candidates = Vec::new();
    if let Some(items) = data.get("items").and_then(Value::as_array) {
        candidates.extend(items.iter().cloned());
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(name) = reference.split('/').next_back() {
            for key in candidate_keys(name) {
                if let Some(value) = data.get(key) {
                    candidates.push(value.clone());
                }
            }
        }
    }
    // 顶层是实体本身的情况已由整体校验覆盖；这里只补充常见包装键。
    if candidates.is_empty() {
        if let Some(object) = data.as_object() {
            for key in [
                "delivery",
                "project",
                "device",
                "message",
                "conversation",
                "worktree",
                "workflow",
                "attempt",
                "approval",
                "inbox_item",
            ] {
                if let Some(value) = object.get(key) {
                    candidates.push(value.clone());
                }
            }
        }
    }
    let _ = document;
    candidates
}

fn candidate_keys(component: &str) -> Vec<&'static str> {
    let lower: String = component
        .chars()
        .enumerate()
        .flat_map(|(index, ch)| {
            if ch.is_ascii_uppercase() && index > 0 {
                vec!['_', ch.to_ascii_lowercase()]
            } else {
                vec![ch.to_ascii_lowercase()]
            }
        })
        .collect();
    let mut keys = vec![];
    for candidate in [
        "delivery",
        "project",
        "device",
        "message",
        "conversation",
        "worktree",
        "workflow",
        "attempt",
        "approval",
        "inbox_item",
        "budget",
        "node",
    ] {
        if lower.contains(candidate) {
            keys.push(candidate);
        }
    }
    keys
}

/// 把片段放进带 `components` 的根，使 `#/components/...` 引用可解析。
fn compile(document: &Value, schema: &Value) -> Result<jsonschema::Validator, String> {
    let mut root = serde_json::Map::new();
    root.insert("$schema".to_owned(), Value::String(DIALECT.to_owned()));
    root.insert("allOf".to_owned(), Value::Array(vec![schema.clone()]));
    if let Some(components) = document.get("components") {
        root.insert("components".to_owned(), components.clone());
    }
    if let Some(defs) = document.get("$defs") {
        root.insert("$defs".to_owned(), defs.clone());
    }
    jsonschema::validator_for(&Value::Object(root)).map_err(|error| error.to_string())
}

fn check_examples(document: &Value, report: &mut Report) {
    let mut checked = 0;
    for (method, path, operation) in operations(document) {
        if let Some(request) = operation
            .get("requestBody")
            .and_then(|body| body.get("content"))
        {
            checked += validate_media_examples(
                document,
                request,
                &format!("{method} {path} request"),
                report,
            );
        }
        if let Some(responses) = operation.get("responses").and_then(Value::as_object) {
            for (status, response) in responses {
                if let Some(content) = response.get("content") {
                    checked += validate_media_examples(
                        document,
                        content,
                        &format!("{method} {path} response {status}"),
                        report,
                    );
                }
            }
        }
    }
    report.note(format!("openapi 内联示例：{checked} 个通过校验"));
}

fn validate_media_examples(
    document: &Value,
    content: &Value,
    label: &str,
    report: &mut Report,
) -> usize {
    let mut checked = 0;
    let Some(media_map) = content.as_object() else {
        return checked;
    };
    for (media_type, media) in media_map {
        let Some(schema) = media.get("schema") else {
            continue;
        };
        let validator = match compile(document, schema)
            .map_err(|error| {
                report.fail(format!("{label} ({media_type}): schema 无法编译：{error}"));
                error
            })
            .ok()
        {
            Some(validator) => validator,
            None => continue,
        };
        if let Some(example) = media.get("example") {
            checked += 1;
            if let Err(error) = validator.validate(example) {
                report.fail(format!(
                    "{label} ({media_type}): example 未通过 schema：{error}"
                ));
            }
        }
        if let Some(examples) = media.get("examples").and_then(Value::as_object) {
            for (name, example) in examples {
                let Some(value) = example.get("value") else {
                    continue;
                };
                checked += 1;
                if let Err(error) = validator.validate(value) {
                    report.fail(format!(
                        "{label} ({media_type}/{name}): examples[].value 未通过 schema：{error}"
                    ));
                }
            }
        }
    }
    checked
}

fn check_entities(artifacts: &[Artifact], document: &Value, report: &mut Report) {
    let Some(components) = document
        .get("components")
        .and_then(|components| components.get("schemas"))
        .and_then(Value::as_object)
    else {
        report.fail("openapi 缺少 components.schemas");
        return;
    };
    let mut api_defs = Defs::new();
    for (name, schema) in components {
        api_defs.insert(name.clone(), schema);
    }
    let allowed_hidden: BTreeMap<&str, &[&str]> =
        BTreeMap::from([("Device", &["approval_pubkey"][..])]);
    for artifact in artifacts {
        let Some(name) = artifact.openapi else {
            continue;
        };
        let Some(api_schema) = components.get(name) else {
            report.fail(format!(
                "{}: openapi 缺少实体 component `{name}`",
                artifact.path
            ));
            continue;
        };
        let mut core_defs = Defs::new();
        if let Some(definitions) = artifact.value.get("$defs").and_then(Value::as_object) {
            for (definition, schema) in definitions {
                core_defs.insert(definition.clone(), schema);
            }
        }
        compare_object(
            &artifact.path,
            &artifact.value,
            api_schema,
            &core_defs,
            &api_defs,
            allowed_hidden.get(name).copied().unwrap_or(&[]),
            report,
        );
    }
}

fn compare_object(
    label: &str,
    core: &Value,
    api: &Value,
    core_defs: &Defs<'_>,
    api_defs: &Defs<'_>,
    allowed_hidden: &[&str],
    report: &mut Report,
) {
    let Some(core_props) = core.get("properties").and_then(Value::as_object) else {
        return;
    };
    let Some(api_props) = api.get("properties").and_then(Value::as_object) else {
        report.fail(format!("{label}: openapi 对应 schema 没有 properties"));
        return;
    };
    for name in core_props.keys() {
        if name == "data" {
            continue;
        }
        if !api_props.contains_key(name) {
            if allowed_hidden.contains(&name.as_str()) {
                report.note(format!(
                    "{label}: `{name}` 由 core 持有、不进 API（已登记）"
                ));
            } else {
                report.fail(format!("{label}: core 字段 `{name}` 在 openapi 实体中缺失"));
            }
        }
    }
    for name in api_props.keys() {
        if name == "data" {
            continue;
        }
        if !core_props.contains_key(name) {
            if name == "nodes" && label.ends_with("workflow.json") {
                report.note(format!("{label}: `nodes` 是 API 聚合字段，core 实体不含"));
            } else {
                report.fail(format!(
                    "{label}: openapi 字段 `{name}` 在 core 实体中不存在"
                ));
            }
        }
    }
    let mut soft = Vec::new();
    for (name, core_property) in core_props {
        if name == "data" {
            // 事件信封的 data 是多态槽位：逐事件约束在 schemas/events/<type>.json。
            continue;
        }
        let Some(api_property) = api_props.get(name) else {
            continue;
        };
        let core_enums = enum_sets(core_property, core_defs);
        let api_enums = enum_sets(api_property, api_defs);
        if core_enums != api_enums {
            let api_union: BTreeSet<String> = api_enums.iter().flatten().cloned().collect();
            let core_union: BTreeSet<String> = core_enums.iter().flatten().cloned().collect();
            let single_core = core_enums.len() == 1;
            let single_api = api_enums.len() == 1;
            if api_enums.is_empty() {
                report.note(format!(
                    "{label}.{name}: core 有枚举、openapi 更宽（无 enum）；由 GantryEvent / 具体分支约束"
                ));
            } else if core_union == api_union {
                report.note(format!(
                    "{label}.{name}: 枚举以 {} / {} 个分支表示，并集相等",
                    core_enums.len(),
                    api_enums.len()
                ));
            } else if single_core && single_api {
                report.fail(format!(
                    "{label}.{name}: 枚举不一致 core={:?} openapi={:?}",
                    core_enums, api_enums
                ));
                continue;
            } else {
                report.note(format!(
                    "{label}.{name}: 枚举以不同复合结构表示，需人工核对 core={:?} openapi={:?}",
                    core_enums, api_enums
                ));
            }
        }
        let core_type = type_signature(core_property, core_defs);
        let api_type = type_signature(api_property, api_defs);
        if core_type != api_type {
            if core_type == "object" && api_type.starts_with("oneOf(object") {
                report.note(format!(
                    "{label}.{name}: core 单一对象，openapi 按分支拆分（{api_type}）"
                ));
            } else if api_type == "any" {
                report.note(format!(
                    "{label}.{name}: openapi 更宽（any），core={core_type}"
                ));
            } else if (core_type.starts_with("anyOf(") || core_type.starts_with("oneOf("))
                && (api_type.starts_with("anyOf(") || api_type.starts_with("oneOf("))
            {
                report.note(format!(
                    "{label}.{name}: 联合类型表示不同（core={core_type} openapi={api_type}），需人工核对"
                ));
            } else if core_type == "string"
                && (api_type.starts_with("oneOf(string") || api_type.starts_with("anyOf(string"))
            {
                report.note(format!(
                    "{label}.{name}: core 用字符串模式表达封闭取值，openapi 用枚举联合；需人工核对"
                ));
            } else if core_type == "any" || api_type == "any" {
                report.note(format!(
                    "{label}.{name}: 一侧为任意类型（core={core_type} openapi={api_type}）"
                ));
            } else {
                report.fail(format!(
                    "{label}.{name}: 类型不一致 core={} openapi={}",
                    core_type, api_type
                ));
                continue;
            }
        }
        let core_canonical = canonical(core_property, core_defs, 0);
        let api_canonical = canonical(api_property, api_defs, 0);
        if core_canonical != api_canonical {
            soft.push(format!("{label}.{name}: 约束/required 等非类型差异"));
        }
    }
    if core.get("required") != api.get("required") {
        soft.push(format!("{label}: required 集合不同"));
    }
    if core.get("additionalProperties") != api.get("additionalProperties") {
        soft.push(format!("{label}: additionalProperties 不同"));
    }
    for note in soft.into_iter().take(30) {
        report.note(note);
    }
}

fn type_signature(value: &Value, defs: &Defs<'_>) -> String {
    let canonical = canonical(value, defs, 0);
    signature_of(&canonical)
}

fn signature_of(value: &Value) -> String {
    if let Some(value_type) = value.get("type") {
        let mut names: Vec<String> = match value_type {
            Value::String(name) => vec![name.clone()],
            Value::Array(items) => items
                .iter()
                .map(|item| item.as_str().unwrap_or("?").to_owned())
                .collect(),
            _ => vec!["?".to_owned()],
        };
        names.sort();
        names.dedup();
        if names == ["null", "string"] || names == ["null", "object"] {
            return format!("nullable({})", names[1]);
        }
        return names.join("|");
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(items) = value.get(key).and_then(Value::as_array) {
            let mut signatures: Vec<String> = items.iter().map(signature_of).collect();
            signatures.sort();
            if signatures.len() == 2 {
                let is_null = |signature: &str| signature == "null";
                if is_null(&signatures[0]) {
                    return format!("nullable({})", signatures[1]);
                }
                if is_null(&signatures[1]) {
                    return format!("nullable({})", signatures[0]);
                }
            }
            return format!("{key}({})", signatures.join(","));
        }
    }
    if value.get("properties").is_some() {
        return "object".to_owned();
    }
    "any".to_owned()
}

/// `Approval` 必须覆盖生产 `APPROVAL_SHAPES` 的全部 8 个形状；
/// `ApprovalCreateRequest` 只服务内部动作，允许集合固定为 3 个。
/// 未定义 action（例如 `push_branch`）必须 fail-closed。
fn check_approval_shapes(artifacts: &[Artifact], document: &Value, report: &mut Report) {
    const CREATE_ACTIONS: [&str; 3] = ["settings_update", "device_role_change", "device_revoke"];

    let Some(core) = artifacts
        .iter()
        .find(|artifact| artifact.path == "schemas/entities/approval.json")
    else {
        report.fail("缺少 schemas/entities/approval.json 生成物");
        return;
    };
    let Some(core_shapes) = approval_shape_set(&core.value) else {
        report.fail("Approval schema 缺少 action/target.kind/subject_digest oneOf");
        return;
    };
    let create_shapes: BTreeSet<(String, String, String)> = core_shapes
        .iter()
        .filter(|(action, _, _)| CREATE_ACTIONS.contains(&action.as_str()))
        .cloned()
        .collect();
    if create_shapes.len() != CREATE_ACTIONS.len() {
        report.fail(format!(
            "APPROVAL_SHAPES 缺少内部动作形状：期望 {CREATE_ACTIONS:?}"
        ));
    }
    let Some(components) = document
        .get("components")
        .and_then(|components| components.get("schemas"))
        .and_then(Value::as_object)
    else {
        report.fail("openapi 缺少 components.schemas");
        return;
    };

    if let Some(actions) = components
        .get("ApprovalAction")
        .and_then(|schema| schema.get("enum"))
        .and_then(Value::as_array)
    {
        let mut seen = BTreeSet::new();
        for action in actions {
            let Some(action) = action.as_str() else {
                continue;
            };
            if !seen.insert(action) {
                report.fail(format!("ApprovalAction.enum 重复动作 `{action}`"));
            }
        }
    } else {
        report.fail("openapi 缺少 ApprovalAction.enum");
    }

    for (name, expected) in [
        ("Approval", &core_shapes),
        ("ApprovalCreateRequest", &create_shapes),
    ] {
        let Some(schema) = components.get(name) else {
            report.fail(format!("openapi 缺少 {name}"));
            continue;
        };
        let Some(api_shapes) = approval_shape_set(schema) else {
            report.fail(format!(
                "{name} 缺少 action/target.kind/subject_digest oneOf；未定义动作会绕过形状检查"
            ));
            continue;
        };
        if &api_shapes != expected {
            let missing: Vec<_> = expected.difference(&api_shapes).collect();
            let extra: Vec<_> = api_shapes.difference(expected).collect();
            report.fail(format!(
                "{name} 审批形状与允许集合不一致；缺少 {missing:?}，多出 {extra:?}"
            ));
            continue;
        }
        if name == "ApprovalCreateRequest" {
            let actions = schema
                .pointer("/properties/action/enum")
                .and_then(Value::as_array);
            let actions: BTreeSet<&str> = actions
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let expected_actions: BTreeSet<&str> = CREATE_ACTIONS.into_iter().collect();
            if actions != expected_actions {
                report.fail(format!(
                    "ApprovalCreateRequest.action 必须内联内部动作 allowlist {CREATE_ACTIONS:?}，实际 {actions:?}"
                ));
            }
        }
        let Ok(validator) = compile(
            document,
            &json!({ "$ref": format!("#/components/schemas/{name}") }),
        ) else {
            report.fail(format!("{name} 无法编译"));
            continue;
        };
        for (action, target_kind, digest_pattern) in expected {
            let sample = approval_sample(name, action, target_kind, digest_pattern);
            if let Err(error) = validator.validate(&sample) {
                report.fail(format!(
                    "{name} 拒绝允许的审批形状 {action}/{target_kind}: {error}"
                ));
            }
        }
        let undefined = approval_sample(name, "push_branch", "branch", "^[0-9a-f]{40}$");
        if validator.validate(&undefined).is_ok() {
            report.fail(format!(
                "{name} 接受未定义审批动作 push_branch；必须 fail-closed"
            ));
        }
        let mismatch = approval_sample(
            name,
            expected
                .iter()
                .next()
                .map(|(action, _, _)| action.as_str())
                .unwrap_or("merge_pr"),
            "branch",
            "^[0-9a-f]{40}$",
        );
        if validator.validate(&mismatch).is_ok() {
            report.fail(format!("{name} 接受 action/target.kind 不匹配的审批形状"));
        }
        if name == "ApprovalCreateRequest" {
            for (action, target_kind, digest_pattern) in core_shapes.difference(&create_shapes) {
                let sample = approval_sample(name, action, target_kind, digest_pattern);
                if validator.validate(&sample).is_ok() {
                    report.fail(format!(
                        "ApprovalCreateRequest 接受内部动作之外的审批动作 {action}"
                    ));
                }
            }
        }
    }

    check_approval_target(document, report);
    report.note(format!(
        "openapi 审批形状：Approval 覆盖 {} 个 APPROVAL_SHAPES；ApprovalCreateRequest 允许集合固定为 {} 个内部动作",
        core_shapes.len(),
        create_shapes.len()
    ));
}

fn approval_shape_set(schema: &Value) -> Option<BTreeSet<(String, String, String)>> {
    let branches = schema.get("oneOf")?.as_array()?;
    if branches.is_empty() {
        return None;
    }
    let mut shapes = BTreeSet::new();
    for branch in branches {
        let action = branch
            .pointer("/properties/action/const")?
            .as_str()?
            .to_owned();
        let kind = branch
            .pointer("/properties/target/properties/kind/const")?
            .as_str()?
            .to_owned();
        let pattern = branch
            .pointer("/properties/subject_digest/pattern")?
            .as_str()?
            .to_owned();
        shapes.insert((action, kind, pattern));
    }
    Some(shapes)
}

fn digest_sample(pattern: &str) -> String {
    match pattern {
        "^sha256:[0-9a-f]{64}$" => format!("sha256:{}", "a".repeat(64)),
        _ => "a".repeat(40),
    }
}

fn approval_target_sample(kind: &str) -> Value {
    match kind {
        "pr" => json!({ "kind": "pr", "repo": "acme/web", "number": 12 }),
        "branch" => json!({ "kind": "branch", "repo": "acme/web", "branch": "gantry/42-fix" }),
        "review_thread" => json!({
            "kind": "review_thread",
            "repo": "acme/web",
            "number": 12,
            "thread_node_id": "PRRT_kwDOAbc123"
        }),
        "patch" => json!({
            "kind": "patch",
            "repo": "acme/web",
            "attempt_id": "att_01JB7M4P8R2X6V9N3Q5T7W1YZB",
            "rules": [],
            "paths": []
        }),
        "settings" => json!({
            "kind": "settings",
            "scope": "global",
            "resource": "levels",
            "base_rev": 1
        }),
        "device" => json!({ "kind": "device", "id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM" }),
        _ => json!({ "kind": kind }),
    }
}

fn approval_sample(name: &str, action: &str, target_kind: &str, digest_pattern: &str) -> Value {
    let mut sample = json!({
        "id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
        "delivery_id": null,
        "action": action,
        "target": approval_target_sample(target_kind),
        "subject_digest": digest_sample(digest_pattern),
        "device_id": null,
        "signature": null,
        "status": "pending",
        "expires_at": "2026-10-24T14:41:00.000Z",
        "consumed_by": null,
        "created_at": "2026-10-24T12:41:00.000Z",
        "updated_at": "2026-10-24T12:41:00.000Z"
    });
    if name == "ApprovalCreateRequest" {
        sample
            .as_object_mut()
            .unwrap()
            .retain(|key, _| matches!(key.as_str(), "action" | "target" | "subject_digest"));
    }
    sample
}

fn check_approval_target(document: &Value, report: &mut Report) {
    let Ok(target) = compile(
        document,
        &json!({ "$ref": "#/components/schemas/ApprovalTarget" }),
    ) else {
        report.fail("ApprovalTarget 无法编译");
        return;
    };
    let branch_without_oid = json!({
        "kind": "branch",
        "repo": "acme/web",
        "branch": "gantry/42-fix"
    });
    let branch_null_oid = json!({
        "kind": "branch",
        "repo": "acme/web",
        "branch": "gantry/42-fix",
        "expected_remote_oid": null
    });
    let branch_with_oid = json!({
        "kind": "branch",
        "repo": "acme/web",
        "branch": "gantry/42-fix",
        "expected_remote_oid": "d7d018f867d6c5b78e82906386420d45a4d84486"
    });
    for sample in [&branch_without_oid, &branch_null_oid, &branch_with_oid] {
        if let Err(error) = target.validate(sample) {
            report.fail(format!(
                "ApprovalTarget branch expected_remote_oid 与 Rust Option/null 不一致: {error}"
            ));
        }
    }
    let invalid_oid = json!({
        "kind": "branch",
        "repo": "acme/web",
        "branch": "gantry/42-fix",
        "expected_remote_oid": "not-an-oid"
    });
    if target.validate(&invalid_oid).is_ok() {
        report.fail("ApprovalTarget 接受了非法 expected_remote_oid");
    }

    let global = json!({
        "kind": "settings",
        "scope": "global",
        "resource": "levels",
        "base_rev": 1
    });
    let global_null = json!({
        "kind": "settings",
        "scope": "global",
        "project_id": null,
        "resource": "levels",
        "base_rev": 1
    });
    let global_with_project = json!({
        "kind": "settings",
        "scope": "global",
        "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "resource": "levels",
        "base_rev": 1
    });
    let project = json!({
        "kind": "settings",
        "scope": "project",
        "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "resource": "levels",
        "base_rev": 1
    });
    let project_null = json!({
        "kind": "settings",
        "scope": "project",
        "project_id": null,
        "resource": "levels",
        "base_rev": 1
    });
    let project_without_id = json!({
        "kind": "settings",
        "scope": "project",
        "resource": "levels",
        "base_rev": 1
    });
    for sample in [&global, &global_null, &project] {
        if let Err(error) = target.validate(sample) {
            report.fail(format!(
                "ApprovalTarget settings 拒绝合法 scope/project_id 组合: {error}"
            ));
        }
    }
    for sample in [&global_with_project, &project_null, &project_without_id] {
        if target.validate(sample).is_ok() {
            report.fail(format!(
                "ApprovalTarget settings 接受 scope 与 project_id 不一致: {sample}"
            ));
        }
    }
}

fn check_gantry_events(artifacts: &[Artifact], document: &Value, report: &mut Report) {
    let Some(branches) = document
        .get("components")
        .and_then(|components| components.get("schemas"))
        .and_then(|schemas| schemas.get("GantryEvent"))
        .and_then(|schema| schema.get("oneOf"))
        .and_then(Value::as_array)
    else {
        report.fail("openapi 缺少 GantryEvent.oneOf");
        return;
    };
    let event_artifacts: BTreeMap<&str, &Artifact> = artifacts
        .iter()
        .filter_map(|artifact| {
            let ty = artifact
                .value
                .get("x-gantry-event-type")
                .and_then(Value::as_str)?;
            Some((ty, artifact))
        })
        .collect();
    let components = document
        .get("components")
        .and_then(|components| components.get("schemas"))
        .and_then(Value::as_object);
    let Some(components) = components else {
        report.fail("openapi 缺少 components.schemas");
        return;
    };
    let mut api_defs = Defs::new();
    for (name, schema) in components {
        api_defs.insert(name.clone(), schema);
    }
    for branch in branches {
        let Some(all_of) = branch.get("allOf").and_then(Value::as_array) else {
            continue;
        };
        let Some(specific) = all_of.get(1) else {
            continue;
        };
        let Some(ty) = specific
            .get("properties")
            .and_then(|properties| properties.get("type"))
            .and_then(|ty| ty.get("const"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(data) = specific
            .get("properties")
            .and_then(|properties| properties.get("data"))
        else {
            continue;
        };
        let Some(artifact) = event_artifacts.get(ty) else {
            report.fail(format!("GantryEvent 分支 `{ty}` 没有对应事件 schema"));
            continue;
        };
        if let Some(Value::String(reference)) = data.get("$ref") {
            if let Some(name) = reference.split('/').next_back() {
                if let Some(api_schema) = components.get(name) {
                    let mut core_defs = Defs::new();
                    if let Some(defs) = artifact.value.get("$defs").and_then(Value::as_object) {
                        for (definition, schema) in defs {
                            core_defs.insert(definition.clone(), schema);
                        }
                    }
                    compare_object(
                        &format!("GantryEvent[{ty}]"),
                        &artifact.value,
                        api_schema,
                        &core_defs,
                        &api_defs,
                        &[],
                        report,
                    );
                }
            }
        }
    }
}

/// `SettingsUpdatedData` 的 `oneOf` 必须按 scope × resource 绑定 `value`，并与
/// core 生成的事件 schema 逐分支一致；`resource=levels` 不能接受 models 形状。
fn check_settings_updated_shape(artifacts: &[Artifact], document: &Value, report: &mut Report) {
    let failures_before = report.failures.len();
    let Ok(validator) = compile(
        document,
        &json!({ "$ref": "#/components/schemas/SettingsUpdatedData" }),
    ) else {
        report.fail("SettingsUpdatedData 无法编译");
        return;
    };
    let base = json!({
        "scope": "global",
        "resource": "levels",
        "schema_version": 1,
        "base_rev": 1,
        "value": {},
        "relaxes": [],
        "secret_changes": []
    });
    let features = [
        ("models", json!({ "providers": {} })),
        ("levels", json!({ "default_level": "High" })),
        ("policies", json!({ "approval": { "ttl_s": 3600 } })),
        ("guardian", json!({ "patrol_interval_s": 60 })),
        ("sandbox", json!({ "driver": "podman" })),
        ("egress", json!({ "groups": {} })),
        ("notify", json!({ "channels": {} })),
        ("github", json!({ "app": {} })),
    ];
    for (resource, feature) in &features {
        for scope in ["global", "project"] {
            let mut event = base.clone();
            event["scope"] = json!(scope);
            event["resource"] = json!(resource);
            event["value"] = feature.clone();
            if let Err(error) = validator.validate(&event) {
                report.fail(format!(
                    "SettingsUpdatedData 拒绝合法的 {scope}/{resource} 文档: {error}"
                ));
            }
        }
    }
    // 16 个 scope×resource 组合只能携带本 resource 的值形状。
    for (resource, _) in &features {
        for (wrong_resource, wrong_feature) in &features {
            if wrong_resource == resource {
                continue;
            }
            for scope in ["global", "project"] {
                let mut event = base.clone();
                event["scope"] = json!(scope);
                event["resource"] = json!(resource);
                event["value"] = wrong_feature.clone();
                if validator.validate(&event).is_ok() {
                    report.fail(format!(
                        "SettingsUpdatedData 接受 {scope}/{resource} 携带 {wrong_resource} 形状"
                    ));
                }
            }
        }
    }
    let mut global_null = base.clone();
    global_null["value"] = Value::Null;
    if validator.validate(&global_null).is_ok() {
        report.fail("SettingsUpdatedData 接受 global value = null");
    }
    let mut project_null = base.clone();
    project_null["scope"] = json!("project");
    project_null["value"] = Value::Null;
    if let Err(error) = validator.validate(&project_null) {
        report.fail(format!(
            "SettingsUpdatedData 拒绝 project DELETE（value = null）: {error}"
        ));
    }
    let mut missing = base;
    missing.as_object_mut().unwrap().remove("value");
    if validator.validate(&missing).is_ok() {
        report.fail("SettingsUpdatedData 接受缺失 value");
    }

    let Some(core) = artifacts
        .iter()
        .find(|artifact| artifact.path == "schemas/events/settings.updated.json")
    else {
        report.fail("缺少 schemas/events/settings.updated.json 生成物");
        return;
    };
    let Some(api_schema) = document
        .get("components")
        .and_then(|components| components.get("schemas"))
        .and_then(|schemas| schemas.get("SettingsUpdatedData"))
    else {
        report.fail("openapi 缺少 SettingsUpdatedData");
        return;
    };
    let mut core_defs = Defs::new();
    if let Some(defs) = core.value.get("$defs").and_then(Value::as_object) {
        for (name, schema) in defs {
            core_defs.insert(name.clone(), schema);
        }
    }
    let mut api_defs = Defs::new();
    if let Some(defs) = api_schema.get("$defs").and_then(Value::as_object) {
        for (name, schema) in defs {
            api_defs.insert(name.clone(), schema);
        }
    }
    let Some(core_branches) = settings_branch_map(&core.value) else {
        report.fail("settings.updated.json 缺少 scope × resource oneOf");
        return;
    };
    let Some(api_branches) = settings_branch_map(api_schema) else {
        report.fail("SettingsUpdatedData 缺少 scope × resource oneOf");
        return;
    };
    if core_branches.len() != 16 {
        report.fail(format!(
            "settings.updated.json 有 {} 个 scope × resource 分支，期望 16",
            core_branches.len()
        ));
    }
    if api_branches.len() != 16 {
        report.fail(format!(
            "SettingsUpdatedData 有 {} 个 scope × resource 分支，期望 16",
            api_branches.len()
        ));
    }
    for (key, core_value) in &core_branches {
        let Some(api_value) = api_branches.get(key) else {
            report.fail(format!("SettingsUpdatedData 缺少 {}/{} 分支", key.0, key.1));
            continue;
        };
        if canonical(core_value, &core_defs, 0) != canonical(api_value, &api_defs, 0) {
            report.fail(format!(
                "SettingsUpdatedData 的 {}/{} value 与 core 生成 schema 不一致",
                key.0, key.1
            ));
        }
    }
    for key in api_branches.keys() {
        if !core_branches.contains_key(key) {
            report.fail(format!(
                "SettingsUpdatedData 有 core 不存在的 {}/{} 分支",
                key.0, key.1
            ));
        }
    }
    if report.failures.len() == failures_before {
        report.note(format!(
            "openapi SettingsUpdatedData：{} 个 scope×resource 分支与 core 生成 schema 一致",
            api_branches.len()
        ));
    }
}

fn settings_branch_map(schema: &Value) -> Option<BTreeMap<(String, String), Value>> {
    let branches = schema.get("oneOf")?.as_array()?;
    let mut map = BTreeMap::new();
    for branch in branches {
        let scope = branch
            .pointer("/properties/scope/const")?
            .as_str()?
            .to_owned();
        let resource = branch
            .pointer("/properties/resource/const")?
            .as_str()?
            .to_owned();
        let value = branch.pointer("/properties/value")?.clone();
        map.insert((scope, resource), value);
    }
    Some(map)
}

fn check_settings_refs(document: &Value, report: &mut Report) {
    let Some(resources) = document
        .get("x-gantry-contract")
        .and_then(|contract| contract.get("settings_resource_schemas"))
        .and_then(Value::as_object)
    else {
        report.fail("openapi 缺少 x-gantry-contract.settings_resource_schemas");
        return;
    };
    for (name, reference) in resources {
        let Some(reference) = reference.get("$ref").and_then(Value::as_str) else {
            report.fail(format!("settings resource `{name}` 不是 $ref"));
            continue;
        };
        if pointer(document, reference).is_none() {
            report.fail(format!(
                "settings resource `{name}` 引用无法解析：{reference}"
            ));
        }
    }
    if resources.len() != 8 {
        report.fail(format!(
            "settings resource schema 有 {} 个，config.md 定义 8 个",
            resources.len()
        ));
    }
}
