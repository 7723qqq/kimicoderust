use serde_json::{Value, json};

use crate::callbacks::HostCallbacks;
use crate::goal::{
    GOAL_INTERRUPTED_PAUSE_REASON, GOAL_PROVIDER_FILTERED_PAUSE_REASON, GOAL_RUNTIME_PAUSE_PREFIX,
    continuation_prompt, goal_budget_block_reason, goal_failure_pause_reason,
};
use crate::rpc::types::StateWriteRequest;
use crate::turn_loop::types::LoopTurnStopReason;

use super::TurnOutcome;

#[derive(Debug, Default)]
pub(crate) struct GoalAfterTurn {
    pub continuation: Option<String>,
}

pub(crate) enum GoalTurnEnd<'a> {
    Ran {
        stop: &'a LoopTurnStopReason,
        output_tokens: u64,
    },

    Failed {
        error: &'a str,
    },

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

        LoopTurnStopReason::MaxSteps => GoalAfterTurn {
            continuation: Some(continuation_prompt(true)),
        },
        LoopTurnStopReason::Filtered => {
            settle_paused(callbacks, GOAL_PROVIDER_FILTERED_PAUSE_REASON).await
        }
        LoopTurnStopReason::Aborted => {
            settle_paused(callbacks, GOAL_INTERRUPTED_PAUSE_REASON).await
        }

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
    let submitted = value.clone();
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
    let goal = response
        .value
        .get("goal")
        .filter(|goal| goal.is_object())?
        .clone();
    emit_goal_fold(callbacks, &submitted, &goal);
    Some(goal)
}

fn emit_goal_fold(callbacks: &dyn HostCallbacks, submitted: &Value, goal: &Value) {
    let agent_id = crate::callbacks::MAIN_AGENT_ID;
    let mut record = json!({
        "type": "goal.update",
        "agent_id": agent_id,
    });
    match submitted.get("action").and_then(Value::as_str) {
        Some("record_usage") => {
            let mut seen = false;
            for key in ["turnsUsed", "tokensUsed"] {
                if let Some(total) = goal.get(key).and_then(Value::as_u64) {
                    record[key] = json!(total);
                    seen = true;
                }
            }
            if !seen {
                return;
            }
            callbacks.emit_event(record);
            if let Some(blocked) =
                crate::session::agent_records::goal_blocked_record(agent_id, goal)
            {
                callbacks.emit_event(blocked);
            }
            return;
        }
        Some("lifecycle") => {
            let Some(status) = submitted.get("status").and_then(Value::as_str) else {
                return;
            };
            record["status"] = json!(status);
            if let Some(reason) = submitted.get("reason").and_then(Value::as_str) {
                record["reason"] = json!(reason);
            }
            if let Some(wall_clock_ms) = goal.get("wallClockMs").and_then(Value::as_u64) {
                record["wallClockMs"] = json!(wall_clock_ms);
            }
            if status == "active" {
                record["wallClockResumedAt"] = json!(chrono::Utc::now().timestamp_millis() as u64);
            }
        }
        _ => return,
    }
    callbacks.emit_event(record);
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
