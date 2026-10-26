//! 小工具：哈希、JSON Pointer 与差异报告。

use std::fmt;

use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn pointer<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer == "#" || pointer.is_empty() {
        return Some(root);
    }
    let rest = pointer.strip_prefix('#')?;
    if !rest.starts_with('/') {
        return None;
    }
    let mut current = root;
    for token in rest.split('/').skip(1) {
        let token = token.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&token)?,
            Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

#[derive(Default)]
pub struct Report {
    pub failures: Vec<String>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn fail(&mut self, message: impl Into<String>) {
        self.failures.push(message.into());
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.notes.push(message.into());
    }

    pub fn print(&self, title: &str) {
        println!(
            "{title}: {} failure(s), {} warning(s)",
            self.failures.len(),
            self.warnings.len()
        );
        for failure in &self.failures {
            println!("  FAIL {failure}");
        }
        for warning in &self.warnings {
            println!("  WARN {warning}");
        }
        for note in &self.notes {
            println!("  info {note}");
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} failure(s), {} warning(s)",
            self.failures.len(),
            self.warnings.len()
        )
    }
}

/// 规范化用于结构比较的 schema 片段：展开 `$ref`、忽略 `description` / `title`、
/// 把 `anyOf: [T, null]` 与 `type: [T, "null"]` 统一。
pub fn canonical<'a>(value: &'a Value, defs: &Defs<'a>, depth: usize) -> Value {
    if depth > 16 {
        return Value::String("$depth".to_owned());
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                if let Some(name) = reference.split('/').next_back() {
                    if let Some(target) = defs.get(name) {
                        return canonical(target, defs, depth + 1);
                    }
                }
            }
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                if matches!(
                    key.as_str(),
                    "description" | "title" | "$comment" | "$schema" | "deprecated"
                ) {
                    continue;
                }
                out.insert(key.clone(), canonical(item, defs, depth + 1));
            }
            let null_union = map
                .get("anyOf")
                .or_else(|| map.get("oneOf"))
                .and_then(Value::as_array)
                .filter(|items| items.len() == 2)
                .and_then(|items| {
                    let is_null =
                        |value: &Value| value.get("type").and_then(Value::as_str) == Some("null");
                    if is_null(&items[0]) {
                        Some((items[1].clone(), "first"))
                    } else if is_null(&items[1]) {
                        Some((items[0].clone(), "second"))
                    } else {
                        None
                    }
                });
            if let Some((inner, _)) = null_union {
                let inner = canonical(&inner, defs, depth + 1);
                out.insert(
                    "anyOf".to_owned(),
                    Value::Array(vec![inner, serde_json::json!({"type": "null"})]),
                );
            }
            if let Some(Value::Array(types)) = out.get("type") {
                let mut types = types.clone();
                types.sort_by_key(|value| value.as_str().unwrap_or("").to_owned());
                out.insert("type".to_owned(), Value::Array(types));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| canonical(item, defs, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub struct Defs<'a> {
    map: std::collections::BTreeMap<String, &'a Value>,
}

impl<'a> Defs<'a> {
    pub fn new() -> Self {
        Self {
            map: std::collections::BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, name: String, value: &'a Value) {
        self.map.insert(name, value);
    }

    pub fn get(&self, name: &str) -> Option<&'a Value> {
        self.map.get(name).copied()
    }
}

/// 收集一个 schema 片段里所有 `enum` 值集合（解开 `$ref`）。
pub fn enum_sets<'a>(value: &'a Value, defs: &Defs<'a>) -> std::collections::BTreeSet<Vec<String>> {
    let mut sets = std::collections::BTreeSet::new();
    collect_enums(value, defs, &mut sets, 0);
    sets
}

fn collect_enums<'a>(
    value: &'a Value,
    defs: &Defs<'a>,
    sets: &mut std::collections::BTreeSet<Vec<String>>,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                if let Some(name) = reference.split('/').next_back() {
                    if let Some(target) = defs.get(name) {
                        collect_enums(target, defs, sets, depth + 1);
                    }
                }
                return;
            }
            if let Some(Value::Array(values)) = map.get("enum") {
                let mut names: Vec<String> = values.iter().map(|value| value.to_string()).collect();
                names.sort();
                sets.insert(names);
            }
            for child in map.values() {
                collect_enums(child, defs, sets, depth + 1);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_enums(item, defs, sets, depth + 1);
            }
        }
        _ => {}
    }
}
