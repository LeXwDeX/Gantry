//! `ApprovalService`：challenge / grant / deny、内部动作的创建与消费
//! （policy.md §6.4、§7.5、§7.6、§12.2「challenge / grant」「消费」）。

mod support;

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::canon::{sha256, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, DeviceRole,
    DeviceStatus, ForgeActionKind, Seq,
};
use gantry_core::events::EventType;
use gantry_core::ids::{Dev, Dlv, IdGen};
use gantry_core::ports::{
    ActionDecision, ActionFacts, ActionRequest, ClientProfile, MismatchReason, PageQuery,
    PolicyError, PolicyPort,
};
use gantry_core::settings::{SettingsResource, SettingsScope};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_fakes::forge::FakeForge;
use gantry_policy::service::{
    ApprovalQuery, ChallengeError, ChallengeReason, ConsumeError, ConsumeInternal, CurrentSubject,
    DeviceKey, GrantError, GrantRequest, InternalAction, InternalApprovalSpec, RevCheck,
};
use gantry_policy::{ApprovalService, PolicyApprovals, PolicyDeps, PolicyEngine, POLICY_MODULE};
use gantry_store::{Store, StoreDeps};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use support::*;

struct Desk {
    store: Store,
    clock: Arc<FakeClock>,
    engine: Arc<PolicyEngine>,
    approvals: PolicyApprovals,
}

impl Desk {
    async fn new() -> Self {
        let clock = Arc::new(FakeClock::new(MS as i64));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
        let store = Store::open_in_memory(
            StoreDeps {
                clock: clock.clone(),
                ids: ids.clone(),
            },
            &[POLICY_MODULE],
        )
        .await
        .expect("in-memory store");
        let forge = Arc::new(FakeForge::new_in(store.clone()));
        forge.insert_project(project());
        forge.upsert_pr(project_id(), "gantry/feature", pull_request(oid("head")));
        let engine = Arc::new(PolicyEngine::new(PolicyDeps {
            store: store.clone(),
            forge,
            clock: clock.clone(),
            ids,
        }));
        let approvals = PolicyApprovals::new(engine.clone(), Arc::new(SeqEntropy::new(11)));
        Self {
            store,
            clock,
            engine,
            approvals,
        }
    }

    /// `merge_pr`（PR 7、head）的 pending 审批。
    async fn merge_approval(&self) -> Approval {
        let req = ActionRequest {
            project_id: project_id(),
            delivery_id: Some(id::<Dlv>(3)),
            kind: ForgeActionKind::MergePr,
            facts: ActionFacts::MergePr {
                repo: slug(),
                number: 7,
                expected_head_sha: oid("head"),
            },
            requested_by: ctx().actor,
            epoch: 1,
        };
        let ActionDecision::NeedsApproval { spec, .. } = self
            .engine
            .decide_action(req, &ctx())
            .await
            .expect("decide")
        else {
            panic!("expected NeedsApproval");
        };
        self.engine
            .request_approval(spec, ctx())
            .await
            .expect("request approval")
    }

    async fn events(&self, ty: EventType) -> Vec<gantry_core::events::Event<serde_json::Value>> {
        let log = self.store.events();
        let mut out = Vec::new();
        let filter = gantry_store::EventFilter {
            project_ids: None,
            include: vec![gantry_store::TypePattern::Exact(ty)],
            exclude: Vec::new(),
        };
        let page = log.scan(Seq::ZERO, &filter, 200).await.expect("scan");
        out.extend(page.events);
        out
    }
}

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).expect("signing key")
}

fn device(seed: u8) -> DeviceKey {
    DeviceKey {
        device_id: id::<Dev>(u128::from(seed)),
        role: DeviceRole::Approver,
        profile: ClientProfile::Desktop,
        status: DeviceStatus::Active,
        approval_pubkey_fpr: sha(&format!("pubkey-{seed}")),
        verifying_key: *signing_key(seed).verifying_key(),
    }
}

