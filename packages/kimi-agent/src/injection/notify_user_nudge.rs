//! NotifyUser nudge (upstream #4153, `features/notify/notifyUserNudge.ts`).
//!
//! When the model runs round after round of tool calls without ever sending a
//! `NotifyUser` update, the user has seen no progress; upstream injects a
//! reminder after a threshold of silent tool-call rounds. This module ports
//! the *decision* — the streak scan, the threshold rule, and the rendered
//! reminder — as pure functions over the live history, the same inputs the
//! upstream service reads at its reminder registration point.
//!
//! Two upstream halves are deliberately NOT here:
//!
//! - **The availability gate** (#4054, `notifyUserAvailable`): two inputs —
//!   the flag AND a host that can show an update. On this fork the resolved
//!   answer arrives as `notify_user_enabled` (`PipelineSpec`), and the gate
//!   the nudge flows through is the advertised-table gate in
//!   `tools/tool_policy.rs`: when NotifyUser is unavailable the tool is not
//!   advertised, the streak can never start, and the provider below is not
//!   even registered. Registering an unreachable nudge would be the dead
//!   branch the ledger warned about.
//! - **The telemetry event** (`NotifyUserNudgeShownEvent`): the fork's nudge
//!   rides the injection pipeline, which the turn telemetry already counts;
//!   a separate event would double-report.

use crate::turn_loop::types::LLMMessage;
#[cfg(test)]
use crate::turn_loop::types::ToolCall;

/// The injection variant name (upstream `NOTIFY_USER_NUDGE_VARIANT`).
pub const NOTIFY_USER_NUDGE_VARIANT: &str = "notify_user_nudge";

/// Silent tool-call rounds before the first nudge
/// (`NOTIFY_USER_NUDGE_THRESHOLD`; upstream default 8).
pub const NOTIFY_USER_NUDGE_THRESHOLD: u64 = 8;

/// How many rounds without a `NotifyUser` update the model has run, counted
/// backwards from the live history, plus how many nudges are already inside
/// that silent window (`notifyStreak`).
///
/// The scan stops at the first turn boundary — a new turn resets the audit,
/// exactly like upstream's `startsNewTurn`.
pub fn notify_streak(history: &[LLMMessage]) -> NotifyStreak {
    let mut rounds = 0u64;
    let mut nudges = 0u64;
    for message in history.iter().rev() {
        if starts_new_turn(message) {
            break;
        }
        if is_nudge(message) {
            nudges += 1;
        }
        if !is_tool_call_round(message) {
            continue;
        }
        if message
            .tool_calls
            .iter()
            .any(|call| call.name == crate::tools::core_tool_defs::NOTIFY_USER_TOOL_NAME)
        {
            break;
        }
        rounds += 1;
    }
    NotifyStreak { rounds, nudges }
}

/// The streak's two counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifyStreak {
    /// Silent tool-call rounds since the last `NotifyUser` (or turn start).
    pub rounds: u64,
    /// Nudges already injected inside that window.
    pub nudges: u64,
}

/// Whether the model should be nudged now (`shouldNudgeNotifyUser`): the
/// streak has reached the threshold, and either nothing was nudged yet this
/// window or a full threshold of rounds passed since the last nudge.
pub fn should_nudge_notify_user(streak: u64, rounds_since_last_nudge: Option<u64>) -> bool {
    if streak < NOTIFY_USER_NUDGE_THRESHOLD {
        return false;
    }
    match rounds_since_last_nudge {
        None => true,
        Some(n) => n >= NOTIFY_USER_NUDGE_THRESHOLD,
    }
}

/// The reminder text (`renderNotifyUserNudge`).
pub fn render_notify_user_nudge(count: u64) -> String {
    format!(
        "You have gone through {count} rounds of tool calls without a NotifyUser update — the user has seen no progress from you since. Send one now: a one-line title with what you have concluded so far, and a structured, chat-style message (a well structured paragraph with a few bullet points, under ~1000 characters) with the details and what you will do next, batched with your next tool calls."
    )
}

