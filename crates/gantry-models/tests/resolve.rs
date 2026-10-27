//! models.md §12.2：解析矩阵、项目覆盖、无默认模型、热加载。
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{global_models, other_project, project, put_settings, store};
use gantry_core::entities::{SettingsResource, Tier};
use gantry_core::level::{Effort, Level};
use gantry_core::ports::context::Health;
use gantry_core::settings::WorkerAdapter;
use gantry_models::{ModelsError, Purpose, ResolveQuery, StoreSettings, ViewCache};
use serde_json::json;

fn query(tier: Tier, level: Option<Level>, critical: bool, purpose: Purpose) -> ResolveQuery {
    ResolveQuery {
        project_id: project(),
        tier,
        level,
        critical,
        purpose,
    }
}

async fn configured() -> (gantry_store::Store, ViewCache) {
    let (store, _) = store();
    put_settings(&store, None, SettingsResource::Models, global_models()).await;
    let cache = ViewCache::new(Arc::new(StoreSettings::new(store.clone())));
    (store, cache)
}

#[tokio::test]
async fn utility_and_planner_use_their_fixed_defaults() {
    let (_, cache) = configured().await;
    let view = cache.view(Some(&project())).await.unwrap();

    let utility = view
        .resolve(&query(
            Tier::Utility,
            Some(Level::Max),
            true,
            Purpose::Direct,
        ))
        .unwrap();
    assert_eq!(utility.reasoning_effort, Effort::Low);
    assert_eq!(utility.model, "utility-model");
    assert_eq!(utility.provider_id, "anthropic");
    assert_eq!(utility.request_timeout_s, 60);
    assert_eq!(utility.total_timeout_s, Some(120));
    assert!(utility.sandbox.is_none());

    let planner = view
        .resolve(&query(
            Tier::Planner,
            Some(Level::Low),
            false,
            Purpose::Direct,
        ))
        .unwrap();
    assert_eq!(planner.reasoning_effort, Effort::High);
    assert_eq!(planner.request_timeout_s, 300);
    assert_eq!(planner.total_timeout_s, Some(900));
}

#[tokio::test]
async fn worker_by_level_matrix_matches_config_defaults() {
    let (_, cache) = configured().await;
    let view = cache.view(Some(&project())).await.unwrap();
    // config §3.2 默认 effort_by_level：low→low；medium→medium；
    // high→medium / critical high；max→high / critical max；Ultimate 取 max 项。
    let cases = [
        (Some(Level::Low), false, Effort::Low),
        (Some(Level::Low), true, Effort::Low),
        (Some(Level::Medium), false, Effort::Medium),
        (Some(Level::Medium), true, Effort::Medium),
        (Some(Level::High), false, Effort::Medium),
        (Some(Level::High), true, Effort::High),
        (Some(Level::Max), false, Effort::High),
        (Some(Level::Max), true, Effort::Max),
        (Some(Level::Ultimate), false, Effort::High),
        (Some(Level::Ultimate), true, Effort::Max),
        (Some(Level::None), false, Effort::Low),
        (None, false, Effort::Low),
        (None, true, Effort::Low),
    ];
    for (level, critical, expected) in cases {
        let resolved = view
            .resolve(&query(Tier::Worker, level, critical, Purpose::Sandbox))
            .unwrap();
        assert_eq!(
            resolved.reasoning_effort, expected,
            "level {level:?} critical {critical}"
        );
        let sandbox = resolved.sandbox.unwrap();
        assert_eq!(sandbox.adapter, WorkerAdapter::ClaudeCode);
        assert_eq!(sandbox.attempt_wall_s, 3600);
        assert_eq!(sandbox.max_turns, 60);
        assert_eq!(resolved.total_timeout_s, None);
    }
}

#[tokio::test]
async fn planner_in_sandbox_keeps_worker_adapter_and_limits() {
    let (_, cache) = configured().await;
    let view = cache.view(None).await.unwrap();
    let resolved = view
        .resolve(&query(
            Tier::Planner,
            Some(Level::High),
            false,
            Purpose::Sandbox,
        ))
        .unwrap();
    assert_eq!(resolved.model, "planner-model");
    assert_eq!(resolved.reasoning_effort, Effort::High);
    let sandbox = resolved.sandbox.unwrap();
    assert_eq!(sandbox.attempt_wall_s, 3600);
    assert_eq!(sandbox.max_turns, 60);
}

#[tokio::test]
async fn tier_and_purpose_must_match() {
    let (_, cache) = configured().await;
    let view = cache.view(None).await.unwrap();
    for (tier, purpose) in [
        (Tier::Utility, Purpose::Sandbox),
        (Tier::Worker, Purpose::Direct),
    ] {
        let error = view
            .resolve(&query(tier, None, false, purpose))
            .unwrap_err();
        assert_eq!(error.code(), "invalid_role", "{tier:?}/{purpose:?}");
    }
}

#[tokio::test]
async fn project_override_changes_worker_model_and_effort_only_for_that_project() {
    let (store, cache) = configured().await;
    put_settings(
        &store,
        Some(&project()),
        SettingsResource::Models,
        json!({"worker": {"model": "project-worker", "reasoning_effort": "max"}}),
    )
    .await;

    let view = cache.view(Some(&project())).await.unwrap();
    let resolved = view
        .resolve(&query(
            Tier::Worker,
            Some(Level::Low),
            false,
            Purpose::Sandbox,
        ))
        .unwrap();
    assert_eq!(resolved.model, "project-worker");
    assert_eq!(resolved.reasoning_effort, Effort::Max);

    let other = cache.view(Some(&other_project())).await.unwrap();
    let resolved = other
        .resolve(&query(
            Tier::Worker,
            Some(Level::Low),
            false,
            Purpose::Sandbox,
        ))
        .unwrap();
    assert_eq!(resolved.model, "worker-model");
    assert_eq!(resolved.reasoning_effort, Effort::Low);
}

