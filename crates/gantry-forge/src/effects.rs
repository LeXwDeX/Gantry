//! 自身事件归因的反查索引 `forge_effect_index`（forge.md §7.3、§7.6）。
//!
//! 执行器在 write-ahead 记录预期效果（`record_effects`）的**同一事务**里按效果元组写索引；
//! 动作成功且拿到 PR 时再写 `pr` 绑定行。归一化按同样的规则拼键查找，不扫 Outbox 全表。

use std::collections::BTreeMap;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::ExpectedEffect;
use gantry_core::ids::{ActionId, DeliveryId};
use gantry_store::{ModuleConn, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EFFECT_TABLE: &str = "forge_effect_index";

/// 一行：效果归属的动作与交付单元。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectRow {
    pub action_id: ActionId,
    pub delivery_id: DeliveryId,
}

/// 预期效果元组的键：`effect|{repo}|{event}|{match 的规范 JSON}`（`match` 是有序映射）。
pub fn effect_key(repo: &RepoSlug, event: &str, matches: &BTreeMap<String, Value>) -> String {
    format!(
        "effect|{repo}|{event}|{}",
        serde_json::to_string(matches).unwrap_or_default()
    )
}

/// 分支最近一次**成功**的 `push_branch`：`push|{repo}|{branch}`（head_diverged 的依据，§7.7）。
pub fn push_key(repo: &RepoSlug, branch: &str) -> String {
    format!("push|{repo}|{branch}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastPush {
    pub action_id: ActionId,
    pub delivery_id: DeliveryId,
    pub head_sha: GitOid,
}

/// Gantry 创建的 PR 的绑定行：`pr|{repo}#{n}`。
pub fn pr_key(repo: &RepoSlug, number: u64) -> String {
    format!("pr|{repo}#{number}")
}

pub fn put_effects(
    conn: &mut ModuleConn<'_>,
    repo: &RepoSlug,
    effects: &[ExpectedEffect],
    row: &EffectRow,
) -> Result<(), StoreError> {
    for effect in effects {
        conn.put(
            EFFECT_TABLE,
            &effect_key(repo, &effect.event, &effect.match_),
            row,
        )?;
    }
    Ok(())
}
