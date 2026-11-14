use serde_json::{Map, Value, json};

use super::loop_fold::TOOL_INTERRUPTED_ON_RESUME_OUTPUT;

const COMPACT_USER_MESSAGE_MAX_TOKENS: i64 = 20_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ContextTranscript {
    pub entries: Vec<Value>,
    pub times: Vec<Option<Value>>,
    pub folded_length: f64,
}

#[derive(Debug, Clone)]
struct Entry {
    message: Value,
    time: Option<Value>,
}

#[derive(Debug, Clone, Default)]
struct SealMeta {
    usage: Option<Value>,
    llm_timing: Option<Value>,
}

#[derive(Debug)]
pub struct ContextTranscriptReducer {
    transcript: Vec<Entry>,
    folded_length: f64,
    clear_floor: usize,
    open_entry: Option<usize>,
    open_step_uuid: Option<String>,
    open_has_tool_calls: bool,
    open_vacuous: bool,
    pending: Vec<(String, String)>,
    deferred: Vec<Entry>,
}

impl ContextTranscriptReducer {
    pub fn new() -> Self {
        Self {
            transcript: Vec::new(),
            folded_length: 0.0,
            clear_floor: 0,
            open_entry: None,
            open_step_uuid: None,
            open_has_tool_calls: false,
            open_vacuous: true,
            pending: Vec::new(),
            deferred: Vec::new(),
        }
    }

    pub fn add(&mut self, record: &Value) {
        let time = record.get("time").cloned();
        match record.get("type").and_then(Value::as_str) {
            Some("context.append_message") => {
                self.append_message(record.get("message").unwrap_or(&Value::Null), time);
            }
            Some("context.append_loop_event") => {
                self.loop_event(record.get("event").unwrap_or(&Value::Null), time);
            }
            Some("context.apply_compaction") => {
                if read_number(record, "keptUserMessageCount").is_some() {
                    self.settle(time.clone());
                } else {
                    self.reset();
                }
                let summary = read_compaction_summary_text(record);
                self.transcript.push(Entry {
                    message: json!({
                        "role": "user",
                        "content": [{"type": "text", "text": summary}],
                        "toolCalls": [],
                        "origin": {"kind": "compaction_summary"},
                    }),
                    time,
                });
                self.folded_length = self.recover_folded_length(record);
            }
            Some("context.undo") => {
                self.apply_undo(record.get("count").and_then(Value::as_u64).unwrap_or(0));
            }
            Some("context.clear") => {
                self.clear_floor = self.transcript.len();
                self.folded_length = 0.0;
                self.reset();
            }
            _ => {}
        }
    }

    pub fn result(&self) -> ContextTranscript {
        ContextTranscript {
            entries: self
                .transcript
                .iter()
                .map(|entry| entry.message.clone())
                .collect(),
            times: self
                .transcript
                .iter()
                .map(|entry| entry.time.clone())
                .collect(),
            folded_length: self.folded_length,
        }
    }

    fn push(&mut self, entry: Entry) {
        self.transcript.push(entry);
        self.folded_length += 1.0;
    }

    fn push_message(&mut self, message: &Value, time: Option<Value>) {
        self.push(to_mutable_entry(message, time));
    }

    fn push_tool_message(&mut self, message: Value, time: Option<Value>) {
        self.push(Entry { message, time });
    }

    fn open_assistant(&mut self, time: Option<Value>) {
        self.push(Entry {
            message: json!({"role": "assistant", "content": [], "toolCalls": []}),
            time,
        });
        self.open_entry = Some(self.transcript.len() - 1);
    }

    fn append_open_content(&mut self, part: &Value) {
        let Some(index) = self.open_entry else {
            return;
        };
        if let Some(content) = self.transcript[index]
            .message
            .get_mut("content")
            .and_then(Value::as_array_mut)
        {
            content.push(part.clone());
        }
    }

    fn append_open_tool_call(&mut self, call: Value) {
        let Some(index) = self.open_entry else {
            return;
        };
        if let Some(calls) = self.transcript[index]
            .message
            .get_mut("toolCalls")
            .and_then(Value::as_array_mut)
        {
            calls.push(call);
        }
    }

