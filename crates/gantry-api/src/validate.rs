//! 请求体 schema 校验（管线步骤 12，本文 §7.5，I7 的执行点）。
//!
//! 直接用契约 `schemas/openapi.yaml` 里的请求体组件校验，保证与 Mock API 同源：
//! 未知字段 → `unknown_field`（指针指向该字段，绝不静默忽略），缺字段 →
//! `required`，类型 → `type`，枚举 / 常量 → `enum`，长度 / 范围 → `range`。
//! 校验通过后处理程序再反序列化为强类型。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use jsonschema::error::ValidationErrorKind;
use jsonschema::Validator;
use serde_json::{json, Value};

use crate::problem::{FieldError, Problem};

const OPENAPI: &str = include_str!("../../../schemas/openapi.yaml");

fn document() -> &'static Value {
    static DOC: OnceLock<Value> = OnceLock::new();
    DOC.get_or_init(|| serde_json::from_str(OPENAPI).expect("openapi.yaml is JSON"))
}

fn validator(component: &str) -> Arc<Validator> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Validator>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
    cache
        .entry(component.to_owned())
        .or_insert_with(|| {
            let root = json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "allOf": [{ "$ref": format!("#/components/schemas/{component}") }],
                "components": document()["components"],
            });
            Arc::new(jsonschema::validator_for(&root).expect("contract schema compiles"))
        })
        .clone()
}

/// 用 `#/components/schemas/<component>` 校验请求体。
pub fn body(component: &str, value: &Value) -> Result<(), Problem> {
    let validator = validator(component);
    let mut errors: Vec<FieldError> = Vec::new();
    for error in validator.iter_errors(value) {
        let pointer = error.instance_path().to_string();
        match error.kind() {
            ValidationErrorKind::AdditionalProperties { unexpected } => {
                for field in unexpected {
                    errors.push(unknown_field(&pointer, field));
                }
            }
            // `additionalProperties: false` 且没有 `properties`（`EmptyRequest`）时，
            // 校验器对整个对象报 `false` schema：其中每个键都是未知字段。
            ValidationErrorKind::FalseSchema => match value.pointer(&pointer) {
                Some(Value::Object(map)) => {
                    errors.extend(map.keys().map(|field| unknown_field(&pointer, field)))
                }
                _ => errors.push(FieldError::new(pointer, "unknown_field", "unknown field")),
            },
            ValidationErrorKind::Required { property } => {
                let name = property.as_str().unwrap_or_default();
                errors.push(FieldError::new(
                    format!("{pointer}/{}", escape(name)),
                    "required",
                    "field is required",
                ));
            }
            ValidationErrorKind::AnyOf { context }
            | ValidationErrorKind::OneOfNotValid { context } => {
                // 各分支都不匹配：取分支里最具体的原因（枚举 > 范围 > 类型）。
                let code = context
                    .iter()
                    .flatten()
                    .map(|e| code_of(e.kind()))
                    .min_by_key(|code| match *code {
                        "enum" => 0,
                        "range" => 1,
                        _ => 2,
                    })
                    .unwrap_or("type");
                errors.push(FieldError::new(pointer, code, message_of(code)));
            }
            kind => {
                let code = code_of(kind);
                errors.push(FieldError::new(pointer, code, message_of(code)));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        // `allOf` 包装与 `oneOf` 分支可能对同一位置报多条；按指针与 code 去重。
        errors.dedup_by(|a, b| a.pointer == b.pointer && a.code == b.code);
        Err(Problem::validation(errors))
    }
}

fn unknown_field(pointer: &str, field: &str) -> FieldError {
    FieldError::new(
        format!("{pointer}/{}", escape(field)),
        "unknown_field",
        "unknown field",
    )
}

fn code_of(kind: &ValidationErrorKind) -> &'static str {
    match kind {
        ValidationErrorKind::Enum { .. } | ValidationErrorKind::Constant { .. } => "enum",
        ValidationErrorKind::MinLength { .. }
        | ValidationErrorKind::MaxLength { .. }
        | ValidationErrorKind::Minimum { .. }
        | ValidationErrorKind::Maximum { .. }
        | ValidationErrorKind::MinItems { .. }
        | ValidationErrorKind::MaxItems { .. } => "range",
        _ => "type",
    }
}

fn message_of(code: &str) -> &'static str {
    match code {
        "enum" => "value is not allowed",
        "range" => "value is out of range",
        _ => "value has the wrong shape",
    }
}

/// JSON Pointer 转义（RFC 6901）。
fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}