fn device_ctx(device: &DeviceKey) -> gantry_core::ports::ActorCtx {
    let mut ctx = ctx();
    ctx.actor = Actor::new(ActorKind::Device, device.device_id.to_string());
    ctx
}

/// 客户端：对待签字节签名（前缀 + JCS 载荷），返回 base64url。
fn sign(seed: u8, payload: &gantry_core::canon::ApprovalPayload) -> String {
    let bytes = payload.signing_bytes().expect("signing bytes");
    let signature: Signature = signing_key(seed).sign(&bytes);
    URL_SAFE_NO_PAD.encode(signature.to_bytes())
}

fn grant_req(
    approval: &Approval,
    device: &DeviceKey,
    nonce: &str,
    signature: String,
) -> GrantRequest {
    GrantRequest {
        approval_id: approval.id,
        device: device.clone(),
        nonce: nonce.to_owned(),
        signature,
        current_subject: None,
    }
}

#[tokio::test]
async fn challenge_is_stable_until_used_or_expired() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let first = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    let again = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    assert_eq!(first, again);
    assert_eq!(first.payload.device_id, dev.device_id);
    assert_eq!(first.payload.delivery_id, Some(id::<Dlv>(3)));
    assert_eq!(first.payload.nonce.as_str().len(), 43);
    assert_eq!(first.signing.prefix, "gantry/approval/v1\n");
    // TTL 120 s。
    assert_eq!(
        first.payload.expires_at.as_millis() - approval.created_at.as_millis(),
        120_000
    );
    desk.clock.advance_ms(121_000);
    let fresh = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    assert_ne!(fresh.payload.nonce, first.payload.nonce);
    // 另一台设备有自己的 challenge。
    let other = desk
        .approvals
        .challenge(&approval.id, &device(2))
        .await
        .expect("challenge");
    assert_ne!(other.payload.nonce, fresh.payload.nonce);
}

#[tokio::test]
async fn only_active_desktop_approvers_get_challenges() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let mut viewer = device(1);
    viewer.role = DeviceRole::Operator;
    let mut mobile = device(1);
    mobile.profile = ClientProfile::Mobile;
    let mut revoked = device(1);
    revoked.status = DeviceStatus::Revoked;
    for dev in [viewer, mobile, revoked] {
        assert_eq!(
            desk.approvals.challenge(&approval.id, &dev).await,
            Err(ChallengeError::Forbidden)
        );
    }
    assert_eq!(
        desk.approvals.challenge(&id(999), &device(1)).await,
        Err(ChallengeError::NotFound)
    );
}

#[tokio::test]
async fn grant_verifies_the_stored_payload_and_records_the_signature() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let challenge = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    let signature = sign(1, &challenge.payload);
    let granted = desk
        .approvals
        .grant(
            grant_req(
                &approval,
                &dev,
                challenge.payload.nonce.as_str(),
                signature.clone(),
            ),
            &device_ctx(&dev),
        )
        .await
        .expect("grant");
    assert_eq!(granted.status, ApprovalStatus::Granted);
    assert_eq!(granted.device_id, Some(dev.device_id));
    assert_eq!(granted.signature.as_deref(), Some(signature.as_str()));

    let events = desk.events(EventType::ApprovalGranted).await;
    assert_eq!(events.len(), 1);
    let bytes = challenge.payload.signing_bytes().expect("bytes");
    assert_eq!(
        events[0].data["payload_sha256"],
        serde_json::to_value(sha256(&bytes)).expect("digest")
    );
    assert_eq!(events[0].actor.kind, ActorKind::Device);

    let read = desk
        .approvals
        .get(&approval.id)
        .await
        .expect("get")
        .expect("some");
    assert_eq!(read.signature.as_deref(), Some(signature.as_str()));
    // grant 之后已不是 pending。
    assert_eq!(
        desk.approvals
            .grant(
                grant_req(&approval, &dev, challenge.payload.nonce.as_str(), signature),
                &device_ctx(&dev),
            )
            .await,
        Err(GrantError::NotPending {
            status: ApprovalStatus::Granted
        })
    );
    // 放行后判定得到 Allow。
    let decision = desk
        .engine
        .decide_action(
            ActionRequest {
                project_id: project_id(),
                delivery_id: Some(id::<Dlv>(3)),
                kind: ForgeActionKind::MergePr,
                facts: ActionFacts::MergePr {
                    repo: slug(),
                    number: 7,
                    expected_head_sha: oid("head"),
                },
                requested_by: ctx().actor,
                epoch: 1,
            },
            &ctx(),
        )
        .await
        .expect("decide");
    assert!(
        matches!(decision, ActionDecision::Allow { approval: Some(a), .. } if a == approval.id)
    );
}

