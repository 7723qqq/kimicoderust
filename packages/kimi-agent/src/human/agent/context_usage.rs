use crate::human::agent::turn::HistoryMessage;
use crate::human::llm::message::{Message, ToolDescription};
use crate::human::llm::tokens::{EstimableMessage, estimate_tokens, estimate_tokens_for_message};
use crate::human::llm::usage::{TokenUsage, empty_usage};

pub struct ContextUsagePrefix {
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<ToolDescription>>,
}

pub fn calculate_context_tokens(usage: &TokenUsage) -> u64 {
    usage.input_other + usage.input_cache_read + usage.input_cache_creation + usage.output
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    estimate_tokens(text)
}

pub fn estimate_message_tokens(message: &Message) -> u64 {
    let estimable = match message {
        Message::System(message) => EstimableMessage {
            role: "system".to_string(),
            content: message.content.clone(),
            tool_calls: None,
        },
        Message::User(message) => EstimableMessage {
            role: "user".to_string(),
            content: message.content.clone(),
            tool_calls: None,
        },
        Message::Assistant(message) => EstimableMessage {
            role: "assistant".to_string(),
            content: message.content.clone(),
            tool_calls: Some(message.tool_calls.clone()),
        },
        Message::Tool(message) => EstimableMessage {
            role: "tool".to_string(),
            content: message.content.clone(),
            tool_calls: None,
        },
    };
    estimate_tokens_for_message(&estimable, None)
}

pub fn estimate_used_context_tokens(
    history: &[HistoryMessage],
    prefix: Option<&ContextUsagePrefix>,
) -> u64 {
    let mut last_usage_index = None;
    let mut usage_tokens = 0;
    for (index, entry) in history.iter().enumerate().rev() {
        let HistoryMessage::Assistant(entry) = entry else {
            continue;
        };
        let usage = entry
            .meta
            .as_ref()
            .map(|meta| meta.usage.clone())
            .unwrap_or_else(empty_usage);
        let tokens = calculate_context_tokens(&usage);
        if tokens > 0 {
            last_usage_index = Some(index);
            usage_tokens = tokens;
            break;
        }
    }
    let mut tokens = usage_tokens;
    let start = last_usage_index.map_or(0, |index| index + 1);
    for entry in &history[start..] {
        tokens += estimate_message_tokens(&entry.message());
    }
    if last_usage_index.is_none()
        && let Some(prefix) = prefix
    {
        if let Some(system_prompt) = &prefix.system_prompt {
            tokens += estimate_text_tokens(system_prompt);
        }
        if let Some(tools) = prefix.tools.as_ref().filter(|tools| !tools.is_empty()) {
            tokens += estimate_text_tokens(&serde_json::to_string(tools).unwrap_or_default());
        }
    }
    tokens
}
