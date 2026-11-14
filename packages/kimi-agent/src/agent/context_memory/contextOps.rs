use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use serde_json::{Map, Value, json};

use crate::agent::context_memory::context_events::{
    CONTEXT_APPEND_LOOP_EVENT_CLASS, CONTEXT_APPEND_MESSAGE_CLASS, CONTEXT_APPLY_COMPACTION_CLASS,
    CONTEXT_CLEAR_CLASS,
};
use crate::persistence::interface::storage::STORAGE_DECODE_FAILED;
use crate::state::state::{
    ReplayableOptions, ReplayableStateKey, StateBlobCodec, UndoableOptions,
};
use crate::wire::record::{PartsTransformer, WireAsyncError, WireRecord};
use kimi_core::primitives::errors::{Error2, Error2Options};

use super::compaction_handoff::{
    ContextCompactionShapeInput, build_context_compaction_shape, create_compaction_summary_message,
    default_token_estimate,
};
use super::compaction_text::extract_text;
use super::conversation_time::{is_prompt_owned_injection, is_undo_anchor};
use super::loop_event_fold::{fold_append_message, fold_loop_event, reset_fold};
use super::types::{ContextMessage, PromptOrigin};

pub static CONTEXT_MEMORY_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "contextMemory".to_string(),
        Arc::new(|| Value::Array(Vec::new())),
        ReplayableOptions {
            schema: json!({"type": "array"}),
            durable: None,
            blobs: Some(StateBlobCodec {
                dehydrate: Arc::new(|record, transform| Box::pin(dehydrate_record(record, transform))),
                rehydrate: Arc::new(|state, transform| Box::pin(dehydrate_messages(state, transform))),
            }),
            mutable_replay: Some(true),
        },
    );
    key.undoable(Some(UndoableOptions {
        on_undo: Some(Arc::new(|state, event, _context| {
            let mut state = serde_json::from_value::<Vec<ContextMessage>>(state).ok()?;
            if state.is_empty() {
                return None;
            }
            let count = event.payload.get("count")?.as_u64()?;
            let cut = compute_undo_cut(&state, count);
            if !is_fully_undoable(&cut, count) {
                return None;
            }
            state.truncate(cut.cut_index);
            serde_json::to_value(reset_fold(&state)).ok()
        })),
    }))
    .expect("context memory undo registration is valid");
    key.on(
        &CONTEXT_APPEND_MESSAGE_CLASS,
        Arc::new(|state, event, context| {
            let state = serde_json::from_value::<Vec<ContextMessage>>(state).ok()?;
            let message = serde_json::from_value::<ContextMessage>(event.payload.get("message")?.clone()).ok()?;
            let state = drop_superseded_reminders(&state, &message);
            serde_json::to_value(fold_append_message(&state, &message, context.silent())).ok()
        }),
    )
    .expect("context append fold registration is valid");
    key.on(
        &CONTEXT_APPEND_LOOP_EVENT_CLASS,
        Arc::new(|state, event, context| {
            let state = serde_json::from_value::<Vec<ContextMessage>>(state).ok()?;
            let recorded = serde_json::from_value(event.payload.get("event")?.clone()).ok()?;
            serde_json::to_value(fold_loop_event(&state, &recorded, context.silent())).ok()
        }),
    )
    .expect("context loop fold registration is valid");
    key.on(
        &CONTEXT_CLEAR_CLASS,
        Arc::new(|state, _event, _context| {
            let state = serde_json::from_value::<Vec<ContextMessage>>(state).ok()?;
            if state.is_empty() {
                None
            } else {
                Some(Value::Array(Vec::new()))
            }
        }),
    )
    .expect("context clear fold registration is valid");
    key.on(
        &CONTEXT_APPLY_COMPACTION_CLASS,
        Arc::new(|state, event, _context| {
            let state = serde_json::from_value::<Vec<ContextMessage>>(state).ok()?;
            let input = read_context_compaction_shape_input(&event.payload).ok()?;
            let result = build_context_compaction_shape(&state, input, &default_token_estimate());
            serde_json::to_value(reset_fold(&result.messages)).ok()
        }),
    )
    .expect("context compaction fold registration is valid");
    Arc::new(key)
});

