//! 审批服务 `ApprovalService`（policy.md §6.4、§7.5、§7.6；api.md §6.1–§6.6）。
//!
//! 同一泳道内供 `api` 调用的服务接口（core.md §6.10 末：不进 `core::ports`）。
//! challenge 存在模块私有表 `policy_challenges`，内部动作的消费记录存在
//! `policy_consumed`（policy.md §7.1）；两张表都经 `WriteTx::module_conn("policy")`
//! 与事件追加同一事务读写。`Store` 打开时必须注册 [`POLICY_MODULE`]，否则这些方法
//! 返回存储错误（失败即关闭）。
//!
//! 签名只经服务端保存的待签字节验证：客户端提交 nonce 与签名，不提交载荷。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use gantry_core::canon::{
    sha256, ApprovalPayload, GitOid, Nonce, RepoSlug, Sha256Digest, SubjectDigest,
};
use gantry_core::entities::{
    Actor, Approval, ApprovalAction, ApprovalStatus, ApprovalTarget, DeviceRole, DeviceStatus, Seq,
};
use gantry_core::events::{ApprovalGrantedData, ApprovalInvalidatedData};
use gantry_core::ids::{ApprovalId, DeliveryId, DeviceId, Entropy};
use gantry_core::ports::{
    ActorCtx, ApprovalSpec, ClientProfile, MismatchReason, Page, PageQuery, PolicyError,
    PolicyPort, StoreError,
};
use gantry_core::time::Timestamp;
use gantry_store::{ModuleSpec, WriteTx, MODULE_SCAN_MAX};
use p256::ecdsa::VerifyingKey;
use serde::{Deserialize, Serialize};

use crate::approvals::{self, port_error, Record};
use crate::engine::PolicyEngine;
use crate::signing;

const MODULE: &str = "policy";
const CHALLENGES: &str = "policy_challenges";
const CONSUMED: &str = "policy_consumed";

/// policy 的模块私有表（policy.md §7.1）。`Store::open*` 时注册。
pub const POLICY_MODULE: ModuleSpec = ModuleSpec::new(MODULE, &[CHALLENGES, CONSUMED]);

/// challenge 的存活时间上限（policy.md §7.5；不超过审批的 `expires_at`）。
pub const CHALLENGE_TTL: Duration = Duration::from_secs(120);
const NONCE_BYTES: usize = 32;
const LIST_DEFAULT_LIMIT: u32 = 50;

#[async_trait]
pub trait ApprovalService: Send + Sync {
    /// 读一个审批（先惰性过期）。
    async fn get(&self, id: &ApprovalId) -> Result<Option<Approval>, PolicyError>;
    /// 按 `status` / `delivery_id` 过滤，按审批 ID（创建顺序）分页。
    async fn list(&self, q: ApprovalQuery, page: PageQuery) -> Result<Page<Approval>, PolicyError>;
    /// 内部动作的审批（`POST /v1/approvals`）。同主体已有 pending 则返回它。
    async fn create_internal(
        &self,
        spec: InternalApprovalSpec,
        ctx: &ActorCtx,
    ) -> Result<Approval, PolicyError>;
    /// 同一 `(approval, device)` 在有效期内重复调用返回同一份；用掉或过期后换新 nonce。
    async fn challenge(
        &self,
        id: &ApprovalId,
        device: &DeviceKey,
    ) -> Result<Challenge, ChallengeError>;
    async fn grant(&self, req: GrantRequest, ctx: &ActorCtx) -> Result<Approval, GrantError>;
    async fn deny(
        &self,
        id: &ApprovalId,
        device: &DeviceKey,
        reason: Option<String>,
        ctx: &ActorCtx,
    ) -> Result<Approval, GrantError>;
    /// 内部动作的消费：与调用方的业务写入同一事务（api.md §6.6 第 4 步 #2–#5）。
    async fn consume_internal(
        &self,
        tx: &mut WriteTx,
        req: ConsumeInternal,
    ) -> Result<(), ConsumeError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovalQuery {
    pub status: Option<ApprovalStatus>,
    pub delivery_id: Option<DeliveryId>,
}

/// 需要签名的内部动作（api.md §6.6）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InternalAction {
    SettingsUpdate,
    DeviceRoleChange,
    DeviceRevoke,
}

impl InternalAction {
    pub fn approval_action(self) -> ApprovalAction {
        match self {
            Self::SettingsUpdate => ApprovalAction::SettingsUpdate,
            Self::DeviceRoleChange => ApprovalAction::DeviceRoleChange,
            Self::DeviceRevoke => ApprovalAction::DeviceRevoke,
        }
    }

