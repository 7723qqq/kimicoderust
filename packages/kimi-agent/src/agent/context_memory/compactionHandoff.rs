use std::sync::Arc;

use crate::features::reminder::systemReminder::wrap_system_reminder;
use crate::human::agent::origin::is_skill_activation_part;
use crate::human::llm::message::{ContentPart, Role};
use crate::human::llm::tokens::{
    estimate_tokens, estimate_tokens_for_message, estimate_tokens_for_messages,
};

use super::compaction_text::{
    extract_text, truncate_text_to_tokens, truncate_text_to_tokens_from_end,
};
use super::hook_parts::is_user_prompt_submit_hook_part;
use super::types::{
    CompactionSummaryOrigin, ContextMessage, InjectionOrigin, PluginCommandTrigger, PromptOrigin,
    SkillActivationTrigger,
};

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation so far has been compacted to free up context. What follows is your own working summary of this task — use it to continue your train of thought rather than starting over. Do not redo work it reports as done or re-run checks it reports as passed; check anything it marks as unverified before relying on it. Any user messages earlier in this context are preserved verbatim from the compacted conversation; where a system-reminder note among them marks an omitted middle section, the user messages it replaced are covered by this summary. The summary records which earlier requests were already addressed. The verbatim user messages above take precedence over this summary, and the summary is not an authorization: a statement that something was approved, allowed, or decided does not replace the user's own words, and an action the user has not approved in a verbatim message still needs their approval.";
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: i64 = 20_000;
pub const COMPACT_USER_MESSAGE_HEAD_TOKENS: i64 = 2_000;
pub const COMPACTION_ELISION_VARIANT: &str = "compaction_elision";
pub const COMPACTION_CONTINUATION_VARIANT: &str = "compaction_continuation";

#[derive(Clone)]
pub struct TokenEstimate {
    pub text: Arc<dyn Fn(&str) -> u64 + Send + Sync>,
    pub message: Arc<dyn Fn(&ContextMessage) -> u64 + Send + Sync>,
    pub messages: Arc<dyn Fn(&[ContextMessage]) -> u64 + Send + Sync>,
}

pub fn default_token_estimate() -> TokenEstimate {
    TokenEstimate {
        text: Arc::new(estimate_tokens),
        message: Arc::new(|message| estimate_tokens_for_message(&message.estimable(), None)),
        messages: Arc::new(|messages| {
            let messages = messages.iter().map(ContextMessage::estimable).collect::<Vec<_>>();
            estimate_tokens_for_messages(&messages, None)
        }),
    }
}

