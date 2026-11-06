use super::defs::{
    SUBAGENT_STOPPED_MESSAGE, USER_INTERRUPTED_SUBAGENT_MESSAGE, timeout_resume_hint,
};
use super::events::{emit_cancelled, emit_completed, emit_failed};
use crate::turn_loop::types::LoopTurnStopReason;

/// The v2 success shape (`formatForegroundAgentSuccess`).
pub(super) fn format_success(agent_id: &str, profile: &str, summary: &str) -> String {
    format!(
        "agent_id: {agent_id}\nactual_subagent_type: {profile}\nstatus: completed\n\n[summary]\n{summary}"
    )
}

/// The v2 failure shape (`formatForegroundAgentFailure`); the timeout case
/// appends the resume hint.
pub(super) fn format_failure(
    agent_id: &str,
    profile: &str,
    message: &str,
    timed_out: bool,
) -> String {
    let mut text = format!(
        "agent_id: {agent_id}\nactual_subagent_type: {profile}\nstatus: failed\n\nsubagent error: {message}"
    );
    if timed_out {
        text.push('\n');
        text.push_str(&timeout_resume_hint(agent_id));
    }
    text
}

/// Report a finished foreground turn — the fresh-spawn and resume arms share
/// this. An aborted turn is a stop, not a completion: the v2 stopped message
/// leads (a user interrupt emits `subagent.cancelled`, anything else
/// `subagent.failed` — silently dropping the event would strand the
/// `subagent.started` the spawn already sent) and the turn's partial output
/// rides below it. Anything else reports the summary as a completion.
pub(super) fn report_foreground_turn(
    callbacks: &dyn crate::callbacks::HostCallbacks,
    agent_id: &str,
    profile_name: &str,
    turn: &crate::turn_loop::types::TurnResult,
    parent_cancel: Option<&crate::subagent::types::ParentCancel>,
) -> (String, bool) {
    if matches!(turn.stop_reason, LoopTurnStopReason::Aborted) {
        let interrupted = parent_cancel.is_some_and(|signal| signal.triggered());
        let message = if interrupted {
            emit_cancelled(callbacks, agent_id, profile_name);
            USER_INTERRUPTED_SUBAGENT_MESSAGE
        } else {
            emit_failed(callbacks, agent_id, profile_name, SUBAGENT_STOPPED_MESSAGE);
            SUBAGENT_STOPPED_MESSAGE
        };
        let mut content = format_failure(agent_id, profile_name, message, false);
        if let Some(section) = crate::subagent::manager::partial_output_section(turn) {
            content.push_str(&section);
        }
        (content, true)
    } else {
        let summary = crate::subagent::manager::final_assistant_summary(&turn.messages);
        emit_completed(callbacks, agent_id, profile_name, &summary, &turn.usage);
        (format_success(agent_id, profile_name, &summary), false)
    }
}

/// A background task's output for a stopped subagent: the v2 stopped message
/// with the partial output appended — TaskOutput renders this verbatim, which
/// is the "stopped task's partial output comes back marked as stopped"
/// contract. The lifecycle event keeps the bare v2 message.
pub(super) fn stopped_task_output(
    message: &str,
    turn: &crate::turn_loop::types::TurnResult,
) -> String {
    match crate::subagent::manager::partial_output_section(turn) {
        Some(section) => format!("{message}{section}"),
        None => message.to_string(),
    }
}

/// Fire-and-forget subagent lifecycle event to the host (v2
/// `mirrorAgentRun` event surface: `SubagentSpawned` / `SubagentStarted` /
/// `SubagentCompleted` / `SubagentFailed`; the adapter maps them onto the
/// host's event dispatcher).
/// The `agent_call` display a host renders instead of the raw argument string
/// (v2 `AgentTool.resolveExecution` / `AgentSwarmTool.resolveExecution`
/// `display`).
///
/// The two tools that declare one upstream are `Agent` and `AgentSwarm`; v2
/// labels a swarm by its size, which is what tells the reader the call fans out
/// rather than spawning a single child. Other tools declare a display upstream
/// too, but this port never constructed one for any of them — populating those
/// is a separate, wider piece of work and is recorded as such in ROADMAP.
///
/// `None` means "no display", which every host already handles.
pub fn agent_call_display(name: &str, args: &serde_json::Value) -> Option<serde_json::Value> {
    let normalized = name.to_ascii_lowercase().replace('_', "");
    let args = args.as_object()?;
    let description = args
        .get("description")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    match normalized.as_str() {
        "agentswarm" => {
            // v2 counts the RAW argument lengths, not the filtered ones.
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
