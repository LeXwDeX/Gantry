//! `ApprovalService`：challenge / grant / deny、内部动作的创建与消费
//! （policy.md §6.4、§7.5、§7.6、§12.2「challenge / grant」「消费」）。

mod support;

use gantry_core::canon::{sha256, SubjectDigest};
use gantry_core::entities::{
    Actor, ActorKind, Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, DeviceRole,
    DeviceStatus, ForgeActionKind, Seq,
};
use gantry_core::events::EventType;
use gantry_core::ids::Dlv;
use gantry_core::ports::{
    ActionDecision, ActionFacts, ActionRequest, ClientProfile, MismatchReason, PageQuery,
    PolicyError, PolicyPort,
};
use gantry_core::settings::{SettingsResource, SettingsScope};
use gantry_policy::service::{
    ApprovalQuery, ChallengeError, ChallengeReason, ConsumeError, ConsumeInternal, CurrentSubject,
    GrantError, InternalAction, InternalApprovalSpec, RevCheck,
};
use gantry_policy::ApprovalService;
use support::desk::*;
use support::*;

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
                order: None,
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
                order: None,
            },
        )
        .await
        .expect("list");
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.next_cursor, None);
    for order in [
        gantry_core::ports::PageOrder::Asc,
        gantry_core::ports::PageOrder::Desc,
    ] {
        let result = desk
            .approvals
            .list(
                ApprovalQuery::default(),
                PageQuery {
                    order: Some(order),
                    ..PageQuery::default()
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(gantry_core::ports::PolicyError::InvalidRequest { .. })
        ));
    }
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

/// §12.2：手机档案（即使 approver）、operator、已吊销设备 `grant` → `Forbidden`，
/// 且这一步先于其余校验，没有副作用。
#[tokio::test]
async fn grant_and_deny_reject_devices_that_may_not_approve() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let challenge = desk
        .approvals
        .challenge(&approval.id, &device(1))
        .await
        .expect("challenge");
    let signature = sign(1, &challenge.payload);
    let mut operator = device(1);
    operator.role = DeviceRole::Operator;
    let mut mobile = device(1);
    mobile.profile = ClientProfile::Mobile;
    let mut revoked = device(1);
    revoked.status = DeviceStatus::Revoked;
    for dev in [operator, mobile, revoked] {
        let req = grant_req(
            &approval,
            &dev,
            challenge.payload.nonce.as_str(),
            signature.clone(),
        );
        assert_eq!(
            desk.approvals.grant(req, &device_ctx(&dev)).await,
            Err(GrantError::Forbidden)
        );
        assert_eq!(
            desk.approvals
                .deny(&approval.id, &dev, None, &device_ctx(&dev))
                .await,
            Err(GrantError::Forbidden)
        );
    }
    assert!(desk.events(EventType::ApprovalGranted).await.is_empty());
    assert!(desk.events(EventType::ApprovalDenied).await.is_empty());
}

/// 用过的 nonce → `ChallengeInvalid{Used}`。按校验顺序，正常流程里审批一旦 granted
/// 就先报 `NotPending`；这里直接把 challenge 行标成已用，覆盖这一支。
#[tokio::test]
async fn used_challenge_is_reported_as_used() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let challenge = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    let key = format!(
        "{}/{}/{}",
        approval.id, dev.device_id, challenge.payload.nonce
    );
    let mut tx = desk.store.begin_write().await.expect("begin");
    let mut row: serde_json::Value = tx
        .module_conn("policy")
        .get("policy_challenges", &key)
        .expect("get")
        .expect("challenge row");
    row["used_at"] = serde_json::to_value(ts()).expect("ts");
    tx.module_conn("policy")
        .put("policy_challenges", &key, &row)
        .expect("put");
    tx.commit().await.expect("commit");
    let req = grant_req(
        &approval,
        &dev,
        challenge.payload.nonce.as_str(),
        sign(1, &challenge.payload),
    );
    assert_eq!(
        desk.approvals.grant(req, &device_ctx(&dev)).await,
        Err(GrantError::ChallengeInvalid {
            reason: ChallengeReason::Used
        })
    );
    // 用过的行不再复用：下一次 challenge 换新 nonce。
    let fresh = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    assert_ne!(fresh.payload.nonce, challenge.payload.nonce);
}

