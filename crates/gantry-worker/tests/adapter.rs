//! worker.md §12.I：服务端侧 `WorkerAdapter`（M1）。

use std::collections::BTreeMap;

use gantry_core::canon::sha256;
use gantry_core::entities::Exec;
use gantry_core::ports::worker::{AdapterError, ImageRef, WorkerAdapter};
use gantry_core::settings::resource::WorkerAdapter as AdapterSetting;
use gantry_core::worker::{AgentId, JobSpec, WorkerProtocol, PROTOCOL_VERSION};
use gantry_worker::claude_code::{AGENT_ID, PLACEHOLDER_API_KEY};
use gantry_worker::{layout, ClaudeCodeAdapter};
use serde_json::{json, Value};

const JOB_SPEC: &str = include_str!("../../gantry-core/fixtures/protocol/worker/job_spec.json");
const CATALOG: &str = include_str!("../../../fixtures/api/catalog.json");

fn adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new(ImageRef {
        reference: "registry.example/gantry/gantry-worker".into(),
        digest: sha256(b"worker-image"),
    })
}

fn job() -> JobSpec {
    serde_json::from_str(JOB_SPEC).expect("job_spec fixture decodes")
}

#[test]
fn agent_id_and_protocol_match_job_spec() {
    let adapter = adapter();
    let job = job();
    assert_eq!(adapter.agent_id(), AGENT_ID);
    assert_eq!(adapter.agent_id(), job.execution.agent.as_str());
    assert_eq!(adapter.agent_id(), AgentId::ClaudeCode.as_str());
    // 设置 `models.worker.adapter` 与协议同名（C26）。
    assert_eq!(
        serde_json::to_value(AdapterSetting::ClaudeCode).unwrap(),
        json!(AGENT_ID)
    );
    assert_eq!(adapter.protocol_versions(), &[WorkerProtocol]);
    assert_eq!(job.protocol, adapter.protocol_versions()[0]);
    assert_eq!(
        serde_json::to_value(adapter.protocol_versions()[0]).unwrap(),
        json!(PROTOCOL_VERSION)
    );
}

#[test]
fn capabilities_match_catalog_adapters_entry() {
    let caps = adapter().capabilities();
    let catalog: Value = serde_json::from_str(CATALOG).unwrap();
    let entry = &catalog["models"]["adapters"][AGENT_ID];
    let projected = json!({
        "provider_kinds": caps.provider_kinds,
        "supports_fallback": caps.supports_fallback,
    });
    assert_eq!(&projected, entry);
    assert_eq!(projected["provider_kinds"], json!(["anthropic"]));
    assert_eq!(projected["supports_fallback"], json!(false));
    assert!(caps.supports_tools);
    assert_eq!(
        serde_json::to_value(&caps.reasoning_efforts).unwrap(),
        json!(["none", "low", "medium", "high", "max"])
    );
}

#[test]
fn command_is_gantry_worker_run() {
    assert_eq!(adapter().command(), ["gantry-worker", "run"]);
}

#[test]
fn image_is_the_configured_digest_reference() {
    let image = ImageRef {
        reference: "registry.example/gantry/gantry-worker".into(),
        digest: sha256(b"another-image"),
    };
    assert_eq!(ClaudeCodeAdapter::new(image.clone()).image(), image);
}

#[test]
fn validate_agent_options_accepts_only_empty_object() {
    let adapter = adapter();
    assert_eq!(adapter.validate_agent_options(&json!({})), Ok(()));
    assert_eq!(
        adapter
            .validate_agent_options(&serde_json::to_value(job().execution.agent_options).unwrap()),
        Ok(())
    );
}

#[test]
fn validate_agent_options_rejects_unknown_keys_and_widening() {
    let adapter = adapter();
    let cases = [
        (json!({"verbose": true}), "/verbose"),
        // 试图放宽 constraints：一律拒绝。
        (json!({"tools": {"shell": true}}), "/tools"),
        (
            json!({"dangerously_skip_permissions": true}),
            "/dangerously_skip_permissions",
        ),
        (json!({"a/b~c": 1}), "/a~1b~0c"),
    ];
    for (options, pointer) in cases {
        match adapter.validate_agent_options(&options) {
            Err(AdapterError::InvalidAgentOptions { pointer: got, .. }) => {
                assert_eq!(got, pointer, "{options}")
            }
            other => panic!("{options} should be rejected, got {other:?}"),
        }
    }
    for options in [json!(null), json!([]), json!("x"), json!(1)] {
        assert!(
            matches!(
                adapter.validate_agent_options(&options),
                Err(AdapterError::InvalidAgentOptions { .. })
            ),
            "{options} should be rejected"
        );
    }
}

