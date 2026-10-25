//! Outbox 动作 payload 的强类型解码与 Proposal schema 的 kind 判别。
//!
//! 线上形状不变：`kind` 与裸 `payload` 对象并列；`kind` 决定 payload 类型。

use std::collections::BTreeMap;

use gantry_core::entities::{ForgeActionKind, OutboxAction};
use gantry_core::forge::ActionPayload;
use gantry_core::ports::{EnqueuedByModule, OutboxEnqueueEnvelope};
use gantry_core::worker::{ProposalKind, Proposals, ProposalsMode};
use serde_json::{json, Value};

const ACTIONS: &str = include_str!("../fixtures/protocol/forge/actions.json");
const CREATE_PR_OUTBOX: &str = include_str!("../fixtures/protocol/forge/create_pr_outbox.json");
const JOB_SPEC: &str = include_str!("../fixtures/protocol/worker/job_spec.json");

fn parse(value: &str) -> Value {
    serde_json::from_str(value).expect("fixture must be valid JSON")
}

fn actions() -> BTreeMap<String, Value> {
    serde_json::from_str(ACTIONS).expect("actions fixture must decode")
}

/// 与 `create_pr_outbox.json` 的入队信封同形，补齐 OutboxAction 的执行字段。
fn outbox_with(kind: &str, payload: Value) -> Value {
    json!({
        "id": "act_01JBA1F3Q9TX2N7B5D8H0M4RVC",
        "project_id": "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK",
        "delivery_id": "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM",
        "idempotency_key": "gk1_anbfwcip7r36is6wsqcuxxfd34",
        "kind": kind,
        "payload": payload,
        "requires_approval": null,
        "depends_on": [],
        "priority": "normal",
        "not_before": null,
        "epoch": 0,
        "status": "queued",
        "attempts": 0,
        "lease_until": null,
        "last_error": null,
        "expected_effects": [],
        "github_ref": null,
        "created_at": "2026-10-24T12:41:03.120Z",
        "updated_at": "2026-10-24T12:41:03.120Z"
    })
}

#[test]
fn outbox_action_decodes_each_kind_with_typed_payload() {
    let actions = actions();
    assert_eq!(actions.len(), ForgeActionKind::ALL.len());
    for kind in ForgeActionKind::ALL {
        let raw = outbox_with(kind.as_str(), actions[kind.as_str()].clone());
        let action: OutboxAction = serde_json::from_value(raw.clone())
            .unwrap_or_else(|error| panic!("{kind:?} must decode: {error}"));
        assert_eq!(action.kind, kind);
        assert_eq!(action.payload.kind(), kind);
        assert_eq!(serde_json::to_value(&action).unwrap(), raw);
    }
}

/// 每个 `kind` 常量必须与它自己的 payload schema 在同一个 `oneOf` 分支。
#[test]
fn outbox_action_schema_pairs_each_kind_with_its_payload() {
    let schema = serde_json::to_value(schemars::schema_for!(OutboxAction)).unwrap();
    let variants = schema["oneOf"]
        .as_array()
        .expect("OutboxAction must be a top-level oneOf");
    assert_eq!(variants.len(), ForgeActionKind::ALL.len());
    for kind in ForgeActionKind::ALL {
        let variant = variants
            .iter()
            .find(|variant| {
                variant["allOf"][1]["properties"]["kind"]["const"] == json!(kind.as_str())
            })
            .unwrap_or_else(|| panic!("missing OutboxAction variant {kind:?}"));
        assert_eq!(
            variant["allOf"][0]["$ref"],
            json!("#/$defs/OutboxActionBase")
        );
        let payload_ref = variant["allOf"][1]["properties"]["payload"]["$ref"]
            .as_str()
            .expect("payload variant must be a $ref");
        let definition = &schema["$defs"][payload_ref.trim_start_matches("#/$defs/")];
        assert_eq!(
            definition["additionalProperties"],
            json!(false),
            "{payload_ref} must reject unknown fields"
        );
    }
    assert!(
        schema.get("additionalProperties").is_none(),
        "entity output must tolerate unknown top-level fields"
    );
    assert!(
        schema["$defs"].get("ActionPayload").is_none(),
        "the unconstrained payload union must not remain in the generated schema"
    );
    assert!(
        schema["$defs"]["OutboxActionBase"]
            .get("additionalProperties")
            .is_none(),
        "OutboxActionBase must keep the entity-output unknown-field tolerance"
    );
}

