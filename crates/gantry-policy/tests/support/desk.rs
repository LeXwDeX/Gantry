//! `ApprovalService` 测试的装配：注册了 policy 模块表的内存 Store、`PolicyEngine`、
//! `PolicyApprovals`，以及用软件 P-256 密钥模拟的 approver 设备。

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::entities::{
    Actor, ActorKind, Approval, DeviceRole, DeviceStatus, ForgeActionKind, Seq,
};
use gantry_core::events::EventType;
use gantry_core::ids::{Dev, Dlv, IdGen};
use gantry_core::ports::{ActionDecision, ActionFacts, ActionRequest, ClientProfile, PolicyPort};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_fakes::forge::FakeForge;
use gantry_policy::service::{DeviceKey, GrantRequest};
use gantry_policy::{PolicyApprovals, PolicyDeps, PolicyEngine, POLICY_MODULE};
use gantry_store::{Store, StoreDeps};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};

use super::*;

pub struct Desk {
    pub store: Store,
    pub clock: Arc<FakeClock>,
    pub engine: Arc<PolicyEngine>,
    pub approvals: PolicyApprovals,
    pub forge: Arc<FakeForge>,
}

impl Desk {
    pub async fn new() -> Self {
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
            forge: forge.clone(),
            clock: clock.clone(),
            ids,
        }));
        let approvals = PolicyApprovals::new(engine.clone(), Arc::new(SeqEntropy::new(11)));
        Self {
            store,
            clock,
            engine,
            approvals,
            forge,
        }
    }

    /// 交付单元 `dlv(3)`（属于测试项目）的 `delivery.created`，取 core 事件夹具的 data。
    pub async fn create_delivery(&self) {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../gantry-core/fixtures/events/cases.json"
        ))
        .expect("event cases");
        let mut tx = self.store.begin_write().await.expect("begin");
        tx.append(gantry_core::events::NewEvent {
            ty: EventType::DeliveryCreated,
            subject: gantry_core::events::Subject::parse(&id::<Dlv>(3).to_string())
                .expect("subject"),
            project_id: Some(project_id()),
            actor: Actor::new(ActorKind::Engine, "engine"),
            causation_id: None,
            correlation_id: None,
            dedup_key: None,
            data: cases["valid"]["delivery.created"].clone(),
        })
        .await
        .expect("append delivery.created");
        tx.commit().await.expect("commit");
    }

    /// `merge_pr`（PR 7、head）的 pending 审批。
    pub async fn merge_approval(&self) -> Approval {
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

    pub async fn events(
        &self,
        ty: EventType,
    ) -> Vec<gantry_core::events::Event<serde_json::Value>> {
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

pub fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).expect("signing key")
}

pub fn device(seed: u8) -> DeviceKey {
    DeviceKey {
        device_id: id::<Dev>(u128::from(seed)),
        role: DeviceRole::Approver,
        profile: ClientProfile::Desktop,
        status: DeviceStatus::Active,
        approval_pubkey_fpr: sha(&format!("pubkey-{seed}")),
        verifying_key: *signing_key(seed).verifying_key(),
    }
}

pub fn device_ctx(device: &DeviceKey) -> gantry_core::ports::ActorCtx {
    let mut ctx = ctx();
    ctx.actor = Actor::new(ActorKind::Device, device.device_id.to_string());
    ctx
}

/// 客户端：对待签字节签名（前缀 + JCS 载荷），返回 base64url。
pub fn sign(seed: u8, payload: &gantry_core::canon::ApprovalPayload) -> String {
    let bytes = payload.signing_bytes().expect("signing bytes");
    let signature: Signature = signing_key(seed).sign(&bytes);
    URL_SAFE_NO_PAD.encode(signature.to_bytes())
}

pub fn grant_req(
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
