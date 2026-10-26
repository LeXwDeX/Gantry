//! 设置模块复核回归：P1 反例、逐键 clamp、null 回退与方向判定。

use gantry_core::entities::{DeviceRole, SettingsResource, SettingsScope};
use gantry_core::settings::{
    diff_relaxes, effective, has_direction, required_authority_for_change,
};
use serde_json::{json, Value};

#[test]
fn budget_mixed_override_keeps_tightened_fields() {
    let builtin = json!({
        "high": {"budget": {
            "nodes": 40, "rounds": 4,
            "tokens_by_tier": {"worker": 1_000}
        }}
    });
    let global = json!({
        "high": {"budget": {
            "nodes": 10, "rounds": 4,
            "tokens_by_tier": {"worker": 1_000}
        }}
    });
    let project = json!({
        "high": {"budget": {
            "nodes": 20, "rounds": 2,
            "tokens_by_tier": {"worker": 2_000}
        }}
    });

    let result = effective(SettingsResource::Levels, &builtin, &global, Some(&project));

    assert_eq!(result.value["high"]["budget"]["nodes"], 10);
    assert_eq!(result.value["high"]["budget"]["rounds"], 2);
    assert_eq!(
        result.value["high"]["budget"]["tokens_by_tier"]["worker"],
        1_000
    );
    assert_eq!(result.clamped, vec!["levels.high.budget".to_owned()]);
}

#[test]
fn sandbox_limits_mixed_override_keeps_tightened_fields() {
    let builtin = json!({"limits": {
        "cpu_cores": 2, "mem_mb": 4096, "disk_mb": 10240,
        "pids": 512, "wall_time_s": 3600
    }});
    let global = json!({"limits": {
        "cpu_cores": 2, "mem_mb": 2048, "disk_mb": 512,
        "pids": 256, "wall_time_s": 1800
    }});
    let project = json!({"limits": {
        "cpu_cores": 4, "mem_mb": 4096, "disk_mb": 256,
        "pids": 128, "wall_time_s": 900
    }});

    let result = effective(SettingsResource::Sandbox, &builtin, &global, Some(&project));

    assert_eq!(result.value["limits"]["cpu_cores"], 2);
    assert_eq!(result.value["limits"]["mem_mb"], 2048);
    assert_eq!(result.value["limits"]["disk_mb"], 256);
    assert_eq!(result.value["limits"]["pids"], 128);
    assert_eq!(result.value["limits"]["wall_time_s"], 900);
    assert_eq!(result.clamped, vec!["sandbox.limits".to_owned()]);
}

#[test]
fn actions_mixed_override_keeps_tightened_actions() {
    let builtin = json!({"actions": {
        "merge_pr": "approve", "push_branch": "auto", "delete_branch": "deny"
    }});
    let global = json!({"actions": {
        "merge_pr": "deny", "push_branch": "auto", "delete_branch": "deny"
    }});
    let project = json!({"actions": {
        "merge_pr": "auto", "push_branch": "deny", "delete_branch": "approve"
    }});

    let result = effective(
        SettingsResource::Policies,
        &builtin,
        &global,
        Some(&project),
    );

    assert_eq!(result.value["actions"]["merge_pr"], "deny");
    assert_eq!(result.value["actions"]["push_branch"], "deny");
    assert_eq!(result.value["actions"]["delete_branch"], "deny");
    assert_eq!(result.clamped, vec!["policies.actions".to_owned()]);
}

#[test]
fn path_rule_mixed_override_keeps_added_globs() {
    let builtin = json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}}});
    let global = json!({"paths": {"x": {"globs": ["a"], "disposition": "deny"}}});
    let project = json!({"paths": {"x": {"globs": ["a", "b"], "disposition": "approve"}}});

    let result = effective(
        SettingsResource::Policies,
        &builtin,
        &global,
        Some(&project),
    );

    assert_eq!(result.value["paths"]["x"]["globs"], json!(["a", "b"]));
    assert_eq!(result.value["paths"]["x"]["disposition"], "deny");
    assert_eq!(result.clamped, vec!["policies.paths".to_owned()]);
}

