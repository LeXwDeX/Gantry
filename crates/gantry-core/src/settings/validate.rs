//! 设置文档校验器。
//!
//! 本轮实现 V01–V03：
//! - V01：未知键、类型、枚举、范围；
//! - V02：内联凭据检测；
//! - V03：`secret_ref` 命名、存在性与 `null` 清除。
//!
//! V04–V22 的语义守卫（模型能力、端口 host、策略地板、项目约束等）**不在本轮**
//! 实现；相关类型、枚举和字段范围按文档保留，后续由对应里程碑补上。

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::rules::{rule_for, RelaxCmp};
use crate::entities::{PolicyActionId, SettingsResource, SettingsScope};
use crate::secret::{Consumer, SecretName, SecretString};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationIssue {
    pub pointer: String,
    pub code: &'static str,
    pub message: String,
    pub severity: Severity,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub errors: Vec<ValidationIssue>,
    pub warnings: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn error(
        &mut self,
        pointer: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) {
        self.errors.push(ValidationIssue {
            pointer: pointer.into(),
            code,
            message: message.into(),
            severity: Severity::Error,
        });
    }

    pub fn warning(
        &mut self,
        pointer: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) {
        self.warnings.push(ValidationIssue {
            pointer: pointer.into(),
            code,
            message: message.into(),
            severity: Severity::Warning,
        });
    }
}

/// V04–V06 等后续校验需要的模型能力；本轮只定义类型，不查询。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapability {
    pub tools: bool,
    pub reasoning_efforts: Vec<crate::level::Effort>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCapability {
    pub provider_kinds: Vec<String>,
    pub supports_fallback: bool,
}

/// 跨 resource / 外部系统的只读上下文。V01–V03 只用 `secret_exists`。
pub trait ValidationContext {
    fn model_capability(&self, provider: &str, model: &str) -> Option<ModelCapability>;
    fn adapter(&self, name: &str) -> Option<AdapterCapability>;
    fn graph_templates(&self, level: crate::level::LevelKey) -> &[String];
    fn secret_exists(&self, consumer: Consumer, name: &SecretName) -> bool;
    fn resource(&self, resource: SettingsResource, scope: &SettingsScope) -> Option<&Value>;
    fn host_memory_mb(&self) -> Option<u64>;
}

/// 不需要跨 resource 上下文的校验（V01 / V02 与 V03 的命名部分）。
pub struct NoContext;

impl ValidationContext for NoContext {
    fn model_capability(&self, _: &str, _: &str) -> Option<ModelCapability> {
        None
    }
    fn adapter(&self, _: &str) -> Option<AdapterCapability> {
        None
    }
    fn graph_templates(&self, _: crate::level::LevelKey) -> &[String] {
        &[]
    }
    fn secret_exists(&self, _: Consumer, _: &SecretName) -> bool {
        false
    }
    fn resource(&self, _: SettingsResource, _: &SettingsScope) -> Option<&Value> {
        None
    }
    fn host_memory_mb(&self) -> Option<u64> {
        None
    }
}

/// 校验入口。`pointer` 相对 `value`（根为空字符串）。
pub fn validate(
    resource: SettingsResource,
    scope: &SettingsScope,
    doc: &Value,
    ctx: &dyn ValidationContext,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    if !doc.is_object() {
        report.error("", "type", "settings value must be an object");
        return report;
    }

    check_nulls(doc, "", &mut report);
    check_inline_secrets(doc, "", &mut report);
    validate_resource(resource, scope, doc, ctx, &mut report);
    check_secret_slots(resource, doc, ctx, &mut report);
    report
}

/// 校验并解析为稀疏类型；用于 api 在 V01 通过后取得类型化文档。
pub fn validate_and_parse(
    resource: SettingsResource,
    scope: &SettingsScope,
    doc: &Value,
    ctx: &dyn ValidationContext,
) -> Result<super::resource::SettingsPatch, ValidationReport> {
    let report = validate(resource, scope, doc, ctx);
    if !report.is_ok() {
        return Err(report);
    }
    super::resource::parse_patch(resource, doc).map_err(|error| {
        let mut report = ValidationReport::default();
        report.error("", "type", error.to_string());
        report
    })
}

fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn child(pointer: &str, segment: &str) -> String {
    format!("{pointer}/{}", escape(segment))
}

/// `null` 不是"没有值"：存储文档用删除键表示清除。SecretRef 的 `value: null`
/// 有独立错误码（V03），由 secret 检查报告，这里跳过整个 SecretRef 子树。
fn check_nulls(value: &Value, pointer: &str, report: &mut ValidationReport) {
    match value {
        Value::Object(map) => {
            if map.contains_key("secret_ref") {
                return;
            }
            for (key, child_value) in map {
                check_nulls(child_value, &child(pointer, key), report);
            }
        }
        Value::Array(items) => {
            for (index, child_value) in items.iter().enumerate() {
                check_nulls(child_value, &format!("{pointer}/{index}"), report);
            }
        }
        Value::Null => report.error(
            pointer,
            "type",
            "null is not allowed in a stored settings document; remove the key to clear it",
        ),
        _ => {}
    }
}

/// V02：SecretRef 之外的键名 / 值不得携带凭据。
fn check_inline_secrets(value: &Value, pointer: &str, report: &mut ValidationReport) {
    match value {
        Value::Object(map) => {
            if map.contains_key("secret_ref") {
                return;
            }
            for (key, child_value) in map {
                let child_pointer = child(pointer, key);
                if let Value::String(text) = child_value {
                    if key_looks_secret(key) {
                        report.error(
                            &child_pointer,
                            "inline_secret_detected",
                            "inline credentials are not allowed; use a SecretRef",
                        );
                        continue;
                    }
                    if looks_like_secret_value(text) {
                        report.error(
                            &child_pointer,
                            "inline_secret_detected",
                            "value looks like a credential; use a SecretRef",
                        );
                    }
                }
                check_inline_secrets(child_value, &child_pointer, report);
            }
        }
        Value::Array(items) => {
            for (index, child_value) in items.iter().enumerate() {
                check_inline_secrets(child_value, &format!("{pointer}/{index}"), report);
            }
        }
        _ => {}
    }
}