#[tokio::test]
async fn grant_rejects_bad_challenges_and_signatures_in_order() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let dev_ctx = device_ctx(&dev);
    let grant = |nonce: &str, signature: String| {
        desk.approvals
            .grant(grant_req(&approval, &dev, nonce, signature), &dev_ctx)
    };
    // 没有 challenge。
    assert_eq!(
        grant("lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js", "x".into()).await,
        Err(GrantError::ChallengeInvalid {
            reason: ChallengeReason::None
        })
    );
    let challenge = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    let nonce = challenge.payload.nonce.as_str().to_owned();
    let good = sign(1, &challenge.payload);
    // 篡改 nonce。
    let mut tampered = nonce.clone();
    tampered.replace_range(0..1, if tampered.starts_with('A') { "B" } else { "A" });
    assert_eq!(
        grant(&tampered, good.clone()).await,
        Err(GrantError::ChallengeInvalid {
            reason: ChallengeReason::Mismatch
        })
    );
    // 别的密钥（例如 mTLS 密钥）签的、载荷被改一个字节后签的。
    assert_eq!(
        grant(&nonce, sign(9, &challenge.payload)).await,
        Err(GrantError::SignatureInvalid)
    );
    let mut altered = challenge.payload.clone();
    altered.subject_digest = SubjectDigest::git(oid("other"));
    assert_eq!(
        grant(&nonce, sign(1, &altered)).await,
        Err(GrantError::SignatureInvalid)
    );
    assert_eq!(
        grant(&nonce, "not-base64!".into()).await,
        Err(GrantError::SignatureInvalid)
    );
    // 过期。
    desk.clock.advance_ms(121_000);
    assert_eq!(
        grant(&nonce, good).await,
        Err(GrantError::ChallengeInvalid {
            reason: ChallengeReason::Expired
        })
    );
    assert!(desk.events(EventType::ApprovalGranted).await.is_empty());
}

#[tokio::test]
async fn grant_with_moved_head_invalidates_first() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let challenge = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    let mut req = grant_req(
        &approval,
        &dev,
        challenge.payload.nonce.as_str(),
        sign(1, &challenge.payload),
    );
    req.current_subject = Some(CurrentSubject::PrHead {
        repo: slug(),
        number: 7,
        head_sha: oid("moved"),
        observed_at: ts(),
    });
    assert_eq!(
        desk.approvals.grant(req, &device_ctx(&dev)).await,
        Err(GrantError::StaleHead {
            current_head_sha: oid("moved")
        })
    );
    let read = desk
        .approvals
        .get(&approval.id)
        .await
        .expect("get")
        .expect("some");
    assert_eq!(read.status, ApprovalStatus::Invalidated);
    let events = desk.events(EventType::ApprovalInvalidated).await;
    assert_eq!(events[0].data["reason"], "head_changed");
    assert_eq!(events[0].data["current_head_sha"], oid("moved").to_string());
}

