use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::human::llm::message::{ContentPart, ToolCall, get_text_content};
use crate::human::llm::tokens::{EstimableMessage, estimate_tokens_for_message};
use crate::human::llm::usage::TokenUsage;
use crate::human::persist::v2::wire::{V2WireError, V2WireRecord};
use crate::human::todo::todo_item::{TodoItem, read_todo_items};

const TOOL_INTERRUPTED_ON_RESUME_OUTPUT: &str = "Tool execution was interrupted before its result was recorded. Do not assume the tool completed successfully.";
const COMPACT_USER_MESSAGE_MAX_TOKENS: u64 = 20_000;
const COMPACT_USER_MESSAGE_HEAD_TOKENS: u64 = 2_000;
static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V2PromptOrigin {
    #[serde(default)]
    pub kind: String,
    pub trigger: Option<String>,
    pub variant: Option<String>,
    pub owner_prompt_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V2ContextMessage {
    pub role: String,
    pub content: Vec<ContentPart>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub partial: Option<bool>,
    pub id: Option<String>,
    pub provider_message_id: Option<String>,
    pub origin: Option<V2PromptOrigin>,
    pub is_error: Option<bool>,
    pub note: Option<String>,
    #[serde(skip)]
    pub(crate) identity: u64,
}

impl V2ContextMessage {
    fn new(role: &str, content: Vec<ContentPart>) -> Self {
        Self {
            role: role.to_string(),
            content,
            tool_calls: Some(Vec::new()),
            tool_call_id: None,
            partial: None,
            id: None,
            provider_message_id: None,
            origin: None,
            is_error: None,
            note: None,
            identity: NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn estimable(&self) -> EstimableMessage {
        EstimableMessage {
            role: self.role.clone(),
            content: self.content.clone(),
            tool_calls: self.tool_calls.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct V2Model {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Default)]
pub struct V2AssistantExtra {
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<String>,
    pub raw_finish_reason: Option<String>,
    pub provider_finish_reason: Option<String>,
    pub model: Option<V2Model>,
    pub message_id: Option<String>,
}

pub struct FoldedV2Agent {
    pub messages: Vec<V2ContextMessage>,
    pub next_turn_id: i64,
    pub todos: Vec<TodoItem>,
    pub assistant_extras: HashMap<u64, V2AssistantExtra>,
}

impl FoldedV2Agent {
    pub fn assistant_extra(&self, message: &V2ContextMessage) -> Option<&V2AssistantExtra> {
        self.assistant_extras.get(&message.identity)
    }
}

#[derive(Clone, Debug, Default)]
struct V2LoopEvent {
    type_: String,
    uuid: Option<String>,
    step_uuid: Option<String>,
    turn_id: Option<String>,
    part: Option<ContentPart>,
    tool_call_id: Option<String>,
    name: Option<String>,
    args: Option<Value>,
    extras: Option<Map<String, Value>>,
    result: Option<V2ToolResult>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
    raw_finish_reason: Option<String>,
    provider_finish_reason: Option<String>,
    message_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct V2ToolResult {
    output: Option<Value>,
    is_error: Option<bool>,
    note: Option<String>,
}

fn as_message(value: Option<&Value>) -> Option<V2ContextMessage> {
    let object = value?.as_object()?;
    if !object.get("role").is_some_and(Value::is_string) {
        return None;
    }
    let mut normalized = object.clone();
    if !normalized.get("content").is_some_and(Value::is_array) {
        normalized.insert("content".to_string(), Value::Array(Vec::new()));
    }
    if !normalized.get("toolCalls").is_some_and(Value::is_array) {
        normalized.insert("toolCalls".to_string(), Value::Array(Vec::new()));
    }
    let mut message = serde_json::from_value::<V2ContextMessage>(Value::Object(normalized)).ok()?;
    message.identity = NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
    Some(message)
}

fn is_js_whitespace(character: char) -> bool {
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
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_vacuous_content_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text, .. } => js_trim(text).is_empty(),
        ContentPart::Think {
            think, encrypted, ..
        } => encrypted.is_none() && js_trim(think).is_empty(),
        _ => false,
    }
}

fn is_undo_anchor_origin(origin: Option<&V2PromptOrigin>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    origin.kind == "user"
        || ((origin.kind == "skill_activation" || origin.kind == "plugin_command")
            && origin.trigger.as_deref() == Some("user-slash"))
}

fn is_undo_anchor(message: &V2ContextMessage) -> bool {
    message.role == "user" && is_undo_anchor_origin(message.origin.as_ref())
}

fn is_prompt_owned_injection(message: &V2ContextMessage, prompt: &V2ContextMessage) -> bool {
    let Some(origin) = message.origin.as_ref() else {
        return false;
    };
    origin.kind == "injection"
        && origin.owner_prompt_id.is_some()
        && origin.owner_prompt_id == prompt.id
}

fn origin(kind: &str, variant: Option<&str>) -> Option<V2PromptOrigin> {
    Some(V2PromptOrigin {
        kind: kind.to_string(),
        variant: variant.map(str::to_string),
        ..V2PromptOrigin::default()
    })
}

fn create_compaction_summary_message(text: &str) -> V2ContextMessage {
    let mut message = V2ContextMessage::new(
        "user",
        vec![ContentPart::Text {
            text: text.to_string(),
            meta: None,
        }],
    );
    message.origin = origin("compaction_summary", None);
    message
}

fn create_compaction_elision_message(omitted_tokens: u64) -> V2ContextMessage {
    let text = crate::injection::wrap_system_reminder(&format!(
        "Some of this conversation's user messages were omitted here during compaction: the messages above this note are the oldest user input, the messages below are the most recent, and roughly {omitted_tokens} tokens in between were dropped. The omitted content is covered by the compaction summary at the end of the conversation."
    ));
    let mut message = V2ContextMessage::new(
        "user",
        vec![ContentPart::Text { text, meta: None }],
    );
    message.origin = origin("injection", Some("compaction_elision"));
    message
}

fn create_compaction_continuation_message() -> V2ContextMessage {
    let text = crate::injection::wrap_system_reminder(
        "Context compaction is complete — continue the work that was in progress when it began.",
    );
    let mut message = V2ContextMessage::new(
        "user",
        vec![ContentPart::Text { text, meta: None }],
    );
    message.origin = origin("injection", Some("compaction_continuation"));
    message
}

fn is_compactable_user_message(message: &V2ContextMessage) -> bool {
    if message.role != "user"
        || message.origin.as_ref().is_some_and(|origin| origin.kind == "compaction_summary")
    {
        return false;
    }
    is_undo_anchor_origin(message.origin.as_ref())
}

struct CompactionShapeInput {
    summary_text: String,
    legacy_summary_message: Option<V2ContextMessage>,
    context_summary: Option<String>,
    compacted_count: f64,
    legacy_tail: bool,
}

fn read_compaction_shape_input(record: &V2WireRecord) -> Result<CompactionShapeInput, V2WireError> {
    let summary = record.get("summary");
    let context_summary = record.get("contextSummary");
    let mut legacy_summary_message = None;
    let summary_text = if let Some(summary) = summary.and_then(Value::as_str) {
        summary.to_string()
    } else if let Some(summary) = context_summary.and_then(Value::as_str) {
        summary.to_string()
    } else {
        let Some(message) = as_message(summary) else {
            return Err(V2WireError::new(
                "invalid-compaction-record",
                "context.apply_compaction record is missing a usable summary",
            ));
        };
        let text = get_text_content(&message.content);
        legacy_summary_message = Some(message);
        text
    };
    let compacted_count = record
        .get("compactedCount")
        .and_then(Value::as_f64)
        .or_else(|| record.get("count").and_then(Value::as_f64))
        .ok_or_else(|| {
            V2WireError::new(
                "invalid-compaction-record",
                "context.apply_compaction record is missing compactedCount",
            )
        })?;
    Ok(CompactionShapeInput {
        summary_text,
        legacy_summary_message,
        context_summary: context_summary
            .and_then(Value::as_str)
            .map(str::to_string),
        compacted_count,
        legacy_tail: record
            .get("legacyTail")
            .and_then(Value::as_bool)
            .unwrap_or(!record.contains_key("keptUserMessageCount")),
    })
}

fn build_compaction_messages(
    history: &[V2ContextMessage],
    input: CompactionShapeInput,
) -> Vec<V2ContextMessage> {
    let context_summary = input.context_summary.unwrap_or(input.summary_text);
    if input.legacy_tail {
        let mut messages = vec![
            input
                .legacy_summary_message
                .unwrap_or_else(|| create_compaction_summary_message(&context_summary)),
        ];
        messages.extend(
            history[js_slice_index(input.compacted_count, history.len())..]
                .iter()
                .cloned(),
        );
        return messages;
    }
    let compactable = history
        .iter()
        .filter(|message| is_compactable_user_message(message))
        .cloned()
        .collect::<Vec<_>>();
    let selection = select_compaction_user_messages(
        &compactable,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        COMPACT_USER_MESSAGE_HEAD_TOKENS,
    );
    let mut messages = selection.head;
    if selection.elided {
        messages.push(create_compaction_elision_message(selection.omitted_tokens));
    }
    messages.extend(selection.tail);
    messages.push(create_compaction_summary_message(&context_summary));
    messages.push(create_compaction_continuation_message());
    messages
}

fn js_slice_index(value: f64, len: usize) -> usize {
    if value.is_nan() {
        return 0;
    }
    let integer = value.trunc();
    if integer < 0.0 {
        (len as f64 + integer).max(0.0) as usize
    } else {
        (integer as usize).min(len)
    }
}

struct CompactionSelection {
    head: Vec<V2ContextMessage>,
    tail: Vec<V2ContextMessage>,
    elided: bool,
    omitted_tokens: u64,
}

fn select_compaction_user_messages(
    messages: &[V2ContextMessage],
    max_tokens: u64,
    head_tokens: u64,
) -> CompactionSelection {
    let total_tokens = messages
        .iter()
        .map(|message| estimate_tokens_for_message(&message.estimable(), None))
        .sum::<u64>();
    if total_tokens <= max_tokens {
        return CompactionSelection {
            head: Vec::new(),
            tail: messages.to_vec(),
            elided: false,
            omitted_tokens: 0,
        };
    }
    let head_budget = head_tokens.min(max_tokens);
    let mut tail_remaining = max_tokens - head_budget;
    let mut tail = Vec::new();
    let mut head_end_exclusive = messages.len();
    let mut tail_boundary_dropped_prefix = None;
    for index in (0..messages.len()).rev() {
        if tail_remaining == 0 {
            break;
        }
        let message = &messages[index];
        let tokens = estimate_tokens_for_message(&message.estimable(), None);
        if tokens <= tail_remaining {
            tail.push(message.clone());
            tail_remaining -= tokens;
            head_end_exclusive = index;
            continue;
        }
        let full_text = get_text_content(&message.content);
        let kept_suffix = truncate_text_to_tokens_from_end(&full_text, tail_remaining);
        tail.push(replace_message_text(message, &kept_suffix));
        head_end_exclusive = index;
        let dropped_prefix = full_text[..full_text.len() - kept_suffix.len()].to_string();
        if !dropped_prefix.is_empty() {
            tail_boundary_dropped_prefix = Some(replace_message_text(message, &dropped_prefix));
        }
        break;
    }
    tail.reverse();
    let mut head_candidates = messages[..head_end_exclusive].to_vec();
    if let Some(prefix) = tail_boundary_dropped_prefix {
        head_candidates.push(prefix);
    }
    let mut head = Vec::new();
    let mut head_remaining = head_budget;
    for message in &head_candidates {
        if head_remaining == 0 {
            break;
        }
        let tokens = estimate_tokens_for_message(&message.estimable(), None);
        if tokens <= head_remaining {
            head.push(message.clone());
            head_remaining -= tokens;
        } else {
            let full_text = get_text_content(&message.content);
            let truncated = truncate_text_to_tokens(&full_text, head_remaining);
            head.push(replace_message_text(message, &truncated));
            break;
        }
    }
    let kept_tokens = head
        .iter()
        .chain(tail.iter())
        .map(|message| estimate_tokens_for_message(&message.estimable(), None))
        .sum::<u64>();
    CompactionSelection {
        head,
        tail,
        elided: true,
        omitted_tokens: total_tokens.saturating_sub(kept_tokens),
    }
}

fn replace_message_text(message: &V2ContextMessage, text: &str) -> V2ContextMessage {
    let mut replaced = message.clone();
    replaced.identity = NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
    replaced.content = message
        .content
        .iter()
        .filter(|part| {
            matches!(
                part,
                ContentPart::Text { meta: Some(meta), .. }
                    if matches!(
                        meta.get("source").and_then(Value::as_str),
                        Some("skill activation" | "user prompt submit hook")
                    )
            )
        })
        .cloned()
        .chain(std::iter::once(ContentPart::Text {
            text: text.to_string(),
            meta: None,
        }))
        .collect();
    replaced.tool_calls = Some(Vec::new());
    replaced
}

fn truncate_text_to_tokens(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii = 0_u64;
    let mut non_ascii = 0_u64;
    let mut end = 0;
    for character in text.chars() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
        if ascii.div_ceil(4) + non_ascii > max_tokens {
            break;
        }
        end += character.len_utf8();
    }
    text[..end].to_string()
}

fn truncate_text_to_tokens_from_end(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii = 0_u64;
    let mut non_ascii = 0_u64;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
        if ascii.div_ceil(4) + non_ascii > max_tokens {
            break;
        }
        start = index;
    }
    text[start..].to_string()
}

struct UndoCut {
    cut_index: Option<usize>,
    removed_count: f64,
}

fn compute_undo_cut(state: &[V2ContextMessage], count: f64) -> UndoCut {
    let mut remaining = count;
    let mut cut_index = None;
    let mut removed_count = 0.0;
    for index in (0..state.len()).rev() {
        if !(remaining > 0.0) {
            break;
        }
        let message = &state[index];
        if message.origin.as_ref().is_some_and(|origin| origin.kind == "injection") {
            continue;
        }
        if message
            .origin
            .as_ref()
            .is_some_and(|origin| origin.kind == "compaction_summary")
        {
            break;
        }
        if is_undo_anchor(message) {
            remaining -= 1.0;
            removed_count += 1.0;
            let mut cut = index;
            while cut > 0 && is_prompt_owned_injection(&state[cut - 1], message) {
                cut -= 1;
            }
            cut_index = Some(cut);
        }
    }
    UndoCut {
        cut_index,
        removed_count,
    }
}

struct FoldState {
    messages: Vec<V2ContextMessage>,
    assistant_extras: HashMap<u64, V2AssistantExtra>,
    open_index: Option<usize>,
    open_step_uuid: Option<String>,
    open_has_tool_calls: bool,
    open_vacuous: bool,
    step_extra: Option<V2AssistantExtra>,
    last_model: Option<V2Model>,
    pending: IndexSet<String>,
    deferred: Vec<V2ContextMessage>,
    todos: Vec<TodoItem>,
    next_turn_id: i64,
    cancelled_turn_ids: Vec<f64>,
}

impl FoldState {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            assistant_extras: HashMap::new(),
            open_index: None,
            open_step_uuid: None,
            open_has_tool_calls: false,
            open_vacuous: true,
            step_extra: None,
            last_model: None,
            pending: IndexSet::new(),
            deferred: Vec::new(),
            todos: Vec::new(),
            next_turn_id: 0,
            cancelled_turn_ids: Vec::new(),
        }
    }

    fn advance_turn_clock(&mut self, mut target: i64) {
        self.cancelled_turn_ids.retain(|id| *id >= target as f64);
        while self.cancelled_turn_ids.contains(&(target as f64)) {
            self.cancelled_turn_ids.retain(|id| *id != target as f64);
            target += 1;
        }
        self.next_turn_id = target;
    }

    fn reset_fold(&mut self) {
        self.open_index = None;
        self.open_step_uuid = None;
        self.open_has_tool_calls = false;
        self.open_vacuous = true;
        self.step_extra = None;
        self.pending.clear();
        self.deferred.clear();
    }

    fn flush_deferred(&mut self) {
        if !self.pending.is_empty() || self.deferred.is_empty() {
            return;
        }
        self.messages.extend(std::mem::take(&mut self.deferred));
    }

    fn close_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        for tool_call_id in std::mem::take(&mut self.pending) {
            let mut message = V2ContextMessage::new(
                "tool",
                vec![ContentPart::Text {
                    text: TOOL_INTERRUPTED_ON_RESUME_OUTPUT.to_string(),
                    meta: None,
                }],
            );
            message.tool_call_id = Some(tool_call_id);
            message.is_error = Some(true);
            self.messages.push(message);
        }
        self.flush_deferred();
    }

    fn settle_open(&mut self) {
        if self.open_step_uuid.is_none() {
            return;
        }
        self.close_pending();
        if let Some(open_index) = self.open_index {
            if !self.open_has_tool_calls && self.open_vacuous {
                self.messages.remove(open_index);
            } else {
                let open = &mut self.messages[open_index];
                open.partial = None;
                let mut extra = self.step_extra.take().unwrap_or_default();
                if extra.model.is_none() {
                    extra.model = self.last_model.clone();
                }
                if extra.usage.is_some()
                    || extra.finish_reason.is_some()
                    || extra.model.is_some()
                    || extra.message_id.is_some()
                {
                    self.assistant_extras.insert(open.identity, extra);
                }
            }
        }
        self.open_index = None;
        self.open_step_uuid = None;
        self.step_extra = None;
    }

    fn accepts_open_step(&self, step_uuid: Option<&str>) -> bool {
        self.open_step_uuid.as_deref().is_some_and(|open| Some(open) == step_uuid)
    }

    fn fold_loop_event(&mut self, event: V2LoopEvent) {
        match event.type_.as_str() {
            "step.begin" => {
                self.settle_open();
                let mut message = V2ContextMessage::new("assistant", Vec::new());
                message.partial = Some(true);
                self.messages.push(message);
                self.open_index = Some(self.messages.len() - 1);
                self.open_step_uuid = event.uuid;
                self.open_has_tool_calls = false;
                self.open_vacuous = true;
            }
            "step.end" => {
                if matches!(event.finish_reason.as_deref(), Some("interrupted" | "error")) {
                    return;
                }
                if self.open_step_uuid.is_some() {
                    self.step_extra = Some(V2AssistantExtra {
                        usage: event.usage,
                        finish_reason: event.finish_reason,
                        raw_finish_reason: event.raw_finish_reason,
                        provider_finish_reason: event.provider_finish_reason,
                        model: None,
                        message_id: event.message_id,
                    });
                }
                self.settle_open();
                self.flush_deferred();
            }
            "content.part" => {
                if !self.accepts_open_step(event.step_uuid.as_deref()) {
                    return;
                }
                let (Some(open_index), Some(part)) = (self.open_index, event.part) else {
                    return;
                };
                self.messages[open_index].content.push(part.clone());
                self.open_vacuous = self.open_vacuous && is_vacuous_content_part(&part);
            }
            "tool.call" => {
                if !self.accepts_open_step(event.step_uuid.as_deref()) {
                    return;
                }
                let (Some(open_index), Some(tool_call_id)) = (self.open_index, event.tool_call_id)
                else {
                    return;
                };
                let arguments = event
                    .args
                    .map(|args| serde_json::to_string(&args).expect("serializing JSON cannot fail"));
                let call = ToolCall {
                    id: tool_call_id.clone(),
                    name: event.name.unwrap_or_default(),
                    arguments,
                    extras: event.extras,
                    raw_id: None,
                    _stream_index: None,
                };
                if let Some(calls) = self.messages[open_index].tool_calls.as_mut() {
                    calls.push(call);
                }
                self.pending.insert(tool_call_id);
                self.open_has_tool_calls = true;
            }
            "tool.result" => {
                let Some(tool_call_id) = event.tool_call_id else {
                    return;
                };
                if !self.pending.shift_remove(&tool_call_id) {
                    return;
                }
                let result = event.result.unwrap_or_default();
                let content = match result.output {
                    Some(Value::String(text)) => vec![ContentPart::Text {
                        text,
                        meta: None,
                    }],
                    Some(Value::Array(parts)) => serde_json::from_value(Value::Array(parts))
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let mut message = V2ContextMessage::new("tool", content);
                message.tool_call_id = Some(tool_call_id);
                message.is_error = result.is_error;
                message.note = result.note;
                self.messages.push(message);
                self.flush_deferred();
            }
            _ => {}
        }
    }
}

