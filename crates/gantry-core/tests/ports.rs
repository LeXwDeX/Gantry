//! `core::ports` 的契约测试：
//! - 17 个 trait 可 `dyn`；
//! - `ActorCtx` 字段与 P-78 一致；
//! - `TxHandle` 支持向下转型；
//! - `PrRefresher` 保持 `repo` + PR number；
//! - 命名空间化的暂存句柄、错误分支与脱敏。

use std::any::Any;

use async_trait::async_trait;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Actor, ActorKind};
use gantry_core::forge::PrHead;
use gantry_core::ids::{DeviceId, EventId, Ulid};
use gantry_core::ports::*;

fn assert_object_safe<T: ?Sized>() {}

#[test]
fn all_seventeen_ports_are_dyn_compatible() {
    assert_object_safe::<dyn ForgeReader>();
    assert_object_safe::<dyn OutboxPort>();
    assert_object_safe::<dyn SandboxService>();
    assert_object_safe::<dyn ModelProvider>();
    assert_object_safe::<dyn RepoService>();
    assert_object_safe::<dyn PolicyPort>();
    assert_object_safe::<dyn InboxPort>();
    assert_object_safe::<dyn EgressControl>();
    assert_object_safe::<dyn DeliveryControl>();
    assert_object_safe::<dyn ConversationService>();
    assert_object_safe::<dyn WorktreeLocks>();
    assert_object_safe::<dyn PrRefresher>();
    assert_object_safe::<dyn HealPort>();
    assert_object_safe::<dyn ClassifyPort>();
    assert_object_safe::<dyn WorkerAdapter>();
    assert_object_safe::<dyn SecretSource>();
    assert_object_safe::<dyn SecretSink>();
    assert_object_safe::<dyn PathRuleSet>();
    assert_object_safe::<dyn AddedLineSource>();
}

#[test]
fn port_counts_match_the_contract() {
    assert_eq!(PORT_GROUP_COUNT, 16);
    assert_eq!(PORT_TRAIT_COUNT, 17);
    assert_eq!(PORT_METHOD_COUNT, 115);
}

fn sample_ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Device, "dev_01J00000000000000000000000"),
        device: Some(DeviceId::from_parts(0, 1).expect("device id")),
        role: Some(gantry_core::entities::DeviceRole::Operator),
        client_profile: Some(ClientProfile::Desktop),
        request_id: Ulid::from_parts(0, 2).expect("valid request id"),
        idempotency_key: Some("idem-1".into()),
        dedup_key: Some("dedup-1".into()),
        causation_id: Some(EventId::from_parts(0, 3).expect("event id")),
    }
}

#[test]
fn actor_ctx_fields_match_p78() {
    let ctx = sample_ctx();
    let ActorCtx {
        actor,
        device,
        role,
        client_profile,
        request_id,
        idempotency_key,
        dedup_key,
        causation_id,
    } = ctx;

    let _: Actor = actor;
    let _: Option<DeviceId> = device;
    let _: Option<gantry_core::entities::DeviceRole> = role;
    let _: Option<ClientProfile> = client_profile;
    let _: RequestId = request_id;
    let _: Option<String> = idempotency_key;
    let _: Option<String> = dedup_key;
    let _: Option<EventId> = causation_id;

    assert_eq!(
        ACTOR_CTX_FIELDS,
        [
            "actor",
            "device",
            "role",
            "client_profile",
            "request_id",
            "idempotency_key",
            "dedup_key",
            "causation_id",
        ]
    );
}

#[test]
fn request_id_is_ulid_compatible() {
    let id: RequestId = Ulid::from_parts(42, 7).expect("valid ulid");
    let _: Ulid = id;
}

struct DummyTx {
    marker: u8,
}

