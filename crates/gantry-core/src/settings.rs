//! 设置（settings）：八个 resource 的类型、默认值、合并、收紧 clamp、
//! 放宽差分、权限判定与通用校验 V01–V03。
//!
//! 规则表见 [`rules`]；它是 clamp / 权限 / 放宽的唯一来源。
//! V04–V22 的真实模块校验不在 M0 范围：相关类型和字段范围已按 config.md
//! 保留，但校验器不产出这些错误码。

#![forbid(unsafe_code)]

pub mod authority;
pub mod clamp;
pub mod defaults;
pub mod merge;
pub mod relax;
pub mod resource;
pub mod rules;
pub mod validate;

pub use authority::{
    is_approver_only, required_authority, required_authority_for_change, Authority,
    SecretWriteIntent, SecretWriteKind,
};
pub use clamp::{effective, effective_resolved, Clamp, Effective, KeyPath};
pub use defaults::{builtin_defaults, builtin_resolved};
pub use merge::{merge_patch, reject_null_storage};
pub use relax::diff_relaxes;
pub use resource::*;
pub use rules::{
    approver_only, bootstrap_writable, key_rule, project_override, KeyRule, ProjectOverride,
    RelaxCmp,
};
pub use validate::{
    find_secret_refs, has_direction, split_secret_values, validate, validate_and_parse,
    AdapterCapability, ModelCapability, NoContext, SecretWriteValue, SecretWrites, Severity,
    ValidationContext, ValidationIssue, ValidationReport,
};

/// `schemas/config/` 的版本；升级规则见 config.md OQ-20 / core.md OQ-13。
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// 文档 `core.md` §6.7 使用的短名称；实际类型复用 `entities`，避免重复定义。
pub type Resource = crate::entities::SettingsResource;
pub type Scope = crate::entities::SettingsScope;
pub use crate::entities::{SettingsResource, SettingsScope};

/// 生成某个 resource 的写形状 JSON Schema（含 `x-gantry-secret`）。
pub fn resource_schema(resource: crate::entities::SettingsResource) -> schemars::Schema {
    match resource {
        crate::entities::SettingsResource::Models => schemars::schema_for!(ModelsPatch),
        crate::entities::SettingsResource::Levels => schemars::schema_for!(LevelsPatch),
        crate::entities::SettingsResource::Policies => schemars::schema_for!(PoliciesPatch),
        crate::entities::SettingsResource::Guardian => schemars::schema_for!(GuardianPatch),
        crate::entities::SettingsResource::Sandbox => schemars::schema_for!(SandboxPatch),
        crate::entities::SettingsResource::Egress => schemars::schema_for!(EgressPatch),
        crate::entities::SettingsResource::Notify => schemars::schema_for!(NotifyPatch),
        crate::entities::SettingsResource::Github => schemars::schema_for!(GithubPatch),
    }
}