#[tokio::test]
async fn deny_needs_pending_and_records_the_reason() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let denied = desk
        .approvals
        .deny(&approval.id, &dev, Some("先别合".into()), &device_ctx(&dev))
        .await
        .expect("deny");
    assert_eq!(denied.status, ApprovalStatus::Denied);
    let events = desk.events(EventType::ApprovalDenied).await;
    assert_eq!(events[0].data, serde_json::json!({"reason": "先别合"}));
    assert_eq!(
        desk.approvals
            .deny(&approval.id, &dev, None, &device_ctx(&dev))
            .await,
        Err(GrantError::NotPending {
            status: ApprovalStatus::Denied
        })
    );
    assert_eq!(
        desk.approvals.challenge(&approval.id, &dev).await,
        Err(ChallengeError::NotPending {
            status: ApprovalStatus::Denied
        })
    );
}

fn settings_target(base_rev: u64) -> ApprovalTarget {
    ApprovalTarget::Settings {
        scope: SettingsScope::Global,
        project_id: None,
        resource: SettingsResource::Policies,
        base_rev: Seq::new(base_rev).expect("seq"),
    }
}

async fn granted_internal(
    desk: &Desk,
    target: ApprovalTarget,
    body: &serde_json::Value,
) -> Approval {
    let approval = desk
        .approvals
        .create_internal(
            InternalApprovalSpec {
                action: ApprovalAction::SettingsUpdate,
                target,
                subject_digest: gantry_policy::signing::request_digest(body).expect("digest"),
            },
            &ctx(),
        )
        .await
        .expect("create internal");
    assert_eq!(approval.delivery_id, None);
    let dev = device(1);
    let challenge = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    assert_eq!(challenge.payload.delivery_id, None);
    desk.approvals
        .grant(
            grant_req(
                &approval,
                &dev,
                challenge.payload.nonce.as_str(),
                sign(1, &challenge.payload),
            ),
            &device_ctx(&dev),
        )
        .await
        .expect("grant")
}

fn consume_req(
    approval: &Approval,
    target: ApprovalTarget,
    body: &serde_json::Value,
    rev: (u64, u64),
) -> ConsumeInternal {
    ConsumeInternal {
        approval_id: approval.id,
        expected_action: InternalAction::SettingsUpdate,
        expected_target: target,
        body_digest: gantry_policy::signing::request_digest(body).expect("digest"),
        rev_check: Some(RevCheck {
            if_match_rev: Seq::new(rev.0).expect("seq"),
            current_rev: Seq::new(rev.1).expect("seq"),
        }),
        request_id: "req_1".into(),
        actor: Actor::new(ActorKind::Device, "dev"),
    }
}

#[tokio::test]
async fn create_internal_rejects_forge_actions() {
    let desk = Desk::new().await;
    let result = desk
        .approvals
        .create_internal(
            InternalApprovalSpec {
                action: ApprovalAction::Forge(ForgeActionKind::MergePr),
                target: ApprovalTarget::Pr {
                    repo: slug(),
                    number: 7,
                },
                subject_digest: SubjectDigest::git(oid("head")),
            },
            &ctx(),
        )
        .await;
    assert!(matches!(result, Err(PolicyError::NotInternalAction { .. })));
}

