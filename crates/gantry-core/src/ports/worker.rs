//! `WorkerAdapter`：服务端对 agent 适配器的声明。
//!
//! 签名以 `docs/modules/worker.md` §6.3 为准。该 trait 由 `worker` crate 的
//! `claude-code` 实现，M0 替身为 `StubWorkerAdapter`。适配器读取 JobSpec 的
//! 可信部分并返回镜像、命令、环境变量与能力表；不启动进程。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::canon::Sha256Digest;
use crate::level::Effort;
use crate::settings::resource::ProviderKind;
use crate::worker::WorkerProtocol;

use super::sandbox::{JobSpecTrusted, TokenEnvSpec};

/// 协议版本。worker-protocol 第一版为 `gantry-worker/1`。
pub type ProtocolVersion = WorkerProtocol;

/// 固定镜像引用。规格书只写 `ImageRef`；digest 是执行面唯一可接受的来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub reference: String,
    pub digest: Sha256Digest,
}

/// 适配器能力表，进入 `catalog.models.adapters`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCaps {
    pub provider_kinds: Vec<ProviderKind>,
    pub supports_fallback: bool,
    pub supports_tools: bool,
    pub reasoning_efforts: Vec<Effort>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("invalid agent options at {pointer}: {message}")]
    InvalidAgentOptions { pointer: String, message: String },
    #[error("adapter does not support {feature}")]
    Unsupported { feature: String },
    #[error("invalid job for adapter: {message}")]
    InvalidJob { message: String },
}

pub trait WorkerAdapter: Send + Sync {
    /// `"claude-code"` == `JobSpec.execution.agent`。
    fn agent_id(&self) -> &'static str;

    fn protocol_versions(&self) -> &[ProtocolVersion];

    fn image(&self) -> ImageRef;

    fn command(&self) -> Vec<String>;

    /// 非密且不含令牌的环境变量。
    fn env(&self, job: &JobSpecTrusted) -> BTreeMap<String, String>;

    /// 需由 sandbox 在签发 `job_token` 后填值的变量名。
    fn token_env(&self) -> TokenEnvSpec;

    fn capabilities(&self) -> AdapterCaps;

    /// 只能收紧 constraints；未知键一律拒绝。
    fn validate_agent_options(&self, options: &Value) -> Result<(), AdapterError>;
}
