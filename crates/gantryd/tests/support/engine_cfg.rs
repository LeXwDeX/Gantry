//! 替身装配里引擎的运行时配置（engine.md §12.2、#323 的换算入口）。
//!
//! 生产里这些值由同一 revision 的设置换算；这里也走 `from_settings`，只覆盖为测试模型
//! 添加 provider/model。单项目 `acme/web`、基线 `BASE`。

use std::time::Duration;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{MergeMethod, Seq, Tier};
use gantry_core::level::Level;
use gantry_core::settings::{
    builtin_defaults, effective, parse_resolved, ResolvedSettings, SettingsResource,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::output_schema;
use gantry_engine::decide::{ModelKey, ModelTable, ProjectFacts, ResolvedModel};
use gantry_engine::runtime::RuntimeSettings;
use gantry_engine::runtime_settings::from_settings;
use gantry_models::{ModelsView, Purpose, ResolveQuery};
use serde_json::json;
use std::sync::OnceLock;

pub const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
/// 不使用真实仓库的测试继续用这条假基线；真实仓库装配在打开前经 [`set_base`] 覆盖。
pub const BASE: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
/// 替身 forge 为 create_pr 绑定的 PR 号。
pub const PR_NUMBER: u64 = 1;

static BASE_OID: OnceLock<GitOid> = OnceLock::new();

/// 当前测试装配的基线。未设置时回退到 [`BASE`]（不经过真实 Git 仓库的测试）。
pub fn base() -> GitOid {
    BASE_OID
        .get()
        .copied()
        .unwrap_or_else(|| GitOid::parse(BASE).expect("legacy fixture"))
}

/// 真实仓库装配在创建 base 提交后覆盖基线；同一测试进程内只设置一次。
/// 测试装配每个进程只打开一次 store；若将来复用装配，需把 base 改为显式参数。
pub fn set_base(oid: GitOid) {
    let _ = BASE_OID.set(oid);
}

pub fn repo_slug() -> RepoSlug {
    RepoSlug::parse("acme/web").expect("fixture")
}

/// 替身装配的引擎配置：由内置设置 + 测试模型覆盖经 `from_settings` 换算。
///
/// 单项目 `acme/web`、基线 `BASE`。模型覆盖只让替身流程有一个可选的 worker 模型；
/// 其余值全部来自 core 的设置默认值，进程级 §12.2 用例因此覆盖真实换算（不再是手写配置）。
pub fn runtime_settings() -> RuntimeSettings {
    let resolve = |resource, global: serde_json::Value, project: serde_json::Value| {
        let merged = effective(
            resource,
            &builtin_defaults(resource),
            &global,
            Some(&project),
        );
        parse_resolved(resource, &merged.value).expect("fixture settings")
    };
    let ResolvedSettings::Models(models) = resolve(
        SettingsResource::Models,
        json!({
            "providers": {"anthropic": {
                "kind": "anthropic",
                "base_url": "https://api.anthropic.com",
                "credential": {"type": "api_key", "secret_ref": "model-key"}
            }},
            "worker": {"provider": "anthropic", "model": "claude"}
        }),
        json!({}),
    ) else {
        unreachable!()
    };
    let ResolvedSettings::Levels(levels) = resolve(SettingsResource::Levels, json!({}), json!({}))
    else {
        unreachable!()
    };
    let ResolvedSettings::Policies(policies) =
        resolve(SettingsResource::Policies, json!({}), json!({}))
    else {
        unreachable!()
    };
    let ResolvedSettings::Sandbox(sandbox) =
        resolve(SettingsResource::Sandbox, json!({}), json!({}))
    else {
        unreachable!()
    };
    let ResolvedSettings::Egress(egress) = resolve(SettingsResource::Egress, json!({}), json!({}))
    else {
        unreachable!()
    };
    let table = model_table(&models, &levels);
    let mut runtime = from_settings(
        Seq::new(1).expect("fixture"),
        Timestamp::from_millis(1_700_000_000_000),
        ProjectFacts {
            repo: repo_slug(),
            default_branch: "main".to_owned(),
            branch_prefix: "gantry/".to_owned(),
            allowed_merge_methods: vec![MergeMethod::Squash],
        },
        &models,
        &levels,
        &policies,
        &sandbox,
        &egress,
        Seq::new(1).expect("fixture"),
        table,
        output_schema("gantry.worker.output.build/1").expect("build schema"),
    )
    .expect("runtime settings");
    // 引擎内部轮询间隔不是设置键；§12.2 进程级用例需要更快的轮询。
    runtime.poll_interval = Duration::from_millis(5);
    runtime
}

/// 由 models 设置与 levels 推导 engine 的模型表；未配置的 tier 不进入表。
fn model_table(
    models: &gantry_core::settings::resource::Models,
    levels: &gantry_core::settings::resource::Levels,
) -> ModelTable {
    let view = ModelsView::build(models.clone(), levels, Seq::new(1).expect("fixture"))
        .expect("models view");
    let project = PROJECT.parse().expect("fixture project");
    let mut table = ModelTable::new();
    for sandbox in [false, true] {
        for level in [
            Level::Low,
            Level::Medium,
            Level::High,
            Level::Max,
            Level::Ultimate,
        ] {
            for tier in [Tier::Utility, Tier::Planner, Tier::Worker] {
                for critical in [false, true] {
                    let Ok(resolved) = view.resolve(&ResolveQuery {
                        project_id: project,
                        tier,
                        level: Some(level),
                        critical,
                        purpose: if sandbox {
                            Purpose::Sandbox
                        } else {
                            Purpose::Direct
                        },
                    }) else {
                        continue;
                    };
                    // 沙盒内的 planner / utility 沿用 worker 的适配器与上限（models.md §7.2）。
                    let attempt_wall_s = models.worker.attempt_wall_s;
                    table.insert(
                        ModelKey {
                            sandbox,
                            level,
                            tier,
                            critical,
                        },
                        ResolvedModel {
                            provider_id: resolved.provider_id,
                            model: resolved.model,
                            reasoning_effort: resolved.reasoning_effort,
                            attempt_wall_s,
                        },
                    );
                }
            }
        }
    }
    table
}
