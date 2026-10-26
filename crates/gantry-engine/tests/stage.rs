//! Low 阶段推进（engine.md §7.6）。

use gantry_core::entities::{Stage, StageStatus};
use gantry_core::level::Level;
use gantry_core::machines::delivery::{AbandonCause, StageTrigger};
use gantry_engine::stage::advance;

fn low(from: Stage, trigger: StageTrigger) -> gantry_core::machines::TransitionError {
    advance(from, Level::Low, trigger, None).expect_err("edge must be rejected")
}

#[test]
fn low_main_path_matches_the_stage_table() {
    for (from, trigger, to) in [
        (
            Stage::Spec,
            StageTrigger::AdmissionSkipped,
            Stage::Implement,
        ),
        (
            Stage::Implement,
            StageTrigger::PatchValidated,
            Stage::Propose,
        ),
        (Stage::Propose, StageTrigger::PrCreated, Stage::Review),
        (
            Stage::Review,
            StageTrigger::ReviewAcceptCiGreen,
            Stage::Merge,
        ),
        (
            Stage::Merge,
            StageTrigger::MergedAndIssuesClosed,
            Stage::Closed,
        ),
    ] {
        let transition = advance(from, Level::Low, trigger, None).expect("legal edge");
        assert_eq!(transition.from, from);
        assert_eq!(transition.to, to);
        assert_eq!(transition.trigger, trigger);
        let expected_status = if to.is_terminal() {
            StageStatus::Done
        } else {
            StageStatus::Running
        };
        assert_eq!(transition.status, expected_status);
    }
}

#[test]
fn low_has_no_plan_or_admission_edges() {
    low(Stage::Spec, StageTrigger::AdmissionAccept);
    low(Stage::Spec, StageTrigger::PlanProduced);
    low(Stage::Plan, StageTrigger::PlanProduced);
}

#[test]
fn low_review_can_return_to_implement_when_the_user_requests_a_fix() {
    let transition = advance(Stage::Review, Level::Low, StageTrigger::ReviewRevise, None)
        .expect("review -> implement is allowed for every level (T9)");
    assert_eq!(transition.to, Stage::Implement);
}

#[test]
fn terminal_stages_have_no_outgoing_edges() {
    for from in [Stage::Closed, Stage::Abandoned] {
        for trigger in [
            StageTrigger::PatchValidated,
            StageTrigger::PrCreated,
            StageTrigger::ReviewAcceptCiGreen,
            StageTrigger::MergedAndIssuesClosed,
        ] {
            low(from, trigger);
        }
    }
}

#[test]
fn user_abandon_needs_no_cause_but_reopen_does() {
    let transition = advance(
        Stage::Implement,
        Level::Low,
        StageTrigger::UserAbandoned,
        None,
    )
    .expect("user abandon");
    assert_eq!(transition.to, Stage::Abandoned);
    assert_eq!(transition.status, StageStatus::Done);

    let missing = advance(Stage::Abandoned, Level::Low, StageTrigger::PrReopened, None);
    assert!(missing.is_err());
}

#[test]
fn pr_closed_and_reopened_keep_their_cause_requirement() {
    let closed = advance(
        Stage::Merge,
        Level::Low,
        StageTrigger::PrClosedUnmerged,
        None,
    )
    .expect("pr closed");
    assert_eq!(closed.to, Stage::Abandoned);

    let reopened = advance(
        Stage::Abandoned,
        Level::Low,
        StageTrigger::PrReopened,
        Some(AbandonCause::PrClosedUnmerged),
    )
    .expect("reopen after pr_closed");
    assert_eq!(reopened.to, Stage::Review);

    let user_abandoned = advance(
        Stage::Abandoned,
        Level::Low,
        StageTrigger::PrReopened,
        Some(AbandonCause::UserAbandoned),
    );
    assert!(user_abandoned.is_err());
}
