//! 契约回归：Approval/Forge/Card/ID/settings.updated/ConfigView 的 schema 与 Rust 行为。

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use gantry_core::egress::ConfigView;
use gantry_core::entities::{Approval, Card};
use gantry_core::events::{
    validate_event_data, ApprovalRequestedData, EventData, EventType, SettingsUpdatedData, Subject,
};
use gantry_core::forge::ForgeEventEnvelope;
use gantry_core::ids::{Clock, Entropy, IdError, IdGen, Prj, ProjectId, Ulid};
use gantry_core::time::Timestamp;
use schemars::schema_for;
use serde_json::{json, Value};

fn schema<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schema_for!(T)).expect("schema serializes")
}

#[test]
fn approval_schema_links_action_target_and_subject() {
    let schema = schema::<Approval>();
    let branches = schema["oneOf"].as_array().expect("oneOf branches");
    assert_eq!(branches.len(), 8, "APPROVAL_SHAPES must drive the schema");

    let actions: BTreeSet<&str> = branches
        .iter()
        .map(|branch| branch["properties"]["action"]["const"].as_str().unwrap())
        .collect();
    assert!(actions.contains("merge_pr"));
    assert!(actions.contains("force_push"));
    assert!(
        !actions.contains("push_branch"),
        "actions without a defined shape must not appear in the schema"
    );

    let merge = branches
        .iter()
        .find(|branch| branch["properties"]["action"]["const"] == "merge_pr")
        .expect("merge_pr branch");
    assert_eq!(
        merge["properties"]["target"]["properties"]["kind"]["const"],
        "pr"
    );
    assert_eq!(
        merge["properties"]["subject_digest"]["pattern"],
        "^[0-9a-f]{40}$"
    );

    let settings = branches
        .iter()
        .find(|branch| branch["properties"]["action"]["const"] == "settings_update")
        .expect("settings_update branch");
    assert_eq!(
        settings["properties"]["target"]["properties"]["kind"]["const"],
        "settings"
    );
    assert_eq!(
        settings["properties"]["subject_digest"]["pattern"],
        "^sha256:[0-9a-f]{64}$"
    );
}

#[test]
fn approval_validate_follows_the_same_shapes() {
    let valid = json!({
        "id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
        "action": "merge_pr",
        "target": { "kind": "pr", "repo": "acme/web", "number": 12 },
        "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
        "status": "pending",
        "expires_at": "2026-10-24T14:41:00.000Z",
        "created_at": "2026-10-24T12:41:00.000Z",
        "updated_at": "2026-10-24T12:41:00.000Z"
    });
    let approval: Approval = serde_json::from_value(valid.clone()).expect("valid approval");
    approval.validate().expect("merge_pr/pr/git oid");

    let mut undefined_action = valid.clone();
    undefined_action["action"] = json!("push_branch");
    assert!(
        serde_json::from_value::<Approval>(undefined_action)
            .expect("known closure value")
            .validate()
            .is_err(),
        "actions without a defined shape must fail closed"
    );

    let mut target_mismatch = valid.clone();
    target_mismatch["target"] = json!({
        "kind": "branch", "repo": "acme/web", "branch": "main"
    });
    assert!(serde_json::from_value::<Approval>(target_mismatch)
        .expect("valid target shape")
        .validate()
        .is_err());

    let mut subject_mismatch = valid;
    subject_mismatch["subject_digest"] =
        json!("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(serde_json::from_value::<Approval>(subject_mismatch)
        .expect("valid digest shape")
        .validate()
        .is_err());
}

#[test]
fn approval_target_settings_scope_matches_project_id() {
    let global = json!({
        "kind": "settings",
        "scope": "global",
        "resource": "levels",
        "base_rev": 1
    });
    let global_with_project = json!({
        "kind": "settings",
        "scope": "global",
        "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "resource": "levels",
        "base_rev": 1
    });
    let project = json!({
        "kind": "settings",
        "scope": "project",
        "project_id": "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA",
        "resource": "levels",
        "base_rev": 1
    });
    let project_without_id = json!({
        "kind": "settings",
        "scope": "project",
        "resource": "levels",
        "base_rev": 1
    });
    let project_null = json!({
        "kind": "settings",
        "scope": "project",
        "project_id": null,
        "resource": "levels",
        "base_rev": 1
    });

    let target: gantry_core::entities::ApprovalTarget =
        serde_json::from_value(global.clone()).expect("global target");
    target.validate().expect("global target has no project_id");
    for bad in [global_with_project, project_without_id, project_null] {
        let target: gantry_core::entities::ApprovalTarget =
            serde_json::from_value(bad).expect("settings target shape decodes");
        assert!(
            target.validate().is_err(),
            "scope/project_id mismatch must fail closed: {target:?}"
        );
    }
    let target: gantry_core::entities::ApprovalTarget =
        serde_json::from_value(project.clone()).expect("project target");
    target.validate().expect("project target has project_id");

    let schema = schema::<gantry_core::entities::ApprovalTarget>();
    assert!(
        schema.get("allOf").is_some(),
        "settings scope/project_id must be constrained in schema too"
    );
}

