//! 每个动作 `kind` 的 `target` / `intent` 推导（forge-actions §3.2；core OQ-9）。
//!
//! 幂等键本身由 `core::canon::IdempotencyKey::derive` 计算；本模块只负责把 payload
//! 投影成"稳定目标 + 想要的最终结果"。forge 入队与引擎的 `expected_actions`（engine.md
//! §7.14.4）共用这一份推导，不各写一份（#70）。
//! intent 只含 §3.2 表中列出的字段，不含时间戳、传输方式等易变字段。

use crate::entities::ForgeActionKind;
use crate::forge::ActionPayload;
use serde::Serialize;
use serde_json::{json, Map, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetIntent {
    pub target: String,
    pub intent: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// 该动作的 intent 推导尚未实现（M2 起的动作，forge.md §1.1）。
    #[error("{0:?} is not supported until M2 (forge.md §1.1)")]
    NotInM1(ForgeActionKind),
    #[error("serialize intent: {0}")]
    Serialize(String),
}

/// 推导 `(target, intent)`。只覆盖 M1 的 5 个动作。
pub fn derive_target_intent(payload: &ActionPayload) -> Result<TargetIntent, KeyError> {
    match payload {
        ActionPayload::PushBranch(p) => Ok(TargetIntent {
            target: format!("branch:{}", p.branch),
            intent: json!({
                "local_head_oid": val(&p.local_head_oid)?,
                "expected_remote_oid": val(&p.expected_remote_oid)?,
                "force": p.force,
            }),
        }),
        ActionPayload::CreatePr(p) => Ok(TargetIntent {
            target: format!("pr:{}->{}", p.head, p.base),
            intent: json!({
                "base": p.base,
                "head": p.head,
                "title": p.title,
                "body": p.body,
                "draft": p.draft,
                "closes": p.closes,
            }),
        }),
        ActionPayload::UpdatePr(p) => {
            // §3.2："仅包含要改的字段"。
            let mut intent = Map::new();
            if let Some(title) = &p.title {
                intent.insert("title".into(), json!(title));
            }
            if let Some(body) = &p.body {
                intent.insert("body".into(), json!(body));
            }
            if let Some(base) = &p.base {
                intent.insert("base".into(), json!(base));
            }
            if let Some(state) = &p.state {
                intent.insert("state".into(), val(state)?);
            }
            Ok(TargetIntent {
                target: format!("pr:{}", p.pull_number),
                intent: Value::Object(intent),
            })
        }
        ActionPayload::CloseIssue(p) => Ok(TargetIntent {
            target: format!("issue:{}:close", p.number),
            intent: json!({
                "state_reason": val(&p.state_reason)?,
                "require_merged_pr": p.require_merged_pr,
            }),
        }),
        ActionPayload::MergePr(p) => Ok(TargetIntent {
            target: format!("pr:{}:merge", p.pull_number),
            intent: json!({
                "expected_head_sha": val(&p.expected_head_sha)?,
                "merge_method": val(&p.merge_method)?,
                "mode": val(&p.mode)?,
                "commit_title": p.commit_title,
                "commit_message": p.commit_message,
            }),
        }),
        other => Err(KeyError::NotInM1(other.kind())),
    }
}

fn val<T: Serialize>(value: &T) -> Result<Value, KeyError> {
    serde_json::to_value(value).map_err(|error| KeyError::Serialize(error.to_string()))
}
