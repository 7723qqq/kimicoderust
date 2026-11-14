use std::collections::{HashMap, HashSet};

use crate::human::llm::message::Message;
use crate::human::llm::requester::requester::ToolCallIdPolicy;
use crate::human::llm::syntax_errors::SyntaxRequestFormatError;
use crate::wire::record::WireAsyncError;

const EMPTY_TOOL_CALL_ID: &str = "tool_call";

pub fn sanitize_tool_call_id(id: &str, max_length: Option<usize>) -> String {
    let mut sanitized = String::with_capacity(id.len());
    for unit in id.encode_utf16() {
        match char::from_u32(u32::from(unit)) {
            Some(character)
                if character.is_ascii_alphanumeric() || character == '_' || character == '-' =>
            {
                sanitized.push(character);
            }
            _ => sanitized.push('_'),
        }
    }
    match max_length {
        Some(max_length) => sanitized.truncate(max_length),
        None => {}
    }
    sanitized
}

pub fn sanitize_open_ai_responses_call_id(id: &str, max_length: Option<usize>) -> String {
    let call_id = id.split('|').next().unwrap_or(id);
    sanitize_tool_call_id(call_id, max_length)
}

pub fn normalize_tool_call_ids_for_provider(
    messages: &[Message],
    policy: &ToolCallIdPolicy,
) -> Result<Vec<Message>, WireAsyncError> {
    let raw_ids = collect_tool_call_ids(messages);
    if raw_ids.is_empty() {
        return Ok(messages.to_vec());
    }
    let mapped_ids = build_tool_call_id_map(&raw_ids, policy)?;
    let mut changed = false;
    let normalized = messages
        .iter()
        .map(|message| match message {
            Message::Assistant(assistant) => {
                let mut message_changed = false;
                let mut assistant = assistant.clone();
                for call in &mut assistant.tool_calls {
                    let Some(mapped) = mapped_ids.get(&call.id) else {
                        continue;
                    };
                    if mapped != &call.id {
                        call.id = mapped.clone();
                        message_changed = true;
                    }
                }
                if message_changed {
                    changed = true;
                }
                Message::Assistant(assistant)
            }
            Message::Tool(tool) => {
                let mapped = mapped_ids
                    .get(&tool.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| tool.tool_call_id.clone());
                if mapped != tool.tool_call_id {
                    changed = true;
                }
                Message::Tool(crate::human::llm::message::ToolMessage {
                    tool_call_id: mapped,
                    ..tool.clone()
                })
            }
            other => other.clone(),
        })
        .collect::<Vec<_>>();
    Ok(if changed { normalized } else { messages.to_vec() })
}

fn collect_tool_call_ids(messages: &[Message]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut append = |id: &str| {
        if seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
    };
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                for call in &assistant.tool_calls {
                    append(&call.id);
                }
            }
            Message::Tool(tool) => append(&tool.tool_call_id),
            _ => {}
        }
    }
    ids
}

fn build_tool_call_id_map(
    raw_ids: &[String],
    policy: &ToolCallIdPolicy,
) -> Result<HashMap<String, String>, WireAsyncError> {
    let mut mapped_ids = HashMap::new();
    let mut used_ids = HashSet::new();
    for raw_id in raw_ids {
        let normalized = (policy.normalize)(raw_id.clone());
        if normalized == *raw_id && !normalized.is_empty() {
            mapped_ids.insert(raw_id.clone(), normalized.clone());
            used_ids.insert(normalized);
        }
    }
    for raw_id in raw_ids {
        if mapped_ids.contains_key(raw_id) {
            continue;
        }
        let normalized = (policy.normalize)(raw_id.clone());
        let unique = make_unique_tool_call_id(&normalized, &used_ids, policy.max_length)?;
        mapped_ids.insert(raw_id.clone(), unique.clone());
        used_ids.insert(unique);
    }
    Ok(mapped_ids)
}

fn make_unique_tool_call_id(
    normalized: &str,
    used_ids: &HashSet<String>,
    max_length: Option<usize>,
) -> Result<String, WireAsyncError> {
    let base = if normalized.is_empty() {
        EMPTY_TOOL_CALL_ID
    } else {
        normalized
    };
    let candidate = truncate_tool_call_id(base, max_length, "")?;
    if !used_ids.contains(&candidate) {
        return Ok(candidate);
    }
    for index in 2.. {
        let suffix = format!("_{index}");
        let suffixed = truncate_tool_call_id(base, max_length, &suffix)?;
        if !used_ids.contains(&suffixed) {
            return Ok(suffixed);
        }
    }
    unreachable!()
}

fn truncate_tool_call_id(
    base: &str,
    max_length: Option<usize>,
    suffix: &str,
) -> Result<String, WireAsyncError> {
    let Some(max_length) = max_length else {
        return Ok(format!("{base}{suffix}"));
    };
    if max_length <= suffix.len() {
        return Err(Box::new(SyntaxRequestFormatError::new(format!(
            "Tool call id maxLength {max_length} is too small for suffix {suffix}."
        ))));
    }
    let base_length = max_length - suffix.len();
    let truncated = base.chars().take(base_length).collect::<String>();
    Ok(format!("{truncated}{suffix}"))
}
