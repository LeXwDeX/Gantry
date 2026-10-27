//! 补丁规则判定 `evaluate_patch` 的纯函数部分（policy.md §7.4；worker-protocol
//! §8.3 P1、P5–P9）。
//!
//! 只读 `PatchFacts`、新增行与当次规则快照，不写事件；`policy.denied` 由
//! `PolicyPort` 实现追加。按 §7.4 表的顺序逐条检查、**收集全部命中**，结论取最严
//! （`deny` > `needs_approval` > `allow`）。补丁路径与内容是不可信数据，只比较、
//! 不解释；命中的密钥明文不进入任何返回值。

use gantry_core::entities::{InboxKind, ReasonCode, Severity};
use gantry_core::ports::AddedLineSource;
use gantry_core::ports::{Disposition, PatchPolicyVerdict, ReportHint, RuleId};
use gantry_core::worker::{
    FileMode, FileStatus, PatchFacts, PatchReason, PatchVerdict, PolicyCode, RepoPath,
};

use crate::glob::GlobSet;
use crate::rules::{PolicyBuild, PolicyRules, RULE_SENSITIVE_FILES, RULE_WORKFLOWS};
use crate::secrets::scan_line;

pub const CODE_SIZE: &str = "policy.size";
pub const CODE_SYMLINK: &str = "policy.symlink";
pub const CODE_SUBMODULE: &str = "policy.submodule";
pub const CODE_BINARY: &str = "policy.binary";
pub const CODE_DOTGIT: &str = "policy.dotgit";
pub const CODE_PATH_INVALID: &str = "policy.path_invalid";
pub const CODE_OUTSIDE_ALLOW: &str = "policy.path_outside_allow";
pub const CODE_PATH_DENIED: &str = "policy.path_denied";
pub const CODE_SECRET: &str = "policy.secret";
/// 权限位变更的伪规则 id（OQ-P5 的临时处理）。
pub const RULE_FILE_MODE_CHANGE: &str = "file_mode_change";
/// workflow 补丁上报 `decision_needed` 时的 `payload.kind`（P-48）。
pub const PAYLOAD_MANUAL_ACTION: &str = "manual_action";

/// `policy.path_protected:<rule_id>`。
pub fn path_protected_code(rule_id: &str) -> String {
    format!("policy.path_protected:{rule_id}")
}

/// 对一份能干净应用的补丁做规则判定。
pub fn evaluate(
    rules: &PolicyRules,
    build: PolicyBuild,
    facts: &PatchFacts,
    added: &dyn AddedLineSource,
) -> PatchPolicyVerdict {
    let mut eval = Eval::default();
    let limits = &rules.patch;

    // a：P1 大小。
    let over_size = facts.patch_bytes > limits.max_bytes
        || facts.files.len() as u64 > limits.max_files
        || facts.total_changed_lines > limits.max_changed_lines;
    if over_size {
        eval.deny(CODE_SIZE, None, None);
    }
    for file in &facts.files {
        if file
            .size_after
            .is_some_and(|size| size > limits.max_file_bytes)
        {
            eval.deny(CODE_SIZE, Some(&file.path), None);
        }
    }

    let job_allow = GlobSet::parse(&facts.job_paths.allow);
    let job_deny = GlobSet::parse(&facts.job_paths.deny);

    for file in &facts.files {
        let path = &file.path;
        // b–d：repo 已机械拒绝的事实，这里再断言一遍（P-68，无审批路径）。
        if file.is_symlink
            || file.new_mode == Some(FileMode::Mode120000)
            || file.old_mode == Some(FileMode::Mode120000)
        {
            eval.deny(CODE_SYMLINK, Some(path), None);
        }
        if file.is_submodule
            || file.new_mode == Some(FileMode::Mode160000)
            || file.old_mode == Some(FileMode::Mode160000)
            || last_component(path) == ".gitmodules"
        {
            eval.deny(CODE_SUBMODULE, Some(path), None);
        }
        if file.is_binary {
            eval.deny(CODE_BINARY, Some(path), None);
        }
        // P4 防线：`.git` 的大小写变体与 NTFS 短名 `GIT~1`。
        if path.is_dotgit() || has_component(path, |part| part.eq_ignore_ascii_case("git~1")) {
            eval.deny(CODE_DOTGIT, Some(path), None);
        }

        // e：JobSpec.allow ∩ allow_globs；JobSpec 通配符不合法时失败即关闭。
        let in_allow = rules.allow_globs.matches(path.as_str())
            && job_allow
                .as_ref()
                .is_ok_and(|set| set.matches(path.as_str()));
        if !in_allow {
            eval.deny(CODE_OUTSIDE_ALLOW, Some(path), None);
        }
        // f：JobSpec.deny 与 deny 规则。
        if job_deny
            .as_ref()
            .map_or(true, |set| set.matches(path.as_str()))
        {
            eval.deny(CODE_PATH_DENIED, Some(path), None);
        }
        // 一条路径可能同时命中多条规则：逐条报告，处置各自生效。
        for rule in rules
            .paths
            .iter()
            .filter(|rule| rule.globs.matches(path.as_str()))
        {
            let id = rule.id.as_str();
            match rule.disposition {
                // g0：workflows 任何里程碑都 deny，上报 manual_action 并附补丁（P-48）。
                _ if id == RULE_WORKFLOWS => {
                    eval.deny(&path_protected_code(id), Some(path), Some(id));
                    eval.workflows = true;
                }
                Disposition::Deny if id == RULE_SENSITIVE_FILES => {
                    eval.deny(CODE_SECRET, Some(path), Some(id));
                }
                Disposition::Deny => eval.deny(CODE_PATH_DENIED, Some(path), Some(id)),
                // g：M1 deny 并上报；M2 needs_approval。
                Disposition::Approve => eval.approve(build, id, path),
                Disposition::Auto => {}
            }
        }

        // h：P6 已有文件的权限位变更；新文件 100755 允许。
        let mode_changed = matches!(file.status, FileStatus::Modified | FileStatus::ModeOnly)
            && matches!(
                (file.old_mode, file.new_mode),
                (Some(FileMode::Mode100644), Some(FileMode::Mode100755))
                    | (Some(FileMode::Mode100755), Some(FileMode::Mode100644))
            );
        if mode_changed {
            eval.approve(build, RULE_FILE_MODE_CHANGE, path);
        }

        // i：P9 新增行密钥扫描；读失败按命中处理（失败即关闭）。
        if file.status != FileStatus::Deleted && scan_added(added, path) {
            eval.deny(CODE_SECRET, Some(path), None);
        }
    }

    eval.finish(rules)
}