    fn drop_open_assistant(&mut self) {
        let Some(index) = self.open_entry.take() else {
            return;
        };
        if index < self.transcript.len() {
            self.transcript.remove(index);
            self.folded_length = (self.folded_length - 1.0).max(0.0);
        }
    }

    fn seal_open_assistant(&mut self, meta: Option<SealMeta>) {
        if let Some(index) = self.open_entry
            && let Some(message) = self.transcript[index].message.as_object_mut()
        {
            match meta {
                Some(meta) => {
                    if let Some(usage) = meta.usage {
                        message.insert("usage".to_string(), usage);
                    } else {
                        message.remove("usage");
                    }
                    if let Some(timing) = meta.llm_timing {
                        message.insert("llmTiming".to_string(), timing);
                    } else {
                        message.remove("llmTiming");
                    }
                }
                None => {
                    message.remove("usage");
                    message.remove("llmTiming");
                }
            }
        }
        self.open_entry = None;
    }

    fn flush_deferred(&mut self) {
        if !self.pending.is_empty() || self.deferred.is_empty() {
            return;
        }
        let deferred = std::mem::take(&mut self.deferred);
        for entry in deferred {
            self.push_message(&entry.message, entry.time);
        }
    }

    fn close_pending(&mut self, time: Option<Value>) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        for (tool_call_id, _) in pending {
            self.push_tool_message(
                json!({
                    "role": "tool",
                    "content": [{"type": "text", "text": TOOL_INTERRUPTED_ON_RESUME_OUTPUT}],
                    "toolCalls": [],
                    "toolCallId": tool_call_id,
                    "isError": true,
                }),
                time.clone(),
            );
        }
        self.flush_deferred();
    }

    fn settle_open(&mut self, time: Option<Value>, meta: Option<SealMeta>) {
        if self.open_step_uuid.is_none() {
            return;
        }
        self.close_pending(time);
        if !self.open_has_tool_calls && self.open_vacuous {
            self.drop_open_assistant();
        } else {
            self.seal_open_assistant(meta);
        }
        self.open_step_uuid = None;
    }

    fn accepts_open_step(&self, step_uuid: &str) -> bool {
        self.open_step_uuid.as_deref() == Some(step_uuid)
    }

    fn append_message(&mut self, message: &Value, time: Option<Value>) {
        if !self.pending.is_empty() {
            self.deferred.push(Entry {
                message: message.clone(),
                time,
            });
            return;
        }
        self.push_message(message, time);
    }

    fn loop_event(&mut self, event: &Value, time: Option<Value>) {
        match event.get("type").and_then(Value::as_str) {
            Some("step.begin") => {
                self.settle_open(time.clone(), None);
                self.open_assistant(time);
                self.open_step_uuid = Some(
                    event
                        .get("uuid")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                );
                self.open_has_tool_calls = false;
                self.open_vacuous = true;
            }
            Some("step.end") => {
                if matches!(
                    event.get("finishReason").and_then(Value::as_str),
                    Some("interrupted" | "error")
                ) {
                    return;
                }
                self.settle_open(time, step_end_seal_meta(event));
                self.flush_deferred();
            }
            Some("content.part") => {
                let step_uuid = event
                    .get("stepUuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !self.accepts_open_step(step_uuid) {
                    return;
                }
                let part = event.get("part").unwrap_or(&Value::Null);
                self.append_open_content(part);
                self.open_vacuous = self.open_vacuous && is_vacuous_content_part(part);
            }
            Some("tool.call") => {
                let step_uuid = event
                    .get("stepUuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !self.accepts_open_step(step_uuid) {
                    return;
                }
                let tool_call_id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = event
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = match event.get("args") {
                    None => Value::Null,
                    Some(args) => Value::String(serde_json::to_string(args).unwrap()),
                };
                let mut call = Map::new();
                call.insert("type".to_string(), Value::String("function".to_string()));
                call.insert("id".to_string(), Value::String(tool_call_id.clone()));
                call.insert("name".to_string(), Value::String(name.clone()));
                call.insert("arguments".to_string(), arguments);
                if let Some(extras) = event.get("extras") {
                    call.insert("extras".to_string(), extras.clone());
                }
                self.append_open_tool_call(Value::Object(call));
                if let Some(pending) = self.pending.iter_mut().find(|(id, _)| id == &tool_call_id) {
                    pending.1 = name;
                } else {
                    self.pending.push((tool_call_id, name));
                }
                self.open_has_tool_calls = true;
            }
            Some("tool.result") => {
                let tool_call_id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(index) = self.pending.iter().position(|(id, _)| id == tool_call_id) else {
                    return;
                };
                let (_, tool_name) = self.pending.remove(index);
                let result = event.get("result").unwrap_or(&Value::Null);
                let output = result.get("output").unwrap_or(&Value::Null);
                let content = match output {
                    Value::String(text) => vec![json!({"type": "text", "text": text})],
                    Value::Array(parts) => parts.clone(),
                    _ => Vec::new(),
                };
                let mut message = Map::new();
                message.insert("role".to_string(), Value::String("tool".to_string()));
                message.insert("content".to_string(), Value::Array(content));
                message.insert("toolCalls".to_string(), Value::Array(Vec::new()));
                message.insert(
                    "toolCallId".to_string(),
                    Value::String(tool_call_id.to_string()),
                );
                if let Some(is_error) = result.get("isError") {
                    message.insert("isError".to_string(), is_error.clone());
                }
                if let Some(note) = result.get("note") {
                    message.insert("note".to_string(), note.clone());
                }
                if should_render_wall_time(&tool_name)
                    && let Some(duration) = result.get("durationMs")
                {
                    message.insert("durationMs".to_string(), duration.clone());
                }
                self.push_tool_message(Value::Object(message), time);
                self.flush_deferred();
            }
            _ => {}
        }
    }

    fn settle(&mut self, time: Option<Value>) {
        self.settle_open(time, None);
        self.flush_deferred();
    }

    fn reset(&mut self) {
        self.open_step_uuid = None;
        self.open_entry = None;
        self.open_has_tool_calls = false;
        self.open_vacuous = true;
        self.pending.clear();
        self.deferred.clear();
    }

    fn apply_undo(&mut self, count: u64) {
        if count == 0 {
            return;
        }
        let mut removed_user_count = 0;
        let mut cursor = self.transcript.len();
        while cursor > self.clear_floor {
            cursor -= 1;
            let message = self.transcript[cursor].message.clone();
            if origin_kind(&message) == Some("injection") {
                continue;
            }
            if origin_kind(&message) == Some("compaction_summary") {
                break;
            }
            self.transcript.remove(cursor);
            self.folded_length = (self.folded_length - 1.0).max(0.0);
            if !is_undo_anchor(&message) {
                continue;
            }
            removed_user_count += 1;
            while cursor > self.clear_floor
                && is_prompt_owned_injection(&self.transcript[cursor - 1].message, &message)
            {
                self.transcript.remove(cursor - 1);
                cursor -= 1;
                self.folded_length = (self.folded_length - 1.0).max(0.0);
            }
            if removed_user_count >= count {
                break;
            }
        }
        self.reset();
    }

    fn recover_folded_length(&self, record: &Value) -> f64 {
        let kept_user_message_count = read_number(record, "keptUserMessageCount");
        let kept_head_user_message_count = read_number(record, "keptHeadUserMessageCount");
        let compacted_count = read_number(record, "compactedCount");
        if let Some(kept) = kept_user_message_count {
            return kept
                + if kept_head_user_message_count.is_none() {
                    2.0
                } else {
                    3.0
                };
        }
        if let Some(compacted) = compacted_count
            && compacted < self.folded_length
        {
            return 1.0 + (self.folded_length - compacted);
        }
        let selected = select_recent_user_message_count(
            &self.transcript[self.clear_floor..],
            COMPACT_USER_MESSAGE_MAX_TOKENS,
        );
        selected as f64 + 1.0
    }
}