#[tokio::test]
async fn project_cannot_add_providers_or_reference_unknown_ones() {
    let (store, cache) = configured().await;
    put_settings(
        &store,
        Some(&project()),
        SettingsResource::Models,
        json!({
            "providers": {
                "rogue": {
                    "kind": "anthropic",
                    "base_url": "https://evil.example",
                    "credential": {"type": "api_key", "secret_ref": "model.rogue"}
                }
            },
            "utility": {"provider": "rogue"}
        }),
    )
    .await;
    let view = cache.view(Some(&project())).await.unwrap();
    assert!(!view.models().providers.contains_key("rogue"));
    let error = view
        .resolve(&query(Tier::Utility, None, false, Purpose::Direct))
        .unwrap_err();
    assert!(
        matches!(
            error,
            ModelsError::NotConfigured {
                tier: "utility",
                ..
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn missing_model_is_not_configured_without_builtin_fallback() {
    let (store, _) = store();
    let cache = ViewCache::new(Arc::new(StoreSettings::new(store.clone())));
    let view = cache.view(None).await.unwrap();
    for (tier, purpose) in [
        (Tier::Utility, Purpose::Direct),
        (Tier::Planner, Purpose::Direct),
        (Tier::Worker, Purpose::Sandbox),
    ] {
        let error = view
            .resolve(&query(tier, None, false, purpose))
            .unwrap_err();
        assert_eq!(error.code(), "not_configured");
    }

    // 有 provider 但没有 model：仍然 NotConfigured，不回退到任何内置模型名。
    put_settings(
        &store,
        None,
        SettingsResource::Models,
        json!({
            "providers": {
                "anthropic": {
                    "kind": "anthropic",
                    "base_url": "https://api.anthropic.com",
                    "credential": {"type": "api_key", "secret_ref": "model.anthropic.main"}
                }
            },
            "utility": {"provider": "anthropic"}
        }),
    )
    .await;
    let view = cache.view(None).await.unwrap();
    let error = view
        .resolve(&query(Tier::Utility, None, false, Purpose::Direct))
        .unwrap_err();
    match error {
        ModelsError::NotConfigured { tier, reason } => {
            assert_eq!(tier, "utility");
            assert!(reason.contains("model"), "{reason}");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn hot_reload_switches_between_calls_and_keeps_old_snapshot() {
    let (store, cache) = configured().await;
    let before = cache.view(None).await.unwrap();
    let status = cache.status();
    assert_eq!(status.health, Health::Ok);
    assert_eq!(status.applied_rev, before.rev());

    let mut models = global_models();
    models["utility"]["model"] = json!("utility-model-2");
    put_settings(&store, None, SettingsResource::Models, models).await;

    let after = cache.view(None).await.unwrap();
    assert!(after.rev() > before.rev());
    let q = query(Tier::Utility, None, false, Purpose::Direct);
    // 进行中的调用持有旧快照，不受热加载影响。
    assert_eq!(before.resolve(&q).unwrap().model, "utility-model");
    assert_eq!(after.resolve(&q).unwrap().model, "utility-model-2");
    assert_eq!(cache.status().applied_rev, after.rev());
}

#[tokio::test]
async fn failed_build_keeps_previous_view_and_reports_degraded() {
    let (store, cache) = configured().await;
    let good = cache.view(None).await.unwrap();

    let mut models = global_models();
    models["retry"] = json!({"max_attempts": 0, "backoff_ms": 1000});
    put_settings(&store, None, SettingsResource::Models, models).await;

    let still = cache.view(None).await.unwrap();
    assert!(Arc::ptr_eq(&good, &still));
    let status = cache.status();
    assert_eq!(status.health, Health::Degraded);
    assert_eq!(status.applied_rev, good.rev());
    assert!(status.detail.unwrap().contains("max_attempts"));

    // 修正后恢复。
    put_settings(&store, None, SettingsResource::Models, global_models()).await;
    let fixed = cache.view(None).await.unwrap();
    assert!(fixed.rev() > good.rev());
    assert_eq!(cache.status().health, Health::Ok);
}

#[tokio::test]
async fn fallback_to_unknown_provider_is_config_invalid() {
    let (store, _) = store();
    let mut models = global_models();
    models["utility"]["fallback"] = json!([{"provider": "nope", "model": "m"}]);
    put_settings(&store, None, SettingsResource::Models, models).await;
    let cache = ViewCache::new(Arc::new(StoreSettings::new(store)));
    let error = cache.view(None).await.unwrap_err();
    assert_eq!(error.code(), "config_invalid");
    assert_eq!(cache.status().health, Health::Degraded);
}

#[tokio::test]
async fn token_weights_and_daily_cap_come_from_levels() {
    let (store, cache) = configured().await;
    let view = cache.view(None).await.unwrap();
    assert_eq!(view.token_weights().output, 5.0);
    assert_eq!(view.daily_tokens(), 500_000);

    put_settings(
        &store,
        None,
        SettingsResource::Levels,
        json!({
            "token_weights": {"input": 2.0, "output": 4.0, "cache_read": 0.5, "cache_write": 1.0},
            "none": {"daily_tokens": 1000}
        }),
    )
    .await;
    let view = cache.view(None).await.unwrap();
    assert_eq!(view.token_weights().input, 2.0);
    assert_eq!(view.daily_tokens(), 1000);
}
