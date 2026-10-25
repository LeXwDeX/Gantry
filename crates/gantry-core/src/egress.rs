//! Egress 控制面契约类型（`gantry-egress-control/1`）。
//!
//! 权威来源：`docs/interfaces/egress-control.md`。仅定义 `/control/v1/` 的请求与响应形状，
//! 不含任何 IO，也不实现数据面代理。
//!
//! 密钥与令牌规则：
//! - 含 secret 值的请求（[`StageSecretRequest`]）只实现 `Serialize`，不实现 `Deserialize`；
//! - 含 `job_token` / `svc_token` 的响应用专用的 newtype，`Debug` 一律脱敏；
//! - 控制面没有任何返回 secret 值的响应类型。

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::entities::{Seq, Tier, TokenWeights};
use crate::ids::{AttemptId, ConversationId, DeliveryId, ProjectId, SandboxId, Ulid};
use crate::secret::{SecretName, SecretString};
use crate::time::{Date, Timestamp};

pub const CONTROL_PROTOCOL: &str = "gantry-egress-control/1";
pub const JOB_TOKEN_PREFIX: &str = "gjt1_";
pub const SVC_TOKEN_PREFIX: &str = "gst1_";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlProtocol;

impl ControlProtocol {
    pub const VALUE: &'static str = CONTROL_PROTOCOL;
}

impl Serialize for ControlProtocol {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(Self::VALUE)
    }
}

impl<'de> Deserialize<'de> for ControlProtocol {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == Self::VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(
                "unsupported egress control protocol",
            ))
        }
    }
}

impl JsonSchema for ControlProtocol {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EgressControlProtocol".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "const": CONTROL_PROTOCOL,
        })
    }
}

fn validate_prefixed_token(value: &str, prefix: &str, kind: &str) -> Result<(), String> {
    let body = value
        .strip_prefix(prefix)
        .ok_or_else(|| format!("{kind} must start with {prefix}"))?;
    if body.len() != 43 {
        return Err(format!("{kind} body must be 43 characters"));
    }
    if !body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(format!("{kind} body must be base64url"));
    }
    Ok(())
}

macro_rules! redacted_token {
    ($name:ident, $prefix:expr, $label:expr) => {
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, String> {
                validate_prefixed_token(value, $prefix, $label)?;
                Ok(Self(value.to_owned()))
            }

            /// 唯一的取值入口；只能用于向 Egress 发起请求或写进容器环境变量。
            pub fn expose_secret(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(***)"))
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
                schemars::json_schema!({
                    "type": "string",
                    "pattern": concat!("^", $prefix, "[A-Za-z0-9_-]{43}$"),
                })
            }
        }
    };
}

redacted_token!(JobToken, "gjt1_", "job token");
redacted_token!(SvcToken, "gst1_", "svc token");

/// 含 `job_token` 的基址表；`Debug` 只输出 provider 键。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelBaseUrls(BTreeMap<String, String>);

impl ModelBaseUrls {
    pub fn new(urls: BTreeMap<String, String>) -> Self {
        Self(urls)
    }

    pub fn get(&self, provider: &str) -> Option<&str> {
        self.0.get(provider).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

impl fmt::Debug for ModelBaseUrls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelBaseUrls")
            .field("providers", &self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// 已含 `job:<job_token>@` 的代理地址；`Debug` 脱敏。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConnectProxyUrl(String);

impl ConnectProxyUrl {
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    /// 唯一取值入口：原样写进容器的 `HTTPS_PROXY`。
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ConnectProxyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectProxyUrl(***)")
    }
}

// Egress 内部标识（`svt_` / `stg_`）不是 data-model 的 15 种 ID。egress-control 称其后缀
// 为 ULID，但文档示例包含 Crockford 之外的字母（`L`）；这里按线上示例放宽为 26 位大写
// 字母数字，避免拒绝契约自身给出的示例。
macro_rules! prefixed_ulid {
    ($name:ident, $prefix:expr) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, String> {
                let body = value
                    .strip_prefix($prefix)
                    .ok_or_else(|| format!("id must start with {}", $prefix))?;
                if body.len() != 26 {
                    return Err("id body must be 26 characters".into());
                }
                if !body.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
                    return Err("id body must be uppercase alphanumeric".into());
                }
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
                schemars::json_schema!({
                    "type": "string",
                    "pattern": concat!("^", $prefix, "[0-9A-Z]{26}$"),
                })
            }
        }
    };
}