#[test]
fn escalation_mixed_override_keeps_tightened_condition() {
    let builtin = json!({"escalation": {"enabled": true, "rules": {}}});
    let global = json!({"escalation": {"enabled": true, "rules": {
        "r": {"when": {"paths": ["a"]}, "raise_to": "Max"}
    }}});
    let project = json!({"escalation": {"rules": {
        "r": {"when": {"paths": ["a", "b"]}, "raise_to": "High"}
    }}});

    let result = effective(
        SettingsResource::Policies,
        &builtin,
        &global,
        Some(&project),
    );

    assert_eq!(
        result.value["escalation"]["rules"]["r"]["when"]["paths"],
        json!(["a", "b"])
    );
    assert_eq!(result.value["escalation"]["rules"]["r"]["raise_to"], "Max");
    assert_eq!(result.value["escalation"]["enabled"], true);
    assert_eq!(result.clamped, vec!["policies.escalation".to_owned()]);
}

#[test]
fn escalation_diff_mixed_thresholds_keep_widening() {
    let builtin = json!({"escalation": {"enabled": true, "rules": {}}});
    let global = json!({"escalation": {"rules": {
        "d": {"when": {"diff": {"files_gte": 30, "lines_gte": 1000}}, "raise_to": "Max"}
    }}});
    let project = json!({"escalation": {"rules": {
        "d": {"when": {"diff": {"files_gte": 20, "lines_gte": 2000}}, "raise_to": "Max"}
    }}});

    let result = effective(
        SettingsResource::Policies,
        &builtin,
        &global,
        Some(&project),
    );

    assert_eq!(
        result.value["escalation"]["rules"]["d"]["when"]["diff"]["files_gte"],
        20
    );
    assert_eq!(
        result.value["escalation"]["rules"]["d"]["when"]["diff"]["lines_gte"],
        1000
    );
}

#[test]
fn patch_limits_mixed_override_keeps_tightened_fields() {
    let builtin = json!({"patch": {
        "max_files": 200, "max_bytes": 5_000_000,
        "max_changed_lines": 10_000, "max_file_bytes": 1_000_000
    }});
    let global = json!({"patch": {
        "max_files": 200, "max_bytes": 2_000_000,
        "max_changed_lines": 10_000, "max_file_bytes": 1_000_000
    }});
    let project = json!({"patch": {
        "max_files": 300, "max_bytes": 1_000_000,
        "max_changed_lines": 5_000, "max_file_bytes": 1_000_000
    }});

    let result = effective(
        SettingsResource::Policies,
        &builtin,
        &global,
        Some(&project),
    );

    assert_eq!(result.value["patch"]["max_files"], 200);
    assert_eq!(result.value["patch"]["max_bytes"], 1_000_000);
    assert_eq!(result.value["patch"]["max_changed_lines"], 5_000);
    assert_eq!(result.value["patch"]["max_file_bytes"], 1_000_000);
    assert_eq!(result.clamped, vec!["policies.patch".to_owned()]);
}

#[test]
fn guardian_mixed_override_keeps_tightened_fields() {
    let builtin = json!({
        "triage": {"security_finding": "escalate", "nit": "batch"},
        "no_progress": {"same_class_repeats": 2, "stalled_rounds": 2}
    });
    let global = json!({
        "triage": {"security_finding": "escalate", "nit": "batch"},
        "no_progress": {"same_class_repeats": 2, "stalled_rounds": 2}
    });
    let project = json!({
        "triage": {"security_finding": "silent_fix", "nit": "escalate"},
        "no_progress": {"same_class_repeats": 5, "stalled_rounds": 1}
    });

    let result = effective(
        SettingsResource::Guardian,
        &builtin,
        &global,
        Some(&project),
    );

    assert_eq!(result.value["triage"]["security_finding"], "escalate");
    assert_eq!(result.value["triage"]["nit"], "escalate");
    assert_eq!(result.value["no_progress"]["same_class_repeats"], 2);
    assert_eq!(result.value["no_progress"]["stalled_rounds"], 1);
    assert_eq!(
        result.clamped,
        vec![
            "guardian.no_progress".to_owned(),
            "guardian.triage".to_owned()
        ]
    );
}

