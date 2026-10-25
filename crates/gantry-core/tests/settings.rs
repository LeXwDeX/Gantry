//! settings 契约测试：默认值、合并 / clamp、放宽差分、权限、V01–V03、秘密拆分。

use std::collections::BTreeSet;

use gantry_core::entities::{DeviceRole, PolicyActionId, SettingsResource, SettingsScope};
use gantry_core::level::LevelKey;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::settings::*;
use serde_json::{json, Value};

const ALL: [SettingsResource; 8] = [
    SettingsResource::Models,
    SettingsResource::Levels,
    SettingsResource::Policies,
    SettingsResource::Guardian,
    SettingsResource::Sandbox,
    SettingsResource::Egress,
    SettingsResource::Notify,
    SettingsResource::Github,
];

fn key(resource: SettingsResource) -> &'static str {
    match resource {
        SettingsResource::Models => "models",
        SettingsResource::Levels => "levels",
        SettingsResource::Policies => "policies",
        SettingsResource::Guardian => "guardian",
        SettingsResource::Sandbox => "sandbox",
        SettingsResource::Egress => "egress",
        SettingsResource::Notify => "notify",
        SettingsResource::Github => "github",
    }
}

#[derive(Default)]
struct TestCtx {
    egress: BTreeSet<SecretName>,
    forge: BTreeSet<SecretName>,
    resources: Vec<(SettingsResource, SettingsScope, Value)>,
}

impl TestCtx {
    fn with_egress(names: &[&str]) -> Self {
        Self {
            egress: names.iter().map(|name| name.parse().unwrap()).collect(),
            ..Self::default()
        }
    }

    fn with_forge(names: &[&str]) -> Self {
        Self {
            forge: names.iter().map(|name| name.parse().unwrap()).collect(),
            ..Self::default()
        }
    }
}

impl ValidationContext for TestCtx {
    fn model_capability(&self, _: &str, _: &str) -> Option<ModelCapability> {
        None
    }
    fn adapter(&self, _: &str) -> Option<AdapterCapability> {
        None
    }
    fn graph_templates(&self, _: LevelKey) -> &[String] {
        &[]
    }
    fn secret_exists(&self, consumer: Consumer, name: &SecretName) -> bool {
        match consumer {
            Consumer::Egress => self.egress.contains(name),
            Consumer::Forge => self.forge.contains(name),
            _ => false,
        }
    }
    fn resource(&self, resource: SettingsResource, scope: &SettingsScope) -> Option<&Value> {
        self.resources
            .iter()
            .find(|(r, s, _)| *r == resource && s == scope)
            .map(|(_, _, value)| value)
    }
    fn host_memory_mb(&self) -> Option<u64> {
        None
    }
}

fn check(resource: SettingsResource, doc: &Value, ctx: &TestCtx) -> ValidationReport {
    validate(resource, &SettingsScope::Global, doc, ctx)
}

fn has(report: &ValidationReport, code: &str, pointer: &str) -> bool {
    report
        .errors
        .iter()
        .any(|issue| issue.code == code && issue.pointer == pointer)
}

