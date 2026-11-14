use std::sync::Arc;

use parking_lot::Mutex;

use crate::human::llm::message::{ContentPart, Role, ToolCall};
use crate::human::llm::tokens::estimate_tokens_for_message;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireRecord;

use super::compaction_handoff::{
    COMPACT_USER_MESSAGE_MAX_TOKENS, collect_compactable_user_messages, select_recent_user_messages,
};
use super::conversation_time::{is_prompt_owned_injection, is_undo_anchor};
use super::loop_event_fold::{
    AssistantSealMeta, LoopEventFold, LoopEventFoldSink, LoopRecordedEvent, create_loop_event_fold,
};
use super::types::{CompactionSummaryOrigin, ContextMessage, PromptOrigin};

#[derive(Debug, Clone)]
pub struct ContextTranscript {
    pub entries: Vec<ContextMessage>,
    pub times: Vec<Option<i64>>,
    pub folded_length: usize,
}

pub struct ContextTranscriptReducer {
    state: Arc<Mutex<TranscriptState>>,
    fold: LoopEventFold,
}

#[derive(Default)]
struct TranscriptState {
    entries: Vec<TranscriptEntry>,
    folded_length: usize,
    clear_floor: usize,
    open_index: Option<usize>,
}

#[derive(Debug, Clone)]
struct TranscriptEntry {
    message: ContextMessage,
    time: Option<i64>,
}

pub fn reduce_context_transcript(
    records: impl IntoIterator<Item = WireRecord>,
) -> ContextTranscript {
    let mut reducer = create_context_transcript_reducer();
    for record in records {
        reducer.add(record);
    }
    reducer.result()
}

pub fn create_context_transcript_reducer() -> ContextTranscriptReducer {
    let state = Arc::new(Mutex::new(TranscriptState::default()));
    let sink = TranscriptSink {
        state: state.clone(),
    };
    ContextTranscriptReducer {
        state,
        fold: create_loop_event_fold(Box::new(sink)),
    }
}

impl ContextTranscriptReducer {
    pub fn add(&mut self, record: WireRecord) {
        let time = record.get("time").and_then(serde_json::Value::as_i64);
        match record.get("type").and_then(serde_json::Value::as_str) {
            Some("context.append_message") => {
                if let Some(message) = record
                    .get("message")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ContextMessage>(value).ok())
                {
                    self.fold.append_message(message, time);
                }
            }
            Some("context.append_loop_event") => {
                if let Some(event) = record
                    .get("event")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<LoopRecordedEvent>(value).ok())
                {
                    self.fold.loop_event(&event, time);
                }
            }
            Some("context.apply_compaction") => {
                if read_number(&record, "keptUserMessageCount").is_some() {
                    self.fold.settle(time);
                } else {
                    self.reset_open_state();
                }
                let mut message = ContextMessage::new(
                    Role::User,
                    vec![ContentPart::Text {
                        text: read_compaction_summary_text(&record),
                        meta: None,
                    }],
                );
                message.origin = Some(PromptOrigin::CompactionSummary(CompactionSummaryOrigin {
                    kind: "compaction_summary".to_string(),
                }));
                self.state.lock().entries.push(TranscriptEntry { message, time });
                let mut state = self.state.lock();
                state.folded_length = recover_folded_length(&record, &state);
            }
            Some("context.undo") => {
                if let Some(count) = record.get("count").and_then(serde_json::Value::as_u64) {
                    self.apply_undo(count);
                }
            }
            Some("context.clear") => {
                {
                    let mut state = self.state.lock();
                    state.clear_floor = state.entries.len();
                    state.folded_length = 0;
                }
                self.reset_open_state();
            }
            _ => {}
        }
    }

    pub fn result(self) -> ContextTranscript {
        let state = self.state.lock();
        ContextTranscript {
            entries: state.entries.iter().map(|entry| entry.message.clone()).collect(),
            times: state.entries.iter().map(|entry| entry.time).collect(),
            folded_length: state.folded_length,
        }
    }

    fn apply_undo(&mut self, count: u64) {
        if count == 0 {
            return;
        }
        {
            let mut state = self.state.lock();
            let mut removed_user_count = 0;
            let mut index = state.entries.len();
            while index > state.clear_floor && removed_user_count < count {
                index -= 1;
                let message = state.entries[index].message.clone();
                if matches!(message.origin, Some(PromptOrigin::Injection(_))) {
                    continue;
                }
                if matches!(message.origin, Some(PromptOrigin::CompactionSummary(_))) {
                    break;
                }
                state.entries.remove(index);
                state.folded_length = state.folded_length.saturating_sub(1);
                if is_undo_anchor(&message) {
                    removed_user_count += 1;
                    while index > state.clear_floor
                        && is_prompt_owned_injection(&state.entries[index - 1].message, &message)
                    {
                        state.entries.remove(index - 1);
                        index -= 1;
                        state.folded_length = state.folded_length.saturating_sub(1);
                    }
                }
            }
        }
        self.reset_open_state();
    }

    fn reset_open_state(&mut self) {
        self.fold.reset();
        self.state.lock().open_index = None;
    }
}

