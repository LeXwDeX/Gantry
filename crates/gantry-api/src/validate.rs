//! 请求体 schema 校验（管线步骤 12，本文 §7.5，I7 的执行点）。
//!
//! 直接用契约 `schemas/openapi.yaml` 里的请求体组件校验，保证与 Mock API 同源：
//! 未知字段 → `unknown_field`（指针指向该字段，绝不静默忽略），缺字段 →
//! `required`，类型 → `type`，枚举 / 常量 → `enum`，长度 / 范围 → `range`。
//! 校验通过后处理程序再反序列化为强类型。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use jsonschema::error::ValidationErrorKind;
use jsonschema::ValidationError;
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
        collect(&error, value, &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        // `allOf` 包装可能对同一位置报多条；按指针与 code 去重。
        errors.dedup_by(|a, b| a.pointer == b.pointer && a.code == b.code);
        Err(Problem::validation(errors))
    }
}

fn collect(error: &ValidationError<'_>, value: &Value, out: &mut Vec<FieldError>) {
    let pointer = error.instance_path().to_string();
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            for field in unexpected {
                out.push(unknown_field(&pointer, field));
            }
        }
        // `additionalProperties: false` 且没有 `properties`（`EmptyRequest`）时，
        // 校验器对整个对象报 `false` schema：其中每个键都是未知字段。
        ValidationErrorKind::FalseSchema => match value.pointer(&pointer) {
            Some(Value::Object(map)) => {
                out.extend(map.keys().map(|field| unknown_field(&pointer, field)))
            }
            _ => out.push(FieldError::new(pointer, "unknown_field", "unknown field")),
        },
        ValidationErrorKind::Required { property } => {
            let name = property.as_str().unwrap_or_default();
            out.push(FieldError::new(
                format!("{pointer}/{}", escape(name)),
                "required",
                "field is required",
            ));
        }
        ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } => {
            match best_branch(context, &pointer) {
                // 选中"种类匹配"的分支，沿用它的逐字段错误与指针。
                Some(branch) => {
                    for inner in branch {
                        collect(inner, value, out);
                    }
                }
                None => out.push(FieldError::new(pointer, "type", message_of("type"))),
            }
        }
        kind => {
            let code = code_of(kind);
            out.push(FieldError::new(pointer, code, message_of(code)));
        }
    }
}

/// 在 `anyOf` / `oneOf` 的各分支里挑出最可能是客户端意图的那个：
/// 排除判别字段（`kind` 等常量）不符的分支与"整个值类型不对"的分支，
/// 余下取错误最少者（`Ref` 按 `kind` 选分支；`level_cap` 选枚举分支而不是 `null`）。
fn best_branch<'a>(
    context: &'a [Vec<ValidationError<'static>>],
    pointer: &str,
) -> Option<&'a Vec<ValidationError<'static>>> {
    let disqualified = |branch: &Vec<ValidationError<'static>>| {
        branch.iter().any(|e| {
            matches!(e.kind(), ValidationErrorKind::Constant { .. })
                || (matches!(e.kind(), ValidationErrorKind::Type { .. })
                    && e.instance_path().to_string() == pointer)
        })
    };
    context
        .iter()
        .filter(|branch| !branch.is_empty() && !disqualified(branch))
        .min_by_key(|branch| branch.len())
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
