use gantry_core::canon::{sha256, RepoSlug};
use gantry_core::entities::{
    BudgetLedger, BudgetLimits, BudgetSubject, BudgetUsed, MergeMethod, Role, Seq, Tier,
};
use gantry_core::level::{Effort, Level};
use gantry_core::settings::{
    builtin_defaults, builtin_resolved, effective, parse_resolved, Egress, Levels, Models,
    Policies, ResolvedSettings, Sandbox, SettingsResource,
};
use gantry_core::time::Timestamp;
use gantry_core::worker::{PatchMode, ProposalKind, ProposalsMode};
use gantry_engine::decide::{EngineConsts, ModelKey, ModelTable, ProjectFacts, ResolvedModel};
use gantry_engine::runtime::RuntimeSettings;
use gantry_engine::runtime_settings::{
    from_settings, RuntimeSettingsError, DEFAULT_CLEANUP_LEASE, DEFAULT_POLL_INTERVAL,
};
use serde_json::json;

const BUILD_SCHEMA: &[u8] = include_bytes!("../../../schemas/worker/output.build.schema.json");

struct Snapshot {
    models: Models,
    levels: Levels,
    policies: Policies,
    sandbox: Sandbox,
    egress: Egress,
}

impl Snapshot {
    fn defaults() -> Self {
        let ResolvedSettings::Models(models) = builtin_resolved(SettingsResource::Models) else {
            unreachable!()
        };
        let ResolvedSettings::Levels(levels) = builtin_resolved(SettingsResource::Levels) else {
            unreachable!()
        };
        let ResolvedSettings::Policies(policies) = builtin_resolved(SettingsResource::Policies)
        else {
            unreachable!()
        };
        let ResolvedSettings::Sandbox(sandbox) = builtin_resolved(SettingsResource::Sandbox) else {
            unreachable!()
        };
        let ResolvedSettings::Egress(egress) = builtin_resolved(SettingsResource::Egress) else {
            unreachable!()
        };
        Self {
            models,
            levels,
            policies,
            sandbox,
            egress,
        }
    }

    fn build(&self, model_rev: u64) -> Result<RuntimeSettings, RuntimeSettingsError> {
        self.build_with(model_rev, project(), ModelTable::new(), BUILD_SCHEMA)
    }

    fn build_with(
        &self,
        model_rev: u64,
        project: ProjectFacts,
        models: ModelTable,
        schema: &[u8],
    ) -> Result<RuntimeSettings, RuntimeSettingsError> {
        self.build_at(
            model_rev,
            project,
            models,
            schema,
            Timestamp::from_millis(1_700_000_000_000),
        )
    }

    fn build_at(
        &self,
        model_rev: u64,
        project: ProjectFacts,
        models: ModelTable,
        schema: &[u8],
        now: Timestamp,
    ) -> Result<RuntimeSettings, RuntimeSettingsError> {
        from_settings(
            Seq::new(7).unwrap(),
            now,
            project,
            &self.models,
            &self.levels,
            &self.policies,
            &self.sandbox,
            &self.egress,
            Seq::new(model_rev).unwrap(),
            models,
            schema,
        )
    }
}

fn project() -> ProjectFacts {
    ProjectFacts {
        repo: RepoSlug::parse("acme/web").unwrap(),
        default_branch: "main".into(),
        branch_prefix: "gantry/".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
    }
}

