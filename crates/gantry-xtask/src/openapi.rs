//! OpenAPI 一致性：路由、引用、示例夹具与 `core` 实体/事件形状。
//!
//! `schemas/openapi.yaml` 归 `api` 契约所有，本工具只读它，绝不写回。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::Value;

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
