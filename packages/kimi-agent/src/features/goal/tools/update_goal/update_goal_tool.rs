use crate::callbacks::HostCallbacks;
use crate::turn_loop::types::{ExecutableToolResult, LLM, LLMMessage, ToolCall};

pub(crate) async fn evaluate_goal_completion(
    callbacks: &dyn HostCallbacks,
    llm: &dyn LLM,
    messages: &[LLMMessage],
    turn_id: &str,
    agent_id: &str,
    tool_call: &mut ToolCall,
    cancellation: tokio_util::sync::CancellationToken,
) -> Option<ExecutableToolResult> {
    let name = tool_call.name.clone();
    let arguments = tool_call.arguments.clone();
    let call_id = tool_call.id.clone();
    if agent_id != crate::callbacks::MAIN_AGENT_ID
        || !name.eq_ignore_ascii_case("UpdateGoal")
        || arguments.get("status").and_then(|value| value.as_str()) != Some("complete")
    {
        return None;
    }

    let goal = callbacks
        .state_read(crate::rpc::types::StateReadRequest {
            domain: "goal".into(),
            key: "goal".into(),
            turn_id: turn_id.to_string(),
            tool_call_id: call_id.clone(),
        })
        .await
        .ok()
        .and_then(|response| {
            crate::goal::judge::goal_snapshot_from_domain(&response.value)
        });
    let Some(goal) = goal else {
        return None;
    };
    let system_offset = usize::from(
        messages
            .first()
            .is_some_and(|message| message.role == "system"),
    );
    let transcript = messages[system_offset..].to_vec();
    let emit_judge_request = |event: serde_json::Value| callbacks.emit_event(event);
    let verdict = crate::goal::judge::evaluate_transcript_with_events(
        llm,
        &transcript,
        &goal,
        Some(cancellation),
        agent_id,
        Some(&emit_judge_request),
    )
    .await;
    if verdict.ok {
        return None;
    }
    if verdict.impossible {
        tool_call.arguments = serde_json::json!({
            "status": "blocked",
            "reason": verdict.reason,
        });
        return None;
    }

    callbacks.emit_event(serde_json::json!({
        "type": "tool.call.started",
        "agent_id": agent_id,
        "turn_id": turn_id,
        "tool_call_id": call_id,
        "tool_name": name,
        "args": arguments,
    }));
    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: format!(
            "Goal completion rejected by judge: {}\nContinue working \
             toward the goal objective.",
            verdict.reason
        ),
        is_error: false,
        note: None,
        display: None,
    })
}