#[test]
fn defaults_map_same_revision_and_real_schema() {
    let s = Snapshot::defaults();
    let runtime = s.build(7).unwrap();
    assert_eq!(runtime.actor.settings.config_rev, Seq::new(7).unwrap());
    assert_eq!(runtime.actor.settings.config_rev, runtime.runner.config_rev);
    assert_eq!(runtime.actor.settings.project.branch_prefix, "gantry/");
    assert_eq!(runtime.runner.repo, runtime.actor.settings.project.repo);
    assert!(runtime.actor.models.is_empty());
    assert_eq!(
        runtime.actor.sandbox_capacity as u64,
        s.sandbox.max_concurrent_sandboxes
    );
    assert_eq!(runtime.runner.io, s.sandbox.io);
    assert_eq!(runtime.runner.token_weights, s.levels.token_weights);
    assert_eq!(runtime.runner.max_turns as u64, s.models.worker.max_turns);
    assert_eq!(
        runtime.runner.egress_profile.as_str(),
        s.egress.default_profile
    );
    let groups: Vec<_> = runtime
        .runner
        .egress_groups
        .iter()
        .map(|g| g.as_str())
        .collect();
    let expected: Vec<_> = s.egress.profiles[&s.egress.default_profile]
        .groups
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(groups, expected);
    assert_eq!(runtime.poll_interval, DEFAULT_POLL_INTERVAL);
    assert_eq!(runtime.cleanup_lease, DEFAULT_CLEANUP_LEASE);
    let role = &runtime.runner.roles[&Role::Single];
    let builtin = gantry_core::worker::output_schema("gantry.worker.output.build/1").unwrap();
    assert_eq!(builtin, BUILD_SCHEMA);
    let queried = s
        .build_with(7, project(), ModelTable::new(), builtin)
        .unwrap();
    assert_eq!(
        queried.runner.roles[&Role::Single].structured_schema,
        role.structured_schema
    );
    assert_eq!(role.structured_schema.id, "gantry.worker.output.build/1");
    assert_eq!(
        role.structured_schema.path,
        "/gantry/inputs/output.schema.json"
    );
    assert_eq!(role.structured_schema.sha256, sha256(BUILD_SCHEMA));
    assert_ne!(role.structured_schema.sha256, sha256(b"build-schema"));
    assert_eq!(role.patch, PatchMode::Optional);
    assert_eq!(
        role.proposals,
        ProposalsMode::Allowed {
            allowed_kinds: vec![ProposalKind::OpenPr, ProposalKind::Escalate],
            max: 20
        }
    );
    assert_eq!(
        role.patch_limits.as_ref().unwrap().max_bytes,
        s.policies.patch.max_bytes
    );
    assert_eq!(
        u64::from(role.patch_limits.as_ref().unwrap().max_files),
        s.policies.patch.max_files
    );

    let now = Timestamp::from_millis(1_700_000_000_000);
    let mut ledger = BudgetLedger {
        id: "bdg_01JB7M4Q1T5X9V2N6R8K3W7YZC".parse().unwrap(),
        subject: BudgetSubject::Delivery,
        delivery_id: Some("dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB".parse().unwrap()),
        conversation_id: None,
        project_id: "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA".parse().unwrap(),
        day: None,
        template: None,
        limits: BudgetLimits::delivery(Default::default(), 0, 0, 0, 0, 0),
        used: BudgetUsed::default(),
        config_rev: None,
        created_at: now,
        updated_at: now,
    };
    ledger.limits.tokens_by_tier.insert(Tier::Worker, 100);
    ledger.used.tokens_by_tier.insert(Tier::Worker, 20);
    assert_eq!(
        runtime.actor.reservation.reserve(&ledger, Tier::Worker, 2),
        40
    );
}

#[test]
fn effective_project_overrides_reach_runner_and_actor() {
    let mut s = Snapshot::defaults();
    let resource = SettingsResource::Sandbox;
    let effective = effective(
        resource,
        &builtin_defaults(resource),
        &json!({}),
        Some(&json!({
            "limits": {"mem_mb": 1024, "wall_time_s": 900}
        })),
    );
    let ResolvedSettings::Sandbox(sandbox) = parse_resolved(resource, &effective.value).unwrap()
    else {
        unreachable!()
    };
    s.sandbox = sandbox;
    let runtime = s.build(7).unwrap();
    assert_eq!(runtime.runner.limits.mem_mb, 1024);
    assert_eq!(runtime.runner.limits.wall_time_s, 900);
    assert_eq!(runtime.actor.settings.sandbox.wall_time_s, 900);
}