pub fn context_memory_key() -> Arc<ReplayableStateKey> {
    CONTEXT_MEMORY_KEY.clone()
}

async fn dehydrate_messages(
    mut state: Value,
    transform: PartsTransformer,
) -> Result<Value, WireAsyncError> {
    let Some(messages) = state.as_array_mut() else {
        return Ok(state);
    };
    for message in messages {
        let Some(content) = message.get("content").and_then(Value::as_array).cloned() else {
            continue;
        };
        let transformed = transform(content.clone()).await?;
        if transformed != content {
            message["content"] = Value::Array(transformed);
        }
    }
    Ok(state)
}

async fn dehydrate_record(
    mut record: WireRecord,
    transform: PartsTransformer,
) -> Result<WireRecord, WireAsyncError> {
    match record.get("type").and_then(Value::as_str) {
        Some("context.append_message") => {
            if let Some(content) = record
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
                .cloned()
            {
                let transformed = transform(content.clone()).await?;
                if transformed != content {
                    record["message"]["content"] = Value::Array(transformed);
                }
            }
        }
        Some("context.append_loop_event") => {
            let event_type = record
                .get("event")
                .and_then(|event| event.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if event_type == "content.part" {
                if let Some(part) = record
                    .get("event")
                    .and_then(|event| event.get("part"))
                    .cloned()
                {
                    let transformed = transform(vec![part.clone()]).await?;
                    if transformed.first() != Some(&part) {
                        match transformed.into_iter().next() {
                            Some(part) => record["event"]["part"] = part,
                            None => {
                                if let Some(event) = record["event"].as_object_mut() {
                                    event.remove("part");
                                }
                            }
                        }
                    }
                }
            } else if event_type == "tool.result" {
                if let Some(output) = record
                    .get("event")
                    .and_then(|event| event.get("result"))
                    .and_then(|result| result.get("output"))
                    .and_then(Value::as_array)
                    .cloned()
                {
                    let transformed = transform(output.clone()).await?;
                    if transformed != output {
                        record["event"]["result"]["output"] = Value::Array(transformed);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(record)
}

fn drop_superseded_reminders(
    state: &[ContextMessage],
    message: &ContextMessage,
) -> Vec<ContextMessage> {
    let Some(PromptOrigin::Injection(origin)) = &message.origin else {
        return state.to_vec();
    };
    if !matches!(origin.variant.as_str(), "environment_binding" | "project_context") {
        return state.to_vec();
    }
    state
        .iter()
        .filter(|existing| {
            !matches!(
                &existing.origin,
                Some(PromptOrigin::Injection(existing_origin))
                    if existing_origin.variant == origin.variant
            )
        })
        .cloned()
        .collect()
}

pub fn pop_swarm_mode_reminder(state: &[ContextMessage]) -> Vec<ContextMessage> {
    pop_mode_reminder(state, "swarm_mode")
}

pub fn pop_hello_world_mode_reminder(state: &[ContextMessage]) -> Vec<ContextMessage> {
    pop_mode_reminder(state, "hello_world_mode")
}

fn pop_mode_reminder(state: &[ContextMessage], variant: &str) -> Vec<ContextMessage> {
    let Some(last) = state.last() else {
        return state.to_vec();
    };
    if !matches!(
        &last.origin,
        Some(PromptOrigin::Injection(origin)) if origin.variant == variant
    ) {
        return state.to_vec();
    }
    reset_fold(&state[..state.len() - 1])
}

pub fn apply_context_compaction_record(
    state: &[ContextMessage],
    record: &Value,
) -> Result<Vec<ContextMessage>, WireAsyncError> {
    let input = read_context_compaction_shape_input(record)?;
    let result = build_context_compaction_shape(state, input, &default_token_estimate());
    Ok(reset_fold(&result.messages))
}

pub fn read_context_compaction_shape_input(
    record: &Value,
) -> Result<ContextCompactionShapeInput, WireAsyncError> {
    let kept_user_message_count = read_optional_number(record, "keptUserMessageCount");
    Ok(ContextCompactionShapeInput {
        summary: read_context_compaction_raw_summary(record)?,
        legacy_summary_message: read_legacy_summary_message(record),
        context_summary: read_optional_string(record, "contextSummary"),
        in_flight_turn: None,
        compacted_count: read_context_compacted_count(record)?,
        tokens_before: read_optional_number(record, "tokensBefore").unwrap_or(0),
        tokens_after: read_optional_number(record, "tokensAfter"),
        summary_output_tokens: read_optional_number(record, "summaryOutputTokens"),
        request_overhead_tokens: None,
        kept_user_message_count: kept_user_message_count.map(|count| count as usize),
        kept_head_user_message_count: read_optional_number(record, "keptHeadUserMessageCount")
            .map(|count| count as usize),
        dropped_count: read_optional_number(record, "droppedCount").map(|count| count as usize),
        legacy_tail: Some(
            read_optional_boolean(record, "legacyTail")
                .unwrap_or(kept_user_message_count.is_none()),
        ),
    })
}

pub fn read_context_compacted_count(record: &Value) -> Result<usize, WireAsyncError> {
    if let Some(count) = record.get("compactedCount").and_then(Value::as_u64) {
        return Ok(count as usize);
    }
    if let Some(count) = record.get("count").and_then(Value::as_u64) {
        return Ok(count as usize);
    }
    Err(decode_error(
        "Invalid context.apply_compaction record: missing compactedCount",
        record,
        &[
            (
                "compactedCountType",
                json!(value_type(record.get("compactedCount"))),
            ),
            ("countType", json!(value_type(record.get("count")))),
        ],
    ))
}

pub fn read_context_compaction_summary(
    record: &Value,
) -> Result<ContextMessage, WireAsyncError> {
    if let Some(summary) = read_optional_string(record, "contextSummary") {
        return Ok(create_compaction_summary_message(&summary));
    }
    if let Some(summary) = read_optional_string(record, "summary") {
        return Ok(create_compaction_summary_message(&summary));
    }
    if let Some(summary) = record.get("summary").filter(|value| is_context_message(value)) {
        return Ok(serde_json::from_value(summary.clone())?);
    }
    Err(decode_error(
        "Invalid context.apply_compaction record: missing summary",
        record,
        &[
            ("summaryType", json!(value_type(record.get("summary")))),
            (
                "contextSummaryType",
                json!(value_type(record.get("contextSummary"))),
            ),
        ],
    ))
}

fn read_context_compaction_raw_summary(record: &Value) -> Result<String, WireAsyncError> {
    if let Some(summary) = read_optional_string(record, "summary") {
        return Ok(summary);
    }
    if let Some(summary) = read_optional_string(record, "contextSummary") {
        return Ok(summary);
    }
    if let Some(summary) = record.get("summary").filter(|value| is_context_message(value)) {
        let message: ContextMessage = serde_json::from_value(summary.clone())?;
        return Ok(extract_text(&message.content));
    }
    Err(decode_error(
        "Invalid context.apply_compaction record: missing summary",
        record,
        &[
            ("summaryType", json!(value_type(record.get("summary")))),
            (
                "contextSummaryType",
                json!(value_type(record.get("contextSummary"))),
            ),
        ],
    ))
}

fn read_legacy_summary_message(record: &Value) -> Option<ContextMessage> {
    let summary = record.get("summary").filter(|value| is_context_message(value))?;
    serde_json::from_value(summary.clone()).ok()
}

fn read_optional_number(record: &Value, key: &str) -> Option<u64> {
    record.get(key).and_then(Value::as_u64)
}

fn read_optional_string(record: &Value, key: &str) -> Option<String> {
    record.get(key).and_then(Value::as_str).map(str::to_string)
}

fn read_optional_boolean(record: &Value, key: &str) -> Option<bool> {
    record.get(key).and_then(Value::as_bool)
}

fn is_context_message(value: &Value) -> bool {
    value.get("role").and_then(Value::as_str).is_some()
        && value.get("content").and_then(Value::as_array).is_some()
}

fn value_type(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn decode_error(message: &str, record: &Value, extra: &[(&str, Value)]) -> WireAsyncError {
    let mut details = HashMap::new();
    details.insert(
        "recordKeys".to_string(),
        record
            .as_object()
            .map(|record| {
                Value::Array(
                    record
                        .keys()
                        .map(|key| Value::String(key.clone()))
                        .collect(),
                )
            })
            .unwrap_or_else(|| Value::Array(Vec::new())),
    );
    for (key, value) in extra {
        details.insert((*key).to_string(), value.clone());
    }
    Box::new(Error2::with_options(
        STORAGE_DECODE_FAILED,
        message,
        Error2Options {
            details: Some(details.into_iter().collect::<Map<_, _>>()),
            ..Default::default()
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoCut {
    pub cut_index: usize,
    pub removed_count: u64,
    pub stopped_at_compaction: bool,
}

impl UndoCut {
    fn empty() -> Self {
        Self {
            cut_index: usize::MAX,
            removed_count: 0,
            stopped_at_compaction: false,
        }
    }
}

pub fn compute_undo_cut(state: &[ContextMessage], count: u64) -> UndoCut {
    let mut remaining = count;
    let mut cut = UndoCut::empty();
    for (index, message) in state.iter().enumerate().rev() {
        if remaining == 0 {
            break;
        }
        if matches!(message.origin.as_ref(), Some(PromptOrigin::Injection(_))) {
            continue;
        }
        if matches!(message.origin.as_ref(), Some(PromptOrigin::CompactionSummary(_))) {
            cut.stopped_at_compaction = true;
            break;
        }
        if is_undo_anchor(message) {
            remaining -= 1;
            cut.removed_count += 1;
            cut.cut_index = index;
            while cut.cut_index > 0
                && is_prompt_owned_injection(&state[cut.cut_index - 1], message)
            {
                cut.cut_index -= 1;
            }
        }
    }
    cut
}

pub fn is_fully_undoable(cut: &UndoCut, count: u64) -> bool {
    cut.cut_index != usize::MAX && cut.removed_count >= count
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoUnavailableReason {
    Empty,
    CompactionBoundary,
    Insufficient,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoPrecheck {
    Ok,
    Failed {
        reason: UndoUnavailableReason,
        requested: u64,
        undoable: u64,
    },
}

pub fn precheck_undo(history: &[ContextMessage], count: u64) -> UndoPrecheck {
    let cut = compute_undo_cut(history, count);
    if is_fully_undoable(&cut, count) {
        return UndoPrecheck::Ok;
    }
    let reason = if cut.stopped_at_compaction {
        UndoUnavailableReason::CompactionBoundary
    } else if cut.removed_count == 0 {
        UndoUnavailableReason::Empty
    } else {
        UndoUnavailableReason::Insufficient
    };
    UndoPrecheck::Failed {
        reason,
        requested: count,
        undoable: cut.removed_count,
    }
}

pub fn format_undo_unavailable_message(precheck: &UndoPrecheck) -> Option<String> {
    let UndoPrecheck::Failed {
        reason,
        requested,
        undoable,
    } = precheck
    else {
        return None;
    };
    Some(match reason {
        UndoUnavailableReason::Empty => "Nothing to undo: no user message to undo".to_string(),
        UndoUnavailableReason::CompactionBoundary => {
            "Nothing to undo: would cross a compaction boundary".to_string()
        }
        UndoUnavailableReason::Insufficient => format!(
            "Nothing to undo: only {undoable} of {requested} requested turn(s) available"
        ),
    })
}
