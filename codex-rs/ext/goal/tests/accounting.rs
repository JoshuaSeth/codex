#![allow(dead_code)]

#[path = "../src/accounting.rs"]
mod accounting;

use accounting::BlockedGoalDecision;
use accounting::BlockedGoalReceipt;
use accounting::GoalAccountingState;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;

#[test]
fn goal_accounting_uses_turn_start_baseline_for_exact_deltas() {
    let state = GoalAccountingState::default();
    state.start_turn(
        "turn-1",
        ModeKind::Default,
        &token_usage(
            /*input_tokens*/ 100, /*cached_input_tokens*/ 10, /*output_tokens*/ 30,
            /*reasoning_output_tokens*/ 5, /*total_tokens*/ 135,
        ),
    );

    let recorded = state
        .record_token_usage(
            "turn-1",
            &token_usage(
                /*input_tokens*/ 120, /*cached_input_tokens*/ 14,
                /*output_tokens*/ 42, /*reasoning_output_tokens*/ 8,
                /*total_tokens*/ 162,
            ),
        )
        .expect("token delta should be recorded");

    assert_eq!(28, recorded.turn_delta);
    assert_eq!(28, recorded.thread_unflushed_delta);
}

#[test]
fn goal_accounting_ignores_plan_mode_turns() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Plan, &TokenUsage::default());

    let recorded = state.record_token_usage(
        "turn-1",
        &token_usage(
            /*input_tokens*/ 20, /*cached_input_tokens*/ 5, /*output_tokens*/ 8,
            /*reasoning_output_tokens*/ 2, /*total_tokens*/ 30,
        ),
    );

    assert_eq!(None, recorded);
}

#[test]
fn goal_accounting_counts_each_failed_goal_turn_once() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");

    assert_eq!(Some(1), state.mark_turn_error("turn-1"));
    assert_eq!(Some(1), state.mark_turn_error("turn-1"));
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    assert_eq!(Some(2), state.mark_turn_error("turn-2"));
    state.finish_turn("turn-2");
    assert_eq!(2, state.consecutive_turn_errors());
}

#[test]
fn successful_goal_turn_resets_consecutive_turn_errors() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");
    assert_eq!(Some(1), state.mark_turn_error("turn-1"));
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    state.finish_turn("turn-2");

    assert_eq!(0, state.consecutive_turn_errors());
}

#[test]
fn blocked_goal_requires_three_distinct_consecutive_turns() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");

    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-1",
            "goal-1",
            &receipt("condition-a", "evidence-1")
        )
    );
    assert_eq!(
        Some(BlockedGoalDecision::AlreadyRecorded { blocked_turns: 1 }),
        state.record_blocked_goal_attempt(
            "turn-1",
            "goal-1",
            &receipt("condition-a", "evidence-2")
        )
    );
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 2,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-2",
            "goal-1",
            &receipt("condition-a", "evidence-2")
        )
    );
    state.finish_turn("turn-2");

    state.start_turn("turn-3", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-3", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Allow),
        state.record_blocked_goal_attempt(
            "turn-3",
            "goal-1",
            &receipt("condition-a", "evidence-3")
        )
    );
}

#[test]
fn successful_intervening_goal_turn_resets_blocked_audit() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-1",
            "goal-1",
            &receipt("condition-a", "evidence-1")
        )
    );
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    state.finish_turn("turn-2");

    state.start_turn("turn-3", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-3", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-3",
            "goal-1",
            &receipt("condition-a", "evidence-3")
        )
    );
}

#[test]
fn technical_error_turn_does_not_reset_blocked_audit() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-1",
            "goal-1",
            &receipt("condition-a", "evidence-1")
        )
    );
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    assert_eq!(Some(1), state.mark_turn_error("turn-2"));
    state.finish_turn("turn-2");

    state.start_turn("turn-3", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-3", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 2,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-3",
            "goal-1",
            &receipt("condition-a", "evidence-3")
        )
    );
}

#[test]
fn external_goal_update_resets_blocked_audit() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-1",
            "goal-1",
            &receipt("condition-a", "evidence-1")
        )
    );
    assert_eq!(
        Some("turn-1".to_string()),
        state.mark_current_turn_external_goal_active("goal-1")
    );
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-2",
            "goal-1",
            &receipt("condition-a", "evidence-2")
        )
    );
}

#[test]
fn changed_blocker_fingerprint_restarts_blocked_audit() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-1",
            "goal-1",
            &receipt("condition-a", "evidence-1")
        )
    );
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: true,
        }),
        state.record_blocked_goal_attempt(
            "turn-2",
            "goal-1",
            &receipt("condition-b", "evidence-2")
        )
    );
}

#[test]
fn repeated_evidence_does_not_advance_blocked_audit() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-1", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::Continue {
            blocked_turns: 1,
            audit_restarted: false,
        }),
        state.record_blocked_goal_attempt(
            "turn-1",
            "goal-1",
            &receipt("condition-a", "evidence-1")
        )
    );
    state.finish_turn("turn-1");

    state.start_turn("turn-2", ModeKind::Default, &TokenUsage::default());
    state.mark_turn_goal_active("turn-2", "goal-1");
    assert_eq!(
        Some(BlockedGoalDecision::StaleEvidence { blocked_turns: 1 }),
        state.record_blocked_goal_attempt(
            "turn-2",
            "goal-1",
            &receipt("condition-a", "evidence-1")
        )
    );
}

fn receipt(condition_fingerprint: &str, evidence_fingerprint: &str) -> BlockedGoalReceipt {
    BlockedGoalReceipt {
        condition_fingerprint: condition_fingerprint.to_string(),
        evidence_fingerprint: evidence_fingerprint.to_string(),
    }
}

fn token_usage(
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    total_tokens: i64,
) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
    }
}
