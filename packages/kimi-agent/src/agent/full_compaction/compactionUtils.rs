use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::_base::errors::errors::SharedError;
use crate::agent::context_memory::compaction_handoff::is_real_user_input;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::llm_requester::llm_requester::AgentLLMRequestFinish;
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::{ContentPart, Role};
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::errors::{
    APIContextOverflowError, APIEmptyResponseError, APIProviderOverloadedError,
    APIProviderQuotaExhaustedError, APIProviderRateLimitError, APIRequestTooLargeError,
    APIStatusError,
};
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::contract::tokens::{EstimableMessage, estimate_tokens_for_message};
use crate::wire::record::WireAsyncError;

pub const COMPACTION_OVERFLOW_SHRINK_RATIOS: [f64; 3] = [0.7, 0.5, 0.35];
pub const SNIPPED_TOOL_RESULT_MARKER: &str = "[snipped tool result — ";
pub const DEFAULT_TOOL_RESULT_SNIP_MIN_BYTES: usize = 1024;
pub const DEFAULT_TOOL_RESULT_SNIP_HEAD_LINES: usize = 40;
pub const DEFAULT_TOOL_RESULT_SNIP_TAIL_LINES: usize = 40;

#[derive(Debug, Clone)]
pub struct CompactionAttemptResult {
    pub summary: String,
    pub usage: Option<TokenUsage>,
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompactionTruncatedError;

impl CompactionTruncatedError {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CompactionTruncatedError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CompactionTruncatedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Compaction response was truncated before producing a complete summary.")
    }
}

impl Error for CompactionTruncatedError {}