fn fixture(name: &str) -> Value {
    let text = match name {
        "global" => include_str!("../fixtures/settings/global_example.json"),
        "project" => include_str!("../fixtures/settings/project_example.json"),
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(text).unwrap()
}

// ---------------------------------------------------------------------------
// 默认值、类型与 schema
// ---------------------------------------------------------------------------

#[test]
fn defaults_match_documented_proposals_and_have_no_secret_values() {
    for resource in ALL {
        let builtin = builtin_defaults(resource);
        assert!(builtin.is_object(), "{resource:?}");
        assert!(
            !contains_key(&builtin, "value"),
            "{resource:?} has secret value"
        );
        assert!(
            check(resource, &builtin, &TestCtx::default()).is_ok(),
            "builtin default for {resource:?} must validate"
        );
        let patch = parse_patch(resource, &builtin).expect("default parses as patch");
        assert_eq!(patch.resource(), resource);
        let resolved = builtin_resolved(resource);
        assert_eq!(resolved.resource(), resource);
    }

    let models = builtin_defaults(SettingsResource::Models);
    assert!(models["utility"].get("provider").is_none());
    assert!(models["utility"].get("model").is_none());
    assert!(models["worker"].get("provider").is_none());
    assert_eq!(models["worker"]["adapter"], "claude-code");
    assert_eq!(models["utility"]["reasoning_effort"], "low");
    assert_eq!(models["planner"]["reasoning_effort"], "high");
    assert_eq!(models["worker"]["reasoning_effort"], "by_level");

    let levels = builtin_defaults(SettingsResource::Levels);
    assert_eq!(levels["default_level"], "Medium");
    assert_eq!(levels["none"]["daily_tokens"], 500_000);
    assert_eq!(levels["high"]["budget"]["nodes"], 40);
    assert_eq!(levels["token_weights"]["output"], 5.0);

    let policies = builtin_defaults(SettingsResource::Policies);
    let actions = policies["actions"].as_object().unwrap();
    assert_eq!(actions.len(), PolicyActionId::ALL.len());
    assert_eq!(policies["actions"]["merge_pr"], "approve");
    assert_eq!(policies["actions"]["push_branch"], "auto");
    assert_eq!(policies["actions"]["delete_branch"], "deny");
    assert_eq!(policies["approval"]["ttl_s"], 86_400);
    assert_eq!(policies["patch"]["max_bytes"], 5_000_000);

    let guardian = builtin_defaults(SettingsResource::Guardian);
    assert_eq!(guardian["triage"]["security_finding"], Value::Null);
    assert_eq!(guardian["triage"]["nit"], "batch");
    assert_eq!(guardian["patrol_interval_s"], 1800);

    let sandbox = builtin_defaults(SettingsResource::Sandbox);
    assert_eq!(sandbox["runtime"], "runsc");
    assert_eq!(sandbox["limits"]["mem_mb"], 4096);

    let egress = builtin_defaults(SettingsResource::Egress);
    assert_eq!(
        egress["profiles"]["default"]["groups"],
        json!(["model_api"])
    );

    let notify = builtin_defaults(SettingsResource::Notify);
    assert_eq!(notify["channels"]["desktop"]["enabled"], true);

    assert_eq!(builtin_defaults(SettingsResource::Github), json!({}));
}

#[test]
fn resource_schemas_are_typed_and_mark_secret_refs() {
    for resource in ALL {
        let schema = serde_json::to_value(resource_schema(resource)).unwrap();
        assert!(schema.is_object(), "{resource:?}");
        assert!(
            schema.get("$defs").is_some(),
            "{resource:?} has definitions"
        );
        assert!(
            !schema.to_string().contains("\"additionalProperties\":true"),
            "{resource:?} must not use a generic object schema"
        );
    }

    for resource in [
        SettingsResource::Models,
        SettingsResource::Egress,
        SettingsResource::Github,
    ] {
        let schema = serde_json::to_value(resource_schema(resource)).unwrap();
        let defs = schema["$defs"].as_object().unwrap();
        let secret = defs
            .values()
            .find(|value| value.get("x-gantry-secret") == Some(&Value::Bool(true)))
            .expect("secret ref schema present");
        assert_eq!(secret["additionalProperties"], false);
        assert_eq!(secret["properties"]["value"]["type"], "string");
        assert_eq!(secret["properties"]["value"]["writeOnly"], true);
        assert_eq!(secret["properties"]["present"]["readOnly"], true);
    }
}

#[test]
fn resolved_settings_is_untagged_and_roundtrips() {
    for resource in ALL {
        let resolved = builtin_resolved(resource);
        let value = resolved.value();
        assert!(value.is_object(), "{resource:?}");
        let reparsed = parse_resolved(resource, &value).unwrap();
        assert_eq!(reparsed.resource(), resource);
    }
}

// ---------------------------------------------------------------------------
// 合并 / clamp
// ---------------------------------------------------------------------------

#[test]
fn effective_merges_global_and_project() {
    let builtin = json!({
        "high": {"budget": {"nodes": 40}},
        "low": {"budget": {"nodes": 6}}
    });
    let global = json!({"high": {"budget": {"nodes": 30}}});
    let project = json!({"high": {"budget": {"nodes": 20}}});
    let result = effective(SettingsResource::Levels, &builtin, &global, Some(&project));
    assert_eq!(result.value["high"]["budget"]["nodes"], 20);
    assert_eq!(result.value["low"]["budget"]["nodes"], 6);
    assert!(result.clamped.is_empty());
}

#[test]
fn global_later_tightened_clamps_existing_project_override() {
    let builtin = json!({"high": {"budget": {"nodes": 40}}});
    let global = json!({"high": {"budget": {"nodes": 10}}});
    let project = json!({"high": {"budget": {"nodes": 20}}});
    let result = effective(SettingsResource::Levels, &builtin, &global, Some(&project));
    assert_eq!(result.value["high"]["budget"]["nodes"], 10);
    assert_eq!(result.clamped, vec!["levels.high.budget".to_owned()]);
}

struct ClampCase {
    resource: SettingsResource,
    builtin: Value,
    global: Value,
    project: Value,
    expected: Value,
    clamped: &'static str,
}

#[test]
fn project_override_matrix_is_enforced_by_clamp() {
    let cases = vec![
        ClampCase {
            resource: SettingsResource::Models,
            builtin: json!({}),
            global: json!({"providers": {"anthropic": {"kind": "anthropic"}}}),
            project: json!({"providers": {"evil": {"kind": "anthropic"}}}),
            expected: json!({"providers": {"anthropic": {"kind": "anthropic"}}}),
            clamped: "models.providers",
        },
        ClampCase {
            resource: SettingsResource::Models,
            builtin: json!({"retry": {"max_attempts": 3}}),
            global: json!({}),
            project: json!({"retry": {"max_attempts": 5}}),
            expected: json!({"retry": {"max_attempts": 3}}),
            clamped: "models.retry",
        },
        ClampCase {
            resource: SettingsResource::Levels,
            builtin: json!({"high": {"graph_template": "high.multi_review"}}),
            global: json!({}),
            project: json!({"high": {"graph_template": "other"}}),
            expected: json!({"high": {"graph_template": "high.multi_review"}}),
            clamped: "levels.high.graph_template",
        },
        ClampCase {
            resource: SettingsResource::Levels,
            builtin: json!({"token_weights": {"input": 1.0}}),
            global: json!({}),
            project: json!({"token_weights": {"input": 0.5}}),
            expected: json!({"token_weights": {"input": 1.0}}),
            clamped: "levels.token_weights",
        },
        ClampCase {
            resource: SettingsResource::Levels,
            builtin: json!({"none": {"daily_tokens": 500000}}),
            global: json!({}),
            project: json!({"none": {"daily_tokens": 600000}}),
            expected: json!({"none": {"daily_tokens": 500000}}),
            clamped: "levels.none.daily_tokens",
        },
        ClampCase {
            resource: SettingsResource::Policies,
            builtin: json!({"actions": {"merge_pr": "approve"}}),
            global: json!({}),
            project: json!({"actions": {"merge_pr": "auto"}}),
            expected: json!({"actions": {"merge_pr": "approve"}}),
            clamped: "policies.actions",
        },
        ClampCase {
            resource: SettingsResource::Policies,
            builtin: json!({"allow_globs": ["**"]}),
            global: json!({}),
            project: json!({"allow_globs": ["src/**"]}),
            expected: json!({"allow_globs": ["**"]}),
            clamped: "policies.allow_globs",
        },
        ClampCase {
            resource: SettingsResource::Policies,
            builtin: json!({"unlisted_action": "deny"}),
            global: json!({}),
            project: json!({"unlisted_action": "approve"}),
            expected: json!({"unlisted_action": "deny"}),
            clamped: "policies.unlisted_action",
        },
        ClampCase {
            resource: SettingsResource::Levels,
            builtin: json!({"high": {"budget": {"tokens_by_tier": {"worker": 1000000}}}}),
            global: json!({}),
            project: json!({"high": {"budget": {"tokens_by_tier": {"worker": 2000000}}}}),
            expected: json!({"high": {"budget": {"tokens_by_tier": {"worker": 1000000}}}}),
            clamped: "levels.high.budget",
        },
        ClampCase {
            resource: SettingsResource::Policies,
            builtin: json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}}}),
            global: json!({}),
            project: json!({"paths": {"x": {"disposition": "approve"}}}),
            expected: json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}}}),
            clamped: "policies.paths",
        },
        ClampCase {
            resource: SettingsResource::Policies,
            builtin: json!({"escalation": {"enabled": true, "rules": {}}}),
            global: json!({}),
            project: json!({"escalation": {"enabled": false}}),
            expected: json!({"escalation": {"enabled": true, "rules": {}}}),
            clamped: "policies.escalation",
        },
        ClampCase {
            resource: SettingsResource::Policies,
            builtin: json!({"approval": {"ttl_s": 86400}}),
            global: json!({}),
            project: json!({"approval": {"ttl_s": 90000}}),
            expected: json!({"approval": {"ttl_s": 86400}}),
            clamped: "policies.approval",
        },
        ClampCase {
            resource: SettingsResource::Policies,
            builtin: json!({"patch": {"max_files": 200}}),
            global: json!({}),
            project: json!({"patch": {"max_files": 300}}),
            expected: json!({"patch": {"max_files": 200}}),
            clamped: "policies.patch",
        },
        ClampCase {
            resource: SettingsResource::Guardian,
            builtin: json!({"triage": {"security_finding": "escalate"}}),
            global: json!({}),
            project: json!({"triage": {"security_finding": "silent_fix"}}),
            expected: json!({"triage": {"security_finding": "escalate"}}),
            clamped: "guardian.triage",
        },
        ClampCase {
            resource: SettingsResource::Guardian,
            builtin: json!({"no_progress": {"same_class_repeats": 2}}),
            global: json!({}),
            project: json!({"no_progress": {"same_class_repeats": 5}}),
            expected: json!({"no_progress": {"same_class_repeats": 2}}),
            clamped: "guardian.no_progress",
        },
        ClampCase {
            resource: SettingsResource::Guardian,
            builtin: json!({"patrol_interval_s": 1800}),
            global: json!({}),
            project: json!({"patrol_interval_s": 3600}),
            expected: json!({"patrol_interval_s": 1800}),
            clamped: "guardian.patrol_interval_s",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"limits": {"mem_mb": 4096}}),
            global: json!({}),
            project: json!({"limits": {"mem_mb": 8192}}),
            expected: json!({"limits": {"mem_mb": 4096}}),
            clamped: "sandbox.limits",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"devcontainer": {"mode": "safe_subset"}}),
            global: json!({}),
            project: json!({"devcontainer": {"mode": "off"}}),
            expected: json!({"devcontainer": {"mode": "safe_subset"}}),
            clamped: "sandbox.devcontainer",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"runtime": "runsc"}),
            global: json!({}),
            project: json!({"runtime": "runc"}),
            expected: json!({"runtime": "runsc"}),
            clamped: "sandbox.runtime",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"max_concurrent_sandboxes": 6}),
            global: json!({}),
            project: json!({"max_concurrent_sandboxes": 10}),
            expected: json!({"max_concurrent_sandboxes": 6}),
            clamped: "sandbox.max_concurrent_sandboxes",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"base_image": "image@sha256:a"}),
            global: json!({}),
            project: json!({"base_image": "image@sha256:b"}),
            expected: json!({"base_image": "image@sha256:a"}),
            clamped: "sandbox.base_image",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"prewarm": {"enabled": false}}),
            global: json!({}),
            project: json!({"prewarm": {"enabled": true}}),
            expected: json!({"prewarm": {"enabled": false}}),
            clamped: "sandbox.prewarm",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"idle_timeout_s": 300}),
            global: json!({}),
            project: json!({"idle_timeout_s": 600}),
            expected: json!({"idle_timeout_s": 300}),
            clamped: "sandbox.idle_timeout_s",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"kill_grace_s": 10}),
            global: json!({}),
            project: json!({"kill_grace_s": 20}),
            expected: json!({"kill_grace_s": 10}),
            clamped: "sandbox.kill_grace_s",
        },
        ClampCase {
            resource: SettingsResource::Sandbox,
            builtin: json!({"io": {"logs_tail_bytes": 1048576}}),
            global: json!({}),
            project: json!({"io": {"logs_tail_bytes": 1}}),
            expected: json!({"io": {"logs_tail_bytes": 1048576}}),
            clamped: "sandbox.io",
        },
        ClampCase {
            resource: SettingsResource::Egress,
            builtin: json!({"groups": {"model_api": {"hosts": ["api.anthropic.com"]}}}),
            global: json!({}),
            project: json!({"groups": {"evil": {"hosts": ["evil.example"]}}}),
            expected: json!({"groups": {"model_api": {"hosts": ["api.anthropic.com"]}}}),
            clamped: "egress.groups",
        },
        ClampCase {
            resource: SettingsResource::Egress,
            builtin: json!({"injection": {}}),
            global: json!({}),
            project: json!({"injection": {"x": {"hosts": ["x"], "kind": "bearer", "credential": {"secret_ref": "x"}}}}),
            expected: json!({"injection": {}}),
            clamped: "egress.injection",
        },
        ClampCase {
            resource: SettingsResource::Egress,
            builtin: json!({"default_profile": "default"}),
            global: json!({}),
            project: json!({"default_profile": "other"}),
            expected: json!({"default_profile": "default"}),
            clamped: "egress.default_profile",
        },
        ClampCase {
            resource: SettingsResource::Egress,
            builtin: json!({"profiles": {"default": {"groups": ["model_api"]}}}),
            global: json!({}),
            project: json!({"profiles": {"default": {"groups": ["model_api", "npm"]}}}),
            expected: json!({"profiles": {"default": {"groups": ["model_api"]}}}),
            clamped: "egress.profiles",
        },
        ClampCase {
            resource: SettingsResource::Egress,
            builtin: json!({"profiles": {"default": {"groups": ["model_api"]}}}),
            global: json!({}),
            project: json!({"profiles": {"extra": {"groups": ["model_api"]}}}),
            expected: json!({"profiles": {"default": {"groups": ["model_api"]}}}),
            clamped: "egress.profiles",
        },
        ClampCase {
            resource: SettingsResource::Notify,
            builtin: json!({"channels": {"desktop": {"enabled": true}}}),
            global: json!({}),
            project: json!({"channels": {"desktop": {"enabled": false}}}),
            expected: json!({"channels": {"desktop": {"enabled": true}}}),
            clamped: "notify",
        },
        ClampCase {
            resource: SettingsResource::Github,
            builtin: json!({}),
            global: json!({"app": {"app_id": 1}}),
            project: json!({"app": {"app_id": 2}}),
            expected: json!({"app": {"app_id": 1}}),
            clamped: "github",
        },
    ];

    for case in cases {
        let result = effective(
            case.resource,
            &case.builtin,
            &case.global,
            Some(&case.project),
        );
        assert_eq!(
            result.value, case.expected,
            "resource {:?}, clamped {:?}",
            case.resource, result.clamped
        );
        assert_eq!(result.clamped, vec![case.clamped.to_owned()]);
    }
}

