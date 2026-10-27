//! 一次判定所用的不可变规则快照（policy.md §7.1、§7.2）。
//!
//! 规则只来自 `policies.*`：M1（`SettingsSource::Builtin`）取 `core` 的出厂默认，
//! M2 取经 `core` merge / clamp 后的生效值。两者走同一个装载入口
//! [`PolicyRules::load`]，硬底线在装载时再断言一次，违反则拒绝整份配置。

use std::collections::BTreeMap;
use std::time::Duration;

use gantry_core::entities::{PolicyActionId, Seq};
use gantry_core::ports::{Disposition, RuleId};
use gantry_core::settings::{
    builtin_resolved, Disposition as SettingsDisposition, PatchLimits, PathDisposition, Policies,
    ResolvedSettings, SettingsResource, UnlistedDisposition,
};

use crate::glob::{GlobError, GlobSet};

/// 规则 id：`workflows`（P-48，任何里程碑都 deny）。
pub const RULE_WORKFLOWS: &str = "workflows";
/// 规则 id：`sensitive_files`（I8，deny 不可降低）。
pub const RULE_SENSITIVE_FILES: &str = "sensitive_files";
/// `approval.ttl_s` 的上限（config.md §5.4）。
pub const MAX_APPROVAL_TTL_S: u64 = 604_800;

/// M1 → M2 的唯一开关（policy.md §7.9）：同一份判定代码，只由这两个字段区分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyBuild {
    /// `approve` 路径规则是否进入补丁审批；M1 为 `false`，一律按 deny。
    pub patch_approvals: bool,
    pub settings: SettingsSource,
}

impl PolicyBuild {
    pub const M1: Self = Self {
        patch_approvals: false,
        settings: SettingsSource::Builtin,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSource {
    /// 内置常量，不读 `policies` 设置，`applied_rev` 恒为 0。
    Builtin,
    /// `policies.*` 生效配置（M2）。
    Store,
}

/// 违反硬底线或语法不合法的配置；整份拒绝装载。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FloorViolation {
    #[error("action {0} must not be auto")]
    DangerousActionAuto(PolicyActionId),
    #[error("path rule {0} must exist with disposition deny")]
    PathRuleMustDeny(&'static str),
    #[error("approval.ttl_s {0} is out of range")]
    ApprovalTtl(u64),
    #[error("glob {pattern:?} in {rule} is invalid: {error}")]
    Glob {
        rule: String,
        pattern: String,
        error: GlobError,
    },
}

#[derive(Clone, Debug)]
pub struct PathRule {
    pub id: RuleId,
    pub globs: GlobSet,
    pub disposition: Disposition,
}

/// 一个作用域的生效规则，装载后不可变。
#[derive(Clone, Debug)]
pub struct PolicyRules {
    pub config_rev: Seq,
    pub actions: BTreeMap<PolicyActionId, Disposition>,
    pub unlisted_action: Disposition,
    pub allow_globs: GlobSet,
    /// 按规则 id 排序，保证命中结果稳定。
    pub paths: Vec<PathRule>,
    pub approval_ttl: Duration,
    pub patch: PatchLimits,
}

impl PolicyRules {
    /// M1 的内置规则：config.md §5 的出厂默认，`config_rev = 0`。
    pub fn builtin() -> Self {
        let ResolvedSettings::Policies(policies) = builtin_resolved(SettingsResource::Policies)
        else {
            unreachable!("policies defaults resolve to Policies");
        };
        Self::load(&policies, Seq::ZERO).expect("built-in policies satisfy the hard floor")
    }

    /// 装载一份生效配置并断言硬底线（policy.md §7.2）。
    pub fn load(policies: &Policies, config_rev: Seq) -> Result<Self, FloorViolation> {
        let mut actions = BTreeMap::new();
        for (id, disposition) in &policies.actions {
            let disposition = from_settings(*disposition);
            if id.is_dangerous() && disposition == Disposition::Auto {
                return Err(FloorViolation::DangerousActionAuto(*id));
            }
            actions.insert(*id, disposition);
        }

        let mut paths = Vec::new();
        for (id, rule) in &policies.paths {
            let globs =
                GlobSet::parse(&rule.globs).map_err(|error| glob_error(id, &rule.globs, error))?;
            paths.push(PathRule {
                id: RuleId::new(id.clone()),
                globs,
                disposition: match rule.disposition {
                    PathDisposition::Approve => Disposition::Approve,
                    PathDisposition::Deny => Disposition::Deny,
                },
            });
        }
        for floor in [RULE_WORKFLOWS, RULE_SENSITIVE_FILES] {
            let denies = paths
                .iter()
                .any(|rule| rule.id.as_str() == floor && rule.disposition == Disposition::Deny);
            if !denies {
                return Err(FloorViolation::PathRuleMustDeny(floor));
            }
        }

        let ttl_s = policies.approval.ttl_s;
        if !(60..=MAX_APPROVAL_TTL_S).contains(&ttl_s) {
            return Err(FloorViolation::ApprovalTtl(ttl_s));
        }

        let allow_globs = GlobSet::parse(&policies.allow_globs)
            .map_err(|error| glob_error("allow_globs", &policies.allow_globs, error))?;

        Ok(Self {
            config_rev,
            actions,
            unlisted_action: match policies.unlisted_action {
                UnlistedDisposition::Approve => Disposition::Approve,
                UnlistedDisposition::Deny => Disposition::Deny,
            },
            allow_globs,
            paths,
            approval_ttl: Duration::from_secs(ttl_s),
            patch: policies.patch.clone(),
        })
    }

    /// 动作 id 的处置；配置缺失时取 `unlisted_action`。
    pub fn disposition(&self, id: PolicyActionId) -> Disposition {
        self.actions
            .get(&id)
            .copied()
            .unwrap_or(self.unlisted_action)
    }
}

fn from_settings(value: SettingsDisposition) -> Disposition {
    match value {
        SettingsDisposition::Auto => Disposition::Auto,
        SettingsDisposition::Approve => Disposition::Approve,
        SettingsDisposition::Deny => Disposition::Deny,
    }
}

fn glob_error(rule: &str, patterns: &[String], error: GlobError) -> FloorViolation {
    let pattern = patterns
        .iter()
        .find(|pattern| crate::glob::Glob::parse(pattern).is_err())
        .cloned()
        .unwrap_or_default();
    FloorViolation::Glob {
        rule: rule.to_owned(),
        pattern,
        error,
    }
}
