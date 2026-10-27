//! `RaiseRequest` 的服务端校验（notify.md §6.1、§8；data-model「上报项的
//! reason_code 与选项词表」，P-63、P-79）。
//!
//! payload 的字段形状已由 `InboxPayload` 的类型保证；这里只校验类型表达不了的
//! 部分：`kind` 与 payload 一致、`dedup_key` 非空，以及 `options[]` 只取固定
//! 词表里该类上报项允许的选项 id。

use std::collections::BTreeSet;

use gantry_core::entities::{DecisionKind, InboxOption, InboxPayload};
use gantry_core::ports::{validate_raise_kind, RaiseError, RaiseRequest};

/// `escalation` 只有 `level_cap_exceeded` 带选项；其余 `reason_code` 不带。
const LEVEL_CAP_EXCEEDED: &str = "level_cap_exceeded";
const ESCALATION_LEVEL_CAP: &[&str] = &["upgrade", "narrow", "abandon"];
const DECISION_GATE_REJECT: &[&str] = &["replan", "abandon"];
const DECISION_MANUAL_ACTION: &[&str] = &["done", "abandon"];
const DECISION_REVIEW_DISPUTE: &[&str] = &["fix", "keep"];
const DECISION_OTHER: &[&str] = &["continue", "abandon"];
const BUDGET_EXHAUSTED: &[&str] = &["raise_cap", "abandon"];

pub(crate) fn raise_request(req: &RaiseRequest) -> Result<(), RaiseError> {
    if !validate_raise_kind(&req.payload, req.kind.clone()) {
        return Err(invalid(
            "/payload",
            "payload kind does not match the inbox kind",
        ));
    }
    if req.dedup_key.as_str().is_empty() {
        return Err(invalid("/dedup_key", "dedup_key must not be empty"));
    }
    match &req.payload {
        InboxPayload::Escalation {
            reason_code,
            options,
            ..
        } => match options {
            None => Ok(()),
            Some(_) if reason_code.as_str() != LEVEL_CAP_EXCEEDED => Err(invalid(
                "/payload/options",
                "this escalation reason_code carries no options",
            )),
            Some(options) => check_options(options, ESCALATION_LEVEL_CAP),
        },
        InboxPayload::DecisionNeeded { kind, options, .. } => {
            let allowed = match kind {
                DecisionKind::GateReject => DECISION_GATE_REJECT,
                DecisionKind::ManualAction => DECISION_MANUAL_ACTION,
                DecisionKind::ReviewDispute => DECISION_REVIEW_DISPUTE,
                DecisionKind::Other => DECISION_OTHER,
            };
            check_options(options, allowed)
        }
        InboxPayload::BudgetExhausted { options, .. } => check_options(options, BUDGET_EXHAUSTED),
        InboxPayload::ApprovalNeeded { .. }
        | InboxPayload::SpecNeeded { .. }
        | InboxPayload::SecurityFinding { .. } => Ok(()),
    }
}

fn check_options(options: &[InboxOption], allowed: &[&str]) -> Result<(), RaiseError> {
    if options.is_empty() {
        return Err(invalid("/payload/options", "options must not be empty"));
    }
    let mut seen = BTreeSet::new();
    for (index, option) in options.iter().enumerate() {
        if !allowed.contains(&option.id.as_str()) {
            return Err(invalid(
                &format!("/payload/options/{index}/id"),
                "option id is not in the vocabulary for this item",
            ));
        }
        if !seen.insert(option.id.as_str()) {
            return Err(invalid(
                &format!("/payload/options/{index}/id"),
                "duplicate option id",
            ));
        }
        if option.label.trim().is_empty() {
            return Err(invalid(
                &format!("/payload/options/{index}/label"),
                "option label must not be empty",
            ));
        }
    }
    Ok(())
}

fn invalid(pointer: &str, message: &str) -> RaiseError {
    RaiseError::InvalidPayload {
        pointer: pointer.to_owned(),
        message: message.to_owned(),
    }
}
