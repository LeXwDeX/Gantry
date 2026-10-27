//! 替身装配里引擎的固定配置（照搬 engine `tests/runtime.rs` 的夹具）。
//!
//! 生产里这些值由 settings 换算（engine 尚未提供换算入口，见 #277 评论）；这里只服务于
//! §12.2 端到端（替身），单项目 `acme/web`、基线 `BASE`。

use std::collections::HashMap;
use std::sync::Arc;

use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::entities::{
    MergeMethod, PullRequestRef, Role, SandboxLimits as Limits, SandboxTrust, Seq, Tier,
    TokenWeights,
};
use gantry_core::level::{Effort, Level};
use gantry_core::ports::sandbox::{GroupName, ProfileName, Timers};
use gantry_core::settings::resource::{
    ArtifactIo, MaterialIo, ProgressIo, SandboxDriver, SandboxIo,
};
use gantry_core::worker::{
    PatchLimits, PatchMode, ProposalKind, ProposalsMode, ReportMode, StructuredSchema,
    ToolConstraints,
};
use gantry_engine::actor::ActorConfig;
use gantry_engine::attempt::{RoleOutput, RunnerConfig};
use gantry_engine::decide::{
    EngineConsts, EngineSettings, ModelKey, ModelTable, ProjectFacts, ReservationPolicy,
    ResolvedModel, SandboxLimits, TemplateRegistry,
};
use serde_json::json;

pub const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
pub const BASE: &str = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
/// 替身 forge 为 create_pr 绑定的 PR 号。
pub const PR_NUMBER: u64 = 12;

pub fn repo_slug() -> RepoSlug {
    RepoSlug::parse("acme/web").expect("fixture")
}

pub struct FixedReserve;
impl ReservationPolicy for FixedReserve {
    fn reserve(&self, _: &gantry_core::entities::BudgetLedger, _: Tier, _: usize) -> u64 {
        1000
    }
}

pub fn actor_config() -> ActorConfig {
    ActorConfig {
        settings: EngineSettings {
            config_rev: Seq::new(1).expect("fixture"),
            project: ProjectFacts {
                repo: repo_slug(),
                default_branch: "main".to_owned(),
                branch_prefix: "gantry/".to_owned(),
                allowed_merge_methods: vec![MergeMethod::Squash],
            },
            sandbox: SandboxLimits {
                wall_time_s: 1800,
                kill_grace_s: 10,
            },
        },
        models: ModelTable::from([(
            ModelKey {
                sandbox: true,
                level: Level::Low,
                tier: Tier::Worker,
                critical: false,
            },
            ResolvedModel {
                provider_id: "anthropic".to_owned(),
                model: "claude".to_owned(),
                reasoning_effort: Effort::Medium,
                attempt_wall_s: 900,
            },
        )]),
        templates: TemplateRegistry,
        reservation: Arc::new(FixedReserve),
        sandbox_capacity: 4,
        consts: EngineConsts::default(),
    }
}

pub fn runner_config() -> RunnerConfig {
    let mut roles = HashMap::new();
    roles.insert(
        Role::Single,
        RoleOutput {
            structured_schema: StructuredSchema {
                id: "gantry.worker.output.build/1".into(),
                path: "/gantry/inputs/output.schema.json".into(),
                sha256: sha256(b"build-schema"),
            },
            patch: PatchMode::Optional,
            report: ReportMode::Optional,
            proposals: ProposalsMode::Allowed {
                allowed_kinds: vec![ProposalKind::OpenPr, ProposalKind::Escalate],
                max: 5,
            },
            tools: ToolConstraints {
                edit: true,
                shell: true,
            },
            patch_limits: Some(PatchLimits {
                max_bytes: 5_000_000,
                max_files: 200,
            }),
        },
    );
    RunnerConfig {
        config_rev: Seq::new(1).expect("fixture"),
        repo: repo_slug(),
        driver: SandboxDriver::Podman,
        trust: SandboxTrust::Trusted,
        limits: Limits {
            cpu_cores: 2.0,
            mem_mb: 4096,
            disk_mb: 10240,
            pids: 512,
            wall_time_s: 3600,
        },
        timers: Timers {
            idle_timeout_s: 300,
            kill_grace_s: 10,
        },
        io: SandboxIo {
            progress: ProgressIo {
                max_line_bytes: 8192,
                max_lines: 20_000,
                max_rate_per_s: 20,
            },
            artifacts: ArtifactIo {
                result_max_bytes: 1_048_576,
                report_max_bytes: 262_144,
                proposals_max_bytes: 262_144,
                proposals_max_count: 20,
            },
            materials: MaterialIo {
                max_item_bytes: 65_536,
                max_total_bytes: 524_288,
            },
            logs_tail_bytes: 1_048_576,
        },
        egress_profile: ProfileName::new("default"),
        egress_groups: vec![GroupName::new("model_api")],
        token_weights: TokenWeights::default(),
        max_turns: 60,
        discussion_max_comments: 50,
        roles,
    }
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