fn loop_event(value: Option<&Value>) -> Option<V2LoopEvent> {
    let object = value?.as_object()?;
    Some(V2LoopEvent {
        type_: object.get("type")?.as_str()?.to_string(),
        uuid: string_field(object, "uuid"),
        step_uuid: string_field(object, "stepUuid"),
        turn_id: string_field(object, "turnId"),
        part: object.get("part").cloned().and_then(|part| serde_json::from_value(part).ok()),
        tool_call_id: string_field(object, "toolCallId"),
        name: string_field(object, "name"),
        args: object.get("args").cloned(),
        extras: object.get("extras").and_then(Value::as_object).cloned(),
        result: object.get("result").and_then(Value::as_object).map(|result| V2ToolResult {
            output: result.get("output").cloned(),
            is_error: result.get("isError").and_then(Value::as_bool),
            note: string_field(result, "note"),
        }),
        finish_reason: string_field(object, "finishReason"),
        usage: object
            .get("usage")
            .cloned()
            .and_then(|usage| serde_json::from_value(usage).ok()),
        raw_finish_reason: string_field(object, "rawFinishReason"),
        provider_finish_reason: string_field(object, "providerFinishReason"),
        message_id: string_field(object, "messageId"),
    })
}

fn string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_js_integer(text: &str) -> Option<i64> {
    let text = text.trim_start_matches(is_js_whitespace);
    let (sign, rest) = if let Some(rest) = text.strip_prefix('-') {
        (-1_i64, rest)
    } else if let Some(rest) = text.strip_prefix('+') {
        (1_i64, rest)
    } else {
        (1_i64, text)
    };
    let digits = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok().map(|value| value * sign)
}