#[tokio::test]
async fn consume_internal_checks_in_order_and_once() {
    let desk = Desk::new().await;
    let body = serde_json::json!({"actions": {"merge_pr": "approve"}});
    let target = settings_target(5);
    let approval = granted_internal(&desk, target.clone(), &body).await;

    let cases = [
        (
            ConsumeInternal {
                expected_action: InternalAction::DeviceRevoke,
                ..consume_req(&approval, target.clone(), &body, (5, 5))
            },
            MismatchReason::ActionMismatch,
        ),
        (
            consume_req(&approval, settings_target(6), &body, (5, 5)),
            MismatchReason::TargetMismatch,
        ),
        (
            consume_req(&approval, target.clone(), &serde_json::json!({}), (5, 5)),
            MismatchReason::DigestMismatch,
        ),
        (
            consume_req(&approval, target.clone(), &body, (5, 6)),
            MismatchReason::BaseRevMismatch,
        ),
    ];
    for (req, reason) in cases {
        let mut tx = desk.store.begin_write().await.expect("begin");
        assert_eq!(
            desk.approvals.consume_internal(&mut tx, req).await,
            Err(ConsumeError::Mismatch { reason })
        );
    }

    let mut tx = desk.store.begin_write().await.expect("begin");
    desk.approvals
        .consume_internal(
            &mut tx,
            consume_req(&approval, target.clone(), &body, (5, 5)),
        )
        .await
        .expect("consume");
    tx.commit().await.expect("commit");
    let events = desk.events(EventType::ApprovalConsumed).await;
    assert_eq!(events[0].data["consumed_by"], "req:req_1");

    let mut tx = desk.store.begin_write().await.expect("begin");
    assert_eq!(
        desk.approvals
            .consume_internal(&mut tx, consume_req(&approval, target, &body, (5, 5)))
            .await,
        Err(ConsumeError::Mismatch {
            reason: MismatchReason::Consumed
        })
    );
}

#[tokio::test]
async fn consume_internal_rolls_back_with_the_callers_transaction() {
    let desk = Desk::new().await;
    let body = serde_json::json!({});
    let target = settings_target(1);
    let approval = granted_internal(&desk, target.clone(), &body).await;
    {
        let mut tx = desk.store.begin_write().await.expect("begin");
        desk.approvals
            .consume_internal(
                &mut tx,
                consume_req(&approval, target.clone(), &body, (1, 1)),
            )
            .await
            .expect("consume");
        // 业务写入失败：调用方丢弃事务。
    }
    let read = desk
        .approvals
        .get(&approval.id)
        .await
        .expect("get")
        .expect("some");
    assert_eq!(read.status, ApprovalStatus::Granted);
    // 过期后不能消费。
    desk.clock.advance_ms(86_401_000);
    let mut tx = desk.store.begin_write().await.expect("begin");
    assert_eq!(
        desk.approvals
            .consume_internal(&mut tx, consume_req(&approval, target, &body, (1, 1)))
            .await,
        Err(ConsumeError::Mismatch {
            reason: MismatchReason::Expired
        })
    );
}

#[tokio::test]
async fn list_filters_and_pages() {
    let desk = Desk::new().await;
    let merge = desk.merge_approval().await;
    let internal = granted_internal(&desk, settings_target(1), &serde_json::json!({})).await;
    let pending = desk
        .approvals
        .list(
            ApprovalQuery {
                status: Some(ApprovalStatus::Pending),
                delivery_id: None,
            },
            PageQuery::default(),
        )
        .await
        .expect("list");
    assert_eq!(
        pending.items.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![merge.id]
    );
    let by_delivery = desk
        .approvals
        .list(
            ApprovalQuery {
                status: None,
                delivery_id: Some(id::<Dlv>(3)),
            },
            PageQuery::default(),
        )
        .await
        .expect("list");
    assert_eq!(by_delivery.items.len(), 1);

    let first = desk
        .approvals
        .list(
            ApprovalQuery::default(),
            PageQuery {
                cursor: None,
                limit: 1,
            },
        )
        .await
        .expect("list");
    assert_eq!(first.items.len(), 1);
    let cursor = first.next_cursor.clone().expect("next page");
    let second = desk
        .approvals
        .list(
            ApprovalQuery::default(),
            PageQuery {
                cursor: Some(cursor),
                limit: 1,
            },
        )
        .await
        .expect("list");
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.next_cursor, None);
    let mut ids = vec![first.items[0].id, second.items[0].id];
    ids.sort();
    let mut expected = vec![merge.id, internal.id];
    expected.sort();
    assert_eq!(ids, expected);
    assert_eq!(
        second.items[0].signature.is_some(),
        second.items[0].id == internal.id
    );
}
