//! `claude-code` 适配器的服务端侧声明（worker.md §6.3、K5）。
//!
//! 只声明镜像、命令、非密环境变量、令牌变量名与能力表，并校验
//! `execution.agent_options`；不启动进程、不读取凭据。

use std::collections::BTreeMap;

use gantry_core::entities::Exec;
use gantry_core::level::Effort;
use gantry_core::ports::sandbox::{JobSpecTrusted, TokenEnvSpec};
use gantry_core::ports::worker::{
    AdapterCaps, AdapterError, ImageRef, ProtocolVersion, WorkerAdapter,
};
use gantry_core::settings::resource::ProviderKind;
use gantry_core::worker::WorkerProtocol;
use serde_json::Value;

use crate::layout;

/// 与 `JobSpec.execution.agent`、设置 `models.worker.adapter` 同名（C26）。
pub const AGENT_ID: &str = "claude-code";

/// Anthropic 的 provider 标识，`token_env().model_base_url` 的键。
pub const ANTHROPIC_PROVIDER: &str = "anthropic";

/// 占位 key，**不是密钥**：CLI 需要有值才启动，Egress 丢弃它并注入真实 key
/// （worker-protocol §10.2、§2.3 E3）。
pub const PLACEHOLDER_API_KEY: &str = "gantry-egress-managed";

/// `claude-code` 适配器。镜像来自设置 `sandbox.base_image`（以 digest 固定，P-66），
/// 由构造方传入。
#[derive(Clone, Debug)]
pub struct ClaudeCodeAdapter {
    image: ImageRef,
}

impl ClaudeCodeAdapter {
    pub fn new(image: ImageRef) -> Self {
        Self { image }
    }
}

impl WorkerAdapter for ClaudeCodeAdapter {
    fn agent_id(&self) -> &'static str {
        AGENT_ID
    }

    fn protocol_versions(&self) -> &[ProtocolVersion] {
        static VERSIONS: [WorkerProtocol; 1] = [WorkerProtocol];
        &VERSIONS
    }

    fn image(&self) -> ImageRef {
        self.image.clone()
    }

    fn command(&self) -> Vec<String> {
        vec!["gantry-worker".to_owned(), "run".to_owned()]
    }

    fn env(&self, job: &JobSpecTrusted) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        let mut set = |name: &str, value: &str| {
            env.insert(name.to_owned(), value.to_owned());
        };
        set(layout::ENV_JOB, layout::JOB_JSON);
        set(layout::ENV_OUT, layout::OUT);
        set(layout::ENV_WORK, layout::WORK);
        set("HOME", layout::HOME);
        set("TMPDIR", layout::TMP);
        set("NO_PROXY", layout::NO_PROXY);
        set("ANTHROPIC_API_KEY", PLACEHOLDER_API_KEY);
        // 这些流量本来也会被白名单挡掉；关闭是为了避免启动重试拖慢（worker-protocol §10.2）。
        set("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        set("DISABLE_AUTOUPDATER", "1");
        set("DISABLE_TELEMETRY", "1");
        set("DISABLE_ERROR_REPORTING", "1");
        if job.node.exec == Exec::SandboxRo {
            // 只读工作区上的 git 不得写 index（worker.md §6.4 权限档）。
            set("GIT_OPTIONAL_LOCKS", "0");
        }
        env
    }

    fn token_env(&self) -> TokenEnvSpec {
        let mut model_base_url = BTreeMap::new();
        model_base_url.insert(
            ANTHROPIC_PROVIDER.to_owned(),
            "ANTHROPIC_BASE_URL".to_owned(),
        );
        TokenEnvSpec {
            model_base_url,
            proxy: vec!["HTTPS_PROXY".to_owned(), "https_proxy".to_owned()],
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

    /// M1 的白名单为空（OQ-W6）：只接受空对象，任何键都拒绝，因此也不可能放宽 constraints。
    fn validate_agent_options(&self, options: &Value) -> Result<(), AdapterError> {
        let Value::Object(map) = options else {
            return Err(AdapterError::InvalidAgentOptions {
                pointer: String::new(),
                message: "agent_options must be an object".into(),
            });
        };
        match map.keys().next() {
            Some(key) => Err(AdapterError::InvalidAgentOptions {
                pointer: format!("/{}", escape_pointer_token(key)),
                message: "unknown agent option; the claude-code adapter accepts none in M1".into(),
            }),
            None => Ok(()),
        }
    }
}

/// RFC 6901：`~` → `~0`，`/` → `~1`。
fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}
