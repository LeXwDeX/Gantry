//! OpenAPI 实体与 core 生成 schema 的逐路径差异判定（#83）。
//!
//! 两侧先经 `util::canonical` 展开 `$ref`，再由 [`normalize`] 抹掉纯表示差异，
//! 然后逐关键字比对。剩下的每一处差异只有三种去向：
//!
//! - 命中通用视图规则（[`ViewDiffs::judge`] 里的两条）；
//! - 命中 [`REGISTERED`] 白名单，并给出理由；
//! - 否则判为漂移，`report.fail`。
//!
//! 白名单条目若在一次检查里一处都没命中，同样 fail：条目随契约收敛及时删掉。

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::util::Report;

/// 已判定为允许的视图差异：位置前缀与理由。
///
/// 位置写成 `{label}.{字段}{JSON Pointer}`，Pointer 指向两侧规范化后的 schema。
/// 前缀匹配，所以一条可以覆盖一个子树。
pub const REGISTERED: &[(&str, &str)] = &[
    // —— API 写入侧上限：API 比 core 严，由 API 层校验，core 不重复编码 ——
    (
        "schemas/entities/device.json.name/m",
        "api.md `PATCH /v1/devices/{id}`：name 1–64 字符（minLength / maxLength），API 层校验",
    ),
    (
        "schemas/entities/message.json.text/maxLength",
        "api.md §3 默认值表：message.text ≤ 16 KiB，API 写入侧上限",
    ),
    (
        "schemas/entities/message.json.refs/maxItems",
        "api.md §3 默认值表：message.refs ≤ 20 条，API 写入侧上限",
    ),
    (
        "GantryEvent[message.posted].text/maxLength",
        "同 Message.text",
    ),
    (
        "GantryEvent[message.posted].refs/maxItems",
        "同 Message.refs",
    ),
    // —— 业务下界：API 写 ≥ 1，core 用无符号整数承载，由构造方保证 ——
    (
        "*/properties/number/minimum",
        "GitHub issue / PR 编号从 1 起（Ref、IssueRef、PullRequestRef、ApprovalTarget）",
    ),
    (
        "schemas/entities/attempt.json.n/minimum",
        "Attempt.n 从 1 起计，由引擎构造保证",
    ),
    (
        "schemas/entities/workflow.json.graph_rev/minimum",
        "graph_rev 从 1 起计（workflow.created）",
    ),
    (
        "schemas/entities/event-envelope.json.seq/minimum",
        "已提交事件的 seq ≥ 1（Seq::new 拒绝 0；Seq::ZERO 只作游标起点，不进信封）",
    ),
    (
        "GantryEvent[settings.updated].schema_version/minimum",
        "配置 schema_version 从 1 起",
    ),
    (
        "GantryEvent[settings.updated].secret_changes/items/properties/version/minimum",
        "密钥版本从 1 起",
    ),
    (
        "schemas/entities/budget-ledger.json.limits/properties/*/minimum",
        "API 只暴露交付台账（GET /v1/deliveries/{id}/budget），上限取自 levels，LevelBudget 已要求 ≥ 1；\
         core 的 BudgetLimits 还承载会话台账（nodes 等为 0），不在类型上编码下界",
    ),
    (
        "schemas/entities/budget-ledger.json.limits/properties/tokens/",
        "交付台账视图：core 的 BudgetLimits.tokens 是 Option<u64>，交付台账恒为 null，API 写成 type: null",
    ),
    // —— 结构表示不同、接受集合相同 ——
    (
        "schemas/entities/event-envelope.json.type/enum",
        "信封 type 在 API 里不列枚举，由 GantryEvent 的分支 const 约束；事件目录一致性另由 GantryEvent 检查保证",
    ),
    (
        "schemas/entities/node.json.role/",
        "core 用单个 pattern 表达封闭取值 + review:<name>，API 拆成 enum ∪ pattern 两个分支；\
         取值集合相同（xtask 测试 node_role_accepts_same_values 逐值核对）",
    ),
    (
        "*payload/anyOf|oneOf",
        "InboxPayload：core 是 untagged anyOf、分支开放；API 是 oneOf、分支封闭且 required 键互斥，\
         任一实例至多命中一个分支，anyOf 与 oneOf 接受同一集合；分支内容仍逐个比对",
    ),
    (
        "*/evidence/items/anyOf|oneOf",
        "EvidenceRef：core 是 untagged anyOf[Ref, {kind: result}]，API 是 oneOf；kind 取值互斥，接受集合相同",
    ),
    (
        "*.cards/items/oneOf|flatten",
        "Card：core 为 oneOf[KnownCard(oneOf 5 种), 未知 kind]，API 拍平成 6 个分支；各分支 kind 互斥，接受集合相同",
    ),
    (
        "schemas/entities/approval.json.target/allOf",
        "settings 目标的 scope ↔ project_id 约束：core 挂在顶层 allOf(if kind=settings)，API 挂在 settings 分支内；语义相同",
    ),
    (
        "schemas/entities/approval.json.target/oneOf/4/allOf",
        "同上：API 侧的 scope ↔ project_id 约束",
    ),
    // —— 待决：规格未定，不在 xtask 里拍板；定案后改契约并删掉本条 ——
    (
        "schemas/entities/device.json.cert_serial/pattern",
        "待决 OQ-10：cert_serial 格式未定，API 暂按示例写十六进制",
    ),
    (
        "schemas/entities/worktree.json.lock/anyOf/0/properties/delivery_id/",
        "待决 #122：人工锁（holder=human）是否绑定交付单元未写明；core 的 WorktreeLock.delivery_id 不可空，\
         API 与 Mock 允许 null",
    ),
];