#[test]
fn global_io_and_policy_are_from_merged_settings() {
    let mut s = Snapshot::defaults();
    let resource = SettingsResource::Sandbox;
    let effective = effective(
        resource,
        &builtin_defaults(resource),
        &json!({
            "io": {"artifacts": {"result_max_bytes": 4096}}, "kill_grace_s": 0
        }),
        None,
    );
    let ResolvedSettings::Sandbox(sandbox) = parse_resolved(resource, &effective.value).unwrap()
    else {
        unreachable!()
    };
    s.sandbox = sandbox;
    s.policies.patch.max_bytes = 1000;
    s.policies.patch.max_files = 7;
    let runtime = s.build(7).unwrap();
    assert_eq!(runtime.runner.io.artifacts.result_max_bytes, 4096);
    assert_eq!(runtime.runner.timers.kill_grace_s, 0);
    let patch = runtime.runner.roles[&Role::Single]
        .patch_limits
        .as_ref()
        .unwrap();
    assert_eq!((patch.max_bytes, patch.max_files), (1000, 7));
}

#[test]
fn revision_and_missing_egress_fail_closed() {
    let mut s = Snapshot::defaults();
    assert!(matches!(
        s.build(8),
        Err(RuntimeSettingsError::ModelRevisionMismatch)
    ));
    s.egress.profiles.remove(&s.egress.default_profile);
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::MissingProfile(_))
    ));
    let mut s = Snapshot::defaults();
    let name = s.egress.profiles[&s.egress.default_profile].groups[0].clone();
    s.egress.groups.remove(&name);
    assert!(matches!(s.build(7), Err(RuntimeSettingsError::MissingGroup(group)) if group == name));
}

#[test]
fn resolved_model_table_is_preserved() {
    let mut s = Snapshot::defaults();
    let key = ModelKey {
        sandbox: true,
        level: Level::Low,
        tier: Tier::Worker,
        critical: false,
    };
    let table = ModelTable::from([(
        key,
        ResolvedModel {
            provider_id: "configured-provider".into(),
            model: "configured-model".into(),
            reasoning_effort: Effort::Low,
            attempt_wall_s: 123,
        },
    )]);
    assert!(matches!(
        s.build_with(7, project(), table.clone(), BUILD_SCHEMA),
        Err(RuntimeSettingsError::WorkerModelMismatch)
    ));
    s.models.worker.provider = Some("configured-provider".into());
    assert!(matches!(
        s.build_with(7, project(), table.clone(), BUILD_SCHEMA),
        Err(RuntimeSettingsError::WorkerModelMismatch)
    ));
    s.models.worker.model = Some("configured-model".into());
    s.models.worker.attempt_wall_s = 123;
    assert!(matches!(
        s.build_with(7, project(), table.clone(), BUILD_SCHEMA),
        Err(RuntimeSettingsError::WorkerModelMismatch)
    ));
    s.models.providers.insert(
        "configured-provider".into(),
        serde_json::from_value(json!({
            "kind": "anthropic", "base_url": "https://api.anthropic.com",
            "credential": {"type": "api_key", "secret_ref": "model-key"}
        }))
        .unwrap(),
    );
    let runtime = s
        .build_with(7, project(), table.clone(), BUILD_SCHEMA)
        .unwrap();
    assert_eq!(runtime.actor.models, table);
    let mut wrong = table.clone();
    wrong.get_mut(&key).unwrap().provider_id = "other".into();
    assert!(matches!(
        s.build_with(7, project(), wrong, BUILD_SCHEMA),
        Err(RuntimeSettingsError::WorkerModelMismatch)
    ));
    let mut wrong = table.clone();
    wrong.get_mut(&key).unwrap().model = "other".into();
    assert!(matches!(
        s.build_with(7, project(), wrong, BUILD_SCHEMA),
        Err(RuntimeSettingsError::WorkerModelMismatch)
    ));
    for attempt_wall_s in [0, 124] {
        let mut wrong = table.clone();
        wrong.get_mut(&key).unwrap().attempt_wall_s = attempt_wall_s;
        assert!(matches!(
            s.build_with(7, project(), wrong, BUILD_SCHEMA),
            Err(RuntimeSettingsError::WorkerModelMismatch)
        ));
    }
    assert!(matches!(
        s.build_with(8, project(), table, BUILD_SCHEMA),
        Err(RuntimeSettingsError::ModelRevisionMismatch)
    ));
}