#[test]
fn free_and_tightening_project_overrides_are_kept() {
    let cases = [
        (
            SettingsResource::Models,
            json!({}),
            json!({}),
            json!({"utility": {"model": "x"}}),
            json!({"utility": {"model": "x"}}),
        ),
        (
            SettingsResource::Models,
            json!({}),
            json!({}),
            json!({"planner": {"model": "p"}}),
            json!({"planner": {"model": "p"}}),
        ),
        (
            SettingsResource::Models,
            json!({}),
            json!({}),
            json!({"worker": {"reasoning_effort": "high"}}),
            json!({"worker": {"reasoning_effort": "high"}}),
        ),
        (
            SettingsResource::Levels,
            json!({"default_level": "Medium"}),
            json!({}),
            json!({"default_level": "High"}),
            json!({"default_level": "High"}),
        ),
        (
            SettingsResource::Levels,
            json!({"high": {"budget": {"nodes": 40}}}),
            json!({}),
            json!({"high": {"budget": {"nodes": 20}}}),
            json!({"high": {"budget": {"nodes": 20}}}),
        ),
        (
            SettingsResource::Policies,
            json!({"actions": {"merge_pr": "approve"}}),
            json!({}),
            json!({"actions": {"merge_pr": "deny"}}),
            json!({"actions": {"merge_pr": "deny"}}),
        ),
        (
            SettingsResource::Policies,
            json!({"escalation": {"enabled": true, "rules": {}}}),
            json!({}),
            json!({"escalation": {"rules": {"x": {"when": {"paths": ["src/**"]}, "raise_to": "High"}}}}),
            json!({"escalation": {"enabled": true, "rules": {"x": {"when": {"paths": ["src/**"]}, "raise_to": "High"}}}}),
        ),
        (
            SettingsResource::Guardian,
            json!({"status_checklist": {"enabled": true}}),
            json!({}),
            json!({"status_checklist": {"enabled": false}}),
            json!({"status_checklist": {"enabled": false}}),
        ),
        (
            SettingsResource::Sandbox,
            json!({"limits": {"mem_mb": 4096}}),
            json!({}),
            json!({"limits": {"mem_mb": 2048}}),
            json!({"limits": {"mem_mb": 2048}}),
        ),
        (
            SettingsResource::Policies,
            json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}}}),
            json!({}),
            json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}, "infra": {"globs": ["deploy/**"], "disposition": "approve"}}}),
            json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}, "infra": {"globs": ["deploy/**"], "disposition": "approve"}}}),
        ),
        (
            SettingsResource::Egress,
            json!({"profiles": {"default": {"groups": ["model_api", "npm"]}}}),
            json!({}),
            json!({"profiles": {"default": {"groups": ["model_api"]}}}),
            json!({"profiles": {"default": {"groups": ["model_api"]}}}),
        ),
    ];
    for (resource, builtin, global, project, expected) in cases {
        let result = effective(resource, &builtin, &global, Some(&project));
        assert_eq!(result.value, expected);
        assert!(result.clamped.is_empty(), "{:?}", result.clamped);
    }
}