/// 去掉不影响接受集合的表示差异。
///
/// - `default` / `format` / `examples` 是注解（2020-12 默认不断言 format）；
/// - `type: [T, "null"]` 与 `anyOf: [{type: T}, {type: null}]` 等价；
/// - 有 `const` 时 `type` 冗余；单元素 `enum` 与 `const` 等价；
/// - `required` / `enum` 的顺序无关；
/// - 正则里的 `\d` 在 ECMA-262 下等于 `[0-9]`。
pub fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                if matches!(key.as_str(), "default" | "format" | "examples") {
                    continue;
                }
                out.insert(key.clone(), normalize(item));
            }
            if let Some(Value::Array(values)) = out.get("enum") {
                if values.len() == 1 {
                    let only = values[0].clone();
                    out.remove("enum");
                    out.insert("const".to_owned(), only);
                }
            }
            if out.contains_key("const") {
                out.remove("type");
            }
            for key in ["required", "enum"] {
                if let Some(Value::Array(values)) = out.get_mut(key) {
                    values.sort_by_key(|value| value.to_string());
                }
            }
            if let Some(Value::String(pattern)) = out.get_mut("pattern") {
                *pattern = pattern.replace("\\d", "[0-9]");
            }
            if let Some(Value::Array(types)) = out.get("type") {
                let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
                if names.len() == 2 && names.contains(&"null") {
                    let inner = names.iter().find(|name| **name != "null").copied();
                    if let Some(inner) = inner {
                        let mut rest = out.clone();
                        rest.insert("type".to_owned(), Value::String(inner.to_owned()));
                        return Value::Object(Map::from_iter([(
                            "anyOf".to_owned(),
                            Value::Array(vec![
                                Value::Object(rest),
                                serde_json::json!({ "type": "null" }),
                            ]),
                        )]));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        other => other.clone(),
    }
}

/// `oneOf` 的某个分支本身只是一个 `oneOf` 时，返回展平后的分支表。
fn nested_one_of(map: &Map<String, Value>) -> Option<Vec<Value>> {
    let branches = map.get("oneOf")?.as_array()?;
    let nested = |branch: &Value| {
        branch
            .as_object()
            .filter(|branch| branch.len() == 1)
            .and_then(|branch| branch.get("oneOf"))
            .and_then(Value::as_array)
            .cloned()
    };
    if !branches.iter().any(|branch| nested(branch).is_some()) {
        return None;
    }
    let mut flat = Vec::new();
    for branch in branches {
        match nested(branch) {
            Some(inner) => flat.extend(inner),
            None => flat.push(branch.clone()),
        }
    }
    Some(flat)
}

/// 白名单位置：前缀匹配，`*` 匹配任意一段文本。
fn matches(place: &str, pattern: &str) -> bool {
    let mut rest = place;
    let mut parts = pattern.split('*').peekable();
    let mut first = true;
    while let Some(part) = parts.next() {
        if first {
            let Some(after) = rest.strip_prefix(part) else {
                return false;
            };
            rest = after;
            first = false;
        } else if parts.peek().is_none() && part.is_empty() {
            return true;
        } else {
            let Some(index) = rest.find(part) else {
                return false;
            };
            rest = &rest[index + part.len()..];
        }
    }
    true
}

fn is_nullable(schema: &Value) -> bool {
    if schema.get("type").and_then(Value::as_str) == Some("null") {
        return true;
    }
    ["anyOf", "oneOf"].iter().any(|key| {
        schema
            .get(*key)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(is_nullable))
    })
}