#[test]
fn proposal_count_uses_settings_and_current_worker_limit() {
    for (configured, expected) in [(1, 1), (20, 20), (21, 20), (u64::MAX, 20)] {
        let mut s = Snapshot::defaults();
        s.sandbox.io.artifacts.proposals_max_count = configured;
        let runtime = s.build(7).unwrap();
        let ProposalsMode::Allowed { max, .. } = &runtime.runner.roles[&Role::Single].proposals
        else {
            panic!("single proposals must be allowed")
        };
        assert_eq!(*max, expected);
    }
}

#[test]
fn project_facts_and_build_schema_fail_closed() {
    let s = Snapshot::defaults();
    for (field, value) in [
        ("project.default_branch", ""),
        ("project.default_branch", "main\nother"),
        ("project.branch_prefix", ""),
        ("project.branch_prefix", "gantry/\0"),
    ] {
        let mut facts = project();
        if field == "project.default_branch" {
            facts.default_branch = value.into()
        } else {
            facts.branch_prefix = value.into()
        }
        assert!(
            matches!(s.build_with(7, facts, ModelTable::new(), BUILD_SCHEMA), Err(RuntimeSettingsError::Invalid(actual)) if actual == field)
        );
    }
    let mut facts = project();
    facts.allowed_merge_methods.clear();
    assert!(matches!(
        s.build_with(7, facts, ModelTable::new(), BUILD_SCHEMA),
        Err(RuntimeSettingsError::Invalid(
            "project.allowed_merge_methods"
        ))
    ));
    assert!(matches!(
        s.build_with(7, project(), ModelTable::new(), b"build-schema"),
        Err(RuntimeSettingsError::Invalid("build_schema"))
    ));
    let mut altered = BUILD_SCHEMA.to_vec();
    altered.push(b' ');
    assert!(matches!(
        s.build_with(7, project(), ModelTable::new(), &altered),
        Err(RuntimeSettingsError::Invalid("build_schema"))
    ));
}

#[test]
fn numeric_boundaries_fail_without_truncation() {
    for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut s = Snapshot::defaults();
        s.sandbox.limits.cpu_cores = value;
        assert!(matches!(
            s.build(7),
            Err(RuntimeSettingsError::Invalid("sandbox.limits.cpu_cores"))
        ));
        let mut s = Snapshot::defaults();
        s.levels.token_weights.cache_read = value;
        assert!(matches!(
            s.build(7),
            Err(RuntimeSettingsError::Invalid(
                "levels.token_weights.cache_read"
            ))
        ));
    }
    for value in [0, u64::from(u32::MAX) + 1] {
        let mut s = Snapshot::defaults();
        s.sandbox.limits.pids = value;
        assert!(matches!(
            s.build(7),
            Err(RuntimeSettingsError::Invalid("sandbox.limits.pids"))
        ));
        let mut s = Snapshot::defaults();
        s.sandbox.max_concurrent_sandboxes = value;
        assert!(matches!(
            s.build(7),
            Err(RuntimeSettingsError::Invalid(
                "sandbox.max_concurrent_sandboxes"
            ))
        ));
        let mut s = Snapshot::defaults();
        s.models.worker.max_turns = value;
        assert!(matches!(
            s.build(7),
            Err(RuntimeSettingsError::Invalid("models.worker.max_turns"))
        ));
        let mut s = Snapshot::defaults();
        s.policies.patch.max_files = value;
        assert!(matches!(
            s.build(7),
            Err(RuntimeSettingsError::Invalid("policies.patch.max_files"))
        ));
        let mut s = Snapshot::defaults();
        s.sandbox.io.materials.max_total_bytes = value;
        assert!(matches!(
            s.build(7),
            Err(RuntimeSettingsError::Invalid(
                "sandbox.io.materials.max_total_bytes"
            ))
        ));
    }
    let mut s = Snapshot::defaults();
    s.sandbox.io.artifacts.result_max_bytes = 0;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid(
            "sandbox.io.artifacts.result_max_bytes"
        ))
    ));
    let mut s = Snapshot::defaults();
    s.sandbox.kill_grace_s = 0;
    assert_eq!(s.build(7).unwrap().runner.timers.kill_grace_s, 0);
}

