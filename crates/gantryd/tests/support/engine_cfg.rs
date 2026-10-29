//! 替身装配里引擎的运行时配置（engine.md §12.2、#323 的换算入口）。
//!
//! 生产里这些值由同一 revision 的设置换算；这里也走 `from_settings`，只覆盖为测试模型
//! 添加 provider/model。单项目 `acme/web`、基线 `BASE`。

use std::time::Duration;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{MergeMethod, PullRequestRef, Seq, Tier};
use gantry_core::level::{Effort, Level};
use gantry_core::settings::{
    builtin_defaults, effective, parse_resolved, ResolvedSettings, SettingsResource,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::output_schema;
use gantry_engine::decide::{ModelKey, ModelTable, ProjectFacts, ResolvedModel};
use gantry_engine::runtime::RuntimeSettings;
use gantry_engine::runtime_settings::from_settings;
use serde_json::json;

pub const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
pub const BASE: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
/// 替身 forge 为 create_pr 绑定的 PR 号。
pub const PR_NUMBER: u64 = 12;

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
    let table = ModelTable::from([(
        ModelKey {
            sandbox: true,
            level: Level::Low,
            tier: Tier::Worker,
            critical: false,
        },
        ResolvedModel {
            provider_id: "anthropic".to_owned(),
            model: "claude".to_owned(),
            reasoning_effort: Effort::Low,
            attempt_wall_s: models.worker.attempt_wall_s,
        },
    )]);
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

pub fn snapshot(state: &str, head: GitOid) -> PullRequestRef {
    serde_json::from_value(json!({
        "repo": "acme/web", "number": 12, "node_id": "PR_x",
        "title": {"text": "t", "source": "github", "truncated": false},
        "state": state, "head_sha": head, "base_ref": "main", "base_sha": BASE,
        "draft": false, "mergeable_state": "clean", "in_merge_queue": false,
        "checks": {"passed": 1, "failed": 0, "pending": 0, "required_ok": true},
        "review_state": "none", "synced_at": "2026-10-24T12:41:03.120Z"
    }))
    .expect("fixture")
}
