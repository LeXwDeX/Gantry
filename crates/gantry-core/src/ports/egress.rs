//! `EgressControl`：`gantryd` 经 `egress.sock` 调用的控制面端口。
//!
//! 线上形状以 `docs/interfaces/egress-control.md` 为准；模块签名以
//! `docs/modules/egress.md` §6.2 为准。控制面只写不读密钥值，本模块没有任何
//! 返回 secret 明文的类型。
//!
//! 复用 `crate::egress` 的线上 DTO（`JobToken`、`ConfigView`、`UsagePage` 等），
//! 不复制令牌或实体 ID。

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;

use async_trait::async_trait;

use crate::egress::{
    CallTerminal, CloseCallResponse, ConfigApplyResponse, ConfigView, ConnectProxyUrl,
    FinalUsage as WireFinalUsage, IssueSvcTokenResponse, JobToken, ModelBaseUrls, StagedSecretItem,
    SvcTokenId, UsagePage, UsageRecord,
};
use crate::entities::{Seq, TokenUsage, TokenWeights};
use crate::ids::{AttemptId, ConversationId, DeliveryId, ProjectId, SandboxId};
use crate::secret::{Consumer, SecretName, SecretString};
use crate::time::Timestamp;

use super::context::{ActorCtx, Health, StoreError};
use super::models::{CallId, UsageSource};
use super::sandbox::{GroupName, ProfileName};
use super::secrets::SecretVersion;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InjectionId(String);

impl InjectionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InjectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 静态端点；不含任何令牌。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressEndpoints {
    /// 沙盒侧 CONNECT 代理地址（不含 token）。
    pub connect_proxy_host: String,
    pub add_host: Option<(String, IpAddr)>,
}

/// 为一个 Attempt 签发 `job_token`。会话台账的 `day` 取签发当天（UTC，P-58）。
#[derive(Clone, Debug, PartialEq)]
pub struct IssueJobToken {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
    pub project_id: ProjectId,
    pub delivery_id: Option<DeliveryId>,
    pub conversation_id: Option<ConversationId>,
    pub tier: crate::egress::SandboxTier,
    pub provider: ProviderId,
    pub profile: ProfileName,
    pub groups: Vec<GroupName>,
    pub tokens_total: u64,
    pub weights: TokenWeights,
    /// 令牌硬上限 = 签发时刻 + `wall_time_s` + 600 s（egress-control §3.1、§4.1）。
    pub wall_time_s: u64,
    /// `weights` 所属的配置修订；Egress 写进该尝试每条用量记录的 `weight_rev`
    /// （egress-control §4.5「加权」行）。
    pub config_rev: Seq,
}

impl IssueJobToken {
    /// 文档要求 `delivery_id` 与 `conversation_id` 恰有一个。
    pub fn validate(&self) -> Result<(), EgressError> {
        if self.delivery_id.is_some() == self.conversation_id.is_some() {
            return Err(EgressError::Rejected {
                code: "invalid_ledger".into(),
                message: "exactly one of delivery_id or conversation_id is required".into(),
            });
        }
        Ok(())
    }
}

/// 签发结果。`Debug` 不打印任何含令牌的 URL。
#[derive(Clone, PartialEq, Eq)]
pub struct JobTokenGrant {
    pub job_token: JobToken,
    pub model_base_urls: ModelBaseUrls,
    pub connect_proxy_url: ConnectProxyUrl,
    pub registry_routes: BTreeMap<InjectionId, String>,
    pub effective_groups: Vec<GroupName>,
    pub issued_at: Timestamp,
    pub expires_at: Timestamp,
    pub reissued: bool,
}

impl fmt::Debug for JobTokenGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobTokenGrant")
            .field("job_token", &self.job_token)
            .field("model_base_urls", &self.model_base_urls)
            .field("connect_proxy_url", &self.connect_proxy_url)
            .field(
                "registry_routes",
                &self.registry_routes.keys().collect::<Vec<_>>(),
            )
            .field("effective_groups", &self.effective_groups)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("reissued", &self.reissued)
            .finish()
    }
}

pub type SvcTokenGrant = IssueSvcTokenResponse;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobUsage {
    pub usage: TokenUsage,
    pub weighted_tokens: u64,
    pub capped: bool,
    pub incomplete: bool,
    pub upstream_errors: crate::egress::UpstreamErrors,
    pub denied: crate::egress::DeniedCounters,
}

/// 最终用量；复用控制面线上类型（含 `state` / `revoked_at`）。
pub type FinalUsage = WireFinalUsage;

pub type EgressConfigView = ConfigView;
pub type AppliedConfig = ConfigApplyResponse;

/// 两段式密钥写入的输入。只允许写入，不允许从线上读回值。
#[derive(PartialEq, Eq)]
pub struct StageSecret {
    pub secret_ref: SecretName,
    pub value: SecretString,
}

impl fmt::Debug for StageSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StageSecret")
            .field("secret_ref", &self.secret_ref)
            .field("value", &"***")
            .finish()
    }
}

/// 暂存句柄；与控制面 wire 类型相比补充 `consumer`，使同名 secret 的命名空间
/// 在端口层显式可见。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedSecret {
    pub consumer: Consumer,
    pub staged_id: crate::egress::StagedId,
    pub secret_ref: SecretName,
    pub version: SecretVersion,
}

