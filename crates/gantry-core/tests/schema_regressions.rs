//! 契约回归：Approval/Forge/Card/ID/settings.updated/ConfigView 的 schema 与 Rust 行为。

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use gantry_core::egress::ConfigView;
use gantry_core::entities::{Approval, Card};
use gantry_core::events::{
    validate_event_data, ApprovalDeniedData, ApprovalExpiredData, ApprovalRequestedData,
    AttemptProgressData, BudgetChargedData, EventData, EventType, ForgeSyncedData,
    GuardianEscalatedData, GuardianHealRequestedData, GuardianReasonCode, GuardianTriageClass,
    GuardianTriagedData, OutboxCancelReason, OutboxEnqueuedData, OutboxExecutingData,
    PolicyDeniedData, PolicyReasonCode, PolicyTarget, SettingsUpdatedData, Subject,
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
fn config_view_accepts_valid_injection_and_rejects_invalid() {
    let empty: ConfigView =
        serde_json::from_value(config_view(json!({}))).expect("empty injection");
    assert!(empty.injection.is_empty());
    assert!(serde_json::to_value(&empty).is_ok());
    assert!(format!("{empty:?}").contains("redacted"));

    let mut valid = config_view(json!({
        "npm": {
            "hosts": ["npm.corp.example"],
            "kind": "bearer",
            "credential": { "secret_ref": "registry.npm.corp" }
        }
    }));
    valid["groups"] = json!({ "npm": { "hosts": ["npm.corp.example"] } });
    valid["profiles"] = json!({ "default": { "groups": ["npm"] } });
    let parsed: ConfigView = serde_json::from_value(valid).expect("valid non-empty injection");
    assert_eq!(parsed.injection.len(), 1);
    assert_eq!(
        parsed.injection["npm"].credential.secret_ref.as_str(),
        "registry.npm.corp"
    );
    assert!(serde_json::to_value(&parsed).is_ok());
    assert!(format!("{parsed:?}").contains("redacted"));
    assert!(!format!("{parsed:?}").contains("registry.npm.corp"));

    let invalid = config_view(json!({
        "npm": {
            "hosts": ["npm.corp.example"],
            "kind": "bearer",
            "header": "x-api-key",
            "credential": { "secret_ref": "registry.npm.corp" }
        }
    }));
    assert!(
        serde_json::from_value::<ConfigView>(invalid).is_err(),
        "bearer with an explicit header is invalid"
    );

    let schema = schema::<ConfigView>();
    assert!(
        schema["properties"]["injection"]
            .get("maxProperties")
            .is_none(),
        "non-empty injection is now defined"
    );
    assert_eq!(
        schema["properties"]["injection"]["propertyNames"]["pattern"],
        "^[a-z0-9][a-z0-9._-]{0,63}$"
    );
    assert_eq!(
        schema["$defs"]["InjectionView"]["additionalProperties"],
        false
    );
    assert_eq!(
        schema["$defs"]["InjectionView"]["properties"]["hosts"]["minItems"],
        1
    );
    assert_eq!(
        schema["$defs"]["InjectionCredentialView"]["additionalProperties"],
        false
    );
}

fn required_fields(schema: &Value) -> Vec<String> {
    schema["required"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn attempt_progress_schema_and_tool_branches_are_strict() {
    let schema = schema::<AttemptProgressData>();
    let branches = schema["oneOf"].as_array().expect("attempt branches");
    assert_eq!(branches.len(), 4);
    let stage = branches
        .iter()
        .find(|branch| branch["properties"]["type"]["const"] == "stage")
        .expect("stage branch");
    assert!(required_fields(stage).contains(&"detail".to_owned()));
    let tool_start = branches
        .iter()
        .find(|branch| {
            branch["properties"]["type"]["const"] == "tool"
                && branch["properties"]["phase"]["const"] == "start"
        })
        .expect("tool start branch");
    assert!(required_fields(tool_start).contains(&"summary".to_owned()));
    assert!(required_fields(tool_start).contains(&"call_id".to_owned()));

    let valid = json!({"v":1,"seq":2,"type":"stage","stage":"edit","detail":"x"});
    validate_event_data(&EventType::AttemptProgress, &valid).expect("valid stage");
    for invalid in [
        json!({"v":1,"seq":2,"type":"stage","stage":"edit","detail":"x","attempt_id":"att_01JBX4Q9ZK3M8T5V2N7RHC6WYD"}),
        json!({"v":1,"seq":3,"type":"tool","phase":"start","call_id":"c1","tool":"Bash","summary":"x","ok":true}),
        json!({"v":1,"seq":3,"type":"tool","phase":"end","call_id":"c1","tool":"Bash","ok":true,"duration_ms":1,"summary":"x"}),
        json!({"v":1,"seq":5,"type":"usage","input_tokens":1,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"turns":0,"detail":"x"}),
    ] {
        assert!(
            validate_event_data(&EventType::AttemptProgress, &invalid).is_err(),
            "invalid progress accepted: {invalid}"
        );
    }
}

#[test]
fn guardian_reason_and_class_vocabularies_are_closed() {
    let reasons = schema::<GuardianReasonCode>();
    assert_eq!(reasons["enum"].as_array().expect("reason enum").len(), 27);
    let classes = schema::<GuardianTriageClass>();
    assert_eq!(classes["enum"].as_array().expect("class enum").len(), 14);

    let valid = schema::<GuardianTriagedData>();
    assert_eq!(
        valid["properties"]["action"]["$ref"],
        "#/$defs/GuardianTriageAction"
    );
    assert_eq!(
        valid["$defs"]["GuardianTriageAction"]["enum"]
            .as_array()
            .expect("action enum")
            .len(),
        7
    );
}

fn triaged_round_closed() -> Value {
    json!({
        "event_ref": {"source":"settlement","head_sha":"f2c03d84a858156b0121130fd1ec2f391aa90865"},
        "action": "round_closed",
        "outcome": "pushed",
        "class": "ci_failure_in_scope",
        "head_before": "f2c03d84a858156b0121130fd1ec2f391aa90865",
        "head_after": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
        "failing_before": [],
        "failing_after": [],
        "progress": "done",
        "stalled_rounds": 0,
        "round_id": "01JBX4Q9ZK3M8T5V2N7RHC6WYD",
        "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "rule": "H-02",
        "also_matched": [],
        "config_rev": 18240,
        "action_ids": []
    })
}

#[test]
fn guardian_triaged_and_escalated_fail_closed() {
    let round = triaged_round_closed();
    validate_event_data(&EventType::GuardianTriaged, &round).expect("valid round_closed");

    let mut invalid = round.clone();
    invalid["progress"] = json!(null);
    assert!(validate_event_data(&EventType::GuardianTriaged, &invalid).is_err());

    let mut invalid = round.clone();
    invalid["fingerprint"] =
        json!("sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a");
    assert!(validate_event_data(&EventType::GuardianTriaged, &invalid).is_err());

    let mut invalid = round.clone();
    invalid.as_object_mut().unwrap().remove("progress");
    assert!(validate_event_data(&EventType::GuardianTriaged, &invalid).is_err());

    let mut invalid = json!({
        "event_ref": {"source":"forge","kind":"check_suite_completed","dedup_key":"k"},
        "action": "finalize",
        "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "rule": "K",
        "also_matched": [],
        "config_rev": 1,
        "action_ids": [],
        "progress": null
    });
    assert!(validate_event_data(&EventType::GuardianTriaged, &invalid).is_err());
    invalid.as_object_mut().unwrap().remove("progress");
    validate_event_data(&EventType::GuardianTriaged, &invalid).expect("valid finalize");

    let invalid = json!({
        "event_ref": {"source":"forge","kind":"check_suite_completed","dedup_key":"k"},
        "action": "finalize",
        "class": "nit",
        "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "rule": "K-13",
        "also_matched": [],
        "config_rev": 1,
        "action_ids": []
    });
    assert!(validate_event_data(&EventType::GuardianTriaged, &invalid).is_err());

    let escalated = json!({
        "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "class": "unknown",
        "reason_code": "guardian.unknown_event",
        "inbox_id": "inb_01JBX2F5H7K9M1P3R5T7V9X0ZC",
        "suppressed_duplicate": false
    });
    let typed: GuardianEscalatedData =
        serde_json::from_value(escalated.clone()).expect("typed escalation");
    typed.validate().expect("valid escalation");
    validate_event_data(&EventType::GuardianEscalated, &escalated).expect("valid escalation data");

    let invalid = json!({
        "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "class": "security_finding",
        "reason_code": "guardian.unknown_event",
        "inbox_id": "inb_01JBX2F5H7K9M1P3R5T7V9X0ZC",
        "suppressed_duplicate": false
    });
    let error = validate_event_data(&EventType::GuardianEscalated, &invalid).unwrap_err();
    assert!(error.pointer.contains("/reason_code"));
}

#[test]
fn guardian_heal_requested_validates_problem_sets() {
    let valid = json!({
        "round_id": "01JBX4Q9ZK3M8T5V2N7RHC6WYD",
        "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB",
        "task": "nit_batch",
        "head_sha": "f2c03d84a858156b0121130fd1ec2f391aa90865",
        "problems": [
            {"fingerprint":"sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a","kind":"nit"},
            {"fingerprint":"sha256:1d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a","kind":"nit"}
        ],
        "batch_included": ["sha256:1d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a"],
        "request_digest": "sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a",
        "config_rev": 18240
    });
    let typed: GuardianHealRequestedData =
        serde_json::from_value(valid.clone()).expect("typed heal request");
    typed.validate().expect("valid heal request");
    validate_event_data(&EventType::GuardianHealRequested, &valid).expect("valid heal request");

    let mut invalid = valid.clone();
    invalid["problems"][0]["kind"] = json!("ci_failure_in_scope");
    invalid["batch_included"] =
        json!(["sha256:0d5c4b6e2a91f3d87c0b5e4a3f2d1c6b8a7e9f0d1c2b3a4e5f6a7b8c9d0e1f2a"]);
    assert!(validate_event_data(&EventType::GuardianHealRequested, &invalid).is_err());

    let mut invalid = valid;
    invalid["problems"] = json!([]);
    assert!(validate_event_data(&EventType::GuardianHealRequested, &invalid).is_err());
}

#[test]
fn approval_shapes_are_closed_and_head_changes_require_two_heads() {
    let invalidated = json!({
        "reason": "head_changed",
        "previous_head_sha": "f2c03d84a858156b0121130fd1ec2f391aa90865",
        "current_head_sha": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b"
    });
    validate_event_data(&EventType::ApprovalInvalidated, &invalidated).expect("valid head_changed");

    let mut invalid = invalidated;
    invalid["current_head_sha"] = invalid["previous_head_sha"].clone();
    assert!(validate_event_data(&EventType::ApprovalInvalidated, &invalid).is_err());

    validate_event_data(&EventType::ApprovalExpired, &json!({})).expect("valid expired");
    assert!(validate_event_data(
        &EventType::ApprovalExpired,
        &json!({"expired_at": "2026-10-24T12:41:03.120Z"})
    )
    .is_err());

    let denied = json!({"reason": "先别合"});
    let typed: ApprovalDeniedData = serde_json::from_value(denied.clone()).expect("typed denied");
    typed.validate().expect("valid denied");
    validate_event_data(&EventType::ApprovalDenied, &denied).expect("valid denied");
    let error =
        validate_event_data(&EventType::ApprovalDenied, &json!({"reason": null})).unwrap_err();
    assert_eq!(error.pointer, "/reason");

    let schema = schema::<ApprovalExpiredData>();
    assert_eq!(schema["maxProperties"], 0);
}

fn outbox_enqueued_fixture() -> Value {
    json!({
        "action": {
            "delivery_id": "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM",
            "kind": "create_pr",
            "payload": {
                "repo": "acme/shop-api",
                "head": "gantry/42-fix-pagination",
                "base": "main",
                "title": "Fix pagination off-by-one in /orders",
                "body": "Fixes the off-by-one when page size divides total count.",
                "draft": false,
                "closes": [42],
                "expected_head_oid": "d7d018f867d6c5b78e82906386420d45a4d84486",
                "maintainer_can_modify": false
            },
            "target": "pr:gantry/42-fix-pagination->main",
            "intent": {
                "base": "main",
                "head": "gantry/42-fix-pagination",
                "title": "Fix pagination off-by-one in /orders",
                "body": "Fixes the off-by-one when page size divides total count.",
                "draft": false,
                "closes": [42]
            },
            "idempotency_key": "gk1_anbfwcip7r36is6wsqcuxxfd34",
            "requires_approval": null,
            "depends_on": [],
            "priority": "normal",
            "not_before": null,
            "epoch": 0,
            "coalesce_key": null,
            "enqueued_by": {"module": "engine", "causation_id": null}
        }
    })
}

#[test]
fn outbox_enqueued_binds_kind_payload_target_and_intent() {
    let valid = outbox_enqueued_fixture();
    let typed: OutboxEnqueuedData = serde_json::from_value(valid.clone()).expect("typed enqueue");
    typed.validate().expect("valid enqueue");
    assert_eq!(
        serde_json::to_value(&typed).expect("round-trip serialization"),
        valid
    );
    validate_event_data(&EventType::OutboxEnqueued, &valid).expect("valid event data");

    let mut invalid = valid.clone();
    invalid["action"]["target"] = json!("pr:other");
    assert!(validate_event_data(&EventType::OutboxEnqueued, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["action"]["intent"]["closes"] = json!([7]);
    assert!(validate_event_data(&EventType::OutboxEnqueued, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["action"]
        .as_object_mut()
        .unwrap()
        .remove("requires_approval");
    assert!(serde_json::from_value::<OutboxEnqueuedData>(invalid).is_err());

    let mut invalid = valid.clone();
    invalid["action"]["enqueued_by"]
        .as_object_mut()
        .unwrap()
        .remove("causation_id");
    assert!(serde_json::from_value::<OutboxEnqueuedData>(invalid).is_err());

    let mut invalid = valid.clone();
    invalid["action"]["coalesce_key"] = json!("coalesce");
    assert!(validate_event_data(&EventType::OutboxEnqueued, &invalid).is_err());

    let mut invalid = valid;
    invalid["action"]["kind"] = json!("push_branch");
    assert!(serde_json::from_value::<OutboxEnqueuedData>(invalid).is_err());

    let executing = json!({"attempts": 1, "replay": false});
    let typed: OutboxExecutingData =
        serde_json::from_value(executing.clone()).expect("typed executing");
    typed.validate().expect("valid executing");
    validate_event_data(&EventType::OutboxExecuting, &executing).expect("valid executing");
    assert!(validate_event_data(
        &EventType::OutboxExecuting,
        &json!({"attempts":0,"replay":false})
    )
    .is_err());

    let cancelled = schema::<OutboxCancelReason>();
    assert_eq!(cancelled["enum"].as_array().unwrap().len(), 4);
    validate_event_data(&EventType::OutboxCancelled, &json!({"reason":"manual"}))
        .expect("valid cancel");
    assert!(validate_event_data(&EventType::OutboxCancelled, &json!({})).is_err());
}

#[test]
fn forge_synced_rejects_cross_target_and_untyped_changes() {
    let valid = json!({
        "target": {"type":"pr","number":57,"node_id":"PR_kwDOabc123_example"},
        "source": "webhook",
        "changes": [
            {"field":"checks","from":{"passed":12,"failed":0,"pending":1,"required_ok":true},"to":{"passed":12,"failed":1,"pending":0,"required_ok":false}}
        ],
        "synced_at": "2026-10-24T12:41:03.120Z"
    });
    validate_event_data(&EventType::ForgeSynced, &valid).expect("valid synced");
    let typed: ForgeSyncedData = serde_json::from_value(valid.clone()).expect("typed synced");
    typed.validate().expect("valid synced event");

    let mut invalid = valid.clone();
    invalid["target"] = json!({"type":"issue","number":57,"node_id":"n"});
    assert!(validate_event_data(&EventType::ForgeSynced, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["changes"][0]["from"] = json!({"passed":1,"failed":0,"pending":0});
    assert!(validate_event_data(&EventType::ForgeSynced, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["changes"] = json!([{"field":"title","from":"x","to":"y"}]);
    assert!(validate_event_data(&EventType::ForgeSynced, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["changes"] = json!([{"field":"draft","from":false,"to":false}]);
    assert!(validate_event_data(&EventType::ForgeSynced, &invalid).is_err());

    let mut invalid = valid;
    invalid["changes"] = json!([
        {"field":"draft","from":false,"to":true},
        {"field":"draft","from":false,"to":true}
    ]);
    assert!(validate_event_data(&EventType::ForgeSynced, &invalid).is_err());
}

#[test]
fn forge_synced_initial_snapshot_carries_all_fields() {
    let pr = json!({
        "target": {"type":"pr","number":57,"node_id":"PR_kwDOabc123_example"},
        "source": "webhook",
        "changes": [],
        "synced_at": "2026-10-24T12:41:03.120Z",
        "initial": {
            "type": "pr",
            "state": "open",
            "title": {"text":"Fix pagination","source":"github:pr.title","truncated":false},
            "head_sha": "1a1c743f1c1f1e5faebe1f39feb3ffe2244cfd0b",
            "base_ref": "main",
            "base_sha": "f2c03d84a858156b0121130fd1ec2f391aa90865",
            "draft": false,
            "mergeable_state": "unknown",
            "in_merge_queue": false,
            "checks": {"passed":0,"failed":0,"pending":0,"required_ok":false},
            "review_state": "none"
        }
    });
    validate_event_data(&EventType::ForgeSynced, &pr).expect("valid initial pr");
    let typed: ForgeSyncedData = serde_json::from_value(pr.clone()).expect("typed initial");
    typed.validate().expect("valid initial pr event");
    assert_eq!(serde_json::to_value(&typed).unwrap(), pr, "round trip");

    let issue = json!({
        "target": {"type":"issue","number":42,"node_id":"I_kwDOabc"},
        "source": "reconcile",
        "changes": [],
        "synced_at": "2026-10-24T12:41:03.120Z",
        "initial": {
            "type": "issue",
            "state": "open",
            "state_reason": null,
            "title": {"text":"Pagination bug","source":"github:issue.title","truncated":false}
        }
    });
    validate_event_data(&EventType::ForgeSynced, &issue).expect("valid initial issue");

    // 非首次同步不带 initial，序列化时省略该键。
    let regular: ForgeSyncedData = serde_json::from_value(json!({
        "target": {"type":"pr","number":57,"node_id":"n"},
        "source": "reconcile",
        "changes": [{"field":"draft","from":false,"to":true}],
        "synced_at": "2026-10-24T12:41:03.120Z"
    }))
    .unwrap();
    assert!(serde_json::to_value(&regular)
        .unwrap()
        .get("initial")
        .is_none());

    let reject = |mutate: &dyn Fn(&mut Value)| {
        let mut value = pr.clone();
        mutate(&mut value);
        assert!(
            validate_event_data(&EventType::ForgeSynced, &value).is_err(),
            "{value}"
        );
    };
    // initial 与 changes 不能同时出现。
    reject(&|v| v["changes"] = json!([{"field":"draft","from":false,"to":true}]));
    // 缺字段、多字段、null。
    reject(&|v| {
        v["initial"].as_object_mut().unwrap().remove("checks");
    });
    reject(&|v| v["initial"]["body"] = json!("x"));
    reject(&|v| v["initial"]["head_sha"] = Value::Null);
    reject(&|v| v["initial"] = Value::Null);
    // 类型与 target 不符；Issue 字段集不能用在 PR 上。
    reject(&|v| v["initial"]["type"] = json!("issue"));
    reject(&|v| v["target"]["type"] = json!("issue"));
    reject(&|v| v["initial"]["mergeable_state"] = json!(""));

    let mut merged_issue = issue.clone();
    merged_issue["initial"]["state"] = json!("merged");
    assert!(validate_event_data(&EventType::ForgeSynced, &merged_issue).is_err());
    // 首次快照之外，changes 仍然不能为空。
    let mut empty = issue;
    empty.as_object_mut().unwrap().remove("initial");
    assert!(validate_event_data(&EventType::ForgeSynced, &empty).is_err());
}

fn budget_token_fixture() -> Value {
    json!({
        "ledger": {"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},
        "dimension": "tokens_by_tier",
        "tier": "worker",
        "delta": 32600,
        "used_after": 123456,
        "source": {"kind":"attempt","attempt_id":"att_01JBX4Q9ZK3M8T5V2N7RHC6WYD"},
        "usage": {"input_tokens":18200,"output_tokens":2400,"cache_read_tokens":9000,"cache_write_tokens":1200,"requests":1},
        "weighted_tokens": 32600,
        "weight_audit": {"basis":"job_token","config_rev":18240,"weights":{"input":1.0,"output":5.0,"cache_read":0.1,"cache_write":1.25}}
    })
}

#[test]
fn budget_charged_checks_dimension_source_and_settlement_evidence() {
    let valid = budget_token_fixture();
    let typed: BudgetChargedData = serde_json::from_value(valid.clone()).expect("typed budget");
    typed.validate().expect("valid budget charge");
    validate_event_data(&EventType::BudgetCharged, &valid).expect("valid budget event");

    let mut invalid = valid.clone();
    invalid["source"] = json!({"kind":"node","node_id":"nod_01JBX7A1B2C3D4E5F6G7H8J9K0"});
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["weighted_tokens"] = json!(1);
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["weight_audit"] = json!({"basis":"svc_records","record_count":0,"weight_revs":[{"config_rev":1,"weighted_tokens":32600}],"records_digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"});
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["tier"] = json!(null);
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());

    let mut invalid = valid.clone();
    invalid["usage"]["attempt_id"] = json!("att_01JBX4Q9ZK3M8T5V2N7RHC6WYD");
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());

    let direct_call = json!({
        "ledger": {"subject":"conversation","id":"cnv_01JBX2D1F3H5K7M9P2R4T6V8XZ","day":"2026-10-24"},
        "dimension": "tokens_by_tier",
        "tier": "utility",
        "delta": 11062,
        "used_after": 21062,
        "source": {"kind":"direct_call","call_id":"01JBX5D2F4H6K8M0P2R4T6V8XZ"},
        "usage": {"input_tokens":6402,"output_tokens":820,"cache_read_tokens":5600,"cache_write_tokens":0,"requests":2},
        "weighted_tokens": 11062,
        "weight_audit": {
            "basis":"svc_records","record_count":2,"first_seq":90211,"last_seq":90212,
            "weight_revs":[{"config_rev":18240,"weighted_tokens":11062}],
            "records_digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"
        }
    });
    validate_event_data(&EventType::BudgetCharged, &direct_call).expect("valid direct call");

    let mut invalid = direct_call.clone();
    invalid["weight_audit"] = valid["weight_audit"].clone();
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());

    let mut invalid = direct_call.clone();
    invalid["weight_audit"]["record_count"] = json!(0);
    invalid["weight_audit"]
        .as_object_mut()
        .unwrap()
        .remove("first_seq");
    invalid["weight_audit"]
        .as_object_mut()
        .unwrap()
        .remove("last_seq");
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());

    let mut zero_records = direct_call;
    zero_records["delta"] = json!(0);
    zero_records["weighted_tokens"] = json!(0);
    zero_records["weight_audit"] = json!({
        "basis":"svc_records",
        "record_count":0,
        "weight_revs":[],
        "records_digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"
    });
    assert!(validate_event_data(&EventType::BudgetCharged, &zero_records).is_err());

    let invalid = json!({
        "ledger": {"subject":"delivery","id":"dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD"},
        "dimension":"nodes","delta":0,"used_after":1,
        "source":{"kind":"node","node_id":"nod_01JBX7A1B2C3D4E5F6G7H8J9K0"}
    });
    assert!(validate_event_data(&EventType::BudgetCharged, &invalid).is_err());
}

#[test]
fn policy_denied_separates_action_patch_and_proposal_origins() {
    let action = json!({
        "origin":"action","action_id":"push_branch","kind":"push_branch",
        "target":{"kind":"branch","repo":"acme/shop-api","branch":"gantry/42-fix-pagination"},
        "reason_code":"policy.action_denied",
        "subject":"d7d018f867d6c5b78e82906386420d45a4d84486",
        "config_rev":18240
    });
    let typed: PolicyDeniedData = serde_json::from_value(action.clone()).expect("typed action");
    typed.validate().expect("valid action denial");
    validate_event_data(&EventType::PolicyDenied, &action).expect("valid action event");

    let mut invalid = action.clone();
    invalid["kind"] = json!("create_pr");
    assert!(validate_event_data(&EventType::PolicyDenied, &invalid).is_err());

    let mut invalid = action;
    invalid["action_id"] = json!("delete_branch");
    assert!(validate_event_data(&EventType::PolicyDenied, &invalid).is_err());

    let patch = json!({
        "origin":"patch",
        "target":{
            "kind":"patch","repo":"acme/shop-api",
            "paths":[{"text":"Cargo.lock","source":"sandbox:patch_path","truncated":false}],
            "path_digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333",
            "rules":["lockfiles"]
        },
        "reasons":[{"code":"policy.path_protected:lockfiles","path":{"text":"Cargo.lock","source":"sandbox:patch_path","truncated":false},"rule_id":"lockfiles"}],
        "config_rev":18240,
        "subject":"sha256:2222222222222222222222222222222222222222222222222222222222222222"
    });
    validate_event_data(&EventType::PolicyDenied, &patch).expect("valid patch denial");

    let mut invalid = patch.clone();
    invalid["subject"] = json!("d7d018f867d6c5b78e82906386420d45a4d84486");
    assert!(validate_event_data(&EventType::PolicyDenied, &invalid).is_err());

    let mut invalid = patch.clone();
    invalid["reasons"][0]["code"] = json!("policy.not_a_reason");
    assert!(validate_event_data(&EventType::PolicyDenied, &invalid).is_err());

    let proposal = json!({
        "origin":"proposal","proposal_id":"p2","proposal_kind":"reply_review_thread",
        "target_handle":"thread-1","reason_code":"policy.approval_denied","config_rev":18240
    });
    validate_event_data(&EventType::PolicyDenied, &proposal).expect("valid proposal denial");

    let mut invalid = proposal;
    invalid["target"] = json!({"kind":"pr","repo":"acme/shop-api","number":1});
    assert!(validate_event_data(&EventType::PolicyDenied, &invalid).is_err());

    let targets = schema::<PolicyTarget>();
    assert_eq!(
        targets["oneOf"].as_array().expect("target branches").len(),
        10
    );
    let reasons = schema::<PolicyReasonCode>();
    assert_eq!(
        reasons["enum"].as_array().expect("policy reasons").len(),
        10
    );
}

#[test]
fn every_new_event_data_type_generates_a_schema() {
    assert_eq!(
        schema::<AttemptProgressData>()["title"],
        "AttemptProgressData"
    );
    assert_eq!(
        schema::<GuardianTriagedData>()["title"],
        "GuardianTriagedData"
    );
    assert_eq!(
        schema::<GuardianHealRequestedData>()["title"],
        "GuardianHealRequestedData"
    );
    assert_eq!(
        schema::<GuardianEscalatedData>()["title"],
        "GuardianEscalatedData"
    );
    assert_eq!(
        schema::<gantry_core::events::ApprovalInvalidatedData>()["title"],
        "ApprovalInvalidatedData"
    );
    assert_eq!(
        schema::<ApprovalExpiredData>()["title"],
        "ApprovalExpiredData"
    );
    assert_eq!(
        schema::<ApprovalDeniedData>()["title"],
        "ApprovalDeniedData"
    );
    assert_eq!(
        schema::<OutboxEnqueuedData>()["title"],
        "OutboxEnqueuedData"
    );
    assert_eq!(
        schema::<OutboxExecutingData>()["title"],
        "OutboxExecutingData"
    );
    assert_eq!(
        schema::<gantry_core::events::OutboxCancelledData>()["title"],
        "OutboxCancelledData"
    );
    assert_eq!(schema::<ForgeSyncedData>()["title"], "ForgeSyncedData");
    assert_eq!(schema::<BudgetChargedData>()["title"], "BudgetChargedData");
    assert_eq!(schema::<PolicyDeniedData>()["title"], "PolicyDeniedData");
}
