pub(super) fn emit_subagent_event(
    callbacks: &dyn crate::callbacks::HostCallbacks,
    event: serde_json::Value,
) {
    callbacks.emit_event(event);
}

#[allow(clippy::too_many_arguments)]
pub fn emit_spawned_started(
    callbacks: &dyn crate::callbacks::HostCallbacks,
    agent_id: &str,
    profile_name: &str,
    tool_call_id: Option<&str>,
    description: Option<&str>,
    run_in_background: bool,
    swarm_index: Option<usize>,
    prompt: &str,
) {
    let caller = crate::tools::CALLER_AGENT_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
    emit_subagent_event(
        callbacks,
        serde_json::json!({
            "type": "subagent.spawned",
            "subagent_id": agent_id,
            "subagent_name": profile_name,
            "parent_tool_call_id": tool_call_id,
            "description": description,
            "run_in_background": run_in_background,
            "swarm_index": swarm_index,
            "parent_agent_id": caller,
            "caller_agent_id": caller,
        }),
    );
    emit_subagent_event(
        callbacks,
        serde_json::json!({ "type": "subagent.started", "subagent_id": agent_id }),
    );

    if let Some(guard) = callbacks.hook_guard() {
        let guard = guard.clone();
        let agent_name = profile_name.to_string();
        let prompt = prompt.to_string();
        tokio::spawn(async move { guard.notify_subagent_start(&agent_name, &prompt).await });
    }
}

pub(crate) fn emit_completed(
    callbacks: &dyn crate::callbacks::HostCallbacks,
    agent_id: &str,
    agent_name: &str,
    summary: &str,
    usage: &crate::rpc::types::TokenUsage,
) {
    emit_subagent_event(
        callbacks,
        serde_json::json!({
            "type": "subagent.completed",
            "subagent_id": agent_id,
            "result_summary": summary,
            "usage": usage_json(usage),
        }),
    );
    if let Some(guard) = callbacks.hook_guard() {
        let guard = guard.clone();
        let agent_name = agent_name.to_string();
        let summary = summary.to_string();
        tokio::spawn(async move { guard.notify_subagent_stop(&agent_name, &summary).await });
    }
}

pub(crate) fn emit_failed(
    callbacks: &dyn crate::callbacks::HostCallbacks,
    agent_id: &str,
    agent_name: &str,
    error: &str,
) {
    emit_subagent_event(
        callbacks,
        serde_json::json!({
            "type": "subagent.failed",
            "subagent_id": agent_id,
            "error": error,
        }),
    );
    if let Some(guard) = callbacks.hook_guard() {
        let guard = guard.clone();
        let agent_name = agent_name.to_string();
        let error = error.to_string();
        tokio::spawn(async move { guard.notify_subagent_stop(&agent_name, &error).await });
    }
}

pub(crate) fn emit_cancelled(
    callbacks: &dyn crate::callbacks::HostCallbacks,
    agent_id: &str,
    agent_name: &str,
) {
    emit_subagent_event(
        callbacks,
        serde_json::json!({
            "type": "subagent.cancelled",
            "subagent_id": agent_id,
        }),
    );
    if let Some(guard) = callbacks.hook_guard() {
        let guard = guard.clone();
        let agent_name = agent_name.to_string();
        tokio::spawn(async move { guard.notify_subagent_stop(&agent_name, "").await });
    }
}

pub(crate) fn usage_json(usage: &crate::rpc::types::TokenUsage) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
        "input_cache_read": usage.input_cache_read,
        "input_cache_creation": usage.input_cache_creation,
    })
}
