//! RFC 7386 JSON Merge Patch 与"存储文档不含 null"的守卫。

use serde_json::{Map, Value};

/// RFC 7386：`null` 删除键，对象递归合并，数组整体替换。
///
/// 这里允许 `patch` 中出现 `null`，因为 RFC 7386 用它表达删除；
/// 存储文档不得含 `null`，由 [`reject_null_storage`] 在合并前检查。
pub fn merge_patch(base: &Value, patch: &Value) -> Value {
    match patch {
        Value::Object(patch_map) => {
            let mut result = match base {
                Value::Object(base_map) => base_map.clone(),
                _ => Map::new(),
            };
            for (key, patch_value) in patch_map {
                if patch_value.is_null() {
                    result.remove(key);
                } else {
                    let base_value = result.get(key).cloned().unwrap_or(Value::Null);
                    result.insert(key.clone(), merge_patch(&base_value, patch_value));
                }
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// 存储文档（全局值、项目覆盖）不允许 `null`：清除字段用空对象 / DELETE。
pub fn reject_null_storage(doc: &Value) -> Result<(), String> {
    fn walk(value: &Value, pointer: &str) -> Result<(), String> {
        match value {
            Value::Null => Err(pointer.to_owned()),
            Value::Object(map) => {
                for (key, child) in map {
                    walk(child, &format!("{pointer}/{}", escape(key)))?;
                }
                Ok(())
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{pointer}/{index}"))?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(doc, "")
}

fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rfc7386_appendix_examples() {
        // RFC 7386 Appendix A 的用例（加一条数组整体替换）。
        let cases = [
            (json!({"a":"b"}), json!({"a":"c"}), json!({"a":"c"})),
            (json!({"a":"b"}), json!({"b":"c"}), json!({"a":"b","b":"c"})),
            (json!({"a":"b"}), json!({"a":null}), json!({})),
            (
                json!({"a":"b","b":"c"}),
                json!({"a":null}),
                json!({"b":"c"}),
            ),
            (json!({"a":["b"]}), json!({"a":"c"}), json!({"a":"c"})),
            (json!({"a":"c"}), json!({"a":["b"]}), json!({"a":["b"]})),
            (
                json!({"a":{"b":"c"}}),
                json!({"a":{"b":"d","c":null}}),
                json!({"a":{"b":"d"}}),
            ),
            (json!({"a":[{"b":"c"}]}), json!({"a":[1]}), json!({"a":[1]})),
            (json!(["a", "b"]), json!(["c", "d"]), json!(["c", "d"])),
            (json!({"a":"b"}), json!(["c"]), json!(["c"])),
            (json!({"a":"foo"}), json!(null), json!(null)),
            (json!({"a":"foo"}), json!("bar"), json!("bar")),
            (json!({"e":null}), json!({"a":1}), json!({"e":null,"a":1})),
            (json!([1, 2]), json!({"a":"b","c":null}), json!({"a":"b"})),
            (
                json!({}),
                json!({"a":{"bb":{"ccc":null}}}),
                json!({"a":{"bb":{}}}),
            ),
        ];
        for (base, patch, expected) in cases {
            assert_eq!(
                merge_patch(&base, &patch),
                expected,
                "base={base} patch={patch}"
            );
        }
    }

    #[test]
    fn arrays_replace_wholesale_not_merge() {
        let base = json!({"allow_globs":["**"], "paths": {"x": {"globs":["a","b"]}}});
        let patch = json!({"allow_globs":["src/**"], "paths": {"x": {"globs":["c"]}}});
        assert_eq!(
            merge_patch(&base, &patch),
            json!({"allow_globs":["src/**"], "paths": {"x": {"globs":["c"]}}})
        );
    }

    #[test]
    fn storage_null_is_rejected_with_pointer() {
        let err = reject_null_storage(&json!({"a": {"b": null}})).unwrap_err();
        assert_eq!(err, "/a/b");
        assert!(reject_null_storage(&json!({"a": [1]})).is_ok());
    }
}