    pub fn from_approval_action(action: ApprovalAction) -> Option<Self> {
        match action {
            ApprovalAction::SettingsUpdate => Some(Self::SettingsUpdate),
            ApprovalAction::DeviceRoleChange => Some(Self::DeviceRoleChange),
            ApprovalAction::DeviceRevoke => Some(Self::DeviceRevoke),
            _ => None,
        }
    }
}

/// `POST /v1/approvals` 的请求体（= dry-run 返回的 `signing`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalApprovalSpec {
    pub action: ApprovalAction,
    pub target: ApprovalTarget,
    pub subject_digest: SubjectDigest,
}

/// 由 `api` 从 `pki` 实时读取后传入；本模块不信任调用方已做的授权，自己再查
/// 状态、角色与档案（policy.md §8）。`verifying_key` 是配对时登记的审批签名公钥
/// （P-47），不是 mTLS 证书的公钥。
#[derive(Clone, Debug)]
pub struct DeviceKey {
    pub device_id: DeviceId,
    pub role: DeviceRole,
    pub profile: ClientProfile,
    pub status: DeviceStatus,
    pub approval_pubkey_fpr: Sha256Digest,
    pub verifying_key: VerifyingKey,
}

impl DeviceKey {
    fn may_approve(&self) -> bool {
        self.status == DeviceStatus::Active
            && self.role == DeviceRole::Approver
            && self.profile == ClientProfile::Desktop
    }
}

/// `GET /v1/approvals/{id}/challenge` 的响应（api.md §6.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Challenge {
    pub approval_id: ApprovalId,
    pub payload: ApprovalPayload,
    pub signing: SigningInfo,
}

/// 自描述的签名参数；客户端不认识时必须中止（api.md §6.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SigningInfo {
    pub alg: &'static str,
    pub canonicalization: &'static str,
    pub prefix: &'static str,
}

impl SigningInfo {
    pub const V1: Self = Self {
        alg: "ES256",
        canonicalization: "JCS",
        prefix: "gantry/approval/v1\n",
    };
}

