use std::collections::{HashMap, HashSet};

use kimi_core::types::LLMMessage;

const TOOL_CALL_ID_SAFE_CHARS: fn(char) -> bool =
    |c| c.is_ascii_alphanumeric() || c == '_' || c == '-';

const EMPTY_TOOL_CALL_ID: &str = "tool_call";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallIdPolicy {
    Chat,

    Responses,
}

impl ToolCallIdPolicy {
    fn normalize(self, id: &str, max_length: usize) -> String {
        match self {
            ToolCallIdPolicy::Chat => sanitize_tool_call_id(id, max_length),
            ToolCallIdPolicy::Responses => {
                let call_id = id.split('|').next().unwrap_or(id);
                sanitize_tool_call_id(call_id, max_length)
            }
        }
    }
}

fn sanitize_tool_call_id(id: &str, max_length: usize) -> String {
    id.chars()
        .map(|c| if TOOL_CALL_ID_SAFE_CHARS(c) { c } else { '_' })
        .take(max_length)
        .collect()
}

fn truncate_tool_call_id(base: &str, max_length: usize, suffix: &str) -> String {
    let base_length = max_length.saturating_sub(suffix.chars().count());
    let truncated: String = base.chars().take(base_length).collect();
    format!("{truncated}{suffix}")
}

fn make_unique_tool_call_id(normalized: &str, used: &HashSet<String>, max_length: usize) -> String {
    let base = if normalized.is_empty() {
        EMPTY_TOOL_CALL_ID
    } else {
        normalized
    };
    let candidate = truncate_tool_call_id(base, max_length, "");
    if !used.contains(&candidate) {
        return candidate;
    }
    let mut index = 2;
    loop {
        let suffix = format!("_{index}");
        let suffixed = truncate_tool_call_id(base, max_length, &suffix);
        if !used.contains(&suffixed) {
            return suffixed;
        }
        index += 1;
    }
}

fn collect_tool_call_ids(messages: &[LLMMessage]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for message in messages {
        if message.role == "assistant" {
            for call in &message.tool_calls {
                if seen.insert(call.id.clone()) {
                    ids.push(call.id.clone());
                }
            }
        }
        if message.role == "tool"
            && let Some(id) = message.tool_call_id.as_ref()
            && seen.insert(id.clone())
        {
            ids.push(id.clone());
        }
    }
    ids
}

fn build_tool_call_id_map(
    raw_ids: &[String],
    policy: ToolCallIdPolicy,
    max_length: usize,
) -> HashMap<String, String> {
    let mut mapped = HashMap::new();
    let mut used = HashSet::new();
    for raw in raw_ids {
        let normalized = policy.normalize(raw, max_length);
        if normalized == *raw && !normalized.is_empty() {
            mapped.insert(raw.clone(), normalized.clone());
            used.insert(normalized);
        }
    }
    for raw in raw_ids {
        if mapped.contains_key(raw) {
            continue;
        }
        let normalized = policy.normalize(raw, max_length);
        let unique = make_unique_tool_call_id(&normalized, &used, max_length);
        mapped.insert(raw.clone(), unique.clone());
        used.insert(unique);
    }
    mapped
}

pub fn normalize_tool_call_ids(
    messages: &mut [LLMMessage],
    policy: ToolCallIdPolicy,
    max_length: usize,
) -> HashMap<String, String> {
    let raw_ids = collect_tool_call_ids(messages);
    if raw_ids.is_empty() {
        return HashMap::new();
    }
    let mapped = build_tool_call_id_map(&raw_ids, policy, max_length);
    for message in messages.iter_mut() {
        if message.role == "assistant" {
            for call in &mut message.tool_calls {
                if let Some(mapped_id) = mapped.get(&call.id)
                    && *mapped_id != call.id
                {
                    call.id = mapped_id.clone();
                }
            }
        } else if message.role == "tool"
            && let Some(id) = message.tool_call_id.as_mut()
            && let Some(mapped_id) = mapped.get(id)
        {
            *id = mapped_id.clone();
        }
    }
    mapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimi_core::types::ToolCall;

    fn assistant_with_ids(ids: &[&str]) -> LLMMessage {
        LLMMessage {
            role: kimi_core::types::Role::Assistant,
            content: String::new(),
            tool_calls: ids
                .iter()
                .map(|id| ToolCall {
                    id: (*id).to_string(),
                    name: "f".to_string(),
                    arguments: Some(serde_json::json!({}).to_string()),
                    extras: None,
                    raw_id: None,
                    _stream_index: None,
                })
                .collect(),
            ..LLMMessage::default()
        }
    }

    fn tool_message(id: &str) -> LLMMessage {
        LLMMessage {
            role: kimi_core::types::Role::Tool,
            content: "ok".to_string(),
            tool_call_id: Some(id.to_string()),
            ..LLMMessage::default()
        }
    }

    #[test]
    fn sanitizes_unsafe_characters_and_truncates() {
        assert_eq!(
            sanitize_tool_call_id("call|abc def/ghi", 64),
            "call_abc_def_ghi"
        );
        assert_eq!(sanitize_tool_call_id(&"a".repeat(100), 64).len(), 64);
    }

    #[test]
    fn rewrites_both_sides_of_a_tool_call_consistently() {
        let mut messages = vec![assistant_with_ids(&["call|abc"]), tool_message("call|abc")];
        let mapped = normalize_tool_call_ids(&mut messages, ToolCallIdPolicy::Chat, 64);
        assert_eq!(messages[0].tool_calls[0].id, "call_abc");
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_abc"));
        assert_eq!(mapped.get("call|abc").map(String::as_str), Some("call_abc"));
    }

    #[test]
    fn dedupes_collisions_and_replaces_empty_ids() {
        let mut messages = vec![assistant_with_ids(&["a b", "a/b", ""])];
        normalize_tool_call_ids(&mut messages, ToolCallIdPolicy::Chat, 64);
        let ids: Vec<&str> = messages[0]
            .tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a_b", "a_b_2", "tool_call"]);
    }

    #[test]
    fn responses_policy_keeps_the_part_before_the_separator() {
        let mut messages = vec![assistant_with_ids(&["rs_1|call_abc"])];
        normalize_tool_call_ids(&mut messages, ToolCallIdPolicy::Responses, 64);
        assert_eq!(messages[0].tool_calls[0].id, "rs_1");
    }
}
