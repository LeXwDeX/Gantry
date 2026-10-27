#![forbid(unsafe_code)]

//! 策略与审计（`docs/modules/policy.md`）。
//!
//! M1 的 policy v0 用内置规则（`PolicyBuild::M1`）。`PolicyEngine` 实现
//! `core::ports::PolicyPort`：动作判定、审批的创建 / 作废 / 消费、补丁规则判定与
//! proposals 判定；规则内核（硬底线、通配符、路径规则集、密钥扫描）在各自模块。
//! `invalidation` 提供事件驱动的失效与过期清扫的单步处理。
//! `signing` 提供验签与待签载荷组装；`ApprovalService`（challenge / grant）在后续
//! 切片落地（#89）。

mod approvals;
mod audit;
pub mod engine;
pub mod glob;
pub mod invalidation;
pub mod patch;
pub mod paths;
pub mod rules;
pub mod secrets;
pub mod signing;

pub use engine::{resolve_action_id, PolicyDeps, PolicyEngine};
pub use glob::{Glob, GlobError, GlobSet};
pub use paths::{snapshot as path_rules_snapshot, RulePaths};
pub use rules::{FloorViolation, PathRule, PolicyBuild, PolicyRules, SettingsSource};
pub use secrets::{scan_line, scan_text};
