//! Outbox 意图构造（engine.md §7.12；forge-actions §2.1）。
//!
//! 只做纯构造：调用方给出动作载荷、幂等键的目标与 intent；本模块不写存储。
//! `expected_effects` / `github_ref` / 审批在 M1 的推送、建 PR、合并路径上暂为空。

use gantry_core::canon::{CanonError, IdempotencyKey};
use gantry_core::entities::{OutboxAction, OutboxStatus, Priority};
use gantry_core::forge::ActionPayload;
use gantry_core::ids::{ActionId, DeliveryId, ProjectId};
use gantry_core::time::Timestamp;
use serde_json::Value;

pub struct OutboxIntent<'a> {
    pub id: ActionId,
    pub project_id: ProjectId,
    pub delivery_id: DeliveryId,
    pub payload: ActionPayload,
    /// 幂等键的目标身份，例如分支名、PR 号。
    pub target: &'a str,
    /// 幂等键覆盖的动作意图；必须稳定、可重放。
    pub intent: Value,
    pub epoch: u32,
    pub depends_on: Vec<ActionId>,
    pub priority: Priority,
    pub now: Timestamp,
}

/// 构造一个 `queued` 的 Outbox 动作；不执行、不写库。
pub fn build(request: OutboxIntent<'_>) -> Result<OutboxAction, CanonError> {
    let kind = request.payload.kind();
    let idempotency_key = IdempotencyKey::derive(
        &request.project_id,
        &request.delivery_id,
        kind,
        request.target,
        &request.intent,
        request.epoch,
    )?;
    Ok(OutboxAction {
        id: request.id,
        project_id: request.project_id,
        delivery_id: request.delivery_id,
        idempotency_key,
        kind,
        payload: request.payload,
        requires_approval: None,
        depends_on: request.depends_on,
        priority: request.priority,
        not_before: None,
        epoch: request.epoch,
        status: OutboxStatus::Queued,
        attempts: 0,
        lease_until: None,
        last_error: None,
        expected_effects: Vec::new(),
        github_ref: None,
        created_at: request.now,
        updated_at: request.now,
    })
}