// ---------------------------------------------------------------------------
// 放宽差分
// ---------------------------------------------------------------------------

#[test]
fn diff_relaxes_compares_effective_values() {
    let cases = [
        (
            SettingsResource::Policies,
            json!({"actions": {"merge_pr": "deny"}}),
            json!({"actions": {"merge_pr": "approve"}}),
            "policies.actions",
        ),
        (
            SettingsResource::Policies,
            json!({"approval": {"ttl_s": 86400}}),
            json!({"approval": {"ttl_s": 90000}}),
            "policies.approval",
        ),
        (
            SettingsResource::Levels,
            json!({"high": {"budget": {"nodes": 20}}}),
            json!({"high": {"budget": {"nodes": 40}}}),
            "levels.high.budget",
        ),
        (
            SettingsResource::Levels,
            json!({"token_weights": {"input": 1.0}}),
            json!({"token_weights": {"input": 0.5}}),
            "levels.token_weights",
        ),
        (
            SettingsResource::Policies,
            json!({"paths": {"x": {"globs": ["a", "b"], "disposition": "deny"}}}),
            json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}}}),
            "policies.paths",
        ),
        (
            SettingsResource::Policies,
            json!({"escalation": {"enabled": true, "rules": {}}}),
            json!({"escalation": {"enabled": false, "rules": {}}}),
            "policies.escalation",
        ),
        (
            SettingsResource::Guardian,
            json!({"triage": {"security_finding": "escalate"}}),
            json!({"triage": {"security_finding": "silent_fix"}}),
            "guardian.triage",
        ),
        (
            SettingsResource::Guardian,
            json!({"no_progress": {"same_class_repeats": 2}}),
            json!({"no_progress": {"same_class_repeats": 3}}),
            "guardian.no_progress",
        ),
        (
            SettingsResource::Sandbox,
            json!({"limits": {"mem_mb": 4096}}),
            json!({"limits": {"mem_mb": 8192}}),
            "sandbox.limits",
        ),
        (
            SettingsResource::Sandbox,
            json!({"devcontainer": {"mode": "safe_subset"}}),
            json!({"devcontainer": {"mode": "off"}}),
            "sandbox.devcontainer",
        ),
        (
            SettingsResource::Policies,
            json!({"patch": {"max_files": 200}}),
            json!({"patch": {"max_files": 300}}),
            "policies.patch",
        ),
        (
            SettingsResource::Egress,
            json!({"profiles": {"default": {"groups": ["model_api"]}}}),
            json!({"profiles": {"default": {"groups": ["model_api", "npm"]}}}),
            "egress.profiles",
        ),
        (
            SettingsResource::Policies,
            json!({"unlisted_action": "deny"}),
            json!({"unlisted_action": "approve"}),
            "policies.unlisted_action",
        ),
    ];

    for (resource, old, new, expected) in cases {
        let relaxes = diff_relaxes(resource, &SettingsScope::Project, &old, &new);
        assert!(
            relaxes.contains(&expected.to_owned()),
            "expected {expected} in {relaxes:?}"
        );
    }
}