#[derive(Default)]
pub struct ViewDiffs {
    used: BTreeMap<&'static str, usize>,
    closed: usize,
    required_superset: usize,
}

impl ViewDiffs {
    /// 比对一个字段（或整个对象，`location` 为 label）的规范化 schema。
    pub fn judge(&mut self, location: &str, core: &Value, api: &Value, report: &mut Report) {
        let mut found = Vec::new();
        self.walk(location, core, api, &mut found);
        for (place, core_value, api_value) in found {
            match REGISTERED
                .iter()
                .find(|(pattern, _)| matches(&place, pattern))
            {
                Some((prefix, _)) => *self.used.entry(prefix).or_default() += 1,
                None => report.fail(format!(
                    "{place}: openapi 与 core 不一致且未登记 core={} openapi={}",
                    core_value
                        .as_ref()
                        .map_or("<无>".to_owned(), Value::to_string),
                    api_value
                        .as_ref()
                        .map_or("<无>".to_owned(), Value::to_string),
                )),
            }
        }
    }

    fn walk(
        &mut self,
        place: &str,
        core: &Value,
        api: &Value,
        found: &mut Vec<(String, Option<Value>, Option<Value>)>,
    ) {
        match (core, api) {
            (Value::Object(core_map), Value::Object(api_map))
                if core_map.contains_key("anyOf")
                    && !core_map.contains_key("oneOf")
                    && api_map.contains_key("oneOf")
                    && !api_map.contains_key("anyOf") =>
            {
                // core 的 untagged 枚举生成 anyOf，API 写成 oneOf：关键字差异本身单独登记，
                // 分支内容仍逐个下钻，避免一条白名单盖住分支里的漂移。
                found.push((format!("{place}/anyOf|oneOf"), None, None));
                let mut core_map = core_map.clone();
                let branches = core_map.remove("anyOf").unwrap_or(Value::Null);
                core_map.insert("oneOf".to_owned(), branches);
                self.walk(place, &Value::Object(core_map), api, found);
            }
            (Value::Object(core_map), Value::Object(api_map))
                if nested_one_of(core_map).is_some()
                    && api_map.get("oneOf").and_then(Value::as_array).map(Vec::len)
                        == nested_one_of(core_map).map(|branches| branches.len()) =>
            {
                // core 的 `oneOf[Known(oneOf …), Unknown]` 与 API 拍平的 `oneOf[…, Unknown]`：
                // 展平本身单独登记（要求各分支互斥），分支内容逐个下钻。
                found.push((format!("{place}/oneOf|flatten"), None, None));
                let mut core_map = core_map.clone();
                let branches = nested_one_of(&core_map).unwrap_or_default();
                core_map.insert("oneOf".to_owned(), Value::Array(branches));
                self.walk(place, &Value::Object(core_map), api, found);
            }
            (Value::Object(core_map), Value::Object(api_map)) => {
                let mut keys: Vec<&String> = core_map.keys().chain(api_map.keys()).collect();
                keys.sort();
                keys.dedup();
                for key in keys {
                    let child = format!("{place}/{key}");
                    match (core_map.get(key), api_map.get(key)) {
                        (Some(left), Some(right)) => {
                            if key == "required" && self.required_view(core_map, left, right) {
                                continue;
                            }
                            self.walk(&child, left, right, found);
                        }
                        (None, Some(Value::Bool(false))) if key == "additionalProperties" => {
                            // 通用规则 1：API 视图把对象封闭，core 不 deny_unknown_fields。
                            // 属性名集合由本函数逐键比对，封闭不会掩盖字段缺失。
                            self.closed += 1;
                        }
                        (left, right) => found.push((child, left.cloned(), right.cloned())),
                    }
                }
            }
            (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
                for (index, (left, right)) in left.iter().zip(right).enumerate() {
                    self.walk(&format!("{place}/{index}"), left, right, found);
                }
            }
            _ if core != api => {
                found.push((place.to_owned(), Some(core.clone()), Some(api.clone())))
            }
            _ => {}
        }
    }