pub fn find_api_status_error<'a>(mut error: &'a (dyn Error + 'static)) -> Option<&'a APIStatusError> {
    let mut seen = Vec::<*const ()>::new();
    loop {
        if let Some(status) = status_error_of(error) {
            return Some(status);
        }
        let address = error as *const dyn Error as *const ();
        if seen.contains(&address) {
            return None;
        }
        seen.push(address);
        error = error.source()?;
    }
}

fn status_error_of<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a APIStatusError> {
    if let Some(error) = error.downcast_ref::<APIStatusError>() {
        return Some(error);
    }
    if let Some(error) = error.downcast_ref::<APIContextOverflowError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIRequestTooLargeError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIProviderRateLimitError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIProviderQuotaExhaustedError>() {
        return Some(&error.0);
    }
    error
        .downcast_ref::<APIProviderOverloadedError>()
        .map(|error| &error.0)
}

pub fn collect_summary(
    finish: AgentLLMRequestFinish,
) -> Result<CompactionAttemptResult, SharedError> {
    if finish.provider_finish_reason == Some(FinishReason::Truncated) {
        return Err(Arc::new(CompactionTruncatedError::new()));
    }
    let summary = finish
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>()
        .trim()
        .to_string();
    if summary.is_empty() {
        return Err(Arc::new(APIEmptyResponseError::new(
            "The compaction response did not contain a non-empty summary.",
            finish.provider_finish_reason,
            finish.raw_finish_reason,
        )));
    }
    Ok(CompactionAttemptResult {
        summary,
        usage: Some(finish.usage),
        trace_id: finish.trace_id,
    })
}

pub fn history_safe_to_compact(current: &[ContextMessage], original: &[ContextMessage]) -> bool {
    if current.len() < original.len() {
        return false;
    }
    if current
        .iter()
        .zip(original)
        .any(|(current, original)| current != original)
    {
        return false;
    }
    current[original.len()..].iter().all(|message| {
        is_real_user_input(message)
            || message
                .origin
                .as_ref()
                .is_some_and(|origin| origin.kind() == "injection")
    })
}

fn default_estimate_message(message: &Message) -> u64 {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    estimate_tokens_for_message(
        &EstimableMessage {
            role: role.to_string(),
            content: message.content.clone(),
            tool_calls: Some(message.tool_calls.clone()),
        },
        None,
    )
}

pub fn shrink_compaction_history_after_overflow(
    messages: &[Message],
    attempt: usize,
    estimate_message: Option<&(dyn Fn(&Message) -> u64 + Send + Sync)>,
) -> Vec<Message> {
    if messages.len() <= 1 {
        return messages.to_vec();
    }
    let default_estimate;
    let estimate_message = match estimate_message {
        Some(estimate) => estimate,
        None => {
            default_estimate = default_estimate_message;
            &default_estimate
        }
    };
    let total_tokens = messages
        .iter()
        .fold(0u64, |total, message| total.saturating_add(estimate_message(message)));
    let token_budget = if attempt == 0 {
        u64::MAX
    } else {
        let ratio = COMPACTION_OVERFLOW_SHRINK_RATIOS
            [attempt.saturating_sub(1).min(COMPACTION_OVERFLOW_SHRINK_RATIOS.len() - 1)];
        (total_tokens as f64 * ratio).floor() as u64
    };
    take_recent_messages_within_token_budget(messages, token_budget, Some(estimate_message))
}

pub fn take_recent_messages_within_token_budget(
    messages: &[Message],
    token_budget: u64,
    estimate_message: Option<&(dyn Fn(&Message) -> u64 + Send + Sync)>,
) -> Vec<Message> {
    let default_estimate;
    let estimate_message = match estimate_message {
        Some(estimate) => estimate,
        None => {
            default_estimate = default_estimate_message;
            &default_estimate
        }
    };
    let mut start = messages.len();
    let mut tokens = 0u64;
    for (index, message) in messages.iter().enumerate().rev() {
        let message_tokens = estimate_message(message);
        if tokens.saturating_add(message_tokens) > token_budget {
            break;
        }
        tokens = tokens.saturating_add(message_tokens);
        start = index;
    }
    if start == 0 {
        start = 1;
    }
    let start = start.min(messages.len());
    drop_leading_tool_results(&messages[start..])
}

pub fn drop_oldest_message_and_leading_tool_results(messages: &[Message]) -> Vec<Message> {
    if messages.len() <= 1 {
        return messages.to_vec();
    }
    drop_leading_tool_results(&messages[1..])
}

pub fn drop_leading_tool_results(messages: &[Message]) -> Vec<Message> {
    let start = messages
        .iter()
        .position(|message| message.role != Role::Tool)
        .unwrap_or(messages.len());
    messages[start..].to_vec()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnipToolResultOptions {
    pub min_bytes: Option<usize>,
    pub head_lines: Option<usize>,
    pub tail_lines: Option<usize>,
}

pub fn snip_large_tool_results(
    messages: &[Message],
    options: Option<SnipToolResultOptions>,
) -> Vec<Message> {
    let options = options.unwrap_or_default();
    let min_bytes = options.min_bytes.unwrap_or(DEFAULT_TOOL_RESULT_SNIP_MIN_BYTES);
    let head_lines = options.head_lines.unwrap_or(DEFAULT_TOOL_RESULT_SNIP_HEAD_LINES);
    let tail_lines = options.tail_lines.unwrap_or(DEFAULT_TOOL_RESULT_SNIP_TAIL_LINES);
    let mut changed: Option<Vec<Message>> = None;
    for (index, message) in messages.iter().enumerate() {
        let next = if message.role == Role::Tool {
            snip_tool_result_content(message, min_bytes, head_lines, tail_lines)
        } else {
            None
        };
        match next {
            Some(next) => {
                if changed.is_none() {
                    changed = Some(messages[..index].to_vec());
                }
                changed.as_mut().unwrap().push(next);
            }
            None => {
                if let Some(changed) = changed.as_mut() {
                    changed.push(message.clone());
                }
            }
        }
    }
    changed.unwrap_or_else(|| messages.to_vec())
}

fn snip_tool_result_content(
    message: &Message,
    min_bytes: usize,
    head_lines: usize,
    tail_lines: usize,
) -> Option<Message> {
    let text = message.content.iter().find_map(|part| match part {
        ContentPart::Text { text, .. } => Some(text),
        _ => None,
    })?;
    if text.starts_with(SNIPPED_TOOL_RESULT_MARKER) || text.encode_utf16().count() < min_bytes {
        return None;
    }
    let lines = text.split('\n').collect::<Vec<_>>();
    if lines.len() <= head_lines.saturating_add(tail_lines) {
        return None;
    }
    let head = lines[..head_lines].join("\n");
    let tail = if tail_lines == 0 {
        lines.join("\n")
    } else {
        lines[lines.len() - tail_lines..].join("\n")
    };
    let omitted = lines.len() - head_lines - tail_lines;
    let replacement = format!(
        "{SNIPPED_TOOL_RESULT_MARKER}{omitted} lines omitted for compaction; re-run the tool if the full output is needed]\n{head}\n[... {omitted} lines omitted ...]\n{tail}"
    );
    let mut next = message.clone();
    next.content = vec![ContentPart::Text {
        text: replacement,
        meta: None,
    }];
    Some(next)
}

pub(crate) fn shared_error(error: WireAsyncError) -> SharedError {
    Arc::from(error)
}
