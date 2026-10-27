//! policy.md §12.2「I3 / I8 / 端到端」的 A 级（本机替身）证据。
//!
//! 设备密钥来自 `TestCA`（openssl 生成的软件 P-256 密钥）：审批签名密钥
//! `approval.key` 模拟 approver 设备的 Secure Enclave 密钥，`device_key_pem` 是
//! mTLS 客户端密钥。`forge` 执行前的消费用 `consume_for_execution` 直接驱动；
//! `forge` 执行器本身"没有放行就不发合并请求"由 gantry-forge 的
//! `outbox_executor` 测试断言（同一端口，`FakePolicy` 与 `PolicyEngine` 共享一致性测试）。
//! B / C 级证据（真实 api / pki / desktop-shell）归 M1.4，见 docs/m1-acceptance.md。

mod support;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::canon::ApprovalPayload;
use gantry_core::entities::ForgeActionKind;
use gantry_core::entities::{
    ApprovalAction, ApprovalStatus, ApprovalTarget, DeviceRole, DeviceStatus, Seq,
};
use gantry_core::events::EventType;
use gantry_core::ids::{Act, Dev, Dlv};
use gantry_core::ports::{
    ActionDecision, ActionFacts, ActionRequest, ClientProfile, DenyReason, ExecutionCheck,
    ExecutionPermit, GateError, MismatchReason, PolicyPort,
};
use gantry_core::settings::{SettingsResource, SettingsScope};
use gantry_fakes::test_ca::TestCA;
use gantry_policy::service::{
    ConsumeError, ConsumeInternal, DeviceKey, GrantError, GrantRequest, InternalAction,
    InternalApprovalSpec, RevCheck,
};
use gantry_policy::signing::{parse_approval_pubkey, request_digest};
use gantry_policy::ApprovalService;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey as _;
use support::desk::{device_ctx, Desk};
use support::*;

struct Approver {
    key: DeviceKey,
    approval_key: SigningKey,
    mtls_key: SigningKey,
    _ca: TestCA,
}

impl Approver {
    fn new() -> Self {
        let ca = TestCA::new().expect("openssl test CA");
        let pem =
            std::fs::read_to_string(ca.root_dir().join("approval.key")).expect("approval key");
        let approval_key = SigningKey::from_pkcs8_pem(&pem).expect("approval signing key");
        let mtls_key = SigningKey::from_pkcs8_pem(ca.device_key_pem()).expect("mtls key");
        let key = DeviceKey {
            device_id: id::<Dev>(5),
            role: DeviceRole::Approver,
            profile: ClientProfile::Desktop,
            status: DeviceStatus::Active,
            approval_pubkey_fpr: ca.approval_pubkey_fingerprint(),
            verifying_key: parse_approval_pubkey(ca.approval_public_key())
                .expect("approval pubkey"),
        };
        Self {
            key,
            approval_key,
            mtls_key,
            _ca: ca,
        }
    }

    fn sign_with(key: &SigningKey, payload: &ApprovalPayload) -> String {
        let signature: Signature = key.sign(&payload.signing_bytes().expect("signing bytes"));
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    }

    /// 用审批签名密钥走完 challenge → grant。
    async fn approve(&self, desk: &Desk, approval: &gantry_core::entities::Approval) {
        let challenge = desk
            .approvals
            .challenge(&approval.id, &self.key)
            .await
            .expect("challenge");
        desk.approvals
            .grant(
                GrantRequest {
                    approval_id: approval.id,
                    device: self.key.clone(),
                    nonce: challenge.payload.nonce.as_str().to_owned(),
                    signature: Self::sign_with(&self.approval_key, &challenge.payload),
                    current_subject: None,
                },
                &device_ctx(&self.key),
            )
            .await
            .expect("grant");
    }
}

fn merge_request(epoch: u32) -> ActionRequest {
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
        epoch,
    }
}

fn merge_check() -> ExecutionCheck {
    ExecutionCheck::MergePr {
        repo: slug(),
        number: 7,
        expected_head_sha: oid("head"),
        live_head_sha: oid("head"),
    }
}

/// Low 任务到合并（policy 段）：`merge_pr` 需审批 → approver 设备用审批签名密钥签名 →
/// 再判定得 `Allow{approval}` → `forge` 执行前 `consume_for_execution` 消费 → 不可重用。
#[tokio::test]
async fn merge_pr_needs_a_signed_approval_consumed_before_execution() {
    let desk = Desk::new().await;
    let approver = Approver::new();
    let approval = desk.merge_approval().await;
    approver.approve(&desk, &approval).await;

    let decision = desk
        .engine
        .decide_action(merge_request(1), &ctx())
        .await
        .expect("decide");
    let ActionDecision::Allow {
        approval: Some(allowed),
        ..
    } = decision
    else {
        panic!("expected Allow with approval, got {decision:?}");
    };
    assert_eq!(allowed, approval.id);

    let act = id::<Act>(1);
    desk.engine
        .consume_for_execution(&approval.id, &merge_check(), &act, &ctx())
        .await
        .expect("consume");
    let consumed = desk
        .approvals
        .get(&approval.id)
        .await
        .expect("get")
        .expect("some");
    assert_eq!(consumed.status, ApprovalStatus::Consumed);
    assert_eq!(
        consumed.consumed_by.as_deref(),
        Some(act.to_string().as_str())
    );
    // 崩溃重放同一动作：幂等；别的动作：拒绝。
    assert_eq!(
        desk.engine
            .consume_for_execution(&approval.id, &merge_check(), &act, &ctx())
            .await,
        Ok(ExecutionPermit::AlreadyConsumedBySameAction)
    );
    assert!(matches!(
        desk.engine
            .consume_for_execution(&approval.id, &merge_check(), &id::<Act>(2), &ctx())
            .await,
        Err(GateError::ConsumedByOther { .. })
    ));
    let again = desk
        .engine
        .decide_action(merge_request(1), &ctx())
        .await
        .expect("decide");
    assert!(matches!(
        again,
        ActionDecision::Deny {
            reason: DenyReason::ApprovalConsumed,
            ..
        }
    ));
}