#[test]
fn env_has_fixed_layout_and_switches() {
    let env = adapter().env(&job());
    let expect = [
        ("GANTRY_JOB", "/gantry/job.json"),
        ("GANTRY_OUT", "/gantry/out"),
        ("GANTRY_WORK", "/gantry/work"),
        ("HOME", "/gantry/home"),
        ("TMPDIR", "/gantry/tmp"),
        ("NO_PROXY", "localhost,127.0.0.1"),
        ("ANTHROPIC_API_KEY", PLACEHOLDER_API_KEY),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_TELEMETRY", "1"),
    ];
    for (name, value) in expect {
        assert_eq!(env.get(name).map(String::as_str), Some(value), "{name}");
    }
    // `build` 角色不是只读工作区。
    assert!(!env.contains_key("GIT_OPTIONAL_LOCKS"));
}

#[test]
fn env_sets_optional_locks_for_read_only_roles() {
    let mut job = job();
    job.node.exec = Exec::SandboxRo;
    assert_eq!(
        adapter()
            .env(&job)
            .get("GIT_OPTIONAL_LOCKS")
            .map(String::as_str),
        Some("0")
    );
}

#[test]
fn env_and_token_env_are_disjoint() {
    let adapter = adapter();
    let env = adapter.env(&job());
    let token_env = adapter.token_env();
    assert_eq!(
        token_env.model_base_url,
        BTreeMap::from([("anthropic".to_owned(), "ANTHROPIC_BASE_URL".to_owned())])
    );
    assert_eq!(token_env.proxy, ["HTTPS_PROXY", "https_proxy"]);
    // 令牌变量只由 sandbox 在签发 job_token 后填值，env() 里不得出现。
    for name in token_env
        .model_base_url
        .values()
        .chain(token_env.proxy.iter())
    {
        assert!(!env.contains_key(name), "{name} must not be in env()");
    }
}

/// 正则式前缀 + 熵扫描；占位 key 是唯一豁免（worker.md §12.F / §12.I）。
#[test]
fn env_contains_no_secret_like_values() {
    let mut job = job();
    let env_build = adapter().env(&job);
    job.node.exec = Exec::SandboxRo;
    let env_ro = adapter().env(&job);
    for env in [env_build, env_ro] {
        for (name, value) in &env {
            assert!(
                !["GITHUB_TOKEN", "GH_TOKEN", "ANTHROPIC_AUTH_TOKEN"].contains(&name.as_str()),
                "{name} must never be injected"
            );
            if name == "ANTHROPIC_API_KEY" {
                assert_eq!(value, PLACEHOLDER_API_KEY);
                continue;
            }
            let upper = name.to_ascii_uppercase();
            assert!(
                !(upper.contains("TOKEN")
                    || upper.contains("SECRET")
                    || upper.contains("PASSWORD")),
                "{name} looks like a credential variable"
            );
            assert!(
                !looks_like_secret(value),
                "{name}={value} looks like a secret"
            );
            // job_token 只出现在模型 base URL 与 HTTPS_PROXY 里（P-49、P-72）。
            assert!(!value.contains("/j/") && !value.contains("job:"), "{name}");
        }
    }
}

#[test]
fn placeholder_key_is_not_secret_like_but_real_shapes_are() {
    assert!(!looks_like_secret(PLACEHOLDER_API_KEY));
    for sample in [
        "sk-ant-api03-AbCdEf0123456789",
        "ghp_0123456789abcdefABCDEF0123456789abcd",
        "github_pat_11ABCDEFG0123456789",
        "AKIAIOSFODNN7EXAMPLE",
        "Zx8Qp2Lr7Vt4Nw9Ks1Hd6Fj3Bm5Gc0Ya",
    ] {
        assert!(looks_like_secret(sample), "{sample}");
    }
}

#[test]
fn layout_matches_sandbox_mounts() {
    use gantry_sandbox::spec;
    assert_eq!(layout::JOB_JSON, spec::DST_JOB_JSON);
    assert_eq!(layout::INPUTS, spec::DST_INPUTS);
    assert_eq!(layout::WORK, spec::DST_WORK);
    assert_eq!(layout::OUT, spec::DST_OUT);
    assert_eq!(layout::HOME, spec::DST_HOME);
    assert_eq!(layout::TMP, spec::DST_TMP);
    assert_eq!(
        layout::OUTPUT_SCHEMA,
        gantry_sandbox::stage::OUTPUT_SCHEMA_PATH
    );
    assert_eq!(job().workspace.path, layout::WORK);
}

fn looks_like_secret(value: &str) -> bool {
    const PREFIXES: [&str; 6] = ["sk-", "ghp_", "gho_", "ghs_", "github_pat_", "AKIA"];
    if PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    // 路径、URL 与短开关值不是候选；长的单段高熵串视为疑似密钥。
    if value.len() < 20 || value.contains('/') || value.contains(',') {
        return false;
    }
    shannon_entropy(value) > 4.0
}

fn shannon_entropy(value: &str) -> f64 {
    let mut counts = BTreeMap::new();
    for ch in value.chars() {
        *counts.entry(ch).or_insert(0usize) += 1;
    }
    let len = value.chars().count() as f64;
    counts
        .values()
        .map(|&n| {
            let p = n as f64 / len;
            -p * p.log2()
        })
        .sum()
}
