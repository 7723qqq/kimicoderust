use super::defs::{SUBAGENT_STOPPED_MESSAGE, USER_INTERRUPTED_SUBAGENT_MESSAGE};
use super::events::{emit_cancelled, emit_completed, emit_failed};
use crate::turn_loop::types::LoopTurnStopReason;

pub(super) const REPEAT_BREAKER_NOTICE: &str = "notice: The subagent was stopped by the repeat breaker after issuing the same tool call repeatedly. The summary below is its handoff, not a finished result.";

pub(super) const NO_PARTIAL_OUTPUT: &str =
    "[partial output] none — the subagent wrote no text in this run before it ended.";

pub(super) fn resume_hint(agent_id: &str, prompt: &str) -> String {
    format!(
        "resume_hint: Continue with Agent(resume=\"{agent_id}\", prompt=\"{prompt}\"). \
Use agent_id only; do not set subagent_type. The subagent retains its prior context; \
redo any unfinished tool call if its result was lost."
    )
}

pub(super) fn next_step(reason: &str) -> Option<&'static str> {
    const RESUME_NEXT_STEP: &str = "next_step: Resume to continue where it stopped, or take over the task yourself; if neither works, report the failure to the user.";
    match reason {
        "completed" => None,
        "repeat_breaker" => Some(
            "next_step: The subagent was stuck on one tool call. If you resume it, change the instructions or supply the missing input; otherwise continue the work yourself.",
        ),
        "cancelled" => Some(
            "next_step: The user stopped this subagent. Do not restart it unless the user asks.",
        ),
        "filtered" => Some(
            "next_step: Resuming is unlikely to help; rephrase or split the task before trying again.",
        ),
        "max_tokens" | "max_steps" | "provider_error" | "no_final_message" | "stopped"
        | "timed_out" | "error" => Some(RESUME_NEXT_STEP),
        _ => Some(RESUME_NEXT_STEP),
    }
}

pub(super) fn subagent_stop_reason(
    stop: &LoopTurnStopReason,
    user_cancelled: bool,
) -> &'static str {
    match stop {
        LoopTurnStopReason::EndTurn => "completed",
        LoopTurnStopReason::RepeatBreaker => "repeat_breaker",
        LoopTurnStopReason::MaxSteps => "max_steps",
        LoopTurnStopReason::MaxTokens => "max_tokens",
        LoopTurnStopReason::Filtered => "filtered",
        LoopTurnStopReason::Aborted => {
            if user_cancelled {
                "cancelled"
            } else {
                "stopped"
            }
        }
        _ => "error",
    }
}

pub(super) fn is_success_reason(reason: &str) -> bool {
    matches!(reason, "completed" | "repeat_breaker")
}

pub(super) fn run_failure_reason(message: &str) -> &'static str {
    if message.starts_with(crate::subagent::manager::NO_FINAL_MESSAGE_PREFIX) {
        "no_final_message"
    } else {
        "error"
    }
}

pub(super) fn format_success(agent_id: &str, profile: &str, summary: &str, reason: &str) -> String {
    let mut text = format!(
        "agent_id: {agent_id}\nactual_subagent_type: {profile}\nstatus: completed\nstop_reason: {reason}"
    );
    if reason == "repeat_breaker" {
        text.push('\n');
        text.push_str(REPEAT_BREAKER_NOTICE);
    }
    text.push_str(&format!(
        "\n\n[summary]\n{summary}\n\n{}",
        resume_hint(agent_id, "...")
    ));
    if let Some(next) = next_step(reason) {
        text.push('\n');
        text.push_str(next);
    }
    text
}

pub(super) fn format_failure(
    agent_id: &str,
    profile: &str,
    message: &str,
    reason: &str,
    partial_output: Option<&str>,
) -> String {
    let mut text = format!(
        "agent_id: {agent_id}\nactual_subagent_type: {profile}\nstatus: failed\nstop_reason: {reason}\n\nsubagent error: {message}\n\n{}",
        partial_output.unwrap_or(NO_PARTIAL_OUTPUT)
    );
    if reason != "cancelled" {
        text.push_str("\n\n");
        text.push_str(&resume_hint(agent_id, "continue"));
    }
    if let Some(next) = next_step(reason) {
        text.push('\n');
        text.push_str(next);
    }
    text
}