impl TxHandle for DummyTx {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[test]
fn tx_handle_allows_store_downcast() {
    let mut tx = DummyTx { marker: 9 };
    let handle: &mut dyn TxHandle = &mut tx;
    let recovered = handle
        .as_any_mut()
        .downcast_mut::<DummyTx>()
        .expect("downcast to the concrete write transaction");
    assert_eq!(recovered.marker, 9);
}

struct MockRefresher;

#[async_trait]
impl PrRefresher for MockRefresher {
    async fn refresh_pr(
        &self,
        _repo: &RepoSlug,
        _number: u64,
        _ctx: &ActorCtx,
    ) -> Result<PrHead, ModuleError> {
        Err(ModuleError::NotFound)
    }
}

#[test]
fn refresher_keeps_repo_and_number_signature() {
    let repo = RepoSlug::parse("octo/example").expect("repo");
    let ctx = sample_ctx();
    let mock = MockRefresher;
    let _future = mock.refresh_pr(&repo, 42, &ctx);
    assert_eq!(repo.as_str(), "octo/example");
}

#[test]
fn staged_secret_namespaces_are_distinct() {
    let name = gantry_core::secret::SecretName::parse("model.api_key").expect("name");
    let staged_id_text = format!("stg_{}", "0".repeat(26));
    let staged_id = gantry_core::egress::StagedId::parse(&staged_id_text).expect("staged id");
    let egress: EgressStagedSecret = EgressStagedSecret {
        consumer: gantry_core::secret::Consumer::Egress,
        staged_id,
        secret_ref: name.clone(),
        version: SecretVersion::new(1).expect("version"),
    };
    let secret_version = SecretVersion::new(2).expect("version");
    let secrets: SecretsStagedSecret = SecretsStagedSecret {
        consumer: gantry_core::secret::Consumer::Pki,
        name,
        version: secret_version,
    };
    assert_eq!(egress.version.get(), 1);
    assert_eq!(secrets.version.get(), 2);
}

#[test]
fn secret_value_is_redacted_and_constant_time_comparable() {
    let left = SecretValue::from_bytes(b"top-secret".to_vec());
    let right = SecretValue::from_bytes(b"top-secret".to_vec());
    let other = SecretValue::from_bytes(b"other".to_vec());
    assert_eq!(left, right);
    assert_ne!(left, other);
    assert_eq!(format!("{left:?}"), "SecretValue(***)");
    assert!(!format!("{left:?}").contains("top-secret"));
}

#[test]
fn stage_request_redacts_value_in_debug() {
    let req = StageSecret {
        secret_ref: gantry_core::secret::SecretName::parse("model.api_key").expect("name"),
        value: gantry_core::secret::SecretString::new("sk-live-sentinel".into()),
    };
    let debug = format!("{req:?}");
    assert!(!debug.contains("sk-live-sentinel"));
    assert!(debug.contains("***"));
}

#[test]
fn egress_route_redacts_base_url() {
    let route = EgressRoute {
        base_url: "http://127.0.0.1:9000/s/gst1_secret/anthropic".into(),
        tier: gantry_core::entities::Tier::Planner,
        call_id: Ulid::from_parts(0, 1).expect("ulid"),
        ledger: LedgerId::unattributed(),
    };
    let debug = format!("{route:?}");
    assert!(!debug.contains("gst1_secret"));
    assert!(debug.contains("***"));
}

#[test]
fn safe_summary_strips_controls_and_bounds_length() {
    let summary = SafeSummary::new(format!("a\n\nb{}", "x".repeat(4000)));
    assert!(!summary.as_str().contains('\n'));
    assert!(summary.as_str().starts_with("a b"));
    assert!(summary.as_str().len() <= 1024);
    assert!(summary.as_str().ends_with('…'));

    let stripped = SafeSummary::new("a\u{0000}b");
    assert_eq!(stripped.as_str(), "ab");
}

#[test]
fn store_errors_are_branchable() {
    let error = StoreError::Conflict {
        code: "cursor_ahead",
    };
    assert_eq!(error.code(), "store_conflict");
    match error {
        StoreError::Conflict { code } => assert_eq!(code, "cursor_ahead"),
        _ => panic!("unexpected store error"),
    }
}

#[test]
fn port_errors_are_branchable_without_http() {
    let forge = ForgeError::RateLimited { retry_after: None };
    match forge {
        ForgeError::RateLimited { retry_after } => assert!(retry_after.is_none()),
        _ => panic!("unexpected forge error"),
    }

    let module = ModuleError::Conflict {
        message: "invalid_state".into(),
    };
    match module {
        ModuleError::Conflict { message } => assert_eq!(message, "invalid_state"),
        _ => panic!("unexpected module error"),
    }
}

#[test]
fn client_profile_has_both_profiles() {
    assert_ne!(ClientProfile::Desktop, ClientProfile::Mobile);
}