#[test]
fn tightening_and_free_changes_are_not_relaxes() {
    let cases = [
        (
            SettingsResource::Policies,
            json!({"actions": {"merge_pr": "approve"}}),
            json!({"actions": {"merge_pr": "deny"}}),
        ),
        (
            SettingsResource::Policies,
            json!({"approval": {"ttl_s": 90000}}),
            json!({"approval": {"ttl_s": 86400}}),
        ),
        (
            SettingsResource::Guardian,
            json!({"triage": {"nit": "batch"}}),
            json!({"triage": {"nit": "escalate"}}),
        ),
        (
            SettingsResource::Guardian,
            json!({"status_checklist": {"enabled": true}}),
            json!({"status_checklist": {"enabled": false}}),
        ),
        (
            SettingsResource::Models,
            json!({"worker": {"model": "a"}}),
            json!({"worker": {"model": "b"}}),
        ),
        (
            SettingsResource::Sandbox,
            json!({"limits": {"mem_mb": 4096}}),
            json!({"limits": {"mem_mb": 2048}}),
        ),
        (
            SettingsResource::Egress,
            json!({"profiles": {"default": {"groups": ["model_api", "npm"]}}}),
            json!({"profiles": {"default": {"groups": ["model_api"]}}}),
        ),
    ];
    for (resource, old, new) in cases {
        assert!(
            diff_relaxes(resource, &SettingsScope::Project, &old, &new).is_empty(),
            "{resource:?}"
        );
    }
}

#[test]
fn project_override_delete_compares_old_and_new_effective_values() {
    // 旧项目生效值（deny）→ 删除覆盖后的全局生效值（approve）。
    let relaxes = diff_relaxes(
        SettingsResource::Policies,
        &SettingsScope::Project,
        &json!({"actions": {"merge_pr": "deny"}}),
        &json!({"actions": {"merge_pr": "approve"}}),
    );
    assert_eq!(relaxes, vec!["policies.actions".to_owned()]);

    // 替换成更短的预算覆盖：原始 20 → 全局 40。
    let relaxes = diff_relaxes(
        SettingsResource::Levels,
        &SettingsScope::Project,
        &json!({"high": {"budget": {"nodes": 20}}}),
        &json!({"high": {"budget": {"nodes": 40}}}),
    );
    assert_eq!(relaxes, vec!["levels.high.budget".to_owned()]);
}

