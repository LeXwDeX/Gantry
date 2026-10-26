//! 设置安全回归：事件 SecretRef 形状、settings subject/project 一致性、
//! guardian 锁定类与默认 escalate 的收紧 / 放宽检测。

use gantry_core::entities::{Actor, ActorKind, DeviceRole, SettingsResource, SettingsScope};
use gantry_core::events::{validate_event_data, EventType, NewEvent, SchemaErrorCode, Subject};
use gantry_core::settings::rules::{triage_default_action, LOCKED_TRIAGE_CLASSES};
use gantry_core::settings::{
    builtin_defaults, diff_relaxes, effective, required_authority_for_change, validate, NoContext,
};
use gantry_core::ProjectId;
use serde_json::{json, Map, Value};

const PROJECT_A: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const PROJECT_B: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZB";

fn settings_data(resource: &str, value: Value) -> Value {
    json!({
        "scope": "global",
        "resource": resource,
        "schema_version": 1,
        "base_rev": 0,
        "value": value,
        "relaxes": [],
        "secret_changes": []
    })
}

fn project_settings_data(resource: &str, value: Value) -> Value {
    json!({
        "scope": "project",
        "resource": resource,
        "schema_version": 1,
        "base_rev": 0,
        "value": value,
        "relaxes": [],
        "secret_changes": []
    })
}