pub(super) fn report_foreground_turn(
    callbacks: &dyn crate::callbacks::HostCallbacks,
    agent_id: &str,
    profile_name: &str,
    turn: &crate::turn_loop::types::TurnResult,
    parent_cancel: Option<&crate::subagent::types::ParentCancel>,
) -> (String, bool) {
    let interrupted = parent_cancel.is_some_and(|signal| signal.triggered());
    let reason = subagent_stop_reason(&turn.stop_reason, interrupted);
    if is_success_reason(reason) {
        let summary = crate::subagent::manager::final_assistant_summary(&turn.messages);
        emit_completed(callbacks, agent_id, profile_name, &summary, &turn.usage);
        return (
            format_success(agent_id, profile_name, &summary, reason),
            false,
        );
    }
    if reason == "cancelled" {
        emit_cancelled(callbacks, agent_id, profile_name);
    } else {
        emit_failed(callbacks, agent_id, profile_name, SUBAGENT_STOPPED_MESSAGE);
    }
    let message = failure_message(&turn.stop_reason, turn.steps, interrupted);
    let partial = crate::subagent::manager::partial_output_section(turn);
    let content = format_failure(agent_id, profile_name, &message, reason, partial.as_deref());
    (content, true)
}

pub(super) fn failure_message(stop: &LoopTurnStopReason, steps: u32, interrupted: bool) -> String {
    let message = match stop {
        LoopTurnStopReason::Aborted if interrupted => USER_INTERRUPTED_SUBAGENT_MESSAGE.to_string(),
        LoopTurnStopReason::MaxSteps => format!(
            "Subagent hit the per-turn step cap (maxSteps={steps}) before finishing its handoff."
        ),
        LoopTurnStopReason::Filtered => "Provider safety policy blocked the response.".to_string(),
        LoopTurnStopReason::MaxTokens => {
            "Subagent turn failed before completing its final summary: reason=max_tokens"
                .to_string()
        }
        _ => SUBAGENT_STOPPED_MESSAGE.to_string(),
    };
    truncate_reason(&message)
}

pub(super) fn truncate_reason(reason: &str) -> String {
    const REASON_MAX_CHARS: usize = 2000;
    if reason.encode_utf16().count() <= REASON_MAX_CHARS {
        return reason.to_string();
    }
    let mut units = 0usize;
    let mut cut = reason.len();
    for (index, character) in reason.char_indices() {
        units += character.len_utf16();
        if units > REASON_MAX_CHARS {
            cut = index;
            break;
        }
    }
    format!("{}… [truncated]", &reason[..cut])
}

pub(super) fn stopped_task_output(
    message: &str,
    turn: &crate::turn_loop::types::TurnResult,
) -> String {
    match crate::subagent::manager::partial_output_section(turn) {
        Some(section) => format!("{message}{section}"),
        None => message.to_string(),
    }
}

pub fn agent_call_display(name: &str, args: &serde_json::Value) -> Option<serde_json::Value> {
    let normalized = name.to_ascii_lowercase().replace('_', "");
    let args = args.as_object()?;
    let description = args
        .get("description")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    match normalized.as_str() {
        "agentswarm" => {
            let items = args
                .get("items")
                .and_then(|value| value.as_array())
                .map_or(0, |v| v.len());
            let resumes = args
                .get("resume_agent_ids")
                .and_then(|value| value.as_object())
                .map_or(0, |map| map.len());
            let count = items + resumes;
            Some(serde_json::json!({
                "kind": "agent_call",
                "agent_name": format!("swarm ({count} subagents)"),
                "prompt": description,
            }))
        }
        "agent" => Some(serde_json::json!({
            "kind": "agent_call",
            "agent_name": args
                .get("subagent_type")
                .and_then(|value| value.as_str())
                .unwrap_or(crate::tools::agent_tool::DEFAULT_PROFILE_NAME),
            "prompt": args
                .get("prompt")
                .and_then(|value| value.as_str())
                .unwrap_or_default(),
        })),
        _ => None,
    }
}