    /// 通用规则 2：API `required` ⊇ core `required`，多出的字段在 core 里都可为 null。
    /// core 对 `Option<T>`（无 `skip_serializing_if`）总是输出 `null`，所以响应里键一定在。
    fn required_view(
        &mut self,
        core_object: &Map<String, Value>,
        core: &Value,
        api: &Value,
    ) -> bool {
        let (Some(core), Some(api)) = (core.as_array(), api.as_array()) else {
            return false;
        };
        if core == api {
            return true;
        }
        if !core.iter().all(|name| api.contains(name)) {
            return false;
        }
        let properties = core_object.get("properties");
        let extras_nullable = api.iter().filter(|name| !core.contains(name)).all(|name| {
            name.as_str()
                .and_then(|name| properties.and_then(|properties| properties.get(name)))
                .is_some_and(is_nullable)
        });
        if extras_nullable {
            self.required_superset += 1;
        }
        extras_nullable
    }

    pub fn finish(self, report: &mut Report) {
        for (prefix, reason) in REGISTERED {
            match self.used.get(prefix) {
                Some(count) => {
                    report.note(format!("已登记视图差异 {prefix}（{count} 处）：{reason}"))
                }
                None => report.fail(format!(
                    "视图差异白名单条目 `{prefix}` 未命中任何差异；请删除"
                )),
            }
        }
        report.note(format!(
            "通用视图规则：API 封闭对象 {} 处；API required 多出 core 可空字段 {} 处",
            self.closed, self.required_superset
        ));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn normalize_erases_pure_representation() {
        let core = json!({"type": ["null", "string"], "pattern": "^[0-9]{3}$", "default": null});
        let api = json!({"anyOf": [{"type": "string", "pattern": "^\\d{3}$"}, {"type": "null"}]});
        assert_eq!(normalize(&core), normalize(&api));
        let core = json!({"type": "string", "enum": ["result"]});
        let api = json!({"const": "result"});
        assert_eq!(normalize(&core), normalize(&api));
    }

    #[test]
    fn unregistered_difference_fails() {
        let mut diffs = ViewDiffs::default();
        let mut report = Report::default();
        diffs.judge(
            "X.repo",
            &json!({"type": "string", "pattern": "^a{1,100}$"}),
            &json!({"type": "string", "pattern": "^a+$"}),
            &mut report,
        );
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    }

    #[test]
    fn required_superset_needs_nullable_extras() {
        let core = json!({
            "properties": {"a": {"type": "string"}, "b": {"anyOf": [{"type": "string"}, {"type": "null"}]}, "c": {"type": "string"}},
            "required": ["a"]
        });
        let mut ok = core.clone();
        ok["required"] = json!(["a", "b"]);
        let mut bad = core.clone();
        bad["required"] = json!(["a", "c"]);
        let mut report = Report::default();
        let mut diffs = ViewDiffs::default();
        diffs.judge("X", &core, &ok, &mut report);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        diffs.judge("X", &core, &bad, &mut report);
        assert_eq!(report.failures.len(), 1);
    }
}
