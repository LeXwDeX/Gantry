//! 阶段推进（engine.md §7.6）：只复用 core 的转移表，不另造边。
//!
//! `waiting_human` / `blocked` 由上报项与判定决定，不在单纯迁移时设置；
//! 这里只给出 `running` 或终止态的 `done`。

use gantry_core::entities::{Stage, StageStatus};
use gantry_core::level::Level;
use gantry_core::machines::delivery::{next_stage_with_abandon, AbandonCause, StageTrigger};
use gantry_core::machines::TransitionError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageTransition {
    pub from: Stage,
    pub to: Stage,
    pub status: StageStatus,
    pub trigger: StageTrigger,
}

/// 按 core 的转移表推进；非法边返回 `TransitionError`。
pub fn advance(
    from: Stage,
    level_effective: Level,
    trigger: StageTrigger,
    abandon_cause: Option<AbandonCause>,
) -> Result<StageTransition, TransitionError> {
    let to = next_stage_with_abandon(from, trigger, level_effective, abandon_cause)?;
    let status = if to.is_terminal() {
        StageStatus::Done
    } else {
        StageStatus::Running
    };
    Ok(StageTransition {
        from,
        to,
        status,
        trigger,
    })
}