fn new_settings_event(subject: &str, project_id: Option<ProjectId>, data: Value) -> NewEvent {
    NewEvent {
        ty: EventType::SettingsUpdated,
        subject: Subject::parse(subject).unwrap(),
        project_id,
        actor: Actor::new(ActorKind::Device, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    }
}

fn triage_doc(entries: &[(&str, &str)]) -> Value {
    let triage: Map<String, Value> = entries
        .iter()
        .map(|(class, action)| ((*class).to_owned(), json!(action)))
        .collect();
    json!({"triage": triage})
}

/// 合并 guardian 内置默认后的有效文档，再覆盖指定 triage 类。
fn guardian_effective_triage(entries: &[(&str, &str)]) -> Value {
    let mut doc = builtin_defaults(SettingsResource::Guardian);
    let triage = doc["triage"].as_object_mut().unwrap();
    for (class, action) in entries {
        triage.insert((*class).to_owned(), json!(action));
    }
    doc
}

// ---------------------------------------------------------------------------
// settings.updated：SecretRef 形状与明文
// ---------------------------------------------------------------------------

#[test]
fn event_rejects_secret_value_in_models_inner_document() {
    let data = settings_data(
        "models",
        json!({"providers": {"anthropic": {"credential": {
            "secret_ref": "model.anthropic.main",
            "value": "sk-live"
        }}}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::SecretValueForbidden);
    assert_eq!(error.pointer, "/providers/anthropic/credential/value");

    // `null` 也是 value：清除语义不适用于事件日志。
    let data = settings_data(
        "models",
        json!({"providers": {"anthropic": {"credential": {
            "secret_ref": "model.anthropic.main",
            "value": null
        }}}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::SecretValueForbidden);
    assert_eq!(error.pointer, "/providers/anthropic/credential/value");
}

#[test]
fn event_accepts_valid_secret_ref_without_value() {
    let data = settings_data(
        "models",
        json!({"providers": {"anthropic": {"credential": {
            "type": "api_key",
            "secret_ref": "model.anthropic.main",
            "present": true
        }}}}),
    );
    validate_event_data(&EventType::SettingsUpdated, &data).unwrap();
}

#[test]
fn event_rejects_malformed_secret_ref_shapes() {
    let data = settings_data(
        "models",
        json!({"providers": {"anthropic": {"credential": {
            "secret_ref": "Invalid Name"
        }}}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert_eq!(error.pointer, "/providers/anthropic/credential/secret_ref");

    let data = settings_data(
        "models",
        json!({"providers": {"anthropic": {"credential": "sk-live"}}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert_eq!(error.pointer, "/providers/anthropic/credential");

    let data = settings_data(
        "models",
        json!({"providers": {"anthropic": {"credential": {"present": true}}}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert_eq!(error.pointer, "/providers/anthropic/credential/secret_ref");
}

#[test]
fn event_covers_egress_and_github_secret_positions() {
    let data = settings_data(
        "egress",
        json!({"injection": {"npm": {"credential": {
            "secret_ref": "registry.npm.corp",
            "value": "leak"
        }}}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::SecretValueForbidden);
    assert_eq!(error.pointer, "/injection/npm/credential/value");

    let data = settings_data(
        "egress",
        json!({"injection": {"npm": {"credential": {"secret_ref": "registry.npm.corp"}}}}),
    );
    validate_event_data(&EventType::SettingsUpdated, &data).unwrap();

    let data = settings_data(
        "github",
        json!({"app": {"private_key": {
            "secret_ref": "github.app.private_key",
            "value": "-----BEGIN PRIVATE KEY-----"
        }}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::SecretValueForbidden);
    assert_eq!(error.pointer, "/app/private_key/value");

    let data = settings_data(
        "github",
        json!({"app": {
            "private_key": {"secret_ref": "github.app.private_key"},
            "webhook_secret": {"secret_ref": "github.app.webhook_secret"},
            "webhook_secret_previous": {"secret_ref": "github.app.webhook_secret_previous"}
        }}),
    );
    validate_event_data(&EventType::SettingsUpdated, &data).unwrap();
}

#[test]
fn event_rejects_unknown_fields_inside_the_resource_document() {
    let data = settings_data(
        "models",
        json!({"utility": {"model": "x", "value": "not a secret slot"}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert!(error.message.contains("unknown field `value`"), "{error:?}");
}

#[test]
fn event_validates_value_against_the_resource_patch_shape() {
    let wrong_type = settings_data("levels", json!({"high": {"budget": {"nodes": "many"}}}));
    let error = validate_event_data(&EventType::SettingsUpdated, &wrong_type).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::InvalidData);

    let nested_null = settings_data("levels", json!({"high": {"budget": {"nodes": null}}}));
    let error = validate_event_data(&EventType::SettingsUpdated, &nested_null).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert_eq!(error.pointer, "/high/budget/nodes");

    let valid = settings_data("levels", json!({"high": {"budget": {"nodes": 30}}}));
    validate_event_data(&EventType::SettingsUpdated, &valid).expect("valid levels patch");
}

#[test]
fn event_still_rejects_resource_prefixed_legacy_shape() {
    let data = settings_data(
        "models",
        json!({"models": {"providers": {"anthropic": {"credential": {
            "secret_ref": "model.anthropic.main",
            "value": "sk-live"
        }}}}}),
    );
    let error = validate_event_data(&EventType::SettingsUpdated, &data).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::SecretValueForbidden);
    assert_eq!(
        error.pointer,
        "/models/providers/anthropic/credential/value"
    );
}

#[test]
fn settings_value_is_required_and_scope_gated() {
    let mut missing = settings_data("levels", json!({"high": {"budget": {"nodes": 30}}}));
    missing.as_object_mut().unwrap().remove("value");
    let error = validate_event_data(&EventType::SettingsUpdated, &missing).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert_eq!(error.pointer, "/value");

    let global_null = settings_data("levels", Value::Null);
    let error = validate_event_data(&EventType::SettingsUpdated, &global_null).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert_eq!(error.pointer, "/value");

    let project_null = project_settings_data("levels", Value::Null);
    validate_event_data(&EventType::SettingsUpdated, &project_null)
        .expect("project DELETE carries value = null");

    let mut project_missing = project_settings_data("levels", json!({}));
    project_missing.as_object_mut().unwrap().remove("value");
    let error = validate_event_data(&EventType::SettingsUpdated, &project_missing).unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Shape);
    assert_eq!(error.pointer, "/value");
}

// ---------------------------------------------------------------------------
// settings.updated：subject / project_id 一致性
// ---------------------------------------------------------------------------

#[test]
fn settings_subject_must_match_project_id() {
    let a: ProjectId = PROJECT_A.parse().unwrap();
    let b: ProjectId = PROJECT_B.parse().unwrap();

    let global = settings_data("levels", json!({"high": {"budget": {"nodes": 30}}}));
    let project = project_settings_data("levels", json!({"high": {"budget": {"nodes": 30}}}));

    new_settings_event(PROJECT_A, Some(a), project.clone())
        .validate()
        .unwrap();

    let error = new_settings_event(PROJECT_A, Some(b), project.clone())
        .validate()
        .unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Consistency);
    assert_eq!(error.pointer, "/project_id");

    let error = new_settings_event(PROJECT_A, None, project.clone())
        .validate()
        .unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Consistency);
    assert_eq!(error.pointer, "/project_id");

    new_settings_event("settings", None, global.clone())
        .validate()
        .unwrap();

    let error = new_settings_event("settings", Some(a), global)
        .validate()
        .unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Consistency);
    assert_eq!(error.pointer, "/project_id");

    // data.scope 必须与信封一致，不能靠省略 project_id 绕过。
    let error = new_settings_event("settings", None, project)
        .validate()
        .unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Consistency);
    assert_eq!(error.pointer, "/scope");
    let error = new_settings_event(PROJECT_A, Some(a), settings_data("levels", json!({})))
        .validate()
        .unwrap_err();
    assert_eq!(error.code, SchemaErrorCode::Consistency);
    assert_eq!(error.pointer, "/scope");
}

// ---------------------------------------------------------------------------
// guardian.triage：锁定类、允许取值、clamp 与 relax
// ---------------------------------------------------------------------------

#[test]
fn every_locked_triage_class_is_rejected_in_both_scopes() {
    for class in LOCKED_TRIAGE_CLASSES {
        for scope in [SettingsScope::Global, SettingsScope::Project] {
            let doc = triage_doc(&[(*class, "silent_reply")]);
            let report = validate(SettingsResource::Guardian, &scope, &doc, &NoContext);
            assert!(
                report.errors.iter().any(|issue| {
                    issue.code == "locked_key" && issue.pointer == format!("/triage/{class}")
                }),
                "{scope:?} {class}: {:?}",
                report.errors
            );
        }
    }
}

#[test]
fn unlocked_triage_defaults_validate_and_invalid_values_are_rejected() {
    for class in [
        "ci_failure_in_scope",
        "bot_finding_valid",
        "bot_finding_invalid",
        "nit",
        "mechanical_conflict",
    ] {
        let action = triage_default_action(class).unwrap();
        let doc = triage_doc(&[(class, action)]);
        let report = validate(
            SettingsResource::Guardian,
            &SettingsScope::Global,
            &doc,
            &NoContext,
        );
        assert!(report.is_ok(), "{class}: {:?}", report.errors);
    }

    for (class, action) in [
        ("nit", "silent_fix"),
        ("bot_finding_invalid", "silent_fix"),
        ("mechanical_conflict", "batch"),
    ] {
        let doc = triage_doc(&[(class, action)]);
        let report = validate(
            SettingsResource::Guardian,
            &SettingsScope::Global,
            &doc,
            &NoContext,
        );
        assert!(
            report.errors.iter().any(|issue| {
                issue.code == "triage_value_invalid" && issue.pointer == format!("/triage/{class}")
            }),
            "{class}={action}: {:?}",
            report.errors
        );
    }
}

#[test]
fn project_cannot_add_locked_triage_class() {
    let builtin = builtin_defaults(SettingsResource::Guardian);
    let global = json!({});
    let project = triage_doc(&[("security_finding", "silent_reply")]);

    let result = effective(
        SettingsResource::Guardian,
        &builtin,
        &global,
        Some(&project),
    );

    assert_eq!(result.value["triage"]["security_finding"], "escalate");
    assert_eq!(result.clamped, vec!["guardian.triage".to_owned()]);
}

#[test]
fn project_clamps_every_locked_triage_class() {
    let builtin = builtin_defaults(SettingsResource::Guardian);
    for class in LOCKED_TRIAGE_CLASSES {
        let project = triage_doc(&[(*class, "silent_fix")]);
        let result = effective(
            SettingsResource::Guardian,
            &builtin,
            &json!({}),
            Some(&project),
        );
        assert_eq!(result.value["triage"][*class], "escalate", "{class}");
        assert!(
            result.clamped.contains(&"guardian.triage".to_owned()),
            "{class}"
        );
    }
}

#[test]
fn project_cannot_loosen_default_escalating_class() {
    let builtin = builtin_defaults(SettingsResource::Guardian);
    let project = triage_doc(&[("mechanical_conflict", "silent_fix")]);

    let result = effective(
        SettingsResource::Guardian,
        &builtin,
        &json!({}),
        Some(&project),
    );
    assert_eq!(result.value["triage"]["mechanical_conflict"], "escalate");
    assert_eq!(result.clamped, vec!["guardian.triage".to_owned()]);

    // 全局显式放宽后，项目沿用同一值不算再次放宽。
    let global = triage_doc(&[("mechanical_conflict", "silent_fix")]);
    let project = triage_doc(&[("mechanical_conflict", "silent_fix")]);
    let result = effective(
        SettingsResource::Guardian,
        &builtin,
        &global,
        Some(&project),
    );
    assert_eq!(result.value["triage"]["mechanical_conflict"], "silent_fix");
    assert!(result.clamped.is_empty());
}

#[test]
fn new_locked_class_with_silent_action_is_a_relax_and_needs_approver() {
    let old = builtin_defaults(SettingsResource::Guardian);
    let new = guardian_effective_triage(&[("security_finding", "silent_reply")]);
    let relaxes = diff_relaxes(
        SettingsResource::Guardian,
        &SettingsScope::Project,
        &old,
        &new,
    );
    assert_eq!(relaxes, vec!["guardian.triage".to_owned()]);

    let authority =
        required_authority_for_change(SettingsResource::Guardian, &relaxes, &old, &new, &[]);
    assert_eq!(authority.required_role, DeviceRole::Approver);
    assert!(authority.requires_signature);
}

#[test]
fn triage_relax_detection_covers_missing_baseline_and_tightening() {
    let old = builtin_defaults(SettingsResource::Guardian);
    for (class, action) in [
        ("security_finding", "silent_reply"),
        ("budget_exhausted", "silent_fix"),
        ("mechanical_conflict", "silent_fix"),
    ] {
        let new = guardian_effective_triage(&[(class, action)]);
        let relaxes = diff_relaxes(
            SettingsResource::Guardian,
            &SettingsScope::Project,
            &old,
            &new,
        );
        assert_eq!(relaxes, vec!["guardian.triage".to_owned()], "{class}");
    }

    let tighten = guardian_effective_triage(&[("ci_failure_in_scope", "escalate")]);
    assert!(diff_relaxes(
        SettingsResource::Guardian,
        &SettingsScope::Project,
        &old,
        &tighten
    )
    .is_empty());

    let new = guardian_effective_triage(&[("nit", "escalate")]);
    assert!(diff_relaxes(
        SettingsResource::Guardian,
        &SettingsScope::Project,
        &old,
        &new
    )
    .is_empty());
}
