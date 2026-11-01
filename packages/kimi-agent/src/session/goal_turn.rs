//! Goal settlement after one turn, ported from the v2 goal effects
//! (`goalService.ts` `handleTurnEnded` / `settleAbnormalTurn` /
//! `blockIfBudgetReached`).
//!
//! The engine owns the durable goal store, so the turn driver — the session
//! pump and the standalone server alike — folds the finished turn, settles an
//! abnormal stop onto a pause / block, and decides whether the goal owes the
//! model another continuation turn. All three decisions are made from the
//! store's post-write snapshot, never from a host-side copy that may lag the
//! fold.

use serde_json::{Value, json};

use crate::callbacks::HostCallbacks;
use crate::goal::{
    GOAL_INTERRUPTED_PAUSE_REASON, GOAL_PROVIDER_FILTERED_PAUSE_REASON, GOAL_RUNTIME_PAUSE_PREFIX,
    continuation_prompt, goal_budget_block_reason, goal_failure_pause_reason,
};
use crate::rpc::types::StateWriteRequest;
use crate::turn_loop::types::LoopTurnStopReason;

use super::TurnOutcome;

/// What one finished turn leaves behind for the goal.
#[derive(Debug, Default)]
pub(crate) struct GoalAfterTurn {
    /// The continuation turn the goal owes, when it is still active.
    pub continuation: Option<String>,
}

/// The goal-relevant shape of a finished turn.
pub(crate) enum GoalTurnEnd<'a> {
    /// The turn ran to a stop reason.
    Ran {
        stop: &'a LoopTurnStopReason,
        output_tokens: u64,
    },
    /// The turn failed outright (the engine returned an error).
    Failed { error: &'a str },
    /// The turn never ran (cancelled while still queued).
    Skipped,
}

impl<'a> From<&'a Result<TurnOutcome, String>> for GoalTurnEnd<'a> {
    fn from(outcome: &'a Result<TurnOutcome, String>) -> Self {
        match outcome {
            Ok(TurnOutcome::Ran(result)) => GoalTurnEnd::Ran {
                stop: &result.stop_reason,
                output_tokens: result.usage.output_tokens.into(),
            },
            Ok(TurnOutcome::CancelledBeforeStart) => GoalTurnEnd::Skipped,
            Err(error) => GoalTurnEnd::Failed {
                error: error.as_str(),
            },
        }
    }
}

/// Settle one turn against the session's goal store and return what the goal
/// owes next.
pub(crate) async fn settle_goal_after_turn(
    callbacks: &dyn HostCallbacks,
    outcome: GoalTurnEnd<'_>,
) -> GoalAfterTurn {
    let (write, stop) = match outcome {
        GoalTurnEnd::Skipped => return GoalAfterTurn::default(),
        GoalTurnEnd::Ran {
            stop,
            output_tokens,
        } => (
            json!({ "action": "record_usage", "turns": 1, "output_tokens": output_tokens }),
            Some(stop),
        ),
        // v2 `settleAbnormalTurn`: a turn that failed outright has no usage to
        // fold, but the goal must not stay active behind the error.
        GoalTurnEnd::Failed { error } => {
            let reason = goal_failure_pause_reason(super::turn_failure_code(error), Some(error));
            (
                json!({ "action": "lifecycle", "status": "paused", "reason": reason }),
                None,
            )
        }
    };
    let Some(snapshot) = write_goal(callbacks, write).await else {
        return GoalAfterTurn::default();
    };
    let Some(stop) = stop else {
        return GoalAfterTurn::default();
    };
    if snapshot.get("status").and_then(Value::as_str) != Some("active") {
        return GoalAfterTurn::default();
    }
    match stop {
        LoopTurnStopReason::EndTurn
        | LoopTurnStopReason::MaxTokens
        | LoopTurnStopReason::RepeatBreaker => GoalAfterTurn {
            continuation: Some(continuation_prompt(false)),
        },
        // v2 `isMaxStepsTurnFailure`: the step budget ran out mid-goal, so the
        // next turn resumes the same slice with the step-cap framing.
        LoopTurnStopReason::MaxSteps => GoalAfterTurn {
            continuation: Some(continuation_prompt(true)),
        },
        LoopTurnStopReason::Filtered => {
            settle_paused(callbacks, GOAL_PROVIDER_FILTERED_PAUSE_REASON).await
        }
        LoopTurnStopReason::Aborted => {
            settle_paused(callbacks, GOAL_INTERRUPTED_PAUSE_REASON).await
        }
        // The goal stopped being active mid-turn: the pause is already stored
        // (the host paused it, or its budget blocked it).
        LoopTurnStopReason::Paused => GoalAfterTurn::default(),
        LoopTurnStopReason::BudgetLimited => {
            match snapshot
                .get("budget")
                .and_then(|budget| serde_json::from_value(budget.clone()).ok())
                .and_then(|budget| goal_budget_block_reason(&budget))
            {
                Some(reason) => settle_lifecycle(callbacks, "blocked", reason).await,
                None => settle_paused(callbacks, GOAL_RUNTIME_PAUSE_PREFIX).await,
            }
        }
        LoopTurnStopReason::Unknown => settle_paused(callbacks, GOAL_RUNTIME_PAUSE_PREFIX).await,
    }
}

async fn write_goal(callbacks: &dyn HostCallbacks, value: Value) -> Option<Value> {
    let response = callbacks
        .state_write(StateWriteRequest {
            domain: "goal".into(),
            key: "goal".into(),
            value,
            undoable: false,
            turn_id: String::new(),
            tool_call_id: String::new(),
        })
        .await
        .ok()?;
    response
        .value
        .get("goal")
        .filter(|goal| !goal.is_null())
        .cloned()
}

async fn settle_paused(callbacks: &dyn HostCallbacks, reason: &str) -> GoalAfterTurn {
    settle_lifecycle(callbacks, "paused", reason.to_string()).await
}

async fn settle_lifecycle(
    callbacks: &dyn HostCallbacks,
    status: &str,
    reason: String,
) -> GoalAfterTurn {
    let _ = write_goal(
        callbacks,
        json!({ "action": "lifecycle", "status": status, "reason": reason }),
    )
    .await;
    GoalAfterTurn::default()
}