#[test]
fn outbox_enqueue_envelope_roundtrips_documented_shape() {
    let raw = parse(CREATE_PR_OUTBOX);
    let envelope: OutboxEnqueueEnvelope =
        serde_json::from_value(raw.clone()).expect("documented enqueue envelope must decode");
    assert_eq!(envelope.kind, ForgeActionKind::CreatePr);
    assert!(matches!(envelope.payload, ActionPayload::CreatePr(_)));
    assert_eq!(envelope.enqueued_by.module, EnqueuedByModule::Engine);
    assert_eq!(serde_json::to_value(&envelope).unwrap(), raw);

    let schema = serde_json::to_value(schemars::schema_for!(OutboxEnqueueEnvelope)).unwrap();
    let variants = schema["oneOf"]
        .as_array()
        .expect("OutboxEnqueueEnvelope must be a top-level oneOf");
    assert_eq!(variants.len(), ForgeActionKind::ALL.len());
    for kind in ForgeActionKind::ALL {
        let variant = variants
            .iter()
            .find(|variant| {
                variant["allOf"][1]["properties"]["kind"]["const"] == json!(kind.as_str())
            })
            .unwrap_or_else(|| panic!("missing OutboxEnqueueEnvelope variant {kind:?}"));
        assert_eq!(
            variant["allOf"][0]["$ref"],
            json!("#/$defs/OutboxEnqueueEnvelopeBase")
        );
    }
    assert_eq!(
        schema["$defs"]["OutboxEnqueueEnvelopeBase"]["additionalProperties"],
        json!(false),
        "the enqueue envelope is a strict input and must reject unknown top-level fields"
    );
}

#[test]
fn outbox_enqueue_envelope_rejects_kind_payload_mismatch() {
    let mut raw = parse(CREATE_PR_OUTBOX);
    raw["kind"] = json!("push_branch");
    assert!(serde_json::from_value::<OutboxEnqueueEnvelope>(raw).is_err());

    let mut raw = parse(CREATE_PR_OUTBOX);
    raw["enqueued_by"]["module"] = json!("scheduler");
    assert!(serde_json::from_value::<OutboxEnqueueEnvelope>(raw).is_err());
}

#[test]
fn outbox_action_payload_must_match_kind() {
    let raw = outbox_with("push_branch", actions()["create_pr"].clone());
    assert!(serde_json::from_value::<OutboxAction>(raw).is_err());
}

#[test]
fn outbox_action_rejects_unknown_payload_fields_for_every_kind() {
    for kind in ForgeActionKind::ALL {
        let mut payload = actions()[kind.as_str()].clone();
        payload["unexpected"] = json!(true);
        assert!(
            serde_json::from_value::<OutboxAction>(outbox_with(kind.as_str(), payload)).is_err(),
            "{kind:?} must reject unknown payload fields"
        );
    }
}

#[test]
fn outbox_action_new_derives_kind_from_payload() {
    let outbox = parse(CREATE_PR_OUTBOX);
    let original: OutboxAction =
        serde_json::from_value(outbox_with("create_pr", outbox["payload"].clone())).unwrap();
    assert!(matches!(original.payload, ActionPayload::CreatePr(_)));

    let rebuilt = OutboxAction::new(
        original.id,
        original.project_id,
        original.delivery_id,
        original.idempotency_key.clone(),
        original.payload.clone(),
        original.created_at,
    );
    rebuilt.validate().unwrap();
    assert_eq!(rebuilt.kind, ForgeActionKind::CreatePr);
    assert_eq!(rebuilt.payload, original.payload);
    assert_eq!(rebuilt.updated_at, original.created_at);
    assert_eq!(
        serde_json::to_value(&rebuilt).unwrap()["payload"],
        outbox["payload"]
    );
}

