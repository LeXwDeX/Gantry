//! JSON Schema 2020-12 生成：只从 `gantry-core` 的真实 Rust 类型派生。

use schemars::generate::SchemaGenerator;
use schemars::JsonSchema;
use serde_json::{Map, Value};

pub const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

pub fn generator() -> SchemaGenerator {
    schemars::generate::SchemaSettings::draft2020_12().into_generator()
}

/// 以 2020-12 方言生成根 schema，并补上生成器默认不带的本仓元数据。
pub fn for_type<T: ?Sized + JsonSchema>() -> Value {
    let mut value = generator().into_root_schema_for::<T>().to_value();
    finish(&mut value);
    value
}

pub fn finish(value: &mut Value) {
    if let Value::Object(object) = value {
        object
            .entry("$schema".to_owned())
            .or_insert_with(|| Value::String(DIALECT.to_owned()));
        object.insert(
            "x-gantry-dialect".to_owned(),
            Value::String(DIALECT.to_owned()),
        );
    }
}

pub fn annotate(value: &mut Value, key: &str, annotation: Value) {
    if let Value::Object(object) = value {
        object.insert(key.to_owned(), annotation);
    }
}

/// 把 tagged enum（`#[serde(tag = "kind")]`）收窄到单个 variant。
///
/// 用于 per-role 的 `output.<role>.schema.json` 与 `forge.<kind>.json`：形状仍来自
/// 真实类型的联合 schema，只按 tag 常量选择分支，不手写字段。
pub fn narrow_tagged_variant(value: &Value, tag: &str, wanted: &str) -> Option<Value> {
    let object = value.as_object()?;
    let variants = object
        .get("oneOf")
        .or_else(|| object.get("anyOf"))?
        .as_array()?;
    for variant in variants {
        let Some(properties) = variant.get("properties").and_then(Value::as_object) else {
            continue;
        };
        let Some(tag_schema) = properties.get(tag) else {
            continue;
        };
        let matches = tag_schema
            .get("const")
            .and_then(Value::as_str)
            .map(|value| value == wanted)
            .unwrap_or(false);
        if matches {
            return Some(variant.clone());
        }
    }
    None
}

/// 仅用于记录契约缺口：把已由文档明确声明“形状未定义”的字段替换为拒绝型子 schema。
///
/// 这不是补形状；它把“不猜测”落成机器可校验的拒绝。
pub fn reject_property(schema: &mut Value, property: &str, reason: &str, source: &str) {
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    let mut rejection = Map::new();
    rejection.insert("not".to_owned(), Value::Object(Map::new()));
    rejection.insert(
        "x-gantry-unresolved".to_owned(),
        serde_json::json!({
            "reason": reason,
            "source": source,
            "rejects_all_values": true,
        }),
    );
    properties.insert(property.to_owned(), Value::Object(rejection));
}

pub fn render(value: &Value) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(value).expect("schema is serializable");
    text.push('\n');
    text.into_bytes()
}
