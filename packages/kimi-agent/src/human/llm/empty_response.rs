use crate::human::llm::errors::{LlmErrorKind, LlmErrorMessage};
use crate::human::llm::finish_reason::{FinishInfo, FinishReason};
use crate::human::llm::message::{AssistantMessage, ContentPart};
use crate::human::llm::model::LlmModel;

fn finish_reason_name(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Completed => "completed",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Truncated => "truncated",
        FinishReason::Filtered => "filtered",
        FinishReason::Paused => "paused",
        FinishReason::Error => "error",
        FinishReason::Other => "other",
    }
}

fn format_finish_reason_hint(finish: &FinishInfo) -> String {
    if finish.finish_reason.is_none() && finish.raw_finish_reason.is_none() {
        return String::new();
    }
    let raw = finish
        .raw_finish_reason
        .as_deref()
        .map(|raw| format!(", rawFinishReason={raw}"))
        .unwrap_or_default();
    let filtered_hint = if finish.finish_reason == Some(FinishReason::Filtered) {
        " The provider filtered the response before visible output was emitted."
    } else {
        ""
    };
    let reason = finish
        .finish_reason
        .map(finish_reason_name)
        .unwrap_or("unknown");
    format!(" Provider stop details: finishReason={reason}{raw}.{filtered_hint}")
}

pub fn create_empty_response_error(
    model: &LlmModel,
    finish: &FinishInfo,
    think_only: bool,
) -> LlmErrorMessage {
    let detail = if think_only {
        "The API returned a response containing only thinking content without any text or tool calls. This usually indicates the stream was interrupted or the output token budget was exhausted during reasoning."
    } else {
        "The API returned an empty response (no content, no tool calls)."
    };
    LlmErrorMessage {
        kind: LlmErrorKind::EmptyResponse,
        message: format!(
            "{detail}{} Provider: {}, model: {}",
            format_finish_reason_hint(finish),
            model.provider,
            model.model
        ),
        code: None,
        status_code: None,
        request_id: None,
        retry_after_ms: None,
        headers: None,
        retryable: Some(finish.finish_reason != Some(FinishReason::Filtered)),
        finish_reason: finish.finish_reason,
        raw_finish_reason: finish.raw_finish_reason.clone(),
    }
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

pub fn empty_response_error(
    message: &AssistantMessage,
    model: &LlmModel,
    finish: &FinishInfo,
) -> Option<LlmErrorMessage> {
    let has_tool_calls = !message.tool_calls.is_empty();
    if message.content.is_empty() && !has_tool_calls {
        return Some(create_empty_response_error(model, finish, false));
    }
    let has_think = message
        .content
        .iter()
        .any(|part| matches!(part, ContentPart::Think { .. }));
    let has_text = message.content.iter().any(|part| {
        matches!(part, ContentPart::Text { text, .. } if !js_trim(text).is_empty())
    });
    if has_think && !has_text && !has_tool_calls {
        return Some(create_empty_response_error(model, finish, true));
    }
    None
}