struct TranscriptSink {
    state: Arc<Mutex<TranscriptState>>,
}

impl TranscriptSink {
    fn push(&self, message: ContextMessage, time: Option<i64>) {
        let mut state = self.state.lock();
        state.entries.push(TranscriptEntry { message, time });
        state.folded_length += 1;
    }
}

impl LoopEventFoldSink for TranscriptSink {
    fn open_assistant(&mut self, time: Option<i64>) {
        let message = ContextMessage::new(Role::Assistant, Vec::new());
        let mut state = self.state.lock();
        state.entries.push(TranscriptEntry { message, time });
        state.folded_length += 1;
        state.open_index = Some(state.entries.len() - 1);
    }

    fn append_open_content(&mut self, part: ContentPart) {
        let mut state = self.state.lock();
        if let Some(open) = state.open_index.and_then(|index| state.entries.get_mut(index)) {
            open.message.content.push(part);
        }
    }

    fn append_open_tool_call(&mut self, call: ToolCall, _display: Option<ToolInputDisplay>) {
        let mut state = self.state.lock();
        if let Some(open) = state.open_index.and_then(|index| state.entries.get_mut(index)) {
            open.message.tool_calls.push(call);
        }
    }

    fn drop_open_assistant(&mut self) {
        let mut state = self.state.lock();
        if let Some(index) = state.open_index.take() {
            state.entries.remove(index);
            state.folded_length = state.folded_length.saturating_sub(1);
        }
    }

    fn seal_open_assistant(&mut self, meta: Option<AssistantSealMeta>) {
        let meta = meta.unwrap_or_default();
        let mut state = self.state.lock();
        if let Some(open) = state.open_index.and_then(|index| state.entries.get_mut(index)) {
            open.message.usage = meta.usage;
            open.message.llm_timing = meta.llm_timing;
        }
        state.open_index = None;
    }

    fn push_tool_message(&mut self, message: ContextMessage, time: Option<i64>) {
        self.push(message, time);
    }

    fn push_message(&mut self, message: ContextMessage, time: Option<i64>) {
        let mut projected = ContextMessage::new(message.role, message.content);
        projected.id = message.id;
        projected.tool_calls = message.tool_calls;
        projected.tool_call_id = message.tool_call_id;
        projected.is_error = message.is_error;
        projected.origin = message.origin;
        projected.usage = message.usage;
        projected.llm_timing = message.llm_timing;
        self.push(projected, time);
    }
}

fn recover_folded_length(record: &WireRecord, state: &TranscriptState) -> usize {
    let kept_user_message_count = read_number(record, "keptUserMessageCount");
    let kept_head_user_message_count = read_number(record, "keptHeadUserMessageCount");
    let compacted_count = read_number(record, "compactedCount");
    if let Some(kept) = kept_user_message_count {
        return kept as usize + if kept_head_user_message_count.is_none() { 2 } else { 3 };
    }
    if let Some(compacted) = compacted_count {
        if (compacted as usize) < state.folded_length {
            return 1 + (state.folded_length - compacted as usize);
        }
    }
    let messages = state.entries[state.clear_floor..]
        .iter()
        .map(|entry| entry.message.clone())
        .collect::<Vec<_>>();
    let compactable = collect_compactable_user_messages(&messages);
    select_recent_user_messages(
        &compactable,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        &|message| estimate_tokens_for_message(&message.estimable(), None),
    )
    .len()
        + 1
}

fn read_compaction_summary_text(record: &WireRecord) -> String {
    if let Some(summary) = record.get("summary").and_then(serde_json::Value::as_str) {
        return summary.to_string();
    }
    if let Some(summary) = record.get("contextSummary").and_then(serde_json::Value::as_str) {
        return summary.to_string();
    }
    if let Some(content) = record
        .get("summary")
        .filter(|summary| {
            summary.get("role").and_then(serde_json::Value::as_str).is_some()
                && summary.get("content").and_then(serde_json::Value::as_array).is_some()
        })
        .and_then(|summary| summary.get("content"))
        .and_then(serde_json::Value::as_array)
    {
        let mut text = String::new();
        for part in content {
            if part.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                if let Some(part_text) = part.get("text").and_then(serde_json::Value::as_str) {
                    text.push_str(part_text);
                }
            }
        }
        return text;
    }
    String::new()
}

fn read_number(record: &WireRecord, key: &str) -> Option<u64> {
    record.get(key).and_then(serde_json::Value::as_u64)
}