fn key_looks_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["api_key", "token", "password", "secret", "private_key"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn looks_like_secret_value(value: &str) -> bool {
    const PREFIXES: &[&str] = &["sk-", "ghp_", "github_pat_", "-----BEGIN", "xoxb-"];
    if PREFIXES.iter().any(|prefix| value.starts_with(prefix)) {
        return true;
    }
    if value.len() < 40
        || value.contains("://")
        || value.contains('@')
        || value.chars().any(char::is_whitespace)
    {
        return false;
    }
    let has_upper = value.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = value.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    let has_symbol = value.chars().any(|c| !c.is_ascii_alphanumeric());
    let classes = [has_upper, has_lower, has_digit, has_symbol]
        .into_iter()
        .filter(|present| *present)
        .count();
    let distinct = value.chars().collect::<BTreeSet<_>>().len();
    classes >= 3 && distinct >= 16
}

fn validate_resource(
    resource: SettingsResource,
    scope: &SettingsScope,
    doc: &Value,
    ctx: &dyn ValidationContext,
    report: &mut ValidationReport,
) {
    let map = doc.as_object().expect("checked object");
    match resource {
        SettingsResource::Models => validate_models(map, ctx, report),
        SettingsResource::Levels => validate_levels(map, report),
        SettingsResource::Policies => validate_policies(map, scope, report),
        SettingsResource::Guardian => validate_guardian(map, report),
        SettingsResource::Sandbox => validate_sandbox(map, report),
        SettingsResource::Egress => validate_egress(map, report),
        SettingsResource::Notify => validate_notify(map, report),
        SettingsResource::Github => validate_github(map, report),
    }
}

// ---------------------------------------------------------------------------
// 通用字段辅助
// ---------------------------------------------------------------------------

fn unknown_fields(
    map: &Map<String, Value>,
    pointer: &str,
    allowed: &[&str],
    report: &mut ValidationReport,
) {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            report.error(
                child(pointer, key),
                "unknown_field",
                format!("unknown field `{key}`"),
            );
        }
    }
}

fn object<'a>(
    value: &'a Value,
    pointer: &str,
    report: &mut ValidationReport,
) -> Option<&'a Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => {
            report.error(pointer, "type", "expected an object");
            None
        }
    }
}

fn array<'a>(
    value: &'a Value,
    pointer: &str,
    report: &mut ValidationReport,
) -> Option<&'a Vec<Value>> {
    match value {
        Value::Array(items) => Some(items),
        _ => {
            report.error(pointer, "type", "expected an array");
            None
        }
    }
}

fn string<'a>(value: &'a Value, pointer: &str, report: &mut ValidationReport) -> Option<&'a str> {
    match value {
        Value::String(text) => Some(text),
        _ => {
            report.error(pointer, "type", "expected a string");
            None
        }
    }
}

fn boolean(value: &Value, pointer: &str, report: &mut ValidationReport) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        _ => {
            report.error(pointer, "type", "expected a boolean");
            None
        }
    }
}

fn integer(
    value: &Value,
    pointer: &str,
    min: Option<u64>,
    max: Option<u64>,
    report: &mut ValidationReport,
) -> Option<u64> {
    let Some(number) = value.as_u64() else {
        report.error(pointer, "type", "expected an integer");
        return None;
    };
    if min.is_some_and(|min| number < min) || max.is_some_and(|max| number > max) {
        report.error(pointer, "range", "integer is outside the allowed range");
        return None;
    }
    Some(number)
}

fn positive_number(value: &Value, pointer: &str, report: &mut ValidationReport) -> Option<f64> {
    let Some(number) = value.as_f64() else {
        report.error(pointer, "type", "expected a number");
        return None;
    };
    if !number.is_finite() || number <= 0.0 {
        report.error(pointer, "range", "number must be positive and finite");
        return None;
    }
    Some(number)
}

fn enum_string<'a>(
    value: &'a Value,
    pointer: &str,
    allowed: &[&str],
    report: &mut ValidationReport,
) -> Option<&'a str> {
    let text = string(value, pointer, report)?;
    if !allowed.contains(&text) {
        report.error(
            pointer,
            "enum",
            format!("value must be one of {}", allowed.join(", ")),
        );
        return None;
    }
    Some(text)
}

fn strings(value: &Value, pointer: &str, report: &mut ValidationReport) {
    if let Some(items) = array(value, pointer, report) {
        for (index, item) in items.iter().enumerate() {
            string(item, &format!("{pointer}/{index}"), report);
        }
    }
}