#[derive(Clone, Debug)]
pub struct GrantRequest {
    pub approval_id: ApprovalId,
    pub device: DeviceKey,
    /// base64url，无填充。
    pub nonce: String,
    /// base64url，无填充，86 字符（`r ‖ s` 共 64 字节）。
    pub signature: String,
    /// PR 类审批：`api` 先让 `forge` 对账后填入；`repo` / `number` 与审批不一致视为未提供。
    pub current_subject: Option<CurrentSubject>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentSubject {
    PrHead {
        repo: RepoSlug,
        number: u64,
        head_sha: GitOid,
        observed_at: Timestamp,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumeInternal {
    pub approval_id: ApprovalId,
    pub expected_action: InternalAction,
    /// 由路径参数 / 路由推出。
    pub expected_target: ApprovalTarget,
    /// `signing::request_digest` 对本次请求体算出。
    pub body_digest: SubjectDigest,
    /// settings 写入：`{if_match_rev, current_rev}`。
    pub rev_check: Option<RevCheck>,
    /// `approval.consumed.data.consumed_by = "req:<request_id>"`。
    pub request_id: String,
    pub actor: Actor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevCheck {
    pub if_match_rev: Seq,
    pub current_rev: Seq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChallengeReason {
    Expired,
    Used,
    Mismatch,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChallengeError {
    #[error("approval not found")]
    NotFound,
    /// 设备非 active / 非 approver / 非桌面档案。
    #[error("device may not approve")]
    Forbidden,
    #[error("approval is {status:?}")]
    NotPending { status: ApprovalStatus },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    #[error("approval not found")]
    NotFound,
    #[error("device may not approve")]
    Forbidden,
    #[error("approval is {status:?}")]
    NotPending { status: ApprovalStatus },
    #[error("challenge invalid: {reason:?}")]
    ChallengeInvalid { reason: ChallengeReason },
    #[error("signature invalid")]
    SignatureInvalid,
    /// 审批已置为 `invalidated` 并落盘。
    #[error("stale head, current {current_head_sha}")]
    StaleHead { current_head_sha: GitOid },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConsumeError {
    #[error("approval not found")]
    NotFound,
    #[error("approval mismatch: {reason:?}")]
    Mismatch { reason: MismatchReason },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `policy_challenges` 的一行。键 `<approval>/<device>/<nonce>`。用过的行保留（审计）。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChallengeRow {
    approval_id: ApprovalId,
    device_id: DeviceId,
    nonce: Nonce,
    payload: ApprovalPayload,
    /// 待签字节（前缀 + JCS 载荷，UTF-8）；验签只用这一份。
    payload_canonical: String,
    issued_at: Timestamp,
    expires_at: Timestamp,
    #[serde(default)]
    used_at: Option<Timestamp>,
    /// grant 通过时记下的签名（`Approval.signature` 的来源）。
    #[serde(default)]
    signature: Option<String>,
}

impl ChallengeRow {
    fn key(&self) -> String {
        format!("{}/{}/{}", self.approval_id, self.device_id, self.nonce)
    }

    fn is_open(&self, now: Timestamp) -> bool {
        self.used_at.is_none() && now < self.expires_at
    }
}

/// `policy_consumed` 的一行，键为审批 ID。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ConsumedRow {
    consumed_by: String,
    consumed_at: Timestamp,
}

/// `ApprovalService` 的实现，复用 `PolicyEngine` 的存储、时钟与规则。
pub struct PolicyApprovals {
    engine: Arc<PolicyEngine>,
    entropy: Arc<dyn Entropy>,
}

impl PolicyApprovals {
    /// `entropy` 用于生成 32 字节 nonce，生产环境必须是 CSPRNG。
    pub fn new(engine: Arc<PolicyEngine>, entropy: Arc<dyn Entropy>) -> Self {
        Self { engine, entropy }
    }

    fn now(&self) -> Timestamp {
        self.engine.clock.now()
    }

    async fn load(&self, id: &ApprovalId) -> Result<Option<Record>, StoreError> {
        let log = self.engine.store.events();
        let Some(record) = approvals::load(&log, id).await? else {
            return Ok(None);
        };
        Ok(Some(
            approvals::expire_if_due(&self.engine.store, record, self.now()).await?,
        ))
    }

    /// 已签审批补上签名（来自 grant 时那一行 challenge）。
    async fn with_signature(&self, mut approval: Approval) -> Result<Approval, StoreError> {
        let Some(device) = approval.device_id else {
            return Ok(approval);
        };
        let snapshot = self.engine.store.snapshot().await.map_err(port_error)?;
        let reader = snapshot.module_conn(MODULE);
        let prefix = format!("{}/{}/", approval.id, device);
        let mut after: Option<String> = None;
        loop {
            let rows = reader
                .scan::<ChallengeRow>(CHALLENGES, &prefix, after.as_deref(), MODULE_SCAN_MAX)
                .map_err(port_error)?;
            if let Some(signature) = rows.iter().find_map(|(_, row)| row.signature.clone()) {
                approval.signature = Some(signature);
                return Ok(approval);
            }
            if rows.len() < MODULE_SCAN_MAX as usize {
                return Ok(approval);
            }
            after = rows.last().map(|(key, _)| key.clone());
        }
    }

    /// `current_subject` 缺失时退回读快照：审批的交付单元 → 项目 → `ForgeReader` 的 PR。
    /// 读不到（无交付单元、无快照、仓库不一致、读失败）就跳过这一步，主体由消费时的
    /// 权威检查兜底。
    async fn snapshot_head(&self, approval: &Approval) -> Option<GitOid> {
        let ApprovalTarget::Pr { repo, number } = &approval.target else {
            return None;
        };
        let delivery = approval.delivery_id?;
        let snapshot = self.engine.store.snapshot().await.ok()?;
        let project = snapshot.delivery(&delivery)?.project_id;
        let pr = self
            .engine
            .forge
            .pull_request(&project, *number)
            .await
            .ok()??;
        (pr.repo == *repo).then_some(pr.head_sha)
    }

    fn new_nonce(&self) -> Nonce {
        let mut bytes = [0u8; NONCE_BYTES];
        self.entropy.fill(&mut bytes);
        Nonce::parse(&URL_SAFE_NO_PAD.encode(bytes)).expect("32-byte base64url nonce is valid")
    }
}

/// 某 `(approval, device)` 的全部 challenge 行。
fn device_challenges(
    tx: &mut WriteTx,
    approval: &ApprovalId,
    device: &DeviceId,
) -> Result<Vec<ChallengeRow>, StoreError> {
    let conn = tx.module_conn(MODULE);
    let prefix = format!("{approval}/{device}/");
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let rows = conn
            .scan::<ChallengeRow>(CHALLENGES, &prefix, after.as_deref(), MODULE_SCAN_MAX)
            .map_err(port_error)?;
        let full = rows.len() == MODULE_SCAN_MAX as usize;
        after = rows.last().map(|(key, _)| key.clone());
        out.extend(rows.into_iter().map(|(_, row)| row));
        if !full {
            return Ok(out);
        }
    }
}

/// 常量时间比较（长度不同直接不等；nonce 长度不是秘密）。
fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn add(ts: Timestamp, duration: Duration) -> Timestamp {
    Timestamp::from_millis(
        ts.as_millis()
            .saturating_add(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)),
    )
}

/// 当前 PR head：`current_subject` 的 `repo` / `number` 与审批目标一致才采用。
fn current_head(target: &ApprovalTarget, current: Option<&CurrentSubject>) -> Option<GitOid> {
    let ApprovalTarget::Pr { repo, number } = target else {
        return None;
    };
    match current {
        Some(CurrentSubject::PrHead {
            repo: seen_repo,
            number: seen_number,
            head_sha,
            ..
        }) if seen_repo == repo && seen_number == number => Some(*head_sha),
        _ => None,
    }
}

#[async_trait]
impl ApprovalService for PolicyApprovals {
    async fn get(&self, id: &ApprovalId) -> Result<Option<Approval>, PolicyError> {
        match self.load(id).await? {
            Some(record) => Ok(Some(self.with_signature(record.approval).await?)),
            None => Ok(None),
        }
    }

    async fn list(&self, q: ApprovalQuery, page: PageQuery) -> Result<Page<Approval>, PolicyError> {
        let limit = match page.limit {
            0 => LIST_DEFAULT_LIMIT,
            n => n.min(MODULE_SCAN_MAX),
        } as usize;
        let after: Option<ApprovalId> = match page.cursor.as_deref() {
            Some(cursor) => Some(cursor.parse().map_err(|_| PolicyError::InvalidRequest {
                pointer: "/cursor".into(),
                message: "cursor is not an approval id".into(),
            })?),
            None => None,
        };
        let log = self.engine.store.events();
        let mut records = approvals::load_all(&log).await?;
        records.sort_by_key(|record| record.approval.id);
        let now = self.now();
        let mut items = Vec::new();
        let mut next_cursor = None;
        for record in records {
            if after.is_some_and(|after| record.approval.id <= after) {
                continue;
            }
            let record = approvals::expire_if_due(&self.engine.store, record, now).await?;
            let approval = record.approval;
            if q.status.is_some_and(|status| status != approval.status)
                || q.delivery_id
                    .is_some_and(|delivery| approval.delivery_id != Some(delivery))
            {
                continue;
            }
            if items.len() == limit {
                next_cursor = items.last().map(|last: &Approval| last.id.to_string());
                break;
            }
            items.push(self.with_signature(approval).await?);
        }
        Ok(Page { items, next_cursor })
    }

    async fn create_internal(
        &self,
        spec: InternalApprovalSpec,
        ctx: &ActorCtx,
    ) -> Result<Approval, PolicyError> {
        if InternalAction::from_approval_action(spec.action).is_none() {
            return Err(PolicyError::NotInternalAction {
                action: spec.action,
            });
        }
        let ttl = self.engine.rules().approval_ttl;
        self.engine
            .request_approval(
                ApprovalSpec {
                    action: spec.action,
                    target: spec.target,
                    subject_digest: spec.subject_digest,
                    delivery_id: None,
                    ttl,
                },
                ctx.clone(),
            )
            .await
    }

    async fn challenge(
        &self,
        id: &ApprovalId,
        device: &DeviceKey,
    ) -> Result<Challenge, ChallengeError> {
        if !device.may_approve() {
            return Err(ChallengeError::Forbidden);
        }
        let record = self.load(id).await?.ok_or(ChallengeError::NotFound)?;
        let approval = record.approval;
        if approval.status != ApprovalStatus::Pending {
            return Err(ChallengeError::NotPending {
                status: approval.status,
            });
        }
        let now = self.now();
        let mut tx = self.engine.store.begin_write().await.map_err(port_error)?;
        let open = device_challenges(&mut tx, id, &device.device_id)?
            .into_iter()
            .find(|row| row.is_open(now));
        let row = match open {
            Some(row) => row,
            None => {
                let expires_at = add(now, CHALLENGE_TTL).min(approval.expires_at);
                let nonce = self.new_nonce();
                let payload =
                    signing::build_payload(&approval, &device.device_id, nonce.clone(), expires_at);
                let canonical =
                    payload
                        .signing_bytes()
                        .map_err(|error| StoreError::InvalidState {
                            message: format!("approval payload cannot be signed: {error}"),
                        })?;
                let row = ChallengeRow {
                    approval_id: *id,
                    device_id: device.device_id,
                    nonce,
                    payload,
                    payload_canonical: String::from_utf8(canonical).expect("JCS output is UTF-8"),
                    issued_at: now,
                    expires_at,
                    used_at: None,
                    signature: None,
                };
                tx.module_conn(MODULE)
                    .insert(CHALLENGES, &row.key(), &row)
                    .map_err(port_error)?;
                tx.commit().await.map_err(port_error)?;
                row
            }
        };
        Ok(Challenge {
            approval_id: *id,
            payload: row.payload,
            signing: SigningInfo::V1,
        })
    }

    async fn grant(&self, req: GrantRequest, ctx: &ActorCtx) -> Result<Approval, GrantError> {
        // 1. 设备（防御性复查）。
        if !req.device.may_approve() {
            return Err(GrantError::Forbidden);
        }
        // 2. 审批 pending（懒过期）。
        let record = self
            .load(&req.approval_id)
            .await?
            .ok_or(GrantError::NotFound)?;
        if record.approval.status != ApprovalStatus::Pending {
            return Err(GrantError::NotPending {
                status: record.approval.status,
            });
        }
        // 第 5 步要用的当前 head 在开写事务前读（只读、无副作用，不改变校验顺序）。
        let live_head = match current_head(&record.approval.target, req.current_subject.as_ref()) {
            Some(head) => Some(head),
            None => self.snapshot_head(&record.approval).await,
        };
        // 3. 本设备的 challenge。
        let now = self.now();
        let mut tx = self.engine.store.begin_write().await.map_err(port_error)?;
        let rows = device_challenges(&mut tx, &req.approval_id, &req.device.device_id)?;
        if rows.is_empty() {
            return Err(GrantError::ChallengeInvalid {
                reason: ChallengeReason::None,
            });
        }
        let Some(mut row) = rows
            .into_iter()
            .find(|row| ct_eq(row.nonce.as_str(), &req.nonce))
        else {
            return Err(GrantError::ChallengeInvalid {
                reason: ChallengeReason::Mismatch,
            });
        };
        if row.used_at.is_some() {
            return Err(GrantError::ChallengeInvalid {
                reason: ChallengeReason::Used,
            });
        }
        if now >= row.expires_at {
            return Err(GrantError::ChallengeInvalid {
                reason: ChallengeReason::Expired,
            });
        }
        // 4. 用服务端保存的待签字节验签。
        signing::verify_es256(
            &req.device.verifying_key,
            row.payload_canonical.as_bytes(),
            &req.signature,
        )
        .map_err(|_| GrantError::SignatureInvalid)?;
        // 5. PR 类：主体一致性（提前失败；权威检查在消费时）。不等先落盘失效再返回。
        if let (Some(head), SubjectDigest::Git(signed)) =
            (live_head, record.approval.subject_digest)
        {
            if head != signed {
                approvals::transition(
                    &mut tx,
                    &record,
                    ApprovalStatus::Invalidated,
                    approvals::invalidated_data(ApprovalInvalidatedData::HeadChanged {
                        previous_head_sha: signed,
                        current_head_sha: head,
                    }),
                    ctx.actor.clone(),
                    ctx.causation_id,
                )
                .await?;
                tx.commit().await.map_err(port_error)?;
                return Err(GrantError::StaleHead {
                    current_head_sha: head,
                });
            }
        }
        // 6. 通过：标记 challenge、写 granted。
        row.used_at = Some(now);
        row.signature = Some(req.signature.clone());
        tx.module_conn(MODULE)
            .put(CHALLENGES, &row.key(), &row)
            .map_err(port_error)?;
        let data = ApprovalGrantedData {
            approval_id: req.approval_id,
            device_id: req.device.device_id,
            approval_pubkey_fpr: req.device.approval_pubkey_fpr,
            payload_sha256: sha256(row.payload_canonical.as_bytes()),
        };
        let next = match approvals::transition(
            &mut tx,
            &record,
            ApprovalStatus::Granted,
            serde_json::to_value(data).expect("approval.granted data serializes"),
            ctx.actor.clone(),
            ctx.causation_id,
        )
        .await
        {
            Ok(next) => next,
            // 并发 grant：先提交者赢，其余看到最新状态。
            Err(StoreError::Conflict { .. }) => {
                drop(tx);
                let status = self
                    .load(&req.approval_id)
                    .await?
                    .map_or(ApprovalStatus::Pending, |latest| latest.approval.status);
                return Err(GrantError::NotPending { status });
            }
            Err(error) => return Err(error.into()),
        };
        tx.commit().await.map_err(port_error)?;
        let mut approval = next.approval;
        approval.device_id = Some(req.device.device_id);
        approval.signature = Some(req.signature);
        Ok(approval)
    }

    async fn deny(
        &self,
        id: &ApprovalId,
        device: &DeviceKey,
        reason: Option<String>,
        ctx: &ActorCtx,
    ) -> Result<Approval, GrantError> {
        if !device.may_approve() {
            return Err(GrantError::Forbidden);
        }
        let record = self.load(id).await?.ok_or(GrantError::NotFound)?;
        if record.approval.status != ApprovalStatus::Pending {
            return Err(GrantError::NotPending {
                status: record.approval.status,
            });
        }
        let data = match reason {
            Some(reason) => serde_json::json!({ "reason": reason }),
            None => serde_json::json!({}),
        };
        let mut tx = self.engine.store.begin_write().await.map_err(port_error)?;
        let next = match approvals::transition(
            &mut tx,
            &record,
            ApprovalStatus::Denied,
            data,
            ctx.actor.clone(),
            ctx.causation_id,
        )
        .await
        {
            Ok(next) => next,
            Err(StoreError::Conflict { .. }) => {
                drop(tx);
                let status = self
                    .load(id)
                    .await?
                    .map_or(ApprovalStatus::Pending, |latest| latest.approval.status);
                return Err(GrantError::NotPending { status });
            }
            Err(error) => return Err(error.into()),
        };
        tx.commit().await.map_err(port_error)?;
        Ok(next.approval)
    }

    async fn consume_internal(
        &self,
        tx: &mut WriteTx,
        req: ConsumeInternal,
    ) -> Result<(), ConsumeError> {
        let mismatch = |reason| ConsumeError::Mismatch { reason };
        let log = self.engine.store.events();
        let record = approvals::load(&log, &req.approval_id)
            .await?
            .ok_or(ConsumeError::NotFound)?;
        let approval = &record.approval;
        // #2：granted 且未过期。过期不在调用方事务里落盘（失败会整体回滚），由懒过期 / 清扫处理。
        match approval.status {
            ApprovalStatus::Consumed => return Err(mismatch(MismatchReason::Consumed)),
            ApprovalStatus::Expired => return Err(mismatch(MismatchReason::Expired)),
            ApprovalStatus::Granted if record.is_due(self.now()) => {
                return Err(mismatch(MismatchReason::Expired))
            }
            ApprovalStatus::Granted => {}
            _ => return Err(mismatch(MismatchReason::NotGranted)),
        }
        // #3：action / target。
        if approval.action != req.expected_action.approval_action() {
            return Err(mismatch(MismatchReason::ActionMismatch));
        }
        if approval.target != req.expected_target {
            return Err(mismatch(MismatchReason::TargetMismatch));
        }
        // #4：请求体摘要。
        if approval.subject_digest != req.body_digest {
            return Err(mismatch(MismatchReason::DigestMismatch));
        }
        // #5：settings 的 base_rev。
        if let Some(check) = req.rev_check {
            let ApprovalTarget::Settings { base_rev, .. } = &approval.target else {
                return Err(mismatch(MismatchReason::TargetMismatch));
            };
            if *base_rev != check.if_match_rev || *base_rev != check.current_rev {
                return Err(mismatch(MismatchReason::BaseRevMismatch));
            }
        }
        let consumed_by = format!("req:{}", req.request_id);
        let now = self.now();
        tx.module_conn(MODULE)
            .insert(
                CONSUMED,
                &approval.id.to_string(),
                &ConsumedRow {
                    consumed_by: consumed_by.clone(),
                    consumed_at: now,
                },
            )
            .map_err(port_error)?;
        approvals::transition(
            tx,
            &record,
            ApprovalStatus::Consumed,
            approvals::consumed_data(approval.id, consumed_by, approval.subject_digest),
            req.actor,
            None,
        )
        .await?;
        Ok(())
    }
}