// ---------------------------------------------------------------------------
// 权限
// ---------------------------------------------------------------------------

#[test]
fn required_authority_follows_changes_not_key_presence() {
    let old = json!({"actions": {"merge_pr": "deny"}});
    let new = json!({"actions": {"merge_pr": "approve"}});
    let authority = required_authority_for_change(
        SettingsResource::Policies,
        &["policies.actions".to_owned()],
        &old,
        &new,
        &[],
    );
    assert_eq!(authority.required_role, DeviceRole::Approver);
    assert!(authority.requires_signature);

    let authority = required_authority_for_change(
        SettingsResource::Policies,
        &[],
        &json!({"actions": {"merge_pr": "approve"}}),
        &json!({"actions": {"merge_pr": "deny"}}),
        &[],
    );
    assert_eq!(authority.required_role, DeviceRole::Operator);
    assert!(!authority.requires_signature);

    let provider = json!({"providers": {"anthropic": {"base_url": "https://api.anthropic.com"}}});
    let authority =
        required_authority_for_change(SettingsResource::Models, &[], &provider, &provider, &[]);
    assert_eq!(authority.required_role, DeviceRole::Operator);

    let authority = required_authority_for_change(
        SettingsResource::Models,
        &[],
        &provider,
        &json!({"providers": {"anthropic": {"base_url": "https://evil.example"}}}),
        &[],
    );
    assert_eq!(authority.required_role, DeviceRole::Approver);
    assert!(authority.requires_signature);

    let write = SecretWriteIntent {
        name: "model.x".parse().unwrap(),
        consumer: Consumer::Egress,
        write: SecretWriteKind::Set,
    };
    let authority = required_authority_for_change(
        SettingsResource::Models,
        &[],
        &json!({}),
        &json!({"providers": {"x": {"credential": {"secret_ref": "model.x"}}}}),
        &[write],
    );
    assert_eq!(authority.required_role, DeviceRole::Approver);
    assert!(authority.requires_signature);
}

#[test]
fn conservative_authority_entry_point_requires_approver_for_existing_keys() {
    // core.md §6.7 的 4 参数签名：没有旧值时，任何仅 approver 键都按变化处理。
    let authority = required_authority(
        SettingsResource::Models,
        &[],
        &json!({"providers": {"x": {}}}),
        &[],
    );
    assert_eq!(authority.required_role, DeviceRole::Approver);
    assert!(authority.requires_signature);

    let authority = required_authority(
        SettingsResource::Models,
        &[],
        &json!({"utility": {"model": "x"}}),
        &[],
    );
    assert_eq!(authority.required_role, DeviceRole::Operator);
    assert!(!authority.requires_signature);
}

#[test]
fn approver_only_paths_are_known() {
    assert!(is_approver_only("models.providers"));
    assert!(is_approver_only("models.providers.anthropic.base_url"));
    assert!(is_approver_only("levels.high.graph_template"));
    assert!(is_approver_only("policies.allow_globs"));
    assert!(is_approver_only("github"));
    assert!(is_approver_only("sandbox.base_image"));
    assert!(!is_approver_only("policies.actions"));
    assert!(!is_approver_only("sandbox.limits.mem_mb"));
    assert!(!is_approver_only("notify"));
}

// ---------------------------------------------------------------------------
// V01
// ---------------------------------------------------------------------------