fn require<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    pointer: &str,
    report: &mut ValidationReport,
) -> Option<&'a Value> {
    match map.get(key) {
        Some(value) => Some(value),
        None => {
            report.error(
                child(pointer, key),
                "required",
                format!("`{key}` is required"),
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// models
// ---------------------------------------------------------------------------

fn validate_models(
    map: &Map<String, Value>,
    ctx: &dyn ValidationContext,
    report: &mut ValidationReport,
) {
    let _ = ctx;
    unknown_fields(
        map,
        "",
        &["providers", "retry", "utility", "planner", "worker"],
        report,
    );

    if let Some(value) = map.get("providers") {
        if let Some(providers) = object(value, "/providers", report) {
            for (id, provider) in providers {
                let pointer = child("/providers", id);
                let Some(provider) = object(provider, &pointer, report) else {
                    continue;
                };
                unknown_fields(
                    provider,
                    &pointer,
                    &["kind", "base_url", "credential"],
                    report,
                );
                if let Some(kind) = provider.get("kind") {
                    enum_string(kind, &child(&pointer, "kind"), &["anthropic"], report);
                }
                if let Some(base_url) = provider.get("base_url") {
                    string(base_url, &child(&pointer, "base_url"), report);
                }
                if let Some(credential) = provider.get("credential") {
                    validate_model_credential(credential, &child(&pointer, "credential"), report);
                }
            }
        }
    }

    if let Some(value) = map.get("retry") {
        if let Some(retry) = object(value, "/retry", report) {
            unknown_fields(retry, "/retry", &["max_attempts", "backoff_ms"], report);
            if let Some(value) = retry.get("max_attempts") {
                integer(value, "/retry/max_attempts", Some(1), None, report);
            }
            if let Some(value) = retry.get("backoff_ms") {
                integer(value, "/retry/backoff_ms", Some(0), None, report);
            }
        }
    }

    if let Some(value) = map.get("utility") {
        if let Some(utility) = object(value, "/utility", report) {
            unknown_fields(
                utility,
                "/utility",
                &[
                    "provider",
                    "model",
                    "reasoning_effort",
                    "fallback",
                    "request_timeout_s",
                    "total_timeout_s",
                ],
                report,
            );
            validate_model_common(utility, "/utility", false, report);
            if let Some(value) = utility.get("total_timeout_s") {
                integer(value, "/utility/total_timeout_s", Some(1), None, report);
            }
        }
    }

    for class in ["planner", "worker"] {
        let pointer = format!("/{class}");
        let Some(value) = map.get(class) else {
            continue;
        };
        let Some(settings) = object(value, &pointer, report) else {
            continue;
        };
        let mut allowed = vec![
            "provider",
            "model",
            "reasoning_effort",
            "effort_by_level",
            "fallback",
            "request_timeout_s",
        ];
        if class == "planner" {
            allowed.push("total_timeout_s");
        } else {
            allowed.extend(["adapter", "attempt_wall_s", "max_turns"]);
        }
        unknown_fields(settings, &pointer, &allowed, report);
        validate_model_common(settings, &pointer, true, report);
        if let Some(value) = settings.get("effort_by_level") {
            validate_effort_by_level(value, &child(&pointer, "effort_by_level"), report);
        }
        if class == "worker" {
            if let Some(value) = settings.get("adapter") {
                enum_string(value, &child(&pointer, "adapter"), &["claude-code"], report);
            }
            if let Some(value) = settings.get("attempt_wall_s") {
                integer(
                    value,
                    &child(&pointer, "attempt_wall_s"),
                    Some(1),
                    None,
                    report,
                );
            }
            if let Some(value) = settings.get("max_turns") {
                integer(value, &child(&pointer, "max_turns"), Some(1), None, report);
            }
        } else if let Some(value) = settings.get("total_timeout_s") {
            integer(
                value,
                &child(&pointer, "total_timeout_s"),
                Some(1),
                None,
                report,
            );
        }
    }
}

fn validate_model_credential(value: &Value, pointer: &str, report: &mut ValidationReport) {
    let Some(credential) = object(value, pointer, report) else {
        return;
    };
    unknown_fields(
        credential,
        pointer,
        &["type", "secret_ref", "present", "value"],
        report,
    );
    if let Some(value) = require(credential, "type", pointer, report) {
        enum_string(value, &child(pointer, "type"), &["api_key"], report);
    }
    require(credential, "secret_ref", pointer, report);
    if let Some(value) = credential.get("secret_ref") {
        string(value, &child(pointer, "secret_ref"), report);
    }
    if let Some(value) = credential.get("present") {
        boolean(value, &child(pointer, "present"), report);
    }
    if let Some(value) = credential.get("value") {
        validate_secret_value(value, &child(pointer, "value"), report);
    }
}

fn validate_secret_value(value: &Value, pointer: &str, report: &mut ValidationReport) {
    match value {
        Value::Null => report.error(
            pointer,
            "secret_clear_in_use",
            "secret values cannot be cleared",
        ),
        Value::String(_) => {}
        _ => report.error(pointer, "type", "expected a secret string"),
    }
}

fn validate_model_common(
    map: &Map<String, Value>,
    pointer: &str,
    allow_by_level: bool,
    report: &mut ValidationReport,
) {
    if let Some(value) = map.get("provider") {
        string(value, &child(pointer, "provider"), report);
    }
    if let Some(value) = map.get("model") {
        string(value, &child(pointer, "model"), report);
    }
    if let Some(value) = map.get("reasoning_effort") {
        let allowed: &[&str] = if allow_by_level {
            &["none", "low", "medium", "high", "max", "by_level"]
        } else {
            &["none", "low", "medium", "high", "max"]
        };
        enum_string(value, &child(pointer, "reasoning_effort"), allowed, report);
    }
    if let Some(value) = map.get("fallback") {
        if let Some(items) = array(value, &child(pointer, "fallback"), report) {
            if items.len() > 3 {
                report.error(
                    child(pointer, "fallback"),
                    "range",
                    "fallback accepts at most 3 entries",
                );
            }
            for (index, item) in items.iter().enumerate() {
                let item_pointer = format!("{}/fallback/{index}", pointer);
                let Some(fallback) = object(item, &item_pointer, report) else {
                    continue;
                };
                unknown_fields(
                    fallback,
                    &item_pointer,
                    &["provider", "model", "reasoning_effort"],
                    report,
                );
                require(fallback, "provider", &item_pointer, report);
                require(fallback, "model", &item_pointer, report);
                if let Some(value) = fallback.get("provider") {
                    string(value, &child(&item_pointer, "provider"), report);
                }
                if let Some(value) = fallback.get("model") {
                    string(value, &child(&item_pointer, "model"), report);
                }
                if let Some(value) = fallback.get("reasoning_effort") {
                    enum_string(
                        value,
                        &child(&item_pointer, "reasoning_effort"),
                        &["none", "low", "medium", "high", "max"],
                        report,
                    );
                }
            }
        }
    }
    if let Some(value) = map.get("request_timeout_s") {
        integer(
            value,
            &child(pointer, "request_timeout_s"),
            Some(5),
            Some(1800),
            report,
        );
    }
}

fn validate_effort_by_level(value: &Value, pointer: &str, report: &mut ValidationReport) {
    let Some(levels) = object(value, pointer, report) else {
        return;
    };
    unknown_fields(levels, pointer, &["low", "medium", "high", "max"], report);
    for key in ["low", "medium", "high", "max"] {
        let Some(value) = require(levels, key, pointer, report) else {
            continue;
        };
        let entry_pointer = child(pointer, key);
        let Some(entry) = object(value, &entry_pointer, report) else {
            continue;
        };
        unknown_fields(entry, &entry_pointer, &["default", "critical"], report);
        let Some(default) = require(entry, "default", &entry_pointer, report) else {
            continue;
        };
        enum_string(
            default,
            &child(&entry_pointer, "default"),
            &["none", "low", "medium", "high", "max"],
            report,
        );
        if let Some(critical) = entry.get("critical") {
            enum_string(
                critical,
                &child(&entry_pointer, "critical"),
                &["none", "low", "medium", "high", "max"],
                report,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// levels
// ---------------------------------------------------------------------------

fn validate_levels(map: &Map<String, Value>, report: &mut ValidationReport) {
    unknown_fields(
        map,
        "",
        &[
            "default_level",
            "token_weights",
            "none",
            "low",
            "medium",
            "high",
            "max",
            "ultimate",
        ],
        report,
    );
    if let Some(value) = map.get("default_level") {
        enum_string(
            value,
            "/default_level",
            &["None", "Low", "Medium", "High", "Max", "Ultimate"],
            report,
        );
    }
    if let Some(value) = map.get("token_weights") {
        if let Some(weights) = object(value, "/token_weights", report) {
            unknown_fields(
                weights,
                "/token_weights",
                &["input", "output", "cache_read", "cache_write"],
                report,
            );
            for key in ["input", "output", "cache_read", "cache_write"] {
                if let Some(value) = weights.get(key) {
                    positive_number(value, &format!("/token_weights/{key}"), report);
                }
            }
        }
    }
    if let Some(value) = map.get("none") {
        if let Some(none) = object(value, "/none", report) {
            unknown_fields(none, "/none", &["daily_tokens"], report);
            if let Some(value) = none.get("daily_tokens") {
                integer(value, "/none/daily_tokens", Some(1), None, report);
            }
        }
    }
    for level in ["low", "medium", "high", "max", "ultimate"] {
        let pointer = format!("/{level}");
        let Some(value) = map.get(level) else {
            continue;
        };
        let Some(entry) = object(value, &pointer, report) else {
            continue;
        };
        unknown_fields(entry, &pointer, &["graph_template", "budget"], report);
        if let Some(value) = entry.get("graph_template") {
            string(value, &child(&pointer, "graph_template"), report);
        }
        if let Some(value) = entry.get("budget") {
            let budget_pointer = child(&pointer, "budget");
            if let Some(budget) = object(value, &budget_pointer, report) {
                unknown_fields(
                    budget,
                    &budget_pointer,
                    &[
                        "tokens_by_tier",
                        "nodes",
                        "rounds",
                        "wall_time_s",
                        "concurrency",
                        "forge_writes",
                        "forge_writes_per_hour",
                    ],
                    report,
                );
                if let Some(value) = budget.get("tokens_by_tier") {
                    let tier_pointer = child(&budget_pointer, "tokens_by_tier");
                    if let Some(tiers) = object(value, &tier_pointer, report) {
                        unknown_fields(
                            tiers,
                            &tier_pointer,
                            &["utility", "planner", "worker"],
                            report,
                        );
                        for tier in ["utility", "planner", "worker"] {
                            if let Some(value) = tiers.get(tier) {
                                integer(value, &child(&tier_pointer, tier), Some(1), None, report);
                            }
                        }
                    }
                }
                for key in [
                    "nodes",
                    "rounds",
                    "wall_time_s",
                    "concurrency",
                    "forge_writes",
                    "forge_writes_per_hour",
                ] {
                    if let Some(value) = budget.get(key) {
                        integer(value, &child(&budget_pointer, key), Some(1), None, report);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// policies
// ---------------------------------------------------------------------------

fn validate_policies(
    map: &Map<String, Value>,
    scope: &SettingsScope,
    report: &mut ValidationReport,
) {
    unknown_fields(
        map,
        "",
        &[
            "allow_globs",
            "actions",
            "unlisted_action",
            "paths",
            "escalation",
            "approval",
            "patch",
        ],
        report,
    );
    if let Some(value) = map.get("allow_globs") {
        strings(value, "/allow_globs", report);
    }
    if let Some(value) = map.get("actions") {
        if let Some(actions) = object(value, "/actions", report) {
            for (id, disposition) in actions {
                let pointer = child("/actions", id);
                if PolicyActionId::parse_opt(id).is_none() {
                    report.error(pointer, "enum", format!("unknown policy action `{id}`"));
                    continue;
                }
                enum_string(disposition, &pointer, &["auto", "approve", "deny"], report);
            }
        }
    }
    if let Some(value) = map.get("unlisted_action") {
        enum_string(value, "/unlisted_action", &["approve", "deny"], report);
    }
    if let Some(value) = map.get("paths") {
        if let Some(paths) = object(value, "/paths", report) {
            for (id, rule) in paths {
                let pointer = child("/paths", id);
                let Some(rule) = object(rule, &pointer, report) else {
                    continue;
                };
                unknown_fields(rule, &pointer, &["globs", "disposition"], report);
                // 项目可只增加 globs 或只提高处置；全局规则必须完整。
                if *scope == SettingsScope::Global {
                    require(rule, "globs", &pointer, report);
                    require(rule, "disposition", &pointer, report);
                }
                if let Some(globs) = rule.get("globs") {
                    strings(globs, &child(&pointer, "globs"), report);
                }
                if let Some(disposition) = rule.get("disposition") {
                    enum_string(
                        disposition,
                        &child(&pointer, "disposition"),
                        &["approve", "deny"],
                        report,
                    );
                }
            }
        }
    }
    if let Some(value) = map.get("escalation") {
        validate_escalation(value, "/escalation", report);
    }
    if let Some(value) = map.get("approval") {
        if let Some(approval) = object(value, "/approval", report) {
            unknown_fields(approval, "/approval", &["ttl_s"], report);
            if let Some(value) = approval.get("ttl_s") {
                integer(value, "/approval/ttl_s", Some(60), Some(604800), report);
            }
        }
    }
    if let Some(value) = map.get("patch") {
        if let Some(patch) = object(value, "/patch", report) {
            unknown_fields(
                patch,
                "/patch",
                &[
                    "max_files",
                    "max_bytes",
                    "max_changed_lines",
                    "max_file_bytes",
                ],
                report,
            );
            for key in [
                "max_files",
                "max_bytes",
                "max_changed_lines",
                "max_file_bytes",
            ] {
                if let Some(value) = patch.get(key) {
                    integer(value, &format!("/patch/{key}"), Some(1), None, report);
                }
            }
        }
    }
}

fn validate_escalation(value: &Value, pointer: &str, report: &mut ValidationReport) {
    let Some(escalation) = object(value, pointer, report) else {
        return;
    };
    unknown_fields(escalation, pointer, &["enabled", "rules"], report);
    if let Some(value) = escalation.get("enabled") {
        boolean(value, &child(pointer, "enabled"), report);
    }
    if let Some(value) = escalation.get("rules") {
        let rules_pointer = child(pointer, "rules");
        if let Some(rules) = object(value, &rules_pointer, report) {
            for (id, rule) in rules {
                let rule_pointer = child(&rules_pointer, id);
                let Some(rule) = object(rule, &rule_pointer, report) else {
                    continue;
                };
                unknown_fields(rule, &rule_pointer, &["when", "raise_to"], report);
                if let Some(when) = require(rule, "when", &rule_pointer, report) {
                    let when_pointer = child(&rule_pointer, "when");
                    if let Some(when) = object(when, &when_pointer, report) {
                        unknown_fields(when, &when_pointer, &["paths", "diff", "labels"], report);
                        if let Some(value) = when.get("paths") {
                            strings(value, &child(&when_pointer, "paths"), report);
                        }
                        if let Some(value) = when.get("labels") {
                            strings(value, &child(&when_pointer, "labels"), report);
                        }
                        if let Some(value) = when.get("diff") {
                            let diff_pointer = child(&when_pointer, "diff");
                            if let Some(diff) = object(value, &diff_pointer, report) {
                                unknown_fields(
                                    diff,
                                    &diff_pointer,
                                    &["files_gte", "lines_gte"],
                                    report,
                                );
                                for key in ["files_gte", "lines_gte"] {
                                    if let Some(value) = diff.get(key) {
                                        integer(
                                            value,
                                            &child(&diff_pointer, key),
                                            Some(1),
                                            None,
                                            report,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(raise_to) = require(rule, "raise_to", &rule_pointer, report) {
                    enum_string(
                        raise_to,
                        &child(&rule_pointer, "raise_to"),
                        &["Medium", "High", "Max"],
                        report,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// guardian
// ---------------------------------------------------------------------------

fn validate_guardian(map: &Map<String, Value>, report: &mut ValidationReport) {
    unknown_fields(
        map,
        "",
        &[
            "triage",
            "no_progress",
            "status_checklist",
            "patrol_interval_s",
        ],
        report,
    );
    if let Some(value) = map.get("triage") {
        if let Some(triage) = object(value, "/triage", report) {
            for (class, action) in triage {
                let pointer = child("/triage", class);
                if !guardian_class_known(class) {
                    report.error(pointer, "enum", format!("unknown guardian class `{class}`"));
                    continue;
                }
                enum_string(
                    action,
                    &pointer,
                    &["silent_fix", "silent_reply", "batch", "escalate"],
                    report,
                );
            }
        }
    }
    if let Some(value) = map.get("no_progress") {
        if let Some(no_progress) = object(value, "/no_progress", report) {
            unknown_fields(
                no_progress,
                "/no_progress",
                &["same_class_repeats", "stalled_rounds"],
                report,
            );
            for key in ["same_class_repeats", "stalled_rounds"] {
                if let Some(value) = no_progress.get(key) {
                    integer(value, &format!("/no_progress/{key}"), Some(1), None, report);
                }
            }
        }
    }
    if let Some(value) = map.get("status_checklist") {
        if let Some(checklist) = object(value, "/status_checklist", report) {
            unknown_fields(
                checklist,
                "/status_checklist",
                &["enabled", "min_update_interval_s"],
                report,
            );
            if let Some(value) = checklist.get("enabled") {
                boolean(value, "/status_checklist/enabled", report);
            }
            if let Some(value) = checklist.get("min_update_interval_s") {
                integer(
                    value,
                    "/status_checklist/min_update_interval_s",
                    Some(0),
                    None,
                    report,
                );
            }
        }
    }
    if let Some(value) = map.get("patrol_interval_s") {
        integer(value, "/patrol_interval_s", Some(300), None, report);
    }
}

fn guardian_class_known(value: &str) -> bool {
    // 与类型枚举保持一致；这里不引入反序列化，避免错误信息丢失 pointer。
    matches!(
        value,
        "ci_failure_in_scope"
            | "bot_finding_valid"
            | "bot_finding_invalid"
            | "nit"
            | "mechanical_conflict"
            | "security_finding"
            | "touches_protected_path"
            | "scope_expanding_fix"
            | "repeated_or_no_progress"
            | "budget_exhausted"
            | "approval_needed"
            | "push_after_approval"
            | "design_or_api_change"
            | "unknown"
    )
}

// ---------------------------------------------------------------------------
// sandbox
// ---------------------------------------------------------------------------

fn validate_sandbox(map: &Map<String, Value>, report: &mut ValidationReport) {
    unknown_fields(
        map,
        "",
        &[
            "driver",
            "runtime",
            "base_image",
            "limits",
            "max_concurrent_sandboxes",
            "prewarm",
            "devcontainer",
            "idle_timeout_s",
            "kill_grace_s",
            "io",
        ],
        report,
    );
    if let Some(value) = map.get("driver") {
        enum_string(value, "/driver", &["podman"], report);
    }
    if let Some(value) = map.get("runtime") {
        enum_string(value, "/runtime", &["runsc", "runc"], report);
    }
    if let Some(value) = map.get("base_image") {
        string(value, "/base_image", report);
    }
    if let Some(value) = map.get("limits") {
        if let Some(limits) = object(value, "/limits", report) {
            unknown_fields(
                limits,
                "/limits",
                &["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"],
                report,
            );
            if let Some(value) = limits.get("cpu_cores") {
                positive_number(value, "/limits/cpu_cores", report);
            }
            for key in ["mem_mb", "disk_mb", "pids", "wall_time_s"] {
                if let Some(value) = limits.get(key) {
                    integer(value, &format!("/limits/{key}"), Some(1), None, report);
                }
            }
        }
    }
    if let Some(value) = map.get("max_concurrent_sandboxes") {
        integer(value, "/max_concurrent_sandboxes", Some(1), None, report);
    }
    if let Some(value) = map.get("prewarm") {
        if let Some(prewarm) = object(value, "/prewarm", report) {
            unknown_fields(prewarm, "/prewarm", &["enabled", "pool_size"], report);
            if let Some(value) = prewarm.get("enabled") {
                boolean(value, "/prewarm/enabled", report);
            }
            if let Some(value) = prewarm.get("pool_size") {
                integer(value, "/prewarm/pool_size", Some(0), None, report);
            }
        }
    }
    if let Some(value) = map.get("devcontainer") {
        if let Some(devcontainer) = object(value, "/devcontainer", report) {
            unknown_fields(devcontainer, "/devcontainer", &["mode"], report);
            if let Some(value) = devcontainer.get("mode") {
                enum_string(value, "/devcontainer/mode", &["off", "safe_subset"], report);
            }
        }
    }
    if let Some(value) = map.get("idle_timeout_s") {
        integer(value, "/idle_timeout_s", Some(1), None, report);
    }
    if let Some(value) = map.get("kill_grace_s") {
        integer(value, "/kill_grace_s", Some(0), None, report);
    }
    if let Some(value) = map.get("io") {
        validate_sandbox_io(value, "/io", report);
    }
}

fn validate_sandbox_io(value: &Value, pointer: &str, report: &mut ValidationReport) {
    let Some(io) = object(value, pointer, report) else {
        return;
    };
    unknown_fields(
        io,
        pointer,
        &["progress", "artifacts", "materials", "logs_tail_bytes"],
        report,
    );
    if let Some(value) = io.get("progress") {
        let pointer = child(pointer, "progress");
        if let Some(progress) = object(value, &pointer, report) {
            unknown_fields(
                progress,
                &pointer,
                &["max_line_bytes", "max_lines", "max_rate_per_s"],
                report,
            );
            for key in ["max_line_bytes", "max_lines", "max_rate_per_s"] {
                if let Some(value) = progress.get(key) {
                    integer(value, &child(&pointer, key), Some(1), None, report);
                }
            }
        }
    }
    if let Some(value) = io.get("artifacts") {
        let pointer = child(pointer, "artifacts");
        if let Some(artifacts) = object(value, &pointer, report) {
            unknown_fields(
                artifacts,
                &pointer,
                &[
                    "result_max_bytes",
                    "report_max_bytes",
                    "proposals_max_bytes",
                    "proposals_max_count",
                ],
                report,
            );
            for key in [
                "result_max_bytes",
                "report_max_bytes",
                "proposals_max_bytes",
                "proposals_max_count",
            ] {
                if let Some(value) = artifacts.get(key) {
                    integer(value, &child(&pointer, key), Some(1), None, report);
                }
            }
        }
    }
    if let Some(value) = io.get("materials") {
        let pointer = child(pointer, "materials");
        if let Some(materials) = object(value, &pointer, report) {
            unknown_fields(
                materials,
                &pointer,
                &["max_item_bytes", "max_total_bytes"],
                report,
            );
            for key in ["max_item_bytes", "max_total_bytes"] {
                if let Some(value) = materials.get(key) {
                    integer(value, &child(&pointer, key), Some(1), None, report);
                }
            }
        }
    }
    if let Some(value) = io.get("logs_tail_bytes") {
        integer(
            value,
            &child(pointer, "logs_tail_bytes"),
            Some(1),
            None,
            report,
        );
    }
}

// ---------------------------------------------------------------------------
// egress
// ---------------------------------------------------------------------------

fn validate_egress(map: &Map<String, Value>, report: &mut ValidationReport) {
    unknown_fields(
        map,
        "",
        &["groups", "profiles", "default_profile", "injection"],
        report,
    );
    if let Some(value) = map.get("groups") {
        if let Some(groups) = object(value, "/groups", report) {
            for (name, group) in groups {
                let pointer = child("/groups", name);
                let Some(group) = object(group, &pointer, report) else {
                    continue;
                };
                unknown_fields(group, &pointer, &["hosts"], report);
                if let Some(value) = require(group, "hosts", &pointer, report) {
                    strings(value, &child(&pointer, "hosts"), report);
                }
            }
        }
    }
    if let Some(value) = map.get("profiles") {
        if let Some(profiles) = object(value, "/profiles", report) {
            for (name, profile) in profiles {
                let pointer = child("/profiles", name);
                let Some(profile) = object(profile, &pointer, report) else {
                    continue;
                };
                unknown_fields(profile, &pointer, &["groups"], report);
                if let Some(value) = require(profile, "groups", &pointer, report) {
                    strings(value, &child(&pointer, "groups"), report);
                }
            }
        }
    }
    if let Some(value) = map.get("default_profile") {
        string(value, "/default_profile", report);
    }
    if let Some(value) = map.get("injection") {
        if let Some(injections) = object(value, "/injection", report) {
            for (name, injection) in injections {
                let pointer = child("/injection", name);
                let Some(injection) = object(injection, &pointer, report) else {
                    continue;
                };
                unknown_fields(
                    injection,
                    &pointer,
                    &["hosts", "kind", "header", "credential"],
                    report,
                );
                if let Some(value) = require(injection, "hosts", &pointer, report) {
                    strings(value, &child(&pointer, "hosts"), report);
                }
                if let Some(value) = require(injection, "kind", &pointer, report) {
                    enum_string(
                        value,
                        &child(&pointer, "kind"),
                        &["bearer", "header"],
                        report,
                    );
                }
                if let Some(value) = injection.get("header") {
                    string(value, &child(&pointer, "header"), report);
                }
                if let Some(value) = require(injection, "credential", &pointer, report) {
                    validate_secret_object(value, &child(&pointer, "credential"), report);
                }
            }
        }
    }
}

fn validate_secret_object(value: &Value, pointer: &str, report: &mut ValidationReport) {
    let Some(secret) = object(value, pointer, report) else {
        return;
    };
    unknown_fields(secret, pointer, &["secret_ref", "present", "value"], report);
    if let Some(value) = require(secret, "secret_ref", pointer, report) {
        string(value, &child(pointer, "secret_ref"), report);
    }
    if let Some(value) = secret.get("present") {
        boolean(value, &child(pointer, "present"), report);
    }
    if let Some(value) = secret.get("value") {
        validate_secret_value(value, &child(pointer, "value"), report);
    }
}

// ---------------------------------------------------------------------------
// notify
// ---------------------------------------------------------------------------

fn validate_notify(map: &Map<String, Value>, report: &mut ValidationReport) {
    unknown_fields(map, "", &["channels", "routes"], report);
    if let Some(value) = map.get("channels") {
        if let Some(channels) = object(value, "/channels", report) {
            unknown_fields(channels, "/channels", &["desktop"], report);
            if let Some(value) = channels.get("desktop") {
                if let Some(desktop) = object(value, "/channels/desktop", report) {
                    unknown_fields(desktop, "/channels/desktop", &["enabled"], report);
                    if let Some(value) = desktop.get("enabled") {
                        boolean(value, "/channels/desktop/enabled", report);
                    }
                }
            }
        }
    }
    if let Some(value) = map.get("routes") {
        if let Some(routes) = object(value, "/routes", report) {
            for (id, route) in routes {
                let pointer = child("/routes", id);
                let Some(route) = object(route, &pointer, report) else {
                    continue;
                };
                unknown_fields(route, &pointer, &["kinds", "channels"], report);
                if let Some(value) = require(route, "kinds", &pointer, report) {
                    let kinds_pointer = child(&pointer, "kinds");
                    if let Some(kinds) = array(value, &kinds_pointer, report) {
                        for (index, kind) in kinds.iter().enumerate() {
                            enum_string(
                                kind,
                                &format!("{kinds_pointer}/{index}"),
                                &[
                                    "approval_needed",
                                    "decision_needed",
                                    "spec_needed",
                                    "budget_exhausted",
                                    "escalation",
                                    "security_finding",
                                ],
                                report,
                            );
                        }
                    }
                }
                if let Some(value) = require(route, "channels", &pointer, report) {
                    let channels_pointer = child(&pointer, "channels");
                    if let Some(channels) = array(value, &channels_pointer, report) {
                        for (index, channel) in channels.iter().enumerate() {
                            enum_string(
                                channel,
                                &format!("{channels_pointer}/{index}"),
                                &["desktop"],
                                report,
                            );
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// github
// ---------------------------------------------------------------------------

fn validate_github(map: &Map<String, Value>, report: &mut ValidationReport) {
    unknown_fields(map, "", &["app", "commit_identity"], report);
    if let Some(value) = map.get("app") {
        if let Some(app) = object(value, "/app", report) {
            unknown_fields(
                app,
                "/app",
                &[
                    "app_id",
                    "private_key",
                    "webhook_secret",
                    "webhook_secret_previous",
                ],
                report,
            );
            if let Some(value) = app.get("app_id") {
                integer(value, "/app/app_id", Some(0), None, report);
            }
            for key in ["private_key", "webhook_secret", "webhook_secret_previous"] {
                if let Some(value) = app.get(key) {
                    validate_secret_object(value, &format!("/app/{key}"), report);
                }
            }
        }
    }
    if let Some(value) = map.get("commit_identity") {
        if let Some(identity) = object(value, "/commit_identity", report) {
            unknown_fields(identity, "/commit_identity", &["name", "email"], report);
            if let Some(value) = require(identity, "name", "/commit_identity", report) {
                string(value, "/commit_identity/name", report);
            }
            if let Some(value) = require(identity, "email", "/commit_identity", report) {
                string(value, "/commit_identity/email", report);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SecretRef：V03
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct SecretSlot<'a> {
    pub pointer: String,
    pub name: Option<SecretName>,
    pub consumer: Consumer,
    pub object: &'a Map<String, Value>,
}

/// 列出所有合法 SecretRef 位置。位置由 resource + 字段路径决定，consumer 也由
/// 路径决定：models / egress → Egress；github → Forge。
pub(crate) fn secret_slots(resource: SettingsResource, doc: &Value) -> Vec<SecretSlot<'_>> {
    let mut slots = Vec::new();
    let Some(root) = doc.as_object() else {
        return slots;
    };
    match resource {
        SettingsResource::Models => {
            if let Some(providers) = root.get("providers").and_then(Value::as_object) {
                for (id, provider) in providers {
                    if let Some(object) = provider.get("credential").and_then(Value::as_object) {
                        slots.push(SecretSlot {
                            pointer: format!("/providers/{}/credential", escape(id)),
                            name: object
                                .get("secret_ref")
                                .and_then(Value::as_str)
                                .and_then(|value| value.parse().ok()),
                            consumer: Consumer::Egress,
                            object,
                        });
                    }
                }
            }
        }
        SettingsResource::Egress => {
            if let Some(injections) = root.get("injection").and_then(Value::as_object) {
                for (id, injection) in injections {
                    if let Some(object) = injection.get("credential").and_then(Value::as_object) {
                        slots.push(SecretSlot {
                            pointer: format!("/injection/{}/credential", escape(id)),
                            name: object
                                .get("secret_ref")
                                .and_then(Value::as_str)
                                .and_then(|value| value.parse().ok()),
                            consumer: Consumer::Egress,
                            object,
                        });
                    }
                }
            }
        }
        SettingsResource::Github => {
            if let Some(app) = root.get("app").and_then(Value::as_object) {
                for key in ["private_key", "webhook_secret", "webhook_secret_previous"] {
                    if let Some(object) = app.get(key).and_then(Value::as_object) {
                        slots.push(SecretSlot {
                            pointer: format!("/app/{key}"),
                            name: object
                                .get("secret_ref")
                                .and_then(Value::as_str)
                                .and_then(|value| value.parse().ok()),
                            consumer: Consumer::Forge,
                            object,
                        });
                    }
                }
            }
        }
        _ => {}
    }
    slots
}

/// 所有 SecretRef 位置（含写入值）；供事件 / 审计使用。
pub fn find_secret_refs(
    resource: SettingsResource,
    doc: &Value,
) -> Vec<(String, SecretName, Consumer)> {
    secret_slots(resource, doc)
        .into_iter()
        .filter_map(|slot| slot.name.map(|name| (slot.pointer, name, slot.consumer)))
        .collect()
}

fn check_secret_slots(
    resource: SettingsResource,
    doc: &Value,
    ctx: &dyn ValidationContext,
    report: &mut ValidationReport,
) {
    for slot in secret_slots(resource, doc) {
        let name_pointer = child(&slot.pointer, "secret_ref");
        let Some(name) = slot.name else {
            // 缺失 / 类型错误已由资源校验报告；这里只补命名规则。
            if let Some(value) = slot.object.get("secret_ref") {
                if let Some(text) = value.as_str() {
                    if text.parse::<SecretName>().is_err() {
                        report.error(
                            name_pointer,
                            "type",
                            "secret_ref must match ^[a-z0-9][a-z0-9._-]{0,63}$",
                        );
                    }
                }
            }
            continue;
        };
        if slot.object.contains_key("value") {
            // 本次带值（含非法值）不要求已存在；null / 类型错误由通用
            // SecretRef 校验报告一次。
        } else if !ctx.secret_exists(slot.consumer, &name) {
            report.error(
                name_pointer,
                "secret_ref_missing",
                format!(
                    "secret `{name}` does not exist for consumer `{}`",
                    slot.consumer.as_str()
                ),
            );
        }
    }
}

/// 一个待转存的秘密值：`(名称, 使用方, 明文)`。明文只存在于这个返回值中。
pub type SecretWriteValue = (SecretName, Consumer, SecretString);
pub type SecretWrites = Vec<SecretWriteValue>;

/// 把请求体里的 `SecretRef.value` 剥出来。
///
/// 返回 `(可入事件的稀疏文档, 待转存的值)`；第二个返回值是唯一出现明文的地方。
/// 同一 ref 被多个 consumer 引用时返回多条写入，调用方需按 `(consumer, name)` 去重。
pub fn split_secret_values(
    resource: SettingsResource,
    mut request_value: Value,
) -> Result<(Value, SecretWrites), ValidationReport> {
    let mut report = ValidationReport::default();
    let mut writes = Vec::new();
    let slots: Vec<(String, Consumer)> = secret_slots(resource, &request_value)
        .into_iter()
        .map(|slot| (slot.pointer, slot.consumer))
        .collect();
    for (pointer, consumer) in slots {
        let Some(slot) = pointer_value_mut(&mut request_value, &pointer) else {
            continue;
        };
        let Some(object) = slot.as_object_mut() else {
            continue;
        };
        let Some(name) = object
            .get("secret_ref")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<SecretName>().ok())
        else {
            report.error(
                child(&pointer, "secret_ref"),
                "type",
                "secret_ref must match ^[a-z0-9][a-z0-9._-]{0,63}$",
            );
            continue;
        };
        if let Some(value) = object.remove("value") {
            match value {
                Value::Null => {
                    report.error(
                        child(&pointer, "value"),
                        "secret_clear_in_use",
                        "secret values cannot be cleared with null",
                    );
                }
                Value::String(text) => {
                    writes.push((name, consumer, SecretString::new(text)));
                }
                _ => {
                    report.error(child(&pointer, "value"), "type", "expected a secret string");
                }
            }
        }
        object.remove("present");
    }
    if report.is_ok() {
        Ok((request_value, writes))
    } else {
        Err(report)
    }
}

fn pointer_value_mut<'a>(root: &'a mut Value, pointer: &str) -> Option<&'a mut Value> {
    let mut current = root;
    for segment in pointer.split('/').skip(1) {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        current = current.get_mut(&segment)?;
    }
    Some(current)
}

/// 判断某个路径是否受方向约束；供设置页显示"只能收紧"。
/// 子路径继承最近的规则；`Incomparable`（仅 approver、无方向）不算。
pub fn has_direction(path: &str) -> bool {
    rule_for(path).is_some_and(|rule| !matches!(rule.relax, RelaxCmp::Incomparable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_unknown_nested_field() {
        let doc = json!({"worker": {"modle": "x"}});
        let report = validate(
            SettingsResource::Models,
            &SettingsScope::Global,
            &doc,
            &NoContext,
        );
        assert!(report
            .errors
            .iter()
            .any(|issue| issue.code == "unknown_field" && issue.pointer == "/worker/modle"));
    }

    #[test]
    fn rejects_wrong_types_and_ranges() {
        let doc = json!({
            "retry": {"max_attempts": 0},
            "utility": {"request_timeout_s": 9999}
        });
        let report = validate(
            SettingsResource::Models,
            &SettingsScope::Global,
            &doc,
            &NoContext,
        );
        assert!(report
            .errors
            .iter()
            .any(|issue| issue.code == "range" && issue.pointer.ends_with("max_attempts")));
        assert!(report
            .errors
            .iter()
            .any(|issue| issue.code == "range" && issue.pointer.ends_with("request_timeout_s")));
    }

    #[test]
    fn rejects_enum_and_unknown_action() {
        let doc = json!({
            "actions": {"merge_pr": "sometimes", "explode": "deny"},
            "unlisted_action": "auto"
        });
        let report = validate(
            SettingsResource::Policies,
            &SettingsScope::Global,
            &doc,
            &NoContext,
        );
        assert!(report
            .errors
            .iter()
            .any(|issue| issue.code == "enum" && issue.pointer == "/actions/merge_pr"));
        assert!(report
            .errors
            .iter()
            .any(|issue| issue.code == "enum" && issue.pointer == "/actions/explode"));
        assert!(report
            .errors
            .iter()
            .any(|issue| issue.code == "enum" && issue.pointer == "/unlisted_action"));
    }

    #[test]
    fn rejects_null_storage() {
        let doc = json!({"limits": {"mem_mb": null}});
        let report = validate(
            SettingsResource::Sandbox,
            &SettingsScope::Global,
            &doc,
            &NoContext,
        );
        assert!(report
            .errors
            .iter()
            .any(|issue| issue.code == "type" && issue.pointer == "/limits/mem_mb"));
    }
}
