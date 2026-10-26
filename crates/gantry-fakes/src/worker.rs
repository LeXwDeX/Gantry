//! `StubWorkerAdapter`：M0 的 `claude-code` 适配器替身。
//!
//! 只声明固定镜像、命令、能力表与非密环境变量；不启动进程、不读取凭据。
//! `env` 里的 `ANTHROPIC_API_KEY` 是占位值，不是密钥；真实 provider key 只在
//! Egress 中，模型基址由 sandbox 在签发 `job_token` 后按 `token_env` 填入。

use std::collections::BTreeMap;

use gantry_core::canon::sha256;
use gantry_core::level::Effort;
use gantry_core::ports::sandbox::JobSpecTrusted;
use gantry_core::ports::sandbox::TokenEnvSpec;
use gantry_core::ports::worker::{
    AdapterCaps, AdapterError, ImageRef, ProtocolVersion, WorkerAdapter,
};
use gantry_core::settings::resource::ProviderKind;
use gantry_core::worker::WorkerProtocol;
use serde_json::Value;

const STUB_IMAGE_REFERENCE: &str = "ghcr.io/gantry/worker-claude-code:0.1.0";

/// 固定的 `claude-code` 适配器声明。
#[derive(Clone, Debug, Default)]
pub struct StubWorkerAdapter;

impl StubWorkerAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl WorkerAdapter for StubWorkerAdapter {
    fn agent_id(&self) -> &'static str {
        "claude-code"
    }

    fn protocol_versions(&self) -> &[ProtocolVersion] {
        static VERSIONS: [WorkerProtocol; 1] = [WorkerProtocol];
        &VERSIONS
    }

    fn image(&self) -> ImageRef {
        ImageRef {
            reference: STUB_IMAGE_REFERENCE.to_owned(),
            digest: sha256(b"gantry-fakes/stub-worker-image/0.1.0"),
        }
    }

    fn command(&self) -> Vec<String> {
        vec!["gantry-worker".to_owned(), "run".to_owned()]
    }

    fn env(&self, job: &JobSpecTrusted) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("GANTRY_JOB".into(), job.job_id.to_string());
        env.insert("GANTRY_OUT".into(), "/gantry/out".into());
        env.insert("GANTRY_WORK".into(), "/gantry/work".into());
        env.insert("GANTRY_PROTOCOL".into(), WorkerProtocol::VALUE.into());
        env.insert("HOME".into(), "/gantry/home".into());
        env.insert("TMPDIR".into(), "/gantry/tmp".into());
        env.insert("NO_PROXY".into(), "127.0.0.1,localhost".into());
        // 占位值，不是密钥；Egress 丢弃它并注入真实 key（worker-protocol §2.3 E3）。
        env.insert("ANTHROPIC_API_KEY".into(), "gantry-egress-managed".into());
        env.insert(
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        );
        env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
        env.insert("DISABLE_TELEMETRY".into(), "1".into());
        env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
        env
    }

    fn token_env(&self) -> TokenEnvSpec {
        let mut providers = BTreeMap::new();
        providers.insert("anthropic".into(), "ANTHROPIC_BASE_URL".into());
        TokenEnvSpec {
            model_base_url: providers,
            proxy: vec!["HTTPS_PROXY".into(), "https_proxy".into()],
        }
    }

    fn capabilities(&self) -> AdapterCaps {
        AdapterCaps {
            provider_kinds: vec![ProviderKind::Anthropic],
            supports_fallback: false,
            supports_tools: true,
            reasoning_efforts: vec![
                Effort::None,
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Max,
            ],
        }
    }

    fn validate_agent_options(&self, options: &Value) -> Result<(), AdapterError> {
        let Value::Object(map) = options else {
            return Err(AdapterError::InvalidAgentOptions {
                pointer: "/".into(),
                message: "agent_options must be an object".into(),
            });
        };
        if let Some(key) = map.keys().next() {
            return Err(AdapterError::InvalidAgentOptions {
                pointer: format!("/{key}"),
                message: "the claude-code adapter accepts no agent_options in M1".into(),
            });
        }
        Ok(())
    }
}