#[test]
fn egress_profile_mixed_override_keeps_narrowing() {
    let builtin = json!({"profiles": {
        "default": {"groups": ["model_api", "npm"]},
        "npm": {"groups": ["model_api"]}
    }});
    let global = builtin.clone();
    let project = json!({"profiles": {
        "default": {"groups": ["model_api"]},
        "npm": {"groups": ["model_api", "npm"]}
    }});

    let result = effective(SettingsResource::Egress, &builtin, &global, Some(&project));

    assert_eq!(
        result.value["profiles"]["default"]["groups"],
        json!(["model_api"])
    );
    assert_eq!(
        result.value["profiles"]["npm"]["groups"],
        json!(["model_api"])
    );
    assert_eq!(result.clamped, vec!["egress.profiles".to_owned()]);
}

#[test]
fn new_profile_does_not_discard_existing_narrowing() {
    let builtin = json!({"profiles": {"default": {"groups": ["model_api", "npm"]}}});
    let global = builtin.clone();
    let project = json!({"profiles": {
        "default": {"groups": ["model_api"]},
        "extra": {"groups": ["model_api"]}
    }});

    let result = effective(SettingsResource::Egress, &builtin, &global, Some(&project));

    assert_eq!(
        result.value["profiles"]["default"]["groups"],
        json!(["model_api"])
    );
    assert!(result.value["profiles"].get("extra").is_none());
    assert_eq!(result.clamped, vec!["egress.profiles".to_owned()]);
}

#[test]
fn project_null_cannot_widen_tightened_global() {
    let builtin = json!({"limits": {"mem_mb": 4096, "disk_mb": 10240}});
    let global = json!({"limits": {"mem_mb": 2048, "disk_mb": 512}});

    let nested_null = json!({"limits": {"mem_mb": null, "disk_mb": 256}});
    let result = effective(
        SettingsResource::Sandbox,
        &builtin,
        &global,
        Some(&nested_null),
    );
    assert_eq!(result.value["limits"]["mem_mb"], 2048);
    assert_eq!(result.value["limits"]["disk_mb"], 256);

    let whole_null = Value::Null;
    let result = effective(
        SettingsResource::Sandbox,
        &builtin,
        &global,
        Some(&whole_null),
    );
    assert_eq!(result.value["limits"]["mem_mb"], 2048);
    assert_eq!(result.value["limits"]["disk_mb"], 512);
}

#[test]
fn global_prewarm_and_io_growth_require_approval() {
    let old = json!({
        "prewarm": {"enabled": false, "pool_size": 0},
        "io": {"progress": {"max_line_bytes": 8192}, "logs_tail_bytes": 1_048_576}
    });
    let new = json!({
        "prewarm": {"enabled": false, "pool_size": 2},
        "io": {"progress": {"max_line_bytes": 16384}, "logs_tail_bytes": 2_097_152}
    });

    let relaxes = diff_relaxes(
        SettingsResource::Sandbox,
        &SettingsScope::Global,
        &old,
        &new,
    );
    assert_eq!(
        relaxes,
        vec!["sandbox.io".to_owned(), "sandbox.prewarm".to_owned()]
    );

    let authority =
        required_authority_for_change(SettingsResource::Sandbox, &relaxes, &old, &new, &[]);
    assert_eq!(authority.required_role, DeviceRole::Approver);
    assert!(authority.requires_signature);
}

#[test]
fn prewarm_and_io_shrink_do_not_relax() {
    let old = json!({
        "prewarm": {"enabled": false, "pool_size": 4},
        "io": {"logs_tail_bytes": 4096}
    });
    let new = json!({
        "prewarm": {"enabled": false, "pool_size": 2},
        "io": {"logs_tail_bytes": 2048}
    });

    let relaxes = diff_relaxes(
        SettingsResource::Sandbox,
        &SettingsScope::Global,
        &old,
        &new,
    );
    assert!(relaxes.is_empty(), "{relaxes:?}");

    let authority =
        required_authority_for_change(SettingsResource::Sandbox, &relaxes, &old, &new, &[]);
    assert_eq!(authority.required_role, DeviceRole::Operator);
    assert!(!authority.requires_signature);
}

#[test]
fn runtime_downgrade_relaxes_and_upgrade_does_not() {
    let downgrade = diff_relaxes(
        SettingsResource::Sandbox,
        &SettingsScope::Global,
        &json!({"runtime": "runsc"}),
        &json!({"runtime": "runc"}),
    );
    assert_eq!(downgrade, vec!["sandbox.runtime".to_owned()]);

    let upgrade = diff_relaxes(
        SettingsResource::Sandbox,
        &SettingsScope::Global,
        &json!({"runtime": "runc"}),
        &json!({"runtime": "runsc"}),
    );
    assert!(upgrade.is_empty(), "{upgrade:?}");
}