/// 两个 approver 设备并发 `grant` 同一审批：恰有一个成功，另一个 `NotPending{granted}`。
#[tokio::test]
async fn concurrent_grants_have_exactly_one_winner() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let (a, b) = (device(1), device(2));
    let ca = desk
        .approvals
        .challenge(&approval.id, &a)
        .await
        .expect("challenge");
    let cb = desk
        .approvals
        .challenge(&approval.id, &b)
        .await
        .expect("challenge");
    let (ctx_a, ctx_b) = (device_ctx(&a), device_ctx(&b));
    let (ra, rb) = tokio::join!(
        desk.approvals.grant(
            grant_req(
                &approval,
                &a,
                ca.payload.nonce.as_str(),
                sign(1, &ca.payload)
            ),
            &ctx_a,
        ),
        desk.approvals.grant(
            grant_req(
                &approval,
                &b,
                cb.payload.nonce.as_str(),
                sign(2, &cb.payload)
            ),
            &ctx_b,
        ),
    );
    let results = [ra, rb];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(results.iter().any(|r| r
        == &Err(GrantError::NotPending {
            status: ApprovalStatus::Granted
        })));
    assert_eq!(desk.events(EventType::ApprovalGranted).await.len(), 1);
}

/// 懒过期：越过审批 `expires_at` 后，challenge / grant 入口先置 `expired`，只发一次事件。
#[tokio::test]
async fn expired_approval_is_expired_lazily_by_challenge_and_grant() {
    let desk = Desk::new().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let challenge = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    desk.clock.advance_ms(86_400_000);
    let req = grant_req(
        &approval,
        &dev,
        challenge.payload.nonce.as_str(),
        sign(1, &challenge.payload),
    );
    assert_eq!(
        desk.approvals.grant(req, &device_ctx(&dev)).await,
        Err(GrantError::NotPending {
            status: ApprovalStatus::Expired
        })
    );
    assert_eq!(
        desk.approvals.challenge(&approval.id, &dev).await,
        Err(ChallengeError::NotPending {
            status: ApprovalStatus::Expired
        })
    );
    assert_eq!(desk.events(EventType::ApprovalExpired).await.len(), 1);
}

/// `consume_internal` 第七种原因：审批还没被批准 → `NotGranted`。
#[tokio::test]
async fn consume_internal_requires_a_granted_approval() {
    let desk = Desk::new().await;
    let body = serde_json::json!({});
    let target = settings_target(1);
    let pending = desk
        .approvals
        .create_internal(
            InternalApprovalSpec {
                action: ApprovalAction::SettingsUpdate,
                target: target.clone(),
                subject_digest: gantry_policy::signing::request_digest(&body).expect("digest"),
            },
            &ctx(),
        )
        .await
        .expect("create internal");
    let mut tx = desk.store.begin_write().await.expect("begin");
    assert_eq!(
        desk.approvals
            .consume_internal(&mut tx, consume_req(&pending, target, &body, (1, 1)))
            .await,
        Err(ConsumeError::Mismatch {
            reason: MismatchReason::NotGranted
        })
    );
}

/// grant 第 5 步：`current_subject` 缺失时退回读快照（交付单元 → 项目 → PR）。
#[tokio::test]
async fn grant_falls_back_to_the_pr_snapshot_for_the_current_head() {
    let desk = Desk::new().await;
    desk.create_delivery().await;
    let approval = desk.merge_approval().await;
    let dev = device(1);
    let challenge = desk
        .approvals
        .challenge(&approval.id, &dev)
        .await
        .expect("challenge");
    desk.forge
        .upsert_pr(project_id(), "gantry/feature", pull_request(oid("moved")));
    let req = grant_req(
        &approval,
        &dev,
        challenge.payload.nonce.as_str(),
        sign(1, &challenge.payload),
    );
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
}
