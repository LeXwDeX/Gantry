#![forbid(unsafe_code)]

//! 策略与审计（`docs/modules/policy.md`）。
//!
//! M1 的 policy v0 用内置规则（`PolicyBuild::M1`）。本 crate 目前落地规则内核：
//! 规则装载与硬底线、gitignore 风格通配符、路径规则集 `PathRuleSet`、密钥扫描器；
//! `PolicyPort` 的判定、审批与消费在后续切片落地（#89）。

pub mod glob;
pub mod paths;
pub mod rules;
pub mod secrets;

pub use glob::{Glob, GlobError, GlobSet};
pub use paths::{snapshot as path_rules_snapshot, RulePaths};
pub use rules::{FloorViolation, PathRule, PolicyBuild, PolicyRules, SettingsSource};
pub use secrets::{scan_line, scan_text};