/// Whether this message starts a new turn (`startsNewTurn`).
///
/// The fork's history carries origins as JSON on the message (the same wire
/// shape the host sends), so the kind is read off `origin.kind` with the same
/// arms upstream switches on. A message with no origin never starts a turn.
fn starts_new_turn(message: &LLMMessage) -> bool {
    let Some(origin) = message
        .origin
        .as_ref()
        .and_then(|o| o.get("kind"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    match origin {
        "user" | "cron_job" | "cron_missed" | "task" | "retry" | "plugin_command" => true,
        "system_trigger" => {
            message
                .origin
                .as_ref()
                .and_then(|o| o.get("name"))
                .and_then(serde_json::Value::as_str)
                != Some("stop_hook")
        }
        "skill_activation" => {
            message
                .origin
                .as_ref()
                .and_then(|o| o.get("trigger"))
                .and_then(serde_json::Value::as_str)
                == Some("user-slash")
        }
        _ => false,
    }
}

/// Whether this message is a previously injected nudge (`isNudge`).
fn is_nudge(message: &LLMMessage) -> bool {
    message
        .origin
        .as_ref()
        .and_then(|o| o.get("kind"))
        .and_then(serde_json::Value::as_str)
        == Some("injection")
        && message
            .origin
            .as_ref()
            .and_then(|o| o.get("variant"))
            .and_then(serde_json::Value::as_str)
            == Some(NOTIFY_USER_NUDGE_VARIANT)
}

fn is_tool_call_round(message: &LLMMessage) -> bool {
    message.role == "assistant" && !message.tool_calls.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: &str) -> LLMMessage {
        LLMMessage {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn assistant_with_calls(calls: &[&str]) -> LLMMessage {
        LLMMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: calls
                .iter()
                .map(|name| ToolCall {
                    id: format!("call-{name}"),
                    name: name.to_string(),
                    arguments: serde_json::Value::Null,
                    extras: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn with_origin(mut m: LLMMessage, origin: serde_json::Value) -> LLMMessage {
        m.origin = Some(origin);
        m
    }

    #[test]
    fn silent_tool_call_rounds_count_until_a_notify_user_breaks_them() {
        let history = vec![
            with_origin(msg("user", "go"), json!({"kind": "user"})),
            assistant_with_calls(&["Read"]),
            assistant_with_calls(&["Bash"]),
            assistant_with_calls(&["Grep"]),
        ];
        let streak = notify_streak(&history);
        assert_eq!(streak.rounds, 3, "every tool-call round counts");
        assert_eq!(streak.nudges, 0);
    }

    #[test]
    fn a_notify_user_call_resets_the_streak() {
        let history = vec![
            with_origin(msg("user", "go"), json!({"kind": "user"})),
            assistant_with_calls(&["Read"]),
            assistant_with_calls(&["NotifyUser"]),
            assistant_with_calls(&["Bash"]),
        ];
        let streak = notify_streak(&history);
        assert_eq!(streak.rounds, 1, "only the rounds after the update count");
    }

    /// A new turn resets the audit: the streak never crosses the boundary.
    #[test]
    fn a_new_turn_resets_the_scan() {
        let history = vec![
            with_origin(msg("user", "first"), json!({"kind": "user"})),
            assistant_with_calls(&["Read"]),
            with_origin(msg("user", "second"), json!({"kind": "user"})),
            assistant_with_calls(&["Bash"]),
        ];
        assert_eq!(notify_streak(&history).rounds, 1);
    }

    /// The stop-hook system trigger does NOT start a turn (upstream's one
    /// special case) — a post-stop-hook continuation keeps counting.
    #[test]
    fn a_stop_hook_system_trigger_does_not_reset() {
        let history = vec![
            with_origin(
                msg("user", "go"),
                json!({"kind": "system_trigger", "name": "stop_hook"}),
            ),
            assistant_with_calls(&["Read"]),
        ];
        assert_eq!(
            notify_streak(&history).rounds,
            1,
            "stop_hook does not reset: the round inside keeps counting"
        );
        let history = vec![
            with_origin(
                msg("user", "go"),
                json!({"kind": "system_trigger", "name": "compact"}),
            ),
            assistant_with_calls(&["Read"]),
        ];
        // The trigger IS the turn boundary: the scan stops there, but the
        // tool-call round after it is inside the new turn, so it counts.
        // (The reset shows up as the pre-trigger history not counting — the
        // first history's `user` message is unreachable in both cases.)
        assert_eq!(notify_streak(&history).rounds, 1);
        assert_eq!(
            notify_streak(&[
                with_origin(msg("user", "go"), json!({"kind": "user"})),
                assistant_with_calls(&["Read"]),
                with_origin(
                    msg("system", "resumed"),
                    json!({"kind": "system_trigger", "name": "compact"}),
                ),
                assistant_with_calls(&["Bash"]),
            ])
            .rounds,
            1,
            "only the post-boundary round counts"
        );
    }

    #[test]
    fn threshold_rule_is_streak_and_cooldown() {
        assert!(!should_nudge_notify_user(7, None), "below the threshold");
        assert!(
            should_nudge_notify_user(8, None),
            "at the threshold, never nudged"
        );
        assert!(
            should_nudge_notify_user(16, Some(8)),
            "a full threshold passed since the last nudge"
        );
        assert!(
            !should_nudge_notify_user(16, Some(7)),
            "still inside the cooldown"
        );
    }

    /// A nudge already inside the window is counted, so a host can compute
    /// the cooldown from `rounds - nudges * threshold` if it wants to; the
    /// rule itself only needs the rounds-since value.
    #[test]
    fn nudges_inside_the_window_are_counted() {
        let history = vec![
            with_origin(msg("user", "go"), json!({"kind": "user"})),
            assistant_with_calls(&["Read"]),
            with_origin(
                msg("system", "[nudge]"),
                json!({"kind": "injection", "variant": NOTIFY_USER_NUDGE_VARIANT}),
            ),
            assistant_with_calls(&["Bash"]),
        ];
        let streak = notify_streak(&history);
        assert_eq!(streak.rounds, 2);
        assert_eq!(streak.nudges, 1);
    }

    #[test]
    fn rendered_nudge_names_the_count() {
        let text = render_notify_user_nudge(8);
        assert!(text.contains("8 rounds"), "{text}");
        assert!(text.contains("NotifyUser"), "{text}");
    }
}