pub fn fold_v2_wire_records(
    records: &[V2WireRecord],
) -> Result<FoldedV2Agent, V2WireError> {
    let mut state = FoldState::new();
    for record in records {
        match record.type_.as_str() {
            "context.append_message" => {
                let Some(message) = as_message(record.get("message")) else {
                    continue;
                };
                if !state.pending.is_empty() {
                    state.deferred.push(message);
                } else {
                    state.messages.push(message);
                }
            }
            "context.append_loop_event" => {
                let Some(event) = loop_event(record.get("event")) else {
                    continue;
                };
                if event.type_ != "tool.result" {
                    if let Some(turn_id) = event.turn_id.as_deref().and_then(parse_js_integer) {
                        if turn_id >= state.next_turn_id {
                            state.advance_turn_clock(turn_id + 1);
                        }
                    }
                }
                state.fold_loop_event(event);
            }
            "context.clear" => {
                state.messages.clear();
                state.reset_fold();
            }
            "context.undo" => {
                let Some(count) = record.get("count").and_then(Value::as_f64) else {
                    continue;
                };
                if state.messages.is_empty() {
                    continue;
                }
                let cut = compute_undo_cut(&state.messages, count);
                if cut.cut_index.is_none() || cut.removed_count < count {
                    continue;
                }
                state.messages.truncate(cut.cut_index.unwrap());
                state.reset_fold();
            }
            "context.apply_compaction" => {
                let input = read_compaction_shape_input(record)?;
                state.messages = build_compaction_messages(&state.messages, input);
                state.reset_fold();
            }
            "turn.prompt" => {
                let target = state.next_turn_id + 1;
                state.advance_turn_clock(target);
            }
            "turn.cancel" => {
                if !record.contains_key("target") {
                    continue;
                }
                let Some(turn_id) = record.get("turnId").and_then(Value::as_f64) else {
                    continue;
                };
                if turn_id < state.next_turn_id as f64 {
                    continue;
                }
                if !state.cancelled_turn_ids.contains(&turn_id) {
                    state.cancelled_turn_ids.push(turn_id);
                }
                let target = state.next_turn_id;
                state.advance_turn_clock(target);
            }
            "llm.request" => {
                state.last_model = Some(V2Model {
                    provider: record
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    model: record
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            "tools.update_store" => {
                if record.get("key").and_then(Value::as_str) == Some("todo") {
                    state.todos = read_todo_items(record.get("value").unwrap_or(&Value::Null));
                }
            }
            _ => {}
        }
    }
    state.settle_open();
    state.flush_deferred();
    Ok(FoldedV2Agent {
        messages: state.messages,
        next_turn_id: state.next_turn_id,
        todos: state.todos,
        assistant_extras: state.assistant_extras,
    })
}