#[test]
fn approval_requested_schema_uses_the_same_shape_projection() {
    let schema = schema::<ApprovalRequestedData>();
    assert_eq!(schema["oneOf"].as_array().expect("oneOf").len(), 8);
    assert_eq!(schema["title"], "ApprovalRequestedData");
    assert_eq!(
        schema["required"],
        json!([
            "action",
            "target",
            "subject_digest",
            "expires_at",
            "config_rev",
            "requested_by"
        ])
    );

    let valid: ApprovalRequestedData = serde_json::from_value(json!({
        "action": "merge_pr",
        "target": { "kind": "pr", "repo": "acme/web", "number": 12 },
        "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
        "expires_at": "2026-10-24T12:45:00.000Z",
        "config_rev": 10,
        "requested_by": { "kind": "engine", "id": "engine" }
    }))
    .expect("valid requested data");
    valid.validate().expect("shape match");

    let mismatch: ApprovalRequestedData = serde_json::from_value(json!({
        "action": "merge_pr",
        "target": { "kind": "device", "id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM" },
        "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
        "expires_at": "2026-10-24T12:45:00.000Z",
        "config_rev": 10,
        "requested_by": { "kind": "engine", "id": "engine" }
    }))
    .expect("shapes decode independently");
    assert!(mismatch.validate().is_err());
}

#[test]
fn forge_envelope_schema_binds_kind_and_data() {
    let schema = schema::<ForgeEventEnvelope>();
    let branches = schema["oneOf"].as_array().expect("kind branches");
    assert_eq!(branches.len(), 23);
    let kind_branch = |kind: &str| {
        branches
            .iter()
            .find(|branch| branch["properties"]["kind"]["const"] == kind)
            .unwrap_or_else(|| panic!("missing forge kind {kind}"))
            .clone()
    };
    assert_eq!(
        kind_branch("pr_opened")["properties"]["data"]["$ref"],
        "#/$defs/PrOpenedData"
    );
    assert_eq!(
        kind_branch("check_suite_completed")["properties"]["data"]["$ref"],
        "#/$defs/CheckSuiteCompletedData"
    );
    assert_eq!(
        schema["allOf"][0]["if"]["properties"]["origin"]["const"],
        "self"
    );
    assert_eq!(
        schema["allOf"][0]["then"]["properties"]["caused_by_action"]["not"]["type"],
        "null"
    );
}

fn envelope(kind: &str, origin: &str, caused_by_action: Value, data: Value) -> Value {
    json!({
        "schema": "forge.event.v1",
        "kind": kind,
        "dedup_key": "pr_sync:acme/shop-api:57:2",
        "source": "webhook",
        "delivery_ids": ["72d3162e-cc78-11e3-81ab-4c9367dc0958"],
        "project_id": "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK",
        "repo": "acme/shop-api",
        "installation_id": 4837261,
        "delivery_id": "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM",
        "target": { "type": "pr", "number": 57 },
        "actor": { "login": "gantry-acme[bot]", "github_id": 9988776, "type": "bot" },
        "origin": origin,
        "caused_by_action": caused_by_action,
        "occurred_at": "2026-10-24T12:41:02.000Z",
        "observed_at": "2026-10-24T12:41:03.412Z",
        "stale": false,
        "data": data
    })
}