/// P-47：用设备的 mTLS 客户端密钥（不是审批签名密钥）签待签字节 → `SignatureInvalid`。
#[tokio::test]
async fn mtls_client_key_cannot_sign_approvals() {
    let desk = Desk::new().await;
    let approver = Approver::new();
    let approval = desk.merge_approval().await;
    let challenge = desk
        .approvals
        .challenge(&approval.id, &approver.key)
        .await
        .expect("challenge");
    let result = desk
        .approvals
        .grant(
            GrantRequest {
                approval_id: approval.id,
                device: approver.key.clone(),
                nonce: challenge.payload.nonce.as_str().to_owned(),
                signature: Approver::sign_with(&approver.mtls_key, &challenge.payload),
                current_subject: None,
            },
            &device_ctx(&approver.key),
        )
        .await;
    assert_eq!(result, Err(GrantError::SignatureInvalid));
    assert!(desk.events(EventType::ApprovalGranted).await.is_empty());
}

/// I3：没有 granted 审批时，`forge` 执行前的消费不放行（`merge_pr` 不会执行），且不发生转移。
#[tokio::test]
async fn merge_cannot_execute_without_a_granted_approval() {
    let desk = Desk::new().await;
    let pending = desk.merge_approval().await;
    assert_eq!(
        desk.engine
            .consume_for_execution(&pending.id, &merge_check(), &id::<Act>(1), &ctx())
            .await,
        Err(GateError::ApprovalInvalid {
            reason: MismatchReason::NotGranted
        })
    );
    assert!(desk.events(EventType::ApprovalConsumed).await.is_empty());
    let decision = desk
        .engine
        .decide_action(merge_request(1), &ctx())
        .await
        .expect("decide");
    assert!(matches!(decision, ActionDecision::NeedsApproval { .. }));
}

/// 写入模型凭据需要 approver 签名（policy 段）：`settings_update` 审批创建 → 签名 →
/// 与业务写入同事务消费；摘要不符 / 已消费 / 过期各被拒。缺 `Gantry-Approval` 头由 `api` 判。
#[tokio::test]
async fn settings_update_signature_chain() {
    let desk = Desk::new().await;
    let approver = Approver::new();
    let body = serde_json::json!({"credential": {"value": "sk-test"}});
    let target = ApprovalTarget::Settings {
        scope: SettingsScope::Global,
        project_id: None,
        resource: SettingsResource::Models,
        base_rev: Seq::new(4).expect("seq"),
    };
    let approval = desk
        .approvals
        .create_internal(
            InternalApprovalSpec {
                action: ApprovalAction::SettingsUpdate,
                target: target.clone(),
                subject_digest: request_digest(&body).expect("digest"),
            },
            &ctx(),
        )
        .await
        .expect("create internal");
    approver.approve(&desk, &approval).await;

    let consume = |body: &serde_json::Value| ConsumeInternal {
        approval_id: approval.id,
        expected_action: InternalAction::SettingsUpdate,
        expected_target: target.clone(),
        body_digest: request_digest(body).expect("digest"),
        rev_check: Some(RevCheck {
            if_match_rev: Seq::new(4).expect("seq"),
            current_rev: Seq::new(4).expect("seq"),
        }),
        request_id: "req_settings".into(),
        actor: device_ctx(&approver.key).actor,
    };
    let mut tx = desk.store.begin_write().await.expect("begin");
    assert_eq!(
        desk.approvals
            .consume_internal(
                &mut tx,
                consume(&serde_json::json!({"credential": {"value": "sk-other"}}))
            )
            .await,
        Err(ConsumeError::Mismatch {
            reason: MismatchReason::DigestMismatch
        })
    );
    desk.approvals
        .consume_internal(&mut tx, consume(&body))
        .await
        .expect("consume");
    tx.commit().await.expect("commit");
    let mut tx = desk.store.begin_write().await.expect("begin");
    assert_eq!(
        desk.approvals
            .consume_internal(&mut tx, consume(&body))
            .await,
        Err(ConsumeError::Mismatch {
            reason: MismatchReason::Consumed
        })
    );
    drop(tx);
    // 凭据明文不进事件。
    for ty in [
        EventType::ApprovalRequested,
        EventType::ApprovalGranted,
        EventType::ApprovalConsumed,
    ] {
        for event in desk.events(ty).await {
            assert!(!event.data.to_string().contains("sk-test"));
        }
    }

    // 另一份已批准但过期的审批。
    let late = desk
        .approvals
        .create_internal(
            InternalApprovalSpec {
                action: ApprovalAction::SettingsUpdate,
                target: target.clone(),
                subject_digest: request_digest(&serde_json::json!({})).expect("digest"),
            },
            &ctx(),
        )
        .await
        .expect("create internal");
    approver.approve(&desk, &late).await;
    desk.clock.advance_ms(86_401_000);
    let mut tx = desk.store.begin_write().await.expect("begin");
    assert_eq!(
        desk.approvals
            .consume_internal(
                &mut tx,
                ConsumeInternal {
                    approval_id: late.id,
                    ..consume(&serde_json::json!({}))
                },
            )
            .await,
        Err(ConsumeError::Mismatch {
            reason: MismatchReason::Expired
        })
    );
}