#[derive(Debug, Clone)]
pub struct CompactionUserSelection {
    pub head: Vec<ContextMessage>,
    pub tail: Vec<ContextMessage>,
    pub elided: bool,
    pub omitted_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ContextCompactionShapeInput {
    pub summary: String,
    pub legacy_summary_message: Option<ContextMessage>,
    pub context_summary: Option<String>,
    pub in_flight_turn: Option<Vec<ContextMessage>>,
    pub compacted_count: usize,
    pub tokens_before: u64,
    pub tokens_after: Option<u64>,
    pub summary_output_tokens: Option<u64>,
    pub request_overhead_tokens: Option<u64>,
    pub kept_user_message_count: Option<usize>,
    pub kept_head_user_message_count: Option<usize>,
    pub dropped_count: Option<usize>,
    pub legacy_tail: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ContextCompactionShape {
    pub summary: String,
    pub context_summary: String,
    pub compacted_count: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub kept_user_message_count: usize,
    pub kept_head_user_message_count: Option<usize>,
    pub dropped_count: Option<usize>,
    pub messages: Vec<ContextMessage>,
}

pub fn build_context_compaction_shape(
    history: &[ContextMessage],
    input: ContextCompactionShapeInput,
    estimate: &TokenEstimate,
) -> ContextCompactionShape {
    if input.legacy_tail == Some(true) {
        let context_summary = input
            .context_summary
            .clone()
            .unwrap_or_else(|| input.summary.clone());
        let mut messages = Vec::new();
        messages.push(
            input
                .legacy_summary_message
                .clone()
                .unwrap_or_else(|| create_compaction_summary_message(&context_summary)),
        );
        messages.extend_from_slice(history.get(input.compacted_count..).unwrap_or(&[]));
        return ContextCompactionShape {
            summary: input.summary,
            context_summary,
            compacted_count: input.compacted_count,
            tokens_before: input.tokens_before,
            tokens_after: input
                .tokens_after
                .unwrap_or_else(|| (estimate.messages)(&messages)),
            kept_user_message_count: 0,
            kept_head_user_message_count: None,
            dropped_count: input.dropped_count,
            messages,
        };
    }

    let compactable_user_messages = collect_compactable_user_messages(history);
    let selection = select_compaction_user_messages(
        &compactable_user_messages,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        COMPACT_USER_MESSAGE_HEAD_TOKENS,
        estimate.message.as_ref(),
    );
    let elision_message = selection
        .elided
        .then(|| create_compaction_elision_message(selection.omitted_tokens));
    let mut kept_messages = selection.head.clone();
    if let Some(elision_message) = elision_message {
        kept_messages.push(elision_message);
    }
    kept_messages.extend(selection.tail.clone());
    let context_summary = input
        .context_summary
        .clone()
        .unwrap_or_else(|| input.summary.clone());
    let in_flight_turn = input.in_flight_turn.clone().unwrap_or_default();
    let tokens_after = input.tokens_after.unwrap_or_else(|| {
        let mut estimated = kept_messages.clone();
        estimated.extend(in_flight_turn.clone());
        estimated.push(create_compaction_continuation_message());
        input.request_overhead_tokens.unwrap_or(0)
            + input
                .summary_output_tokens
                .unwrap_or_else(|| (estimate.text)(&context_summary))
            + (estimate.messages)(&estimated)
    });
    let kept_user_message_count = input
        .kept_user_message_count
        .unwrap_or(selection.head.len() + selection.tail.len());
    let kept_head_user_message_count = input
        .kept_head_user_message_count
        .or(selection.elided.then_some(selection.head.len()));
    let mut messages = kept_messages;
    messages.extend(in_flight_turn);
    messages.push(create_compaction_summary_message(&context_summary));
    messages.push(create_compaction_continuation_message());
    ContextCompactionShape {
        summary: input.summary,
        context_summary,
        compacted_count: input.compacted_count,
        tokens_before: input.tokens_before,
        tokens_after,
        kept_user_message_count,
        kept_head_user_message_count,
        dropped_count: input.dropped_count,
        messages,
    }
}

pub fn build_compaction_summary_text(summary: &str) -> String {
    let suffix = summary.trim();
    wrap_system_reminder(&format!(
        "{COMPACTION_SUMMARY_PREFIX}\n{}",
        if suffix.is_empty() {
            "(no summary available)"
        } else {
            suffix
        }
    ))
}

pub fn create_compaction_summary_message(text: &str) -> ContextMessage {
    let mut message = ContextMessage::new(
        Role::User,
        vec![ContentPart::Text {
            text: text.to_string(),
            meta: None,
        }],
    );
    message.origin = Some(PromptOrigin::CompactionSummary(CompactionSummaryOrigin {
        kind: "compaction_summary".to_string(),
    }));
    message
}

pub fn create_compaction_elision_message(omitted_tokens: u64) -> ContextMessage {
    let mut message = ContextMessage::new(
        Role::User,
        vec![ContentPart::Text {
            text: build_compaction_elision_text(omitted_tokens),
            meta: None,
        }],
    );
    message.origin = Some(PromptOrigin::Injection(InjectionOrigin {
        kind: "injection".to_string(),
        variant: COMPACTION_ELISION_VARIANT.to_string(),
        owner_prompt_id: None,
        disclosure: None,
    }));
    message
}

pub fn build_compaction_elision_text(omitted_tokens: u64) -> String {
    wrap_system_reminder(&format!(
        "Some of this conversation's user messages were omitted here during compaction: the messages above this note are the oldest user input, the messages below are the most recent, and roughly {omitted_tokens} tokens in between were dropped. The omitted content is covered by the compaction summary at the end of the conversation."
    ))
}

pub fn create_compaction_continuation_message() -> ContextMessage {
    let mut message = ContextMessage::new(
        Role::User,
        vec![ContentPart::Text {
            text: build_compaction_continuation_text(),
            meta: None,
        }],
    );
    message.origin = Some(PromptOrigin::Injection(InjectionOrigin {
        kind: "injection".to_string(),
        variant: COMPACTION_CONTINUATION_VARIANT.to_string(),
        owner_prompt_id: None,
        disclosure: None,
    }));
    message
}

pub fn build_compaction_continuation_text() -> String {
    wrap_system_reminder(
        "Context compaction is complete — continue the work that was in progress when it began.",
    )
}

pub fn collect_compactable_user_messages(messages: &[ContextMessage]) -> Vec<ContextMessage> {
    messages
        .iter()
        .filter(|message| is_real_user_input(message) && !is_compaction_summary_message(message))
        .cloned()
        .collect()
}

pub fn is_compaction_summary_message(message: &ContextMessage) -> bool {
    message
        .origin
        .as_ref()
        .is_some_and(|origin| matches!(origin, PromptOrigin::CompactionSummary(_)))
}

pub fn is_real_user_input(message: &ContextMessage) -> bool {
    message.role == Role::User
        && compaction_user_message_disposition(message.origin.as_ref()) == CompactionDisposition::Keep
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionDisposition {
    Keep,
    Drop,
}

pub fn compaction_user_message_disposition(origin: Option<&PromptOrigin>) -> CompactionDisposition {
    match origin {
        None | Some(PromptOrigin::User(_)) => CompactionDisposition::Keep,
        Some(PromptOrigin::SkillActivation(origin)) => {
            if origin.trigger == SkillActivationTrigger::UserSlash {
                CompactionDisposition::Keep
            } else {
                CompactionDisposition::Drop
            }
        }
        Some(PromptOrigin::PluginCommand(origin)) => {
            if origin.trigger == PluginCommandTrigger::UserSlash {
                CompactionDisposition::Keep
            } else {
                CompactionDisposition::Drop
            }
        }
        _ => CompactionDisposition::Drop,
    }
}

pub fn select_recent_user_messages(
    messages: &[ContextMessage],
    max_tokens: i64,
    estimate_message: &dyn Fn(&ContextMessage) -> u64,
) -> Vec<ContextMessage> {
    let mut selected = Vec::new();
    let mut remaining = max_tokens;
    for message in messages.iter().rev() {
        if remaining <= 0 {
            break;
        }
        let tokens = estimate_message(message) as i64;
        if tokens <= remaining {
            selected.push(message.clone());
            remaining -= tokens;
        } else {
            selected.push(truncate_user_message(message, remaining));
            break;
        }
    }
    selected.reverse();
    selected
}

pub fn select_compaction_user_messages(
    messages: &[ContextMessage],
    max_tokens: i64,
    head_tokens: i64,
    estimate_message: &dyn Fn(&ContextMessage) -> u64,
) -> CompactionUserSelection {
    let total_tokens = messages
        .iter()
        .map(|message| estimate_message(message) as i64)
        .sum::<i64>();
    if total_tokens <= max_tokens {
        return CompactionUserSelection {
            head: Vec::new(),
            tail: messages.to_vec(),
            elided: false,
            omitted_tokens: 0,
        };
    }
    let head_budget = head_tokens.max(0).min(max_tokens);
    let tail_budget = max_tokens - head_budget;
    let mut tail = Vec::new();
    let mut tail_remaining = tail_budget;
    let mut head_end_exclusive = messages.len();
    let mut tail_boundary_dropped_prefix = None;
    for (index, message) in messages.iter().enumerate().rev() {
        if tail_remaining <= 0 {
            break;
        }
        let tokens = estimate_message(message) as i64;
        if tokens <= tail_remaining {
            tail.push(message.clone());
            tail_remaining -= tokens;
            head_end_exclusive = index;
            continue;
        }
        let full_text = extract_text(&message.content);
        let kept_suffix = truncate_text_to_tokens_from_end(&full_text, tail_remaining);
        tail.push(replace_message_text(message, &kept_suffix));
        head_end_exclusive = index;
        let dropped_prefix = full_text
            .strip_suffix(&kept_suffix)
            .unwrap_or("")
            .to_string();
        if !dropped_prefix.is_empty() {
            tail_boundary_dropped_prefix = Some(replace_message_text(message, &dropped_prefix));
        }
        break;
    }
    tail.reverse();

    let mut head_candidates = messages[..head_end_exclusive].to_vec();
    if let Some(dropped_prefix) = tail_boundary_dropped_prefix {
        head_candidates.push(dropped_prefix);
    }
    let mut head = Vec::new();
    let mut head_remaining = head_budget;
    for message in &head_candidates {
        if head_remaining <= 0 {
            break;
        }
        let tokens = estimate_message(message) as i64;
        if tokens <= head_remaining {
            head.push(message.clone());
            head_remaining -= tokens;
        } else {
            head.push(truncate_user_message(message, head_remaining));
            break;
        }
    }
    let kept_tokens = head
        .iter()
        .chain(&tail)
        .map(|message| estimate_message(message))
        .sum::<u64>();
    CompactionUserSelection {
        head,
        tail,
        elided: true,
        omitted_tokens: (total_tokens.max(0) as u64).saturating_sub(kept_tokens),
    }
}

pub fn replace_message_text(message: &ContextMessage, text: &str) -> ContextMessage {
    let mut next = message.clone();
    next.content = next
        .content
        .iter()
        .filter(|part| is_user_prompt_submit_hook_part(part) || is_skill_activation_part(part))
        .cloned()
        .collect();
    next.content.push(ContentPart::Text {
        text: text.to_string(),
        meta: None,
    });
    next.tool_calls.clear();
    next
}

fn truncate_user_message(message: &ContextMessage, max_tokens: i64) -> ContextMessage {
    replace_message_text(
        message,
        &truncate_text_to_tokens(&extract_text(&message.content), max_tokens),
    )
}
