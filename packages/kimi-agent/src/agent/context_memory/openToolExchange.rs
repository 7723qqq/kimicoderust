use std::collections::HashSet;

use crate::human::llm::message::Role;
use crate::llm_adapter::contract::message::create_tool_message;

use super::types::ContextMessage;

pub const INHERITED_IN_FLIGHT_TOOL_OUTPUT: &str = "This tool call was still executing when this conversation snapshot was inherited from the source agent, so its result is not part of this context. The outcome is unknown — do not assume it succeeded or failed, and do not wait for it.";

pub fn close_trailing_open_tool_exchange(history: &[ContextMessage]) -> Vec<ContextMessage> {
    let mut last_non_tool_index = history.len() as i64 - 1;
    while last_non_tool_index >= 0 && history[last_non_tool_index as usize].role == Role::Tool {
        last_non_tool_index -= 1;
    }
    if last_non_tool_index < 0 {
        return Vec::new();
    }
    let last_non_tool_index = last_non_tool_index as usize;
    let assistant = &history[last_non_tool_index];
    if assistant.role != Role::Assistant || assistant.tool_calls.is_empty() {
        return history.to_vec();
    }
    let answered = history[last_non_tool_index + 1..]
        .iter()
        .filter_map(|message| message.tool_call_id.clone())
        .collect::<HashSet<_>>();
    let open_calls = assistant
        .tool_calls
        .iter()
        .filter(|call| !answered.contains(&call.id))
        .collect::<Vec<_>>();
    if open_calls.is_empty() {
        return history.to_vec();
    }
    let mut out = history[..last_non_tool_index].to_vec();
    let mut settled_assistant = assistant.clone();
    if settled_assistant.partial == Some(true) {
        settled_assistant.partial = None;
    }
    out.push(settled_assistant);
    out.extend_from_slice(&history[last_non_tool_index + 1..]);
    out.extend(open_calls.into_iter().map(|call| {
        ContextMessage::from(create_tool_message(
            &call.id,
            INHERITED_IN_FLIGHT_TOOL_OUTPUT,
        ))
    }));
    out
}