#[test]
fn forge_envelope_validate_rejects_kind_data_mismatch_and_missing_action() {
    let reopened = json!({
        "number": 57,
        "head_sha": "d7d018f867d6c5b78e82906386420d45a4d84486",
        "base_ref": "main"
    });
    let valid: ForgeEventEnvelope = serde_json::from_value(envelope(
        "pr_reopened",
        "external",
        Value::Null,
        reopened.clone(),
    ))
    .expect("external envelope");
    valid.validate().expect("external envelope is valid");

    let missing_action: ForgeEventEnvelope = serde_json::from_value(envelope(
        "pr_reopened",
        "self",
        Value::Null,
        reopened.clone(),
    ))
    .expect("shape decodes");
    assert!(
        missing_action.validate().is_err(),
        "origin=self needs an action"
    );

    let mismatched = envelope(
        "pr_reopened",
        "external",
        Value::Null,
        json!({ "head_sha": "d7d018f867d6c5b78e82906386420d45a4d84486" }),
    );
    assert!(
        serde_json::from_value::<ForgeEventEnvelope>(mismatched).is_err(),
        "kind/data mismatch must fail during decode"
    );
}

#[test]
fn card_schema_keeps_unknown_kind_fallback_without_relaxing_known_kinds() {
    let schema = schema::<Card>();
    let branches = schema["oneOf"].as_array().expect("card branches");
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0]["$ref"], "#/$defs/KnownCard");
    assert_eq!(
        branches[1]["properties"]["kind"]["not"]["enum"],
        json!(["plan", "approval", "escalation", "choice", "summary"])
    );

    let known_card_branches = schema["$defs"]["KnownCard"]["oneOf"]
        .as_array()
        .expect("known card branches");
    for branch in known_card_branches {
        assert_eq!(
            branch["additionalProperties"],
            json!(false),
            "known card branches must be strict"
        );
    }

    let unknown = json!({ "kind": "future_card", "payload": { "x": 1 } });
    let card: Card = serde_json::from_value(unknown.clone()).expect("unknown card");
    assert_eq!(serde_json::to_value(&card).unwrap(), unknown);

    assert!(
        serde_json::from_value::<Card>(json!({ "kind": "approval" })).is_err(),
        "known kind with a bad shape must not fall through to the unknown branch"
    );
    let known = json!({ "kind": "approval", "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK" });
    let card: Card = serde_json::from_value(known.clone()).expect("known card");
    assert_eq!(serde_json::to_value(&card).unwrap(), known);

    let unexpected = json!({
        "kind": "approval",
        "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
        "extra": true
    });
    assert!(
        serde_json::from_value::<Card>(unexpected).is_err(),
        "known card kind must reject unknown fields like the generated schema"
    );
}

#[test]
fn card_deserialization_requires_a_string_kind() {
    for bad in [
        json!({}),
        json!({ "kind": null }),
        json!({ "kind": 1 }),
        json!({ "kind": { "x": 1 } }),
        json!({ "kind": [] }),
    ] {
        assert!(
            serde_json::from_value::<Card>(bad.clone()).is_err(),
            "missing or non-string kind must fail: {bad}"
        );
    }
}

fn posted_message(cards: Value) -> Value {
    json!({
        "id": "msg_01JBX2E3G5J7K9N1Q3S5T7W9YA",
        "conversation_id": "cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ",
        "role": "agent",
        "agent_kind": null,
        "tone": null,
        "text": "hello",
        "level_cap": null,
        "refs": [],
        "cards": cards,
        "delivery_id": null,
        "created_at": "2026-10-24T12:41:03.120Z",
        "updated_at": "2026-10-24T12:41:03.120Z"
    })
}

#[test]
fn message_posted_rejects_cards_without_a_string_kind() {
    for bad in [
        json!({}),
        json!({ "kind": null }),
        json!({ "kind": 1 }),
        json!({ "kind": { "x": 1 } }),
        json!({ "kind": [] }),
    ] {
        let data = posted_message(json!([bad.clone()]));
        assert!(
            validate_event_data(&EventType::MessagePosted, &data).is_err(),
            "message.posted must reject card {bad}"
        );
    }

    let unknown = posted_message(json!([{ "kind": "future_card", "payload": { "x": 1 } }]));
    validate_event_data(&EventType::MessagePosted, &unknown)
        .expect("unknown string kind stays forward compatible");
}

#[test]
fn id_patterns_limit_the_first_character_to_the_128_bit_range() {
    let project = schema::<ProjectId>();
    assert_eq!(project["pattern"], "^prj_[0-7][0-9A-HJKMNP-TV-Z]{25}$");
    let ulid = schema::<Ulid>();
    assert_eq!(ulid["pattern"], "^[0-7][0-9A-HJKMNP-TV-Z]{25}$");
    let subject = schema::<Subject>();
    assert!(subject["pattern"]
        .as_str()
        .expect("subject pattern")
        .contains("[0-7][0-9A-HJKMNP-TV-Z]{25}"));

    assert_eq!(
        ProjectId::from_str("prj_81JB7M4P8R2X6V9N3Q5T7W1YZB").unwrap_err(),
        IdError::Overflow
    );
    assert_eq!(
        Ulid::from_str("81JB7M4P8R2X6V9N3Q5T7W1YZB").unwrap_err(),
        IdError::Overflow
    );
    assert!(Subject::parse("prj_81JB7M4P8R2X6V9N3Q5T7W1YZB").is_err());
}

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.0)
    }
}