#[test]
fn has_direction_recognizes_constrained_subpaths() {
    assert!(has_direction("levels.high.budget.nodes"));
    assert!(has_direction("levels.none.daily_tokens"));
    assert!(has_direction("levels.token_weights.input"));
    assert!(has_direction("policies.actions.merge_pr"));
    assert!(has_direction("policies.paths.x.globs"));
    assert!(has_direction("policies.escalation.rules.x.raise_to"));
    assert!(has_direction("guardian.no_progress.stalled_rounds"));
    assert!(has_direction("sandbox.limits.mem_mb"));
    assert!(has_direction("sandbox.prewarm.pool_size"));
    assert!(has_direction("sandbox.runtime"));
    assert!(has_direction("sandbox.io.progress.max_lines"));
    assert!(has_direction("egress.profiles.default.groups"));
    assert!(has_direction("egress.groups.model_api.hosts"));
    assert!(!has_direction("models.providers.anthropic"));
    assert!(!has_direction("notify.channels.desktop.enabled"));
    assert!(!has_direction("sandbox.driver"));
}

struct DirectionCase {
    resource: SettingsResource,
    old: Value,
    new: Value,
    path: &'static str,
}

#[test]
fn directional_rule_table_has_relaxing_and_tightening_examples() {
    let relaxing = vec![
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"high": {"budget": {"nodes": 20}}}),
            new: json!({"high": {"budget": {"nodes": 40}}}),
            path: "levels.high.budget",
        },
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"high": {"budget": {"tokens_by_tier": {"worker": 100}}}}),
            new: json!({"high": {"budget": {"tokens_by_tier": {"worker": 200}}}}),
            path: "levels.high.budget",
        },
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"none": {"daily_tokens": 500}}),
            new: json!({"none": {"daily_tokens": 600}}),
            path: "levels.none.daily_tokens",
        },
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"token_weights": {"input": 1.0}}),
            new: json!({"token_weights": {"input": 0.5}}),
            path: "levels.token_weights",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"actions": {"merge_pr": "deny"}}),
            new: json!({"actions": {"merge_pr": "approve"}}),
            path: "policies.actions",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"unlisted_action": "deny"}),
            new: json!({"unlisted_action": "approve"}),
            path: "policies.unlisted_action",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"paths": {"x": {"globs": ["a", "b"]}}}),
            new: json!({"paths": {"x": {"globs": ["a"]}}}),
            path: "policies.paths",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"paths": {"x": {"disposition": "deny"}}}),
            new: json!({"paths": {"x": {"disposition": "approve"}}}),
            path: "policies.paths",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"paths": {"x": {"globs": ["a"]}}}),
            new: json!({"paths": {}}),
            path: "policies.paths",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"enabled": true}}),
            new: json!({"escalation": {"enabled": false}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "High"}}}}),
            new: json!({"escalation": {"rules": {}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a", "b"]}, "raise_to": "High"}}}}),
            new: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "High"}}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {"x": {"when": {"diff": {"files_gte": 30}}, "raise_to": "High"}}}}),
            new: json!({"escalation": {"rules": {"x": {"when": {"diff": {"files_gte": 40}}, "raise_to": "High"}}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "Max"}}}}),
            new: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "High"}}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"approval": {"ttl_s": 100}}),
            new: json!({"approval": {"ttl_s": 200}}),
            path: "policies.approval",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"patch": {"max_files": 200}}),
            new: json!({"patch": {"max_files": 300}}),
            path: "policies.patch",
        },
        DirectionCase {
            resource: SettingsResource::Guardian,
            old: json!({"triage": {"security_finding": "escalate"}}),
            new: json!({"triage": {"security_finding": "silent_fix"}}),
            path: "guardian.triage",
        },
        DirectionCase {
            resource: SettingsResource::Guardian,
            old: json!({"no_progress": {"same_class_repeats": 2}}),
            new: json!({"no_progress": {"same_class_repeats": 3}}),
            path: "guardian.no_progress",
        },
        DirectionCase {
            resource: SettingsResource::Guardian,
            old: json!({"patrol_interval_s": 1800}),
            new: json!({"patrol_interval_s": 3600}),
            path: "guardian.patrol_interval_s",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"limits": {"mem_mb": 4096}}),
            new: json!({"limits": {"mem_mb": 8192}}),
            path: "sandbox.limits",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"devcontainer": {"mode": "safe_subset"}}),
            new: json!({"devcontainer": {"mode": "off"}}),
            path: "sandbox.devcontainer",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"runtime": "runsc"}),
            new: json!({"runtime": "runc"}),
            path: "sandbox.runtime",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"max_concurrent_sandboxes": 6}),
            new: json!({"max_concurrent_sandboxes": 10}),
            path: "sandbox.max_concurrent_sandboxes",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"prewarm": {"pool_size": 0}}),
            new: json!({"prewarm": {"pool_size": 2}}),
            path: "sandbox.prewarm",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"idle_timeout_s": 300}),
            new: json!({"idle_timeout_s": 600}),
            path: "sandbox.idle_timeout_s",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"kill_grace_s": 10}),
            new: json!({"kill_grace_s": 20}),
            path: "sandbox.kill_grace_s",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"io": {"logs_tail_bytes": 100}}),
            new: json!({"io": {"logs_tail_bytes": 200}}),
            path: "sandbox.io",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"io": {"progress": {"max_lines": 100}}}),
            new: json!({"io": {"progress": {"max_lines": 200}}}),
            path: "sandbox.io",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"profiles": {"default": {"groups": ["a"]}}}),
            new: json!({"profiles": {"default": {"groups": ["a", "b"]}}}),
            path: "egress.profiles",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"groups": {"model_api": {"hosts": ["a"]}}}),
            new: json!({"groups": {"model_api": {"hosts": ["a", "b"]}}}),
            path: "egress.groups",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"groups": {"model_api": {"hosts": ["a"]}}}),
            new: json!({"groups": {
                "model_api": {"hosts": ["a"]},
                "npm": {"hosts": ["b"]}
            }}),
            path: "egress.groups",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"injection": {}}),
            new: json!({"injection": {"x": {"hosts": ["a"], "kind": "bearer"}}}),
            path: "egress.injection",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"injection": {"x": {"hosts": ["a"]}}}),
            new: json!({"injection": {"x": {"hosts": ["a", "b"]}}}),
            path: "egress.injection",
        },
    ];

    for case in relaxing {
        let relaxes = diff_relaxes(case.resource, &SettingsScope::Global, &case.old, &case.new);
        assert!(
            relaxes.iter().any(|path| path == case.path),
            "expected {} in {relaxes:?} for old={} new={}",
            case.path,
            case.old,
            case.new
        );
    }

    let tightening = vec![
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"high": {"budget": {"nodes": 40}}}),
            new: json!({"high": {"budget": {"nodes": 20}}}),
            path: "levels.high.budget",
        },
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"high": {"budget": {"tokens_by_tier": {"worker": 200}}}}),
            new: json!({"high": {"budget": {"tokens_by_tier": {"worker": 100}}}}),
            path: "levels.high.budget",
        },
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"none": {"daily_tokens": 600}}),
            new: json!({"none": {"daily_tokens": 500}}),
            path: "levels.none.daily_tokens",
        },
        DirectionCase {
            resource: SettingsResource::Levels,
            old: json!({"token_weights": {"input": 0.5}}),
            new: json!({"token_weights": {"input": 1.0}}),
            path: "levels.token_weights",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"actions": {"merge_pr": "approve"}}),
            new: json!({"actions": {"merge_pr": "deny"}}),
            path: "policies.actions",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"unlisted_action": "approve"}),
            new: json!({"unlisted_action": "deny"}),
            path: "policies.unlisted_action",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"paths": {"x": {"globs": ["a"]}}}),
            new: json!({"paths": {"x": {"globs": ["a", "b"]}}}),
            path: "policies.paths",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"paths": {"x": {"disposition": "approve"}}}),
            new: json!({"paths": {"x": {"disposition": "deny"}}}),
            path: "policies.paths",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"paths": {}}),
            new: json!({"paths": {"x": {"globs": ["a"]}}}),
            path: "policies.paths",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"enabled": false}}),
            new: json!({"escalation": {"enabled": true}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {}}}),
            new: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "High"}}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "High"}}}}),
            new: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a", "b"]}, "raise_to": "High"}}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {"x": {"when": {"diff": {"files_gte": 40}}, "raise_to": "High"}}}}),
            new: json!({"escalation": {"rules": {"x": {"when": {"diff": {"files_gte": 30}}, "raise_to": "High"}}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "High"}}}}),
            new: json!({"escalation": {"rules": {"x": {"when": {"paths": ["a"]}, "raise_to": "Max"}}}}),
            path: "policies.escalation",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"approval": {"ttl_s": 200}}),
            new: json!({"approval": {"ttl_s": 100}}),
            path: "policies.approval",
        },
        DirectionCase {
            resource: SettingsResource::Policies,
            old: json!({"patch": {"max_files": 300}}),
            new: json!({"patch": {"max_files": 200}}),
            path: "policies.patch",
        },
        DirectionCase {
            resource: SettingsResource::Guardian,
            old: json!({"triage": {"security_finding": "silent_fix"}}),
            new: json!({"triage": {"security_finding": "escalate"}}),
            path: "guardian.triage",
        },
        DirectionCase {
            resource: SettingsResource::Guardian,
            old: json!({"no_progress": {"same_class_repeats": 3}}),
            new: json!({"no_progress": {"same_class_repeats": 2}}),
            path: "guardian.no_progress",
        },
        DirectionCase {
            resource: SettingsResource::Guardian,
            old: json!({"patrol_interval_s": 3600}),
            new: json!({"patrol_interval_s": 1800}),
            path: "guardian.patrol_interval_s",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"limits": {"mem_mb": 8192}}),
            new: json!({"limits": {"mem_mb": 4096}}),
            path: "sandbox.limits",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"devcontainer": {"mode": "off"}}),
            new: json!({"devcontainer": {"mode": "safe_subset"}}),
            path: "sandbox.devcontainer",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"runtime": "runc"}),
            new: json!({"runtime": "runsc"}),
            path: "sandbox.runtime",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"max_concurrent_sandboxes": 10}),
            new: json!({"max_concurrent_sandboxes": 6}),
            path: "sandbox.max_concurrent_sandboxes",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"prewarm": {"pool_size": 2}}),
            new: json!({"prewarm": {"pool_size": 0}}),
            path: "sandbox.prewarm",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"idle_timeout_s": 600}),
            new: json!({"idle_timeout_s": 300}),
            path: "sandbox.idle_timeout_s",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"kill_grace_s": 20}),
            new: json!({"kill_grace_s": 10}),
            path: "sandbox.kill_grace_s",
        },
        DirectionCase {
            resource: SettingsResource::Sandbox,
            old: json!({"io": {"logs_tail_bytes": 200}}),
            new: json!({"io": {"logs_tail_bytes": 100}}),
            path: "sandbox.io",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"profiles": {"default": {"groups": ["a", "b"]}}}),
            new: json!({"profiles": {"default": {"groups": ["a"]}}}),
            path: "egress.profiles",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"groups": {"model_api": {"hosts": ["a", "b"]}}}),
            new: json!({"groups": {"model_api": {"hosts": ["a"]}}}),
            path: "egress.groups",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"groups": {
                "model_api": {"hosts": ["a"]},
                "npm": {"hosts": ["b"]}
            }}),
            new: json!({"groups": {"model_api": {"hosts": ["a"]}}}),
            path: "egress.groups",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"injection": {"x": {"hosts": ["a"]}}}),
            new: json!({"injection": {}}),
            path: "egress.injection",
        },
        DirectionCase {
            resource: SettingsResource::Egress,
            old: json!({"injection": {"x": {"hosts": ["a", "b"]}}}),
            new: json!({"injection": {"x": {"hosts": ["a"]}}}),
            path: "egress.injection",
        },
    ];

    for case in tightening {
        let relaxes = diff_relaxes(case.resource, &SettingsScope::Global, &case.old, &case.new);
        assert!(
            !relaxes.iter().any(|path| path == case.path),
            "unexpected {} in {relaxes:?} for old={} new={}",
            case.path,
            case.old,
            case.new
        );
    }
}