fn scan_added(added: &dyn AddedLineSource, path: &RepoPath) -> bool {
    for line in added.added_lines(path) {
        match line {
            Ok(line) if scan_line(&line).is_empty() => {}
            Ok(_) | Err(_) => return true,
        }
    }
    false
}

fn last_component(path: &RepoPath) -> &str {
    path.as_str().rsplit('/').next().unwrap_or_default()
}

fn has_component(path: &RepoPath, f: impl Fn(&str) -> bool) -> bool {
    path.as_str().split('/').any(f)
}

#[derive(Default)]
struct Eval {
    reasons: Vec<PatchReason>,
    deny: bool,
    needs_approval: bool,
    workflows: bool,
    matched_approval_rules: Vec<RuleId>,
    /// M1 下被按 deny 执行的 `approve` 规则（第一条用作上报码）。
    protected_denied: Vec<String>,
}

impl Eval {
    fn push(&mut self, code: &str, path: Option<&RepoPath>, rule_id: Option<&str>) {
        let reason = PatchReason {
            code: PolicyCode::new(code),
            path: path.cloned(),
            rule_id: rule_id.map(str::to_owned),
        };
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }

    fn deny(&mut self, code: &str, path: Option<&RepoPath>, rule_id: Option<&str>) {
        self.deny = true;
        self.push(code, path, rule_id);
    }

    fn approve(&mut self, build: PolicyBuild, rule_id: &str, path: &RepoPath) {
        let code = path_protected_code(rule_id);
        if build.patch_approvals {
            self.needs_approval = true;
            self.push(&code, Some(path), Some(rule_id));
            let id = RuleId::new(rule_id);
            if !self.matched_approval_rules.contains(&id) {
                self.matched_approval_rules.push(id);
            }
        } else {
            self.deny(&code, Some(path), Some(rule_id));
            if !self.protected_denied.iter().any(|seen| seen == rule_id) {
                self.protected_denied.push(rule_id.to_owned());
            }
        }
    }

    fn finish(mut self, rules: &PolicyRules) -> PatchPolicyVerdict {
        self.matched_approval_rules.sort();
        let verdict = if self.deny {
            PatchVerdict::Deny
        } else if self.needs_approval {
            PatchVerdict::NeedsApproval
        } else {
            PatchVerdict::Allow
        };
        let report = (verdict == PatchVerdict::Deny).then(|| self.report());
        PatchPolicyVerdict {
            verdict,
            reasons: self.reasons,
            // deny 时不产生审批规格，避免调用方据此发起审批。
            matched_approval_rules: if verdict == PatchVerdict::NeedsApproval {
                self.matched_approval_rules
            } else {
                Vec::new()
            },
            report,
            config_rev: rules.config_rev,
        }
    }

    /// 上报建议：workflow 优先（`decision_needed` / `manual_action`，附补丁，P-48）；
    /// 其余拒绝建议 `escalation`，码取 M1 受保护规则，否则取第一条拒绝原因。
    fn report(&self) -> ReportHint {
        if self.workflows {
            return ReportHint {
                inbox_kind: InboxKind::DecisionNeeded,
                payload_kind: Some(PAYLOAD_MANUAL_ACTION.to_owned()),
                reason_code: reason_code(&path_protected_code(RULE_WORKFLOWS)),
                severity: Severity::Blocker,
                attach_patch: true,
            };
        }
        let code = match self.protected_denied.first() {
            Some(rule_id) => path_protected_code(rule_id),
            None => self
                .reasons
                .first()
                .map(|reason| reason.code.as_str().to_owned())
                .unwrap_or_else(|| CODE_PATH_INVALID.to_owned()),
        };
        ReportHint {
            inbox_kind: InboxKind::Escalation,
            payload_kind: None,
            reason_code: reason_code(&code),
            severity: Severity::Blocker,
            attach_patch: false,
        }
    }
}

/// 规则 id 来自配置（M2 可自定义）；拼出的码不合 `ReasonCode` 词法时退回不带后缀的
/// `policy.path_protected`，不让配置文本让上报失败。
fn reason_code(code: &str) -> ReasonCode {
    ReasonCode::parse(code)
        .or_else(|_| ReasonCode::parse("policy.path_protected"))
        .expect("literal reason code is valid")
}