struct ZeroEntropy;

impl Entropy for ZeroEntropy {
    fn fill(&self, buf: &mut [u8]) {
        buf.fill(0);
    }
}

#[test]
fn id_generator_rejects_timestamps_above_48_bits() {
    const MAX_MS: i64 = (1i64 << 48) - 1;
    let make = |ms: i64| IdGen::new(Arc::new(FixedClock(ms)), Arc::new(ZeroEntropy));

    make(MAX_MS).next::<Prj>().expect("max timestamp fits");
    assert_eq!(
        make(MAX_MS + 1).next::<Prj>().unwrap_err(),
        IdError::Overflow
    );
    assert_eq!(make(i64::MAX).next_ulid().unwrap_err(), IdError::Overflow);
}

#[test]
fn settings_updated_schema_validates_the_real_resource_document() {
    let schema = schema::<SettingsUpdatedData>();
    assert_eq!(
        schema["oneOf"].as_array().expect("resource branches").len(),
        16
    );
    for resource in [
        "models", "levels", "policies", "guardian", "sandbox", "egress", "notify", "github",
    ] {
        assert!(
            schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|branch| branch["properties"]["resource"]["const"] == resource),
            "missing resource branch {resource}"
        );
    }
    for definition in ["SecretRefWrite", "ModelCredentialWrite"] {
        assert_eq!(
            schema["$defs"][definition]["properties"]["value"],
            json!(false),
            "{definition} must reject any value"
        );
    }

    let valid = json!({
        "scope": "global",
        "resource": "levels",
        "schema_version": 1,
        "base_rev": 1,
        "value": { "high": { "budget": { "nodes": 30 } } },
        "relaxes": [],
        "secret_changes": []
    });
    validate_event_data(&EventType::SettingsUpdated, &valid).expect("known resource shape");

    let nested_value = json!({
        "scope": "global",
        "resource": "models",
        "schema_version": 1,
        "base_rev": 1,
        "value": {
            "providers": {
                "anthropic": {
                    "credential": {
                        "secret_ref": "model.anthropic.main",
                        "value": null
                    }
                }
            }
        },
        "relaxes": [],
        "secret_changes": []
    });
    assert!(
        validate_event_data(&EventType::SettingsUpdated, &nested_value).is_err(),
        "SecretRef.value must be rejected even when it is null"
    );

    let legacy_wrapping = json!({
        "scope": "global",
        "resource": "models",
        "schema_version": 1,
        "base_rev": 1,
        "value": {
            "models": {
                "providers": {
                    "anthropic": {
                        "credential": {
                            "secret_ref": "model.anthropic.main",
                            "value": "leaked"
                        }
                    }
                }
            }
        },
        "relaxes": [],
        "secret_changes": []
    });
    assert!(
        validate_event_data(&EventType::SettingsUpdated, &legacy_wrapping).is_err(),
        "value.models is not the models document shape"
    );
}

fn config_view(injection: Value) -> Value {
    json!({
        "rev": 18240,
        "providers": {},
        "groups": {},
        "profiles": {},
        "default_profile": "default",
        "injection": injection,
        "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 }
    })
}

#[test]
fn config_view_only_accepts_an_empty_injection_object() {
    let empty: ConfigView =
        serde_json::from_value(config_view(json!({}))).expect("empty injection");
    assert!(empty.injection.is_empty());
    assert!(serde_json::to_value(&empty).is_ok());
    assert!(format!("{empty:?}").contains("redacted"));

    let non_empty = config_view(json!({ "npm": { "credential": { "secret_ref": "x" } } }));
    assert!(
        serde_json::from_value::<ConfigView>(non_empty).is_err(),
        "non-empty injection is not defined in M0"
    );

    let mut view = empty;
    view.injection
        .insert("npm".to_owned(), json!({ "secret": "should-not-print" }));
    assert!(serde_json::to_value(&view).is_err());
    assert!(!format!("{view:?}").contains("should-not-print"));

    let schema = schema::<ConfigView>();
    assert_eq!(schema["properties"]["injection"]["maxProperties"], 0);
}