pub type StagedInfo = StagedSecretItem;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallUsage {
    pub call_id: CallId,
    pub usage: TokenUsage,
    pub weighted_tokens: u64,
    pub usage_source: UsageSource,
    pub incomplete: bool,
}

/// 调用级关闭的结果：`{call_id, terminal}`（`egress-call-terminal.md` §2）。
pub type CallClosed = CloseCallResponse;

/// `GET usage?call_id=` 的端口视图（`egress-call-terminal.md` §4）。
///
/// - `terminal: None`：调用尚未关闭，不得结算、不得零判定；
/// - `terminal: Some(t)` 且 `t.incomplete`：已关闭但证明不完整（中断请求或已过保留期）；
/// - 其余：已关闭，`t.verify(call_id, &items)` 通过后可以结算。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallStatement {
    pub call_id: CallId,
    pub items: Vec<UsageRecord>,
    pub terminal: Option<CallTerminal>,
}

impl CallStatement {
    pub fn is_closed(&self) -> bool {
        self.terminal.is_some()
    }

    /// 已关闭、证明完整且与 `items` 一致时返回证明；其余情况一律 `None`（不得结算）。
    pub fn settleable_terminal(&self) -> Option<&CallTerminal> {
        let terminal = self.terminal.as_ref()?;
        (terminal.settleable() && terminal.verify(self.call_id, &self.items).is_ok())
            .then_some(terminal)
    }
}

/// 不透明用量游标。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsageCursor(String);

impl UsageCursor {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UsageCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressStatus {
    pub applied_rev: Seq,
    pub health: Health,
    pub detail: Option<String>,
    pub unresolved_secret_refs: Vec<SecretName>,
    pub active_job_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EgressError {
    #[error("egress is unavailable: {message}")]
    Unavailable { message: String },
    #[error("egress rejected the request ({code}): {message}")]
    Rejected { code: String, message: String },
    #[error("invalid egress config at {pointer}: {message}")]
    InvalidConfig { pointer: String, message: String },
    #[error("egress record not found")]
    NotFound,
    #[error("egress conflict: {message}")]
    Conflict { message: String },
    #[error("token invalid")]
    TokenInvalid,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Egress 控制面端口。
#[async_trait]
pub trait EgressControl: Send + Sync {
    async fn endpoints(&self) -> Result<EgressEndpoints, EgressError>;

    async fn issue_job_token(
        &self,
        req: IssueJobToken,
        ctx: &ActorCtx,
    ) -> Result<JobTokenGrant, EgressError>;

    async fn job_usage(&self, job_id: &AttemptId) -> Result<JobUsage, EgressError>;

    /// 吊销并取得最终快照；显式遗忘前保留该快照，不按 TTL 自动删除。
    async fn revoke_job_token(
        &self,
        job_id: &AttemptId,
        ctx: &ActorCtx,
    ) -> Result<FinalUsage, EgressError>;

    /// 调用方须先完成结算和 attempt.finished 提交；不存在时幂等成功。
    /// 已显式吊销、最终快照已持久化且无在途请求才可删除，否则 Conflict。
    /// 完整规则见 egress-control.md §4.2、§8；身份拒绝另遵守 worker-protocol §14。
    async fn forget_job(&self, job_id: &AttemptId, ctx: &ActorCtx) -> Result<(), EgressError>;

    async fn service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError>;

    async fn rotate_service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError>;

    async fn apply_config(
        &self,
        view: EgressConfigView,
        ctx: &ActorCtx,
    ) -> Result<AppliedConfig, EgressError>;

    async fn secret_present(
        &self,
        refs: &[SecretName],
    ) -> Result<BTreeMap<SecretName, bool>, EgressError>;

    async fn stage_secret(
        &self,
        req: StageSecret,
        ctx: &ActorCtx,
    ) -> Result<StagedSecret, EgressError>;

    async fn activate_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<SecretVersion, EgressError>;

    async fn discard_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<(), EgressError>;

    async fn staged_secrets(&self) -> Result<Vec<StagedInfo>, EgressError>;

    async fn call_usage(&self, call_id: &CallId) -> Result<CallUsage, EgressError>;

    async fn usage_since(&self, cursor: UsageCursor, limit: u32) -> Result<UsagePage, EgressError>;

    async fn svc_token_usage(
        &self,
        id: &crate::egress::SvcTokenId,
        cursor: Option<UsageCursor>,
    ) -> Result<UsagePage, EgressError>;

    /// 按全局 `call_id` 关闭一次直接调用（`POST …/svc-tokens/{id}/revoke`，`scope: "call"`）。
    /// 幂等，返回同一份 `terminal`；路径 token 对该调用没有已接受请求而别的 token 有 →
    /// `Conflict`；路径 token 从未签发 → `NotFound`。
    async fn close_call(
        &self,
        svc_token_id: &SvcTokenId,
        call_id: &CallId,
        ctx: &ActorCtx,
    ) -> Result<CallClosed, EgressError>;

    /// 只读：该调用的全部记录与终结证明（`GET usage?call_id=`）；可重复读取，不改变状态。
    async fn call_terminal(&self, call_id: &CallId) -> Result<CallStatement, EgressError>;

    async fn status(&self) -> Result<EgressStatus, EgressError>;
}

/// Egress 侧 secret 的命名空间恒为 `egress`；此常量供实现与测试核对。
pub const EGRESS_SECRET_CONSUMER: Consumer = Consumer::Egress;