impl Default for ContextTranscriptReducer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn create_context_transcript_reducer() -> ContextTranscriptReducer {
    ContextTranscriptReducer::new()
}

pub fn reduce_context_transcript<'a, I>(records: I) -> ContextTranscript
where
    I: IntoIterator<Item = &'a Value>,
{
    let mut reducer = create_context_transcript_reducer();
    for record in records {
        reducer.add(record);
    }
    reducer.result()
}

fn to_mutable_entry(message: &Value, time: Option<Value>) -> Entry {
    let mut out = Map::new();
    if let Some(id) = message.get("id") {
        out.insert("id".to_string(), id.clone());
    }
    out.insert(
        "role".to_string(),
        message.get("role").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "content".to_string(),
        message.get("content").cloned().unwrap_or_else(|| json!([])),
    );
    out.insert(
        "toolCalls".to_string(),
        message
            .get("toolCalls")
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    for key in [
        "toolCallId",
        "isError",
        "origin",
        "usage",
        "llmTiming",
        "tools",
    ] {
        if let Some(value) = message.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    Entry {
        message: Value::Object(out),
        time,
    }
}

fn step_end_seal_meta(event: &Value) -> Option<SealMeta> {
    let usage = event.get("usage").cloned();
    let mut timing = Map::new();
    for key in ["llmFirstTokenLatencyMs", "llmStreamDurationMs"] {
        if let Some(value) = event.get(key) {
            timing.insert(key.to_string(), value.clone());
        }
    }
    if usage.is_none() && timing.is_empty() {
        return None;
    }
    Some(SealMeta {
        usage,
        llm_timing: (!timing.is_empty()).then(|| Value::Object(timing)),
    })
}

fn read_compaction_summary_text(record: &Value) -> String {
    if let Some(summary) = record.get("summary").and_then(Value::as_str) {
        return summary.to_string();
    }
    if let Some(summary) = record.get("contextSummary").and_then(Value::as_str) {
        return summary.to_string();
    }
    let Some(summary) = record.get("summary") else {
        return String::new();
    };
    if summary.get("role").and_then(Value::as_str).is_none() {
        return String::new();
    }
    summary
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

fn read_number(record: &Value, key: &str) -> Option<f64> {
    record.get(key).and_then(Value::as_f64)
}

fn origin_kind(message: &Value) -> Option<&str> {
    message.get("origin")?.get("kind")?.as_str()
}

fn is_undo_anchor(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(origin) = message.get("origin") else {
        return true;
    };
    if origin.get("inTurn").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match origin.get("kind").and_then(Value::as_str) {
        Some("user") => true,
        Some("skill_activation" | "plugin_command") => {
            origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
        }
        _ => false,
    }
}

fn is_prompt_owned_injection(message: &Value, prompt: &Value) -> bool {
    if origin_kind(message) != Some("injection") {
        return false;
    }
    let Some(owner) = message
        .get("origin")
        .and_then(|origin| origin.get("ownerPromptId"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    prompt.get("id").and_then(Value::as_str) == Some(owner)
}

fn is_vacuous_content_part(part: &Value) -> bool {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => part
            .get("text")
            .and_then(Value::as_str)
            .is_none_or(js_trim_is_empty),
        Some("think") => {
            part.get("encrypted").is_none()
                && part
                    .get("think")
                    .and_then(Value::as_str)
                    .is_none_or(js_trim_is_empty)
        }
        _ => false,
    }
}

fn js_trim_is_empty(text: &str) -> bool {
    text.trim_matches(is_javascript_whitespace).is_empty()
}

fn is_javascript_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
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

fn select_recent_user_message_count(entries: &[Entry], max_tokens: i64) -> usize {
    let messages: Vec<&Value> = entries
        .iter()
        .map(|entry| &entry.message)
        .filter(|message| is_compactable_user_message(message))
        .collect();
    let mut selected = 0;
    let mut remaining = max_tokens;
    for message in messages.into_iter().rev() {
        if remaining <= 0 {
            break;
        }
        let tokens = estimate_message_tokens(message) as i64;
        selected += 1;
        if tokens <= remaining {
            remaining -= tokens;
        } else {
            break;
        }
    }
    selected
}

fn is_compactable_user_message(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(origin) = message.get("origin") else {
        return true;
    };
    match origin.get("kind").and_then(Value::as_str) {
        Some("user") => true,
        Some("skill_activation" | "plugin_command") => {
            origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
        }
        _ => false,
    }
}

fn should_render_wall_time(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Agent" | "AgentSwarm" | "Bash" | "FetchURL" | "Glob" | "Grep" | "WebSearch"
    ) || tool_name.starts_with("mcp__")
}

fn estimate_message_tokens(message: &Value) -> u32 {
    let mut total = message
        .get("role")
        .and_then(Value::as_str)
        .map(crate::compaction::estimate_tokens)
        .unwrap_or(0);
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        for part in parts {
            total += match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(crate::compaction::estimate_tokens)
                    .unwrap_or(0),
                Some("think") => part
                    .get("think")
                    .and_then(Value::as_str)
                    .map(crate::compaction::estimate_tokens)
                    .unwrap_or(0),
                Some("image_url" | "audio_url" | "video_url" | "document_url") => {
                    crate::compaction::MEDIA_TOKEN_ESTIMATE
                }
                _ => 0,
            };
        }
    }
    if let Some(calls) = message.get("toolCalls").and_then(Value::as_array) {
        for call in calls {
            total += call
                .get("name")
                .and_then(Value::as_str)
                .map(crate::compaction::estimate_tokens)
                .unwrap_or(0);
            if let Some(arguments) = call.get("arguments") {
                total += serde_json::to_string(arguments)
                    .map(|arguments| crate::compaction::estimate_tokens_for_json(&arguments))
                    .unwrap_or(0);
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}], "toolCalls": []})
    }

    fn user_with_id(text: &str, id: &str) -> Value {
        let mut message = user(text);
        message["id"] = json!(id);
        message["origin"] = json!({"kind": "user"});
        message
    }

    fn injection(text: &str, owner: Option<&str>) -> Value {
        let mut message = user(text);
        message["origin"] =
            json!({"kind": "injection", "variant": "image_compression", "ownerPromptId": owner});
        message
    }

    fn assistant(text: &str) -> Value {
        json!({"role": "assistant", "content": [{"type": "text", "text": text}], "toolCalls": []})
    }

    fn append(message: Value) -> Value {
        json!({"type": "context.append_message", "message": message})
    }

    fn loop_record(event: Value) -> Value {
        json!({"type": "context.append_loop_event", "event": event})
    }

    fn assistant_step(uuid: &str, text: &str) -> Vec<Value> {
        vec![
            loop_record(json!({"type": "step.begin", "uuid": uuid})),
            loop_record(
                json!({"type": "content.part", "stepUuid": uuid, "part": {"type": "text", "text": text}}),
            ),
            loop_record(json!({"type": "step.end", "uuid": uuid})),
        ]
    }

    fn compaction(summary: &str, compacted: f64, kept: Option<f64>, head: Option<f64>) -> Value {
        let mut record = json!({
            "type": "context.apply_compaction",
            "summary": summary,
            "contextSummary": format!("prefixed {summary}"),
            "compactedCount": compacted,
            "tokensBefore": 1000,
            "tokensAfter": 100,
        });
        if let Some(kept) = kept {
            record["keptUserMessageCount"] = json!(kept);
        }
        if let Some(head) = head {
            record["keptHeadUserMessageCount"] = json!(head);
        }
        record
    }

    fn reduce(records: &[Value]) -> ContextTranscript {
        reduce_context_transcript(records.iter())
    }

    fn texts(result: &ContextTranscript) -> Vec<String> {
        result
            .entries
            .iter()
            .map(|message| {
                message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|part| {
                        if part["type"] == "text" {
                            part["text"].as_str().unwrap().to_string()
                        } else {
                            format!("[{}]", part["type"].as_str().unwrap())
                        }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn builds_entries_times_and_folded_length() {
        let mut records = vec![append(user("u1"))];
        records.extend(assistant_step("s1", "a1"));
        let result = reduce(&records);
        assert_eq!(texts(&result), ["u1", "a1"]);
        assert_eq!(result.folded_length, 2.0);
        assert_eq!(result.times.len(), result.entries.len());
    }

    #[test]
    fn compaction_recovers_all_recorded_length_forms() {
        let legacy = reduce(&[
            append(user("u1")),
            assistant_step("s1", "a1")[0].clone(),
            loop_record(
                json!({"type": "content.part", "stepUuid": "s1", "part": {"type": "text", "text": "a1"}}),
            ),
            loop_record(json!({"type": "step.end", "uuid": "s1"})),
            append(user("u2")),
            compaction("SUM", 3.0, None, None),
            append(user("u3")),
        ]);
        assert_eq!(texts(&legacy), ["u1", "a1", "u2", "SUM", "u3"]);
        assert_eq!(legacy.folded_length, 4.0);
        assert_eq!(
            legacy.entries[3]["origin"],
            json!({"kind": "compaction_summary"})
        );

        let kept = reduce(&[
            append(user("u1")),
            append(user("u2")),
            append(user("u3")),
            compaction("SUM", 3.0, Some(1.0), None),
            append(user("u4")),
        ]);
        assert_eq!(kept.folded_length, 4.0);

        let head = reduce(&[
            append(user("u1")),
            append(user("u2")),
            compaction("SUM", 2.0, Some(2.0), Some(1.0)),
        ]);
        assert_eq!(head.folded_length, 5.0);
    }

    #[test]
    fn compaction_legacy_fallback_estimates_recent_user_messages() {
        let records = vec![
            append(user(&"界".repeat(30_000))),
            append(user(&"界".repeat(30_000))),
            append(user(&"界".repeat(30_000))),
            compaction("SUM", 3.0, None, None),
        ];
        assert_eq!(reduce(&records).folded_length, 2.0);
    }

    #[test]
    fn carries_record_times_and_step_end_metadata() {
        let records = vec![
            json!({"type": "context.append_message", "message": user("u1"), "time": 100}),
            json!({"type": "context.append_loop_event", "event": {"type": "step.begin", "uuid": "st1"}, "time": 200}),
            json!({"type": "context.append_loop_event", "event": {"type": "tool.call", "stepUuid": "st1", "toolCallId": "c1", "name": "Bash"}, "time": 210}),
            json!({"type": "context.append_loop_event", "event": {"type": "tool.result", "toolCallId": "c1", "result": {"output": "ok", "isError": false}}, "time": 220}),
            json!({"type": "context.append_loop_event", "event": {"type": "step.end", "uuid": "st1", "usage": {"inputOther": 10, "output": 20}, "llmFirstTokenLatencyMs": 800, "llmStreamDurationMs": 5000}, "time": 230}),
            append(user("u2")),
        ];
        let result = reduce(&records);
        assert_eq!(
            result.times,
            vec![Some(json!(100)), Some(json!(200)), Some(json!(220)), None]
        );
        assert_eq!(
            result.entries[1]["usage"],
            json!({"inputOther": 10, "output": 20})
        );
        assert_eq!(
            result.entries[1]["llmTiming"],
            json!({"llmFirstTokenLatencyMs": 800, "llmStreamDurationMs": 5000})
        );
    }

    #[test]
    fn undo_preserves_the_previous_exchange_and_stops_at_compaction() {
        let without_compaction = reduce(&[
            append(user("message A")),
            append(assistant("reply A")),
            append(user("message B")),
            append(assistant("reply B")),
            json!({"type": "context.undo", "count": 1}),
        ]);
        assert_eq!(texts(&without_compaction), ["message A", "reply A"]);

        let with_compaction = reduce(&[
            append(user("message A")),
            append(assistant("reply A")),
            compaction("summary text", 2.0, Some(1.0), None),
            append(user("message B")),
            append(assistant("reply B")),
            json!({"type": "context.undo", "count": 1}),
        ]);
        assert_eq!(
            texts(&with_compaction),
            ["message A", "reply A", "summary text"]
        );
        assert_eq!(with_compaction.folded_length, 3.0);

        let floor = reduce(&[
            append(user("old")),
            compaction("SUM", 1.0, Some(1.0), None),
            append(user("recent")),
            append(assistant("answer")),
            json!({"type": "context.undo", "count": 2}),
        ]);
        assert_eq!(texts(&floor), ["old", "SUM"]);
    }

    #[test]
    fn undo_removes_only_injections_owned_by_removed_prompts() {
        let multi = reduce(&[
            append(injection("injA", Some("p1"))),
            append(user_with_id("u1", "p1")),
            append(assistant("a1")),
            append(injection("injB", Some("p2"))),
            append(user_with_id("u2", "p2")),
            append(assistant("a2")),
            json!({"type": "context.undo", "count": 2}),
        ]);
        assert!(multi.entries.is_empty());
        assert_eq!(multi.folded_length, 0.0);

        let reused = reduce(&[
            append(injection("injA", Some("shared"))),
            append(user_with_id("u1", "shared")),
            append(assistant("a1")),
            append(injection("injB", Some("shared"))),
            append(user_with_id("u2", "shared")),
            append(assistant("a2")),
            json!({"type": "context.undo", "count": 1}),
        ]);
        assert_eq!(texts(&reused), ["injA", "u1", "a1"]);

        let unowned = reduce(&[
            append(injection("note", None)),
            append(user("u1")),
            append(assistant("a1")),
            json!({"type": "context.undo", "count": 1}),
        ]);
        assert_eq!(texts(&unowned), ["note"]);
        assert_eq!(unowned.folded_length, 1.0);
    }

    #[test]
    fn clear_retains_entries_but_bounds_folded_view_and_undo() {
        let cleared = reduce(&[
            append(user("u1")),
            append(user("u2")),
            json!({"type": "context.clear"}),
            append(user("u3")),
        ]);
        assert_eq!(texts(&cleared), ["u1", "u2", "u3"]);
        assert_eq!(cleared.folded_length, 1.0);

        let undone = reduce(&[
            append(user("u1")),
            json!({"type": "context.clear"}),
            append(user("u2")),
            append(assistant("a2")),
            json!({"type": "context.undo", "count": 1}),
        ]);
        assert_eq!(texts(&undone), ["u1"]);
        assert_eq!(undone.folded_length, 0.0);
    }

    #[test]
    fn folds_tool_calls_results_and_deferred_messages_in_order() {
        let records = vec![
            append(user("q")),
            loop_record(json!({"type": "step.begin", "uuid": "s1"})),
            loop_record(
                json!({"type": "content.part", "stepUuid": "s1", "part": {"type": "text", "text": "hi"}}),
            ),
            loop_record(
                json!({"type": "tool.call", "stepUuid": "s1", "toolCallId": "c1", "name": "Bash", "args": {"command": "echo hi"}, "extras": {"x": 1}}),
            ),
            loop_record(
                json!({"type": "tool.call", "stepUuid": "s1", "toolCallId": "c2", "name": "Read"}),
            ),
            append(user("later")),
            loop_record(
                json!({"type": "tool.result", "toolCallId": "c1", "result": {"output": "hi", "isError": false, "note": "note", "durationMs": 12}}),
            ),
            loop_record(
                json!({"type": "tool.result", "toolCallId": "c2", "result": {"output": [{"type": "text", "text": "read"}], "durationMs": 20}}),
            ),
            loop_record(json!({"type": "step.end", "uuid": "s1"})),
        ];
        let result = reduce(&records);
        assert_eq!(texts(&result), ["q", "hi", "hi", "read", "later"]);
        assert_eq!(
            result.entries[1]["toolCalls"][0]["arguments"],
            json!("{\"command\":\"echo hi\"}")
        );
        assert_eq!(result.entries[1]["toolCalls"][0]["extras"], json!({"x": 1}));
        assert_eq!(result.entries[1]["toolCalls"][1]["arguments"], Value::Null);
        assert_eq!(result.entries[2]["durationMs"], json!(12));
        assert_eq!(result.entries[2]["note"], json!("note"));
        assert!(result.entries[3].get("durationMs").is_none());
        assert_eq!(result.folded_length, 5.0);
    }

    #[test]
    fn drops_vacuous_attempts_and_keeps_reasoning_output() {
        let result = reduce(&[
            append(user("q")),
            loop_record(json!({"type": "step.begin", "uuid": "s1"})),
            loop_record(
                json!({"type": "content.part", "stepUuid": "s1", "part": {"type": "think", "think": ""}}),
            ),
            loop_record(json!({"type": "step.begin", "uuid": "s2"})),
            loop_record(
                json!({"type": "content.part", "stepUuid": "s2", "part": {"type": "think", "think": "real"}}),
            ),
            loop_record(json!({"type": "step.end", "uuid": "s2"})),
            loop_record(json!({"type": "step.begin", "uuid": "s3"})),
            loop_record(
                json!({"type": "content.part", "stepUuid": "s3", "part": {"type": "think", "think": "", "encrypted": "sig"}}),
            ),
            loop_record(json!({"type": "step.end", "uuid": "s3"})),
        ]);
        assert_eq!(result.entries.len(), 3);
        assert_eq!(result.entries[0]["role"], "user");
        assert_eq!(result.entries[1]["role"], "assistant");
        assert_eq!(result.entries[2]["role"], "assistant");
        assert_eq!(result.folded_length, 3.0);
        assert!(is_vacuous_content_part(
            &json!({"type": "text", "text": "\u{feff}"})
        ));
        assert!(!is_vacuous_content_part(
            &json!({"type": "text", "text": "\u{0085}"})
        ));
        let mut malformed = user("u");
        malformed["origin"] = json!({});
        assert!(!is_undo_anchor(&malformed));
        assert!(!is_compactable_user_message(&malformed));
    }

    #[test]
    fn compaction_settles_open_output_and_pending_tools() {
        let settled = reduce(&[
            append(user("u1")),
            loop_record(json!({"type": "step.begin", "uuid": "s2"})),
            loop_record(
                json!({"type": "tool.call", "stepUuid": "s2", "toolCallId": "c1", "name": "Bash"}),
            ),
            append(user("deferred")),
            compaction("SUM", 2.0, Some(1.0), None),
        ]);
        assert_eq!(
            texts(&settled),
            [
                "u1",
                "",
                TOOL_INTERRUPTED_ON_RESUME_OUTPUT,
                "deferred",
                "SUM"
            ]
        );
        assert_eq!(settled.entries[2]["isError"], true);

        let legacy = reduce(&[
            append(user("u1")),
            loop_record(json!({"type": "step.begin", "uuid": "s2"})),
            compaction("SUM", 1.0, None, None),
            loop_record(json!({"type": "step.begin", "uuid": "s3"})),
            loop_record(
                json!({"type": "content.part", "stepUuid": "s3", "part": {"type": "text", "text": "a3"}}),
            ),
            loop_record(json!({"type": "step.end", "uuid": "s3"})),
        ]);
        assert_eq!(texts(&legacy), ["u1", "", "SUM", "a3"]);
        assert_eq!(legacy.folded_length, 3.0);
    }

    #[test]
    fn summary_message_and_mutable_projection_match_v2_shape() {
        let mut appended = user("u1");
        appended["id"] = json!("p1");
        appended["note"] = json!("not copied");
        appended["durationMs"] = json!(50);
        appended["usage"] = json!({"output": 1});
        let result = reduce(&[
            append(appended),
            json!({"type": "context.apply_compaction", "summary": {"role": "user", "content": [{"type": "think", "think": "x"}, {"type": "text", "text": "a"}, {"type": "text", "text": "b"}], "toolCalls": []}, "count": 1}),
        ]);
        assert_eq!(result.entries[0]["id"], "p1");
        assert!(result.entries[0].get("note").is_none());
        assert!(result.entries[0].get("durationMs").is_none());
        assert_eq!(result.entries[0]["usage"], json!({"output": 1}));
        assert_eq!(texts(&result), ["u1", "ab"]);
    }

    #[test]
    fn interrupted_step_end_leaves_the_frame_open_for_a_later_settle() {
        let result = reduce(&[
            append(user("q")),
            loop_record(json!({"type": "step.begin", "uuid": "s1"})),
            loop_record(
                json!({"type": "tool.call", "stepUuid": "s1", "toolCallId": "c1", "name": "Bash"}),
            ),
            loop_record(json!({"type": "step.end", "uuid": "s1", "finishReason": "interrupted"})),
            compaction("SUM", 2.0, Some(1.0), None),
        ]);
        assert_eq!(result.entries[2]["isError"], true);
        assert_eq!(
            result.entries[2]["content"][0]["text"],
            TOOL_INTERRUPTED_ON_RESUME_OUTPUT
        );
    }
}