#[test]
fn timer_and_lease_seconds_are_checked_before_millisecond_conversion() {
    // 固定时钟下，期限必须仍处于四位年份范围。
    let now = Timestamp::from_millis(1_700_000_000_000);
    let max_seconds = u64::try_from((Timestamp::MAX_MILLIS - now.as_millis()) / 1000).unwrap();
    let margin = EngineConsts::default().lock_lease_margin_s;
    let mut s = Snapshot::defaults();
    s.sandbox.limits.wall_time_s = 1;
    s.sandbox.kill_grace_s = 0;
    s.sandbox.idle_timeout_s = max_seconds;
    assert!(s.build(7).unwrap().actor.models.is_empty());
    s.sandbox.idle_timeout_s = max_seconds + 1;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid("sandbox.idle_timeout_s"))
    ));
    s.sandbox.idle_timeout_s = u64::MAX;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid("sandbox.idle_timeout_s"))
    ));

    let mut s = Snapshot::defaults();
    s.sandbox.limits.wall_time_s = 1;
    s.sandbox.kill_grace_s = max_seconds - 1 - margin;
    assert!(s.build(7).is_ok());
    s.sandbox.kill_grace_s += 1;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid("sandbox.lock_lease_s"))
    ));
    s.sandbox.kill_grace_s = u64::MAX;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid("sandbox.lock_lease_s"))
    ));

    let mut s = Snapshot::defaults();
    s.sandbox.kill_grace_s = 0;
    s.sandbox.limits.wall_time_s = max_seconds - margin;
    assert!(s.build(7).is_ok());
    s.sandbox.limits.wall_time_s += 1;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid("sandbox.lock_lease_s"))
    ));
    s.sandbox.limits.wall_time_s = u64::MAX;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid("sandbox.lock_lease_s"))
    ));
}

#[test]
fn time_validation_rejects_unrepresentable_deadlines_and_clocks() {
    let mut s = Snapshot::defaults();
    s.sandbox.kill_grace_s = 1_000_000_000_000;
    assert!(matches!(
        s.build(7),
        Err(RuntimeSettingsError::Invalid("sandbox.lock_lease_s"))
    ));
    s.sandbox.kill_grace_s = 0;
    for millis in [
        Timestamp::MIN_MILLIS - 1,
        Timestamp::MAX_MILLIS + 1,
        i64::MIN,
        i64::MAX,
    ] {
        assert!(matches!(
            s.build_at(
                7,
                project(),
                ModelTable::new(),
                BUILD_SCHEMA,
                Timestamp::from_millis(millis)
            ),
            Err(RuntimeSettingsError::Invalid("now"))
        ));
    }
    assert!(s
        .build_at(
            7,
            project(),
            ModelTable::new(),
            BUILD_SCHEMA,
            Timestamp::from_millis(Timestamp::MIN_MILLIS)
        )
        .is_ok());
    assert!(matches!(
        s.build_at(
            7,
            project(),
            ModelTable::new(),
            BUILD_SCHEMA,
            Timestamp::from_millis(Timestamp::MAX_MILLIS)
        ),
        Err(RuntimeSettingsError::Invalid("sandbox.lock_lease_s"))
    ));
}