#[test]
fn v01_rejects_unknown_fields_at_every_level() {
    let report = check(
        SettingsResource::Models,
        &json!({"worker": {"modle": "x"}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "unknown_field", "/worker/modle"));

    let report = check(
        SettingsResource::Policies,
        &json!({"paths": {"x": {"globs": ["a"], "disposition": "deny", "extra": 1}}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "unknown_field", "/paths/x/extra"));
}

#[test]
fn v01_rejects_types_enums_ranges_and_missing_required_fields() {
    let report = check(
        SettingsResource::Models,
        &json!({"retry": {"max_attempts": "3"}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "type", "/retry/max_attempts"));

    let report = check(
        SettingsResource::Levels,
        &json!({"default_level": "medium"}),
        &TestCtx::default(),
    );
    assert!(has(&report, "enum", "/default_level"));

    let report = check(
        SettingsResource::Policies,
        &json!({"approval": {"ttl_s": 10}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "range", "/approval/ttl_s"));

    let report = check(
        SettingsResource::Guardian,
        &json!({"patrol_interval_s": 10}),
        &TestCtx::default(),
    );
    assert!(has(&report, "range", "/patrol_interval_s"));

    let report = check(
        SettingsResource::Github,
        &json!({"commit_identity": {"name": "x"}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "required", "/commit_identity/email"));
}

#[test]
fn v01_rejects_stored_null_but_allows_secret_value_string() {
    let report = check(
        SettingsResource::Levels,
        &json!({"high": {"budget": {"nodes": null}}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "type", "/high/budget/nodes"));
}

#[test]
fn v01_project_path_rule_may_be_sparse_but_global_must_be_complete() {
    let project = validate(
        SettingsResource::Policies,
        &SettingsScope::Project,
        &json!({"paths": {"x": {"globs": ["deploy/**"]}}}),
        &TestCtx::default(),
    );
    assert!(project.is_ok(), "{:?}", project.errors);

    let global = check(
        SettingsResource::Policies,
        &json!({"paths": {"x": {"globs": ["deploy/**"]}}}),
        &TestCtx::default(),
    );
    assert!(has(&global, "required", "/paths/x/disposition"));
}

// ---------------------------------------------------------------------------
// V02
// ---------------------------------------------------------------------------

#[test]
fn v02_detects_inline_keys_and_secret_like_values() {
    let report = check(
        SettingsResource::Models,
        &json!({"worker": {"api_key": "x"}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "inline_secret_detected", "/worker/api_key"));

    let report = check(
        SettingsResource::Models,
        &json!({"worker": {"model": "sk-ant-api03-abcdefghijklmnopqrstuvwxyz"}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "inline_secret_detected", "/worker/model"));

    let report = check(
        SettingsResource::Models,
        &json!({"providers": {"x": {"credential": {
            "type": "api_key", "secret_ref": "model.x", "value": "sk-ant-secret"
        }}}}),
        &TestCtx::default(),
    );
    assert!(
        !report
            .errors
            .iter()
            .any(|issue| issue.code == "inline_secret_detected"),
        "{:?}",
        report.errors
    );
}

#[test]
fn v02_does_not_flag_secret_ref_or_safe_values() {
    let report = check(
        SettingsResource::Github,
        &json!({"app": {"private_key": {"secret_ref": "github.app.key"}}}),
        &TestCtx::with_forge(&["github.app.key"]),
    );
    assert!(!report
        .errors
        .iter()
        .any(|issue| issue.code == "inline_secret_detected"));

    let report = check(
        SettingsResource::Sandbox,
        &json!({"base_image": "registry.example/gantry/worker@sha256:aaaa"}),
        &TestCtx::default(),
    );
    assert!(!report
        .errors
        .iter()
        .any(|issue| issue.code == "inline_secret_detected"));
}

// ---------------------------------------------------------------------------
// V03
// ---------------------------------------------------------------------------

#[test]
fn v03_requires_existing_secret_or_value() {
    let missing = check(
        SettingsResource::Models,
        &json!({"providers": {"x": {"credential": {
            "type": "api_key", "secret_ref": "model.x"
        }}}}),
        &TestCtx::default(),
    );
    assert!(has(
        &missing,
        "secret_ref_missing",
        "/providers/x/credential/secret_ref"
    ));

    let present = check(
        SettingsResource::Models,
        &json!({"providers": {"x": {"credential": {
            "type": "api_key", "secret_ref": "model.x"
        }}}}),
        &TestCtx::with_egress(&["model.x"]),
    );
    assert!(present.is_ok(), "{:?}", present.errors);

    let supplied = check(
        SettingsResource::Models,
        &json!({"providers": {"x": {"credential": {
            "type": "api_key", "secret_ref": "model.x", "value": "secret"
        }}}}),
        &TestCtx::default(),
    );
    assert!(supplied.is_ok(), "{:?}", supplied.errors);
}

#[test]
fn v03_rejects_null_clear_and_invalid_names() {
    let report = check(
        SettingsResource::Models,
        &json!({"providers": {"x": {"credential": {
            "type": "api_key", "secret_ref": "model.x", "value": null
        }}}}),
        &TestCtx::default(),
    );
    assert!(has(
        &report,
        "secret_clear_in_use",
        "/providers/x/credential/value"
    ));

    let report = check(
        SettingsResource::Models,
        &json!({"providers": {"x": {"credential": {
            "type": "api_key", "secret_ref": "Upper"
        }}}}),
        &TestCtx::default(),
    );
    assert!(has(&report, "type", "/providers/x/credential/secret_ref"));
}

#[test]
fn v03_checks_same_secret_name_per_consumer() {
    let models = json!({"providers": {"x": {"credential": {
        "type": "api_key", "secret_ref": "shared"
    }}}});
    let github = json!({"app": {"private_key": {"secret_ref": "shared"}}});

    // Egress 有 shared，Forge 没有：models 通过，github 报缺失。
    let egress_only = TestCtx::with_egress(&["shared"]);
    assert!(check(SettingsResource::Models, &models, &egress_only).is_ok());
    let report = check(SettingsResource::Github, &github, &egress_only);
    assert!(has(
        &report,
        "secret_ref_missing",
        "/app/private_key/secret_ref"
    ));

    // Forge 有 shared，Egress 没有：github 通过，models 报缺失。
    let forge_only = TestCtx::with_forge(&["shared"]);
    assert!(check(SettingsResource::Github, &github, &forge_only).is_ok());
    let report = check(SettingsResource::Models, &models, &forge_only);
    assert!(has(
        &report,
        "secret_ref_missing",
        "/providers/x/credential/secret_ref"
    ));
}

// ---------------------------------------------------------------------------
// 秘密拆分
// ---------------------------------------------------------------------------

#[test]
fn split_secret_values_strips_values_and_present() {
    let request = json!({"providers": {"x": {"credential": {
        "type": "api_key", "secret_ref": "model.x", "present": true, "value": "secret"
    }}}});
    let (stripped, writes) = split_secret_values(SettingsResource::Models, request).unwrap();
    assert_eq!(
        stripped,
        json!({"providers": {"x": {"credential": {
            "type": "api_key", "secret_ref": "model.x"
        }}}})
    );
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, "model.x".parse().unwrap());
    assert_eq!(writes[0].1, Consumer::Egress);
    assert_eq!(writes[0].2.expose_secret(), "secret");
}

#[test]
fn split_secret_values_uses_per_slot_consumer_and_keeps_same_name() {
    let request = json!({"app": {
        "private_key": {"secret_ref": "shared", "value": "one"},
        "webhook_secret": {"secret_ref": "shared", "value": "two"}
    }});
    let (stripped, writes) = split_secret_values(SettingsResource::Github, request).unwrap();
    assert_eq!(
        stripped,
        json!({"app": {
            "private_key": {"secret_ref": "shared"},
            "webhook_secret": {"secret_ref": "shared"}
        }})
    );
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].1, Consumer::Forge);
    assert_eq!(writes[1].1, Consumer::Forge);
    assert_eq!(writes[0].2.expose_secret(), "one");
    assert_eq!(writes[1].2.expose_secret(), "two");
}

#[test]
fn split_secret_values_rejects_null_and_non_string() {
    let null = json!({"providers": {"x": {"credential": {
        "type": "api_key", "secret_ref": "model.x", "value": null
    }}}});
    let error = split_secret_values(SettingsResource::Models, null).unwrap_err();
    assert!(error
        .errors
        .iter()
        .any(|issue| issue.code == "secret_clear_in_use"));

    let number = json!({"providers": {"x": {"credential": {
        "type": "api_key", "secret_ref": "model.x", "value": 7
    }}}});
    let error = split_secret_values(SettingsResource::Models, number).unwrap_err();
    assert!(error.errors.iter().any(|issue| issue.code == "type"));
}

#[test]
fn find_secret_refs_lists_all_slots_with_consumers() {
    let doc = json!({"app": {
        "private_key": {"secret_ref": "github.key"},
        "webhook_secret": {"secret_ref": "github.hook"},
        "webhook_secret_previous": {"secret_ref": "github.hook.old"}
    }});
    let refs = find_secret_refs(SettingsResource::Github, &doc);
    assert_eq!(refs.len(), 3);
    assert!(refs
        .iter()
        .all(|(_, _, consumer)| *consumer == Consumer::Forge));
    assert!(refs
        .iter()
        .any(|(pointer, name, _)| pointer == "/app/private_key" && name.as_str() == "github.key"));
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

#[test]
fn example_fixtures_validate_and_apply() {
    let global = fixture("global");
    let project = fixture("project");
    let mut ctx = TestCtx::with_egress(&["model.anthropic.main", "registry.npm.corp"]);
    ctx.forge.extend([
        "github.app.private_key".parse().unwrap(),
        "github.app.webhook_secret".parse().unwrap(),
    ]);

    for resource in ALL {
        let doc = &global[key(resource)];
        let report = check(resource, doc, &ctx);
        assert!(report.is_ok(), "{resource:?}: {:?}", report.errors);
    }

    for resource in ALL {
        let builtin = builtin_defaults(resource);
        let global_doc = &global[key(resource)];
        let global_effective = effective(resource, &builtin, global_doc, None);
        let Some(project_doc) = project.get(key(resource)) else {
            continue;
        };
        let project_effective = effective(resource, &builtin, global_doc, Some(project_doc));
        assert!(
            project_effective.clamped.is_empty(),
            "{resource:?}: {:?}",
            project_effective.clamped
        );
        let relaxes = diff_relaxes(
            resource,
            &SettingsScope::Project,
            &global_effective.value,
            &project_effective.value,
        );
        assert!(relaxes.is_empty(), "{resource:?}: {relaxes:?}");
        let resolved = parse_resolved(resource, &project_effective.value).unwrap();
        assert_eq!(resolved.resource(), resource);
    }
}

#[test]
fn fixture_project_items_have_expected_effective_values() {
    let global = fixture("global");
    let project = fixture("project");
    let builtin = builtin_defaults(SettingsResource::Policies);
    let global_doc = &global["policies"];
    let project_doc = &project["policies"];
    let result = effective(
        SettingsResource::Policies,
        &builtin,
        global_doc,
        Some(project_doc),
    );
    assert_eq!(result.value["actions"]["rerun_failed_jobs"], "deny");
    assert_eq!(result.value["paths"]["infra"]["disposition"], "approve");
    assert_eq!(
        result.value["escalation"]["rules"]["payments"]["raise_to"],
        "Max"
    );

    let builtin = builtin_defaults(SettingsResource::Levels);
    let result = effective(
        SettingsResource::Levels,
        &builtin,
        &global["levels"],
        Some(&project["levels"]),
    );
    assert_eq!(result.value["high"]["budget"]["nodes"], 20);
    assert_eq!(result.value["high"]["budget"]["wall_time_s"], 7200);
    assert_eq!(result.value["medium"]["budget"]["nodes"], 20);

    let builtin = builtin_defaults(SettingsResource::Egress);
    let result = effective(
        SettingsResource::Egress,
        &builtin,
        &global["egress"],
        Some(&project["egress"]),
    );
    assert_eq!(
        result.value["profiles"]["default"]["groups"],
        json!(["model_api"])
    );
}

#[test]
fn split_fixture_has_no_secret_values() {
    let global = fixture("global");
    let (stripped, writes) =
        split_secret_values(SettingsResource::Models, global["models"].clone()).unwrap();
    // 夹具是存储 / GET 形态：只有 present，没有 value。
    assert!(writes.is_empty());
    assert_eq!(
        stripped["providers"]["anthropic"]["credential"]["secret_ref"],
        "model.anthropic.main"
    );
    assert!(stripped["providers"]["anthropic"]["credential"]
        .get("value")
        .is_none());
    assert!(stripped["providers"]["anthropic"]["credential"]
        .get("present")
        .is_none());
}

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| contains_key(v, key)),
        Value::Array(items) => items.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}