#[test]
fn proposals_mode_uses_worker_protocol_wire_shape() {
    assert_eq!(
        serde_json::to_value(ProposalsMode::Forbidden).unwrap(),
        json!("forbidden")
    );
    let allowed = ProposalsMode::Allowed {
        allowed_kinds: vec![ProposalKind::OpenPr, ProposalKind::Escalate],
        max: 3,
    };
    assert_eq!(
        serde_json::to_value(&allowed).unwrap(),
        json!({"allowed_kinds": ["open_pr", "escalate"], "max": 3})
    );
    assert_eq!(
        serde_json::from_value::<ProposalsMode>(json!("forbidden")).unwrap(),
        ProposalsMode::Forbidden
    );
    assert_eq!(
        serde_json::from_value::<ProposalsMode>(
            json!({"allowed_kinds": ["open_pr", "escalate"], "max": 3})
        )
        .unwrap(),
        allowed
    );

    assert!(serde_json::from_value::<ProposalsMode>(json!("Forbidden")).is_err());
    assert!(serde_json::from_value::<ProposalsMode>(
        json!({"Allowed": {"allowed_kinds": ["open_pr"], "max": 1}})
    )
    .is_err());
    assert!(serde_json::from_value::<ProposalsMode>(
        json!({"allowed_kinds": ["open_pr"], "max": 1, "extra": true})
    )
    .is_err());
}

#[test]
fn proposal_schema_pairs_each_kind_with_its_params() {
    let schema = serde_json::to_value(schemars::schema_for!(Proposals)).unwrap();
    let variants = schema["$defs"]["Proposal"]["oneOf"]
        .as_array()
        .expect("Proposal must be a oneOf");
    assert_eq!(variants.len(), 4);

    for (kind, params) in [
        ("open_pr", "OpenPrParams"),
        ("reply_review_thread", "ReplyReviewThreadParams"),
        ("resolve_review_thread", "ResolveReviewThreadParams"),
        ("escalate", "EscalateParams"),
    ] {
        let variant = variants
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == json!(kind))
            .unwrap_or_else(|| panic!("missing proposal variant {kind}"));
        assert_eq!(
            variant["properties"]["params"]["$ref"],
            json!(format!("#/$defs/{params}"))
        );
        assert_eq!(
            variant["required"],
            json!(["id", "kind", "params", "rationale"])
        );
        assert_eq!(variant["additionalProperties"], json!(false));
    }
}

#[test]
fn proposals_mode_schema_accepts_forbidden_string_or_allowed_object() {
    let schema = serde_json::to_value(schemars::schema_for!(gantry_core::worker::JobSpec)).unwrap();
    let variants = schema["$defs"]["ProposalsMode"]["oneOf"]
        .as_array()
        .expect("ProposalsMode must be a oneOf");
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0]["const"], json!("forbidden"));
    assert_eq!(
        variants[1]["properties"]["allowed_kinds"]["items"]["$ref"],
        json!("#/$defs/ProposalKind")
    );
    assert_eq!(variants[1]["required"], json!(["allowed_kinds", "max"]));
    assert_eq!(variants[1]["additionalProperties"], json!(false));

    let spec: gantry_core::worker::JobSpec = serde_json::from_str(JOB_SPEC).unwrap();
    assert_eq!(
        serde_json::to_value(&spec).unwrap()["output"]["proposals"],
        json!({"allowed_kinds": ["open_pr", "escalate"], "max": 3})
    );
}