prefixed_ulid!(SvcTokenId, "svt_");
prefixed_ulid!(StagedId, "stg_");

/// 台账引用：交付台账或会话台账（含 UTC 日期，P-58）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "subject", rename_all = "lowercase")]
pub enum LedgerRef {
    Delivery { id: DeliveryId },
    Conversation { id: ConversationId, day: Date },
}

/// 沙盒内只用 planner / worker 两类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxTier {
    Planner,
    Worker,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyObject;

impl Serialize for EmptyObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        serializer.serialize_map(Some(0))?.end()
    }
}

impl<'de> Deserialize<'de> for EmptyObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = BTreeMap::<String, Value>::deserialize(deserializer)?;
        if map.is_empty() {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("expected an empty object"))
        }
    }
}

impl JsonSchema for EmptyObject {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EmptyObject".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "object",
            "maxProperties": 0,
        })
    }
}

// ---------------------------------------------------------------------------
// 4.1 签发 job_token
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueJobTokenRequest {
    pub job_id: AttemptId,
    pub sandbox_id: SandboxId,
    pub project_id: ProjectId,
    pub ledger: LedgerRef,
    pub tier: SandboxTier,
    pub provider: String,
    pub profile: String,
    pub groups: Vec<String>,
    pub tokens_total: u64,
    pub token_weights: TokenWeights,
    pub wall_time_s: u64,
    pub config_rev: Seq,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueJobTokenResponse {
    pub job_id: AttemptId,
    pub job_token: JobToken,
    pub model_base_urls: ModelBaseUrls,
    pub connect_proxy_url: ConnectProxyUrl,
    pub effective_groups: Vec<String>,
    pub issued_at: Timestamp,
    pub expires_at: Timestamp,
    pub reissued: bool,
}

// ---------------------------------------------------------------------------
// 4.2 吊销 / 查询 / 遗忘
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum JobTokenState {
    Active,
    Revoked,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpstreamErrors {
    pub total_5xx: u64,
    pub total_429: u64,
    pub total_conn_errors: u64,
    pub max_consecutive: u64,
    #[serde(default)]
    pub last_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeniedCounters {
    pub not_allowed_host: u64,
    pub path_not_allowed: u64,
    pub budget_exhausted: u64,
}

/// `revoke` 与 `GET …/usage` 共用；`revoked_at` 仅在 `state = revoked` 时出现。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FinalUsage {
    pub job_id: AttemptId,
    pub state: JobTokenState,
    #[serde(default)]
    pub revoked_at: Option<Timestamp>,
    pub usage: crate::entities::TokenUsage,
    pub weighted_tokens: u64,
    pub capped: bool,
    pub incomplete: bool,
    pub upstream_errors: UpstreamErrors,
    pub denied: DeniedCounters,
}

// ---------------------------------------------------------------------------
// 4.3 svc_token
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssuedTo {
    pub process: String,
    pub pid: u32,
    pub started_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueSvcTokenRequest {
    pub issued_to: IssuedTo,
    #[serde(default)]
    pub ttl_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueSvcTokenResponse {
    pub svc_token_id: SvcTokenId,
    pub svc_token: SvcToken,
    pub model_base_urls: ModelBaseUrls,
    pub expires_at: Timestamp,
    pub revoked_previous: Vec<SvcTokenId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevokeSvcTokenResponse {
    pub svc_token_id: SvcTokenId,
    pub state: JobTokenState,
    pub revoked_at: Timestamp,
}

// ---------------------------------------------------------------------------
// 4.4 密钥：只写不读
// ---------------------------------------------------------------------------

/// 暂存请求：只实现 `Serialize`（值只允许写入，不允许从线上反序列化到内存）。
#[derive(PartialEq, Eq)]
pub struct StageSecretRequest {
    pub request_id: Ulid,
    pub secret_ref: SecretName,
    pub value: SecretString,
}

impl fmt::Debug for StageSecretRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StageSecretRequest")
            .field("request_id", &self.request_id)
            .field("secret_ref", &self.secret_ref)
            .field("value", &"***")
            .finish()
    }
}

impl Serialize for StageSecretRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("StageSecretRequest", 3)?;
        state.serialize_field("request_id", &self.request_id)?;
        state.serialize_field("secret_ref", &self.secret_ref)?;
        state.serialize_field("value", self.value.expose_secret())?;
        state.end()
    }
}

impl JsonSchema for StageSecretRequest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "StageSecretRequest".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct Shape {
            request_id: Ulid,
            secret_ref: SecretName,
            value: String,
        }
        let mut schema = Shape::json_schema(generator);
        if let Some(object) = schema.as_object_mut() {
            object.insert("additionalProperties".into(), Value::Bool(false));
        }
        schema
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StageSecretResponse {
    pub staged_id: StagedId,
    pub secret_ref: SecretName,
    pub version: u64,
    pub staged_at: Timestamp,
    pub expires_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivateSecretResponse {
    pub secret_ref: SecretName,
    pub version: u64,
    pub activated_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StagedSecretItem {
    pub staged_id: StagedId,
    pub secret_ref: SecretName,
    pub version: u64,
    pub staged_at: Timestamp,
    pub age_s: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StagedSecretList {
    pub items: Vec<StagedSecretItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresentRequest {
    pub refs: Vec<SecretName>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PresentResponse {
    pub present: BTreeMap<SecretName, bool>,
}

// ---------------------------------------------------------------------------
// 4.5 拉取用量
// ---------------------------------------------------------------------------

/// 查询参数（非 JSON 请求体）；`after` 是不透明游标。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub call_id: Option<Ulid>,
    #[serde(default)]
    pub svc_token_id: Option<SvcTokenId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum UsageCaller {
    Job {
        job_id: AttemptId,
    },
    Svc {
        svc_token_id: SvcTokenId,
        call_id: Ulid,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageRecord {
    pub seq: u64,
    pub at: Timestamp,
    pub caller: UsageCaller,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub ledger: Option<LedgerRef>,
    pub tier: Tier,
    pub provider: String,
    pub model: String,
    pub usage: crate::entities::TokenUsage,
    pub weighted_tokens: u64,
    pub incomplete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsagePage {
    pub items: Vec<UsageRecord>,
    pub next_cursor: String,
}

// ---------------------------------------------------------------------------
// 4.6 下发配置视图
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: String,
    pub base_url: String,
    pub secret_ref: SecretName,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressGroup {
    pub hosts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressProfile {
    pub groups: Vec<String>,
}

/// 配置视图只含引用，不含 secret 值。
///
/// `injection` 的形状在 egress-control §4.6 只给了示例 `{}`、字段表未定义（M2），
/// M0 保留不透明对象并记录为契约缺口，不猜测其结构。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigView {
    pub rev: u64,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub groups: BTreeMap<String, EgressGroup>,
    pub profiles: BTreeMap<String, EgressProfile>,
    pub default_profile: String,
    #[serde(default)]
    pub injection: BTreeMap<String, Value>,
    pub token_weights: TokenWeights,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigApplyResponse {
    pub applied_rev: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProblemError {
    pub pointer: String,
    pub message: String,
}

/// 控制面错误体：`application/problem+json`（形状同 api.md §3.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub type_uri: String,
    pub title: String,
    pub status: u16,
    pub code: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub request_id: Option<Ulid>,
    #[serde(default)]
    pub errors: Vec<ProblemError>,
}

// ---------------------------------------------------------------------------
// 4.7 状态与端点
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EgressHealth {
    Ok,
    Degraded,
    Unhealthy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusResponse {
    pub protocol: ControlProtocol,
    #[serde(default)]
    pub applied_rev: Option<u64>,
    pub health: EgressHealth,
    #[serde(default)]
    pub detail: Option<String>,
    pub unresolved_secret_refs: Vec<SecretName>,
    pub active_jobs: u64,
    pub active_svc_tokens: u64,
    #[serde(default)]
    pub usage_head: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EndpointsResponse {
    pub sandbox_model_base: String,
    pub connect_proxy_url: String,
    pub loopback_model_base: String,
    #[serde(default)]
    pub registry_base: Option<String>,
}
