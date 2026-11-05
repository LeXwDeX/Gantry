//! `egress_config_view` 的组件测试：`egress` 设置 + `models.providers` 派生注入项。

use gantry_core::entities::{Seq, SettingsResource};
use gantry_core::settings::resource::ResolvedSettings;
use gantry_core::settings::{builtin_defaults, builtin_resolved, effective, parse_resolved};
use serde_json::json;

fn models() -> gantry_core::settings::resource::Models {
    let merged = effective(
        SettingsResource::Models,
        &builtin_defaults(SettingsResource::Models),
        &json!({
            "providers": {"anthropic": {
                "kind": "anthropic",
                "base_url": "https://api.anthropic.com",
                "credential": {"type": "api_key", "secret_ref": "model.anthropic.main"}
            }},
            "utility": {"provider": "anthropic", "model": "utility-from-settings"},
            "planner": {"provider": "anthropic", "model": "planner-from-settings"},
            "worker": {"provider": "anthropic", "model": "worker-model"}
        }),
        None,
    );
    let ResolvedSettings::Models(models) =
        parse_resolved(SettingsResource::Models, &merged.value).expect("models")
    else {
        unreachable!()
    };
    models
}

#[test]
fn models_providers_stay_in_providers() {
    let ResolvedSettings::Egress(egress) = builtin_resolved(SettingsResource::Egress) else {
        unreachable!()
    };
    let ResolvedSettings::Levels(levels) = builtin_resolved(SettingsResource::Levels) else {
        unreachable!()
    };
    let models = models();
    let view =
        gantryd::assembly::egress_config_view(&egress, &models, &levels, Seq::new(7).expect("rev"));

    assert_eq!(view.rev, 7);
    let provider = &view.providers["anthropic"];
    assert_eq!(provider.kind, "anthropic");
    assert_eq!(provider.base_url, "https://api.anthropic.com");

    // 模型 API 注入由 providers 表达；injection 保持为空。
    assert!(view.injection.is_empty());
    assert_eq!(
        provider.secret_ref,
        models.providers["anthropic"].credential.secret_ref
    );
    assert_eq!(view.token_weights, levels.token_weights);

    // 视图序列化后再反序列化必须通过 ConfigView 的复核。
    let value = serde_json::to_value(&view).expect("serialize view");
    let parsed: gantry_core::egress::ConfigView =
        serde_json::from_value(value).expect("valid config view");
    assert_eq!(parsed, view);
}

#[test]
fn explicit_injection_entries_are_kept() {
    let merged = effective(
        SettingsResource::Egress,
        &builtin_defaults(SettingsResource::Egress),
        &json!({
            "injection": {"registry": {
                "hosts": ["npm.example.com"],
                "kind": "bearer",
                "credential": {"secret_ref": "npm.token"}
            }}
        }),
        None,
    );
    let ResolvedSettings::Egress(egress) =
        parse_resolved(SettingsResource::Egress, &merged.value).expect("egress")
    else {
        unreachable!()
    };
    let ResolvedSettings::Levels(levels) = builtin_resolved(SettingsResource::Levels) else {
        unreachable!()
    };
    let models = models();
    let view =
        gantryd::assembly::egress_config_view(&egress, &models, &levels, Seq::new(1).expect("rev"));

    assert_eq!(view.injection["registry"].hosts, vec!["npm.example.com"]);
    assert!(!view.injection.contains_key("model.anthropic"));
}

#[tokio::test]
async fn build_egress_points_at_the_control_socket() {
    use gantry_core::egress::IssuedTo;
    use gantry_core::ports::egress::EgressControl;
    use gantry_core::time::Timestamp;
    use std::sync::Arc;

    let client = gantryd::assembly::build_egress(
        "/nonexistent/gantry-egress.sock",
        Arc::new(gantryd::assembly::SystemClock),
        IssuedTo {
            process: "gantryd".to_owned(),
            pid: std::process::id(),
            started_at: Timestamp::from_millis(1_700_000_000_000),
        },
    );
    let status = client.status().await;
    assert!(status.is_err(), "no egress process should answer");
}
