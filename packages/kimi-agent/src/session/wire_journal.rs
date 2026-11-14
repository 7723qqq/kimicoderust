use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::wire_tree;
use crate::callbacks::MAIN_AGENT_ID;
use crate::events::EngineEvent;
use crate::native::event_store::{
    WIRE_MIN_READER_VERSION, WIRE_PROTOCOL_VERSION, compare_wire_versions,
};
use crate::session::sqlite_store::{SqliteSessionStore, encode_workdir_key};
use crate::turn_loop::types::{ContentBlock, LLMMessage};
use kimi_core::types::{MediaKind, ToolCall};

pub const WIRE_FILE_NAME: &str = "wire.jsonl";

const TIMING_KEYS: [(&str, &str); 6] = [
    ("firstTokenLatencyMs", "llmFirstTokenLatencyMs"),
    ("streamDurationMs", "llmStreamDurationMs"),
    ("requestBuildMs", "llmRequestBuildMs"),
    ("serverFirstTokenMs", "llmServerFirstTokenMs"),
    ("serverDecodeMs", "llmServerDecodeMs"),
    ("clientConsumeMs", "llmClientConsumeMs"),
];

pub fn is_wire_journal_path(home: &Path, path: &str) -> bool {
    let sessions = crate::tool_result_truncation::lexical_normalize(&home.join("sessions"));
    let normalized = crate::tool_result_truncation::lexical_normalize(Path::new(path));
    normalized.starts_with(&sessions)
        && normalized.file_name().and_then(|name| name.to_str()) == Some(WIRE_FILE_NAME)
}

pub struct WireJournal {
    home: PathBuf,
    store: Arc<SqliteSessionStore>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    scopes: HashMap<String, Option<String>>,
    agents: HashMap<(String, String), AgentFile>,
}

struct AgentFile {
    path: PathBuf,
    file: Option<std::fs::File>,
    empty: bool,
    open_step: Option<OpenStep>,
    parts: Vec<Value>,
}

#[derive(Clone)]
struct OpenStep {
    uuid: String,
    turn_id: String,
    step: u64,
}

impl AgentFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            empty: true,
            open_step: None,
            parts: Vec::new(),
        }
    }

    fn open(&mut self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.empty = file.metadata()?.len() == 0;
        self.file = Some(file);
        Ok(())
    }

    fn write(&mut self, records: &[Value], durable: bool) -> std::io::Result<()> {
        if self.file.is_none() {
            self.open()?;
        }
        let sealed = self.empty;
        let file = self.file.as_mut().expect("journal file is open");
        if sealed {
            write_line(file, &metadata_record())?;
            self.empty = false;
        }
        for record in records {
            write_line(file, record)?;
        }
        file.flush()?;
        if durable || sealed {
            file.sync_data()?;
        }
        Ok(())
    }

    fn push_part(&mut self, part: &Value) {
        let key = match part.get("type").and_then(Value::as_str) {
            Some("text") => "text",
            Some("think") => "think",
            _ => return,
        };
        if let Some(last) = self.parts.last_mut()
            && last.get("type").and_then(Value::as_str) == Some(key)
        {
            let merged = format!(
                "{}{}",
                last.get(key).and_then(Value::as_str).unwrap_or_default(),
                part.get(key).and_then(Value::as_str).unwrap_or_default()
            );
            last[key] = json!(merged);
            return;
        }
        self.parts.push(part.clone());
    }

    fn take_part_records(&mut self, agent_id: &str, time: i64) -> Vec<Value> {
        let parts = std::mem::take(&mut self.parts);
        let Some(step) = self.open_step.as_ref() else {
            return Vec::new();
        };
        parts
            .into_iter()
            .filter(|part| !is_vacuous_part(part))
            .map(|part| {
                loop_event(
                    agent_id,
                    json!({
                        "type": "content.part",
                        "uuid": ulid::Ulid::new().to_string(),
                        "turnId": step.turn_id,
                        "step": step.step,
                        "stepUuid": step.uuid,
                        "part": part,
                    }),
                    time,
                )
            })
            .collect()
    }

    fn close_step(&mut self, agent_id: &str, time: i64, finish_reason: &str) -> Vec<Value> {
        let mut records = self.take_part_records(agent_id, time);
        let Some(step) = self.open_step.take() else {
            return records;
        };
        records.push(loop_event(
            agent_id,
            json!({
                "type": "step.end",
                "uuid": step.uuid,
                "turnId": step.turn_id,
                "step": step.step,
                "finishReason": finish_reason,
            }),
            time,
        ));
        records
    }
}

impl WireJournal {
    pub fn new(home: impl Into<PathBuf>, store: Arc<SqliteSessionStore>) -> Self {
        Self {
            home: home.into(),
            store,
            state: Mutex::new(State::default()),
        }
    }

    pub fn append(&self, session_id: &str, event: &EngineEvent, now_ms: i64) {
        let mut payload = event.to_json();
        if crate::native::event_store::blobs::has_inline_data_uri(&payload)
            && let Err(error) = self.store.offload_payload(&mut payload)
        {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "could not offload media before appending to the v2 wire journal"
            );
        }
        let Some(event_type) = payload
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        let agent_id = event
            .agent_id()
            .or_else(|| payload.get("agentId").and_then(Value::as_str))
            .unwrap_or(MAIN_AGENT_ID)
            .to_string();
        let mut state = self.state.lock();
        let Some(path) = self.path_for(session_id, &agent_id, &mut state) else {
            return;
        };
        let entry = state
            .agents
            .entry((session_id.to_string(), agent_id.clone()))
            .or_insert_with(|| AgentFile::new(path.clone()));
        if entry.path != path {
            entry.path = path;
            entry.file = None;
            entry.empty = true;
        }
        let records = map_event(&event_type, &payload, &agent_id, now_ms, entry);
        if records.is_empty() {
            return;
        }
        let durable = matches!(
            event_type.as_str(),
            "turn.prompt"
                | "turn.ended"
                | "turn.cancel"
                | "compaction.completed"
                | "context.apply_compaction"
                | "context.undo"
                | "task.started"
                | "task.terminated"
                | "permission.set_mode"
                | "interaction.request"
                | "interaction.resolved"
        );
        if let Err(error) = entry.write(&records, durable) {
            tracing::warn!(
                session_id = %session_id,
                agent_id = %agent_id,
                %error,
                "failed to append the v2 wire journal"
            );
            entry.file = None;
        }
    }

    pub fn rewrite(
        &self,
        session_id: &str,
        agent_id: &str,
        records: &[Value],
    ) -> std::io::Result<()> {
        let mut state = self.state.lock();
        let Some(path) = self.path_for(session_id, agent_id, &mut state) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session has no workspace scope",
            ));
        };
        if let Some(entry) = state
            .agents
            .get_mut(&(session_id.to_string(), agent_id.to_string()))
        {
            entry.file = None;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_file_name(format!("{WIRE_FILE_NAME}.tmp"));
        {
            let mut file = std::fs::File::create(&temporary)?;
            for record in records {
                write_line(&mut file, record)?;
            }
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path)?;
        let entry = state
            .agents
            .entry((session_id.to_string(), agent_id.to_string()))
            .or_insert_with(|| AgentFile::new(path.clone()));
        entry.path = path;
        entry.empty = records.is_empty();
        entry.file = None;
        Ok(())
    }

    pub fn journal_path(&self, session_id: &str, agent_id: &str) -> Option<PathBuf> {
        let mut state = self.state.lock();
        self.path_for(session_id, agent_id, &mut state)
    }

    fn path_for(&self, session_id: &str, agent_id: &str, state: &mut State) -> Option<PathBuf> {
        let scope = match state.scopes.get(session_id) {
            Some(scope) => scope.clone(),
            None => {
                let scope = self
                    .store
                    .get_session(session_id)
                    .ok()
                    .flatten()
                    .and_then(|session| {
                        session
                            .workspace_id
                            .or_else(|| session.cwd.as_deref().map(encode_workdir_key))
                    });
                state.scopes.insert(session_id.to_string(), scope.clone());
                scope
            }
        }?;
        Some(
            self.home
                .join("sessions")
                .join(scope)
                .join(session_id)
                .join("agents")
                .join(agent_id)
                .join(WIRE_FILE_NAME),
        )
    }
}

fn write_line(file: &mut std::fs::File, record: &Value) -> std::io::Result<()> {
    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")
}

fn metadata_record() -> Value {
    json!({
        "type": "metadata",
        "protocol_version": WIRE_PROTOCOL_VERSION,
        "min_protocol_version": WIRE_MIN_READER_VERSION,
        "created_at": chrono::Utc::now().timestamp_millis(),
    })
}

fn map_event(
    event_type: &str,
    payload: &Value,
    agent_id: &str,
    time: i64,
    agent: &mut AgentFile,
) -> Vec<Value> {
    match event_type {
        "turn.prompt" => prompt_records(payload, agent_id, time),
        "turn.steer" => steer_records(payload, agent_id, time),
        "turn.cancel" => cancel_records(payload, agent_id, time),
        "turn.ended" => ended_records(payload, agent_id, time),
        "llm.step.begin" => step_begin_records(payload, agent_id, time, agent),
        "llm.delta" => {
            if let Some(part) = payload.get("part") {
                agent.push_part(part);
            }
            Vec::new()
        }
        "llm.step.retrying" => {
            agent.parts.clear();
            Vec::new()
        }
        "llm.step.interrupted" => {
            let reason = payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("interrupted");
            let finish_reason = if reason == "error" {
                "error"
            } else {
                "interrupted"
            };
            agent.close_step(agent_id, time, finish_reason)
        }
        "llm.step.end" => step_end_records(payload, agent_id, time, agent),
        "tool.call.started" => tool_call_records(payload, agent_id, time, agent),
        "tool.native" => tool_result_records(payload, agent_id, time),
        "compaction.completed" => compaction_records(payload, agent_id, time),
        "context.apply_compaction" => applied_compaction_records(payload, agent_id, time),
        "context.undo" => undo_records(payload, agent_id, time),
        "context.append_message" => append_message_records(payload, agent_id, time),
        "llm.request" => llm_request_records(payload, agent_id, time),
        "tools.update_store" => vec![copied_event(payload, agent_id, time)],
        "permission.set_mode" | "interaction.request" | "interaction.resolved" => {
            vec![copied_event(payload, agent_id, time)]
        }
        "goal.create" | "goal.update" | "goal.clear" => {
            vec![copied_event(payload, agent_id, time)]
        }
        "agent.switched" | "context.undone" => vec![copied_event(payload, agent_id, time)],
        "agent.message.appended" | "agent.turn.started" | "agent.turn.ended" => {
            human_record(payload, time)
        }
        "tool_select.initialized" | "tool_select.tools_changed" => {
            vec![copied_event(payload, agent_id, time)]
        }
        "prompt.submitted" | "prompt.completed" | "prompt.aborted" | "prompt.steered"
        | "subagent.spawned" | "subagent.started" | "subagent.completed" | "subagent.failed"
        | "subagent.cancelled" | "task.started" | "task.terminated" | "task.output"
        | "turn.started" => vec![copied_event(payload, agent_id, time)],
        _ => Vec::new(),
    }
}

fn prompt_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    if payload.get("turnId").is_some_and(|value| !value.is_u64()) {
        return Vec::new();
    }
    if payload.get("input").and_then(Value::as_array).is_none() {
        return Vec::new();
    }
    let mut records = vec![copied_event(payload, agent_id, time)];
    let Some(input) = payload
        .get("input")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    else {
        return records;
    };
    let mut message = json!({
        "role": "user",
        "content": Value::Array(input.clone()),
        "toolCalls": [],
    });
    if let Some(id) = payload.get("promptId").and_then(Value::as_str) {
        message["id"] = json!(id);
    }
    if let Some(origin) = payload.get("origin").filter(|value| value.is_object()) {
        message["origin"] = origin.clone();
    }
    records.push(json!({
        "type": "context.append_message",
        "agentId": agent_id,
        "message": message,
        "time": time,
    }));
    records
}

fn steer_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    let mut records = Vec::new();
    if let Some(input) = payload
        .get("input")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    {
        let mut message = json!({
            "role": "user",
            "content": Value::Array(input.clone()),
            "toolCalls": [],
        });
        if let Some(id) = payload.get("messageId").and_then(Value::as_str) {
            message["id"] = json!(id);
        }
        if let Some(origin) = payload.get("origin").filter(|value| value.is_object()) {
            message["origin"] = origin.clone();
        }
        records.push(json!({
            "type": "context.append_message",
            "agentId": agent_id,
            "message": message,
            "time": time,
        }));
    }
    records.push(copied_event(payload, agent_id, time));
    records
}

fn cancel_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    if payload.get("turnId").is_some_and(|value| !value.is_u64()) {
        return Vec::new();
    }
    vec![copied_event(payload, agent_id, time)]
}

fn ended_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    if payload.get("turnId").and_then(Value::as_u64).is_none() {
        return Vec::new();
    }
    match payload.get("reason").and_then(Value::as_str) {
        Some("completed" | "cancelled" | "failed" | "blocked") => {
            vec![copied_event(payload, agent_id, time)]
        }
        _ => Vec::new(),
    }
}

fn step_begin_records(
    payload: &Value,
    agent_id: &str,
    time: i64,
    agent: &mut AgentFile,
) -> Vec<Value> {
    let turn_id = payload
        .get("turn_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let step = payload.get("step").and_then(Value::as_u64).unwrap_or(0);
    let uuid = payload
        .get("step_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{turn_id}-step-{step}"));
    let mut records = agent.take_part_records(agent_id, time);
    records.push(loop_event(
        agent_id,
        json!({
            "type": "step.begin",
            "uuid": uuid,
            "turnId": turn_id,
            "step": step,
        }),
        time,
    ));
    agent.open_step = Some(OpenStep {
        uuid,
        turn_id,
        step,
    });
    records
}

fn step_end_records(
    payload: &Value,
    agent_id: &str,
    time: i64,
    agent: &mut AgentFile,
) -> Vec<Value> {
    let mut records = agent.take_part_records(agent_id, time);
    let Some(step) = agent.open_step.take() else {
        return records;
    };
    let mut event = json!({
        "type": "step.end",
        "uuid": step.uuid,
        "turnId": step.turn_id,
        "step": step.step,
    });
    let finish_reason = payload
        .get("provider_finish_reason")
        .and_then(Value::as_str)
        .or_else(|| payload.get("finish_reason").and_then(Value::as_str));
    if let Some(reason) = finish_reason {
        event["finishReason"] = json!(reason);
    }
    if let Some(reason) = payload
        .get("provider_finish_reason")
        .and_then(Value::as_str)
    {
        event["providerFinishReason"] = json!(reason);
    }
    if let Some(reason) = payload.get("raw_finish_reason").and_then(Value::as_str) {
        event["rawFinishReason"] = json!(reason);
    }
    if let Some(usage) = payload.get("usage").and_then(usage_record) {
        event["usage"] = usage;
    }
    if let Some(timing) = payload.get("timing").and_then(Value::as_object) {
        for (source, target) in TIMING_KEYS {
            if let Some(value) = timing.get(source).filter(|value| value.is_number()) {
                event[target] = value.clone();
            }
        }
    }
    records.push(loop_event(agent_id, event, time));
    records
}

fn tool_call_records(
    payload: &Value,
    agent_id: &str,
    time: i64,
    agent: &mut AgentFile,
) -> Vec<Value> {
    let Some(step) = agent.open_step.clone() else {
        return Vec::new();
    };
    let Some(tool_call_id) = payload.get("tool_call_id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut event = json!({
        "type": "tool.call",
        "uuid": tool_call_id,
        "turnId": step.turn_id,
        "step": step.step,
        "stepUuid": step.uuid,
        "toolCallId": tool_call_id,
        "name": name,
    });
    if let Some(args) = payload.get("args").filter(|value| !value.is_null()) {
        event["args"] = args.clone();
    }
    if let Some(display) = payload.get("display").filter(|value| !value.is_null()) {
        event["display"] = display.clone();
    }
    let mut records = agent.take_part_records(agent_id, time);
    records.push(loop_event(agent_id, event, time));
    records
}

fn tool_result_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    let Some(tool_call_id) = payload.get("tool_call_id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let output = payload
        .get("content")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    let mut result = json!({ "output": output });
    if let Some(is_error) = payload.get("is_error").and_then(Value::as_bool) {
        result["isError"] = json!(is_error);
    }
    if let Some(note) = payload.get("note").and_then(Value::as_str) {
        result["note"] = json!(note);
    }
    vec![loop_event(
        agent_id,
        json!({
            "type": "tool.result",
            "parentUuid": tool_call_id,
            "toolCallId": tool_call_id,
            "result": result,
        }),
        time,
    )]
}

fn compaction_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    let Some(result) = payload.get("result").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(summary) = result.get("summary").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(compacted_count) = result.get("compactedCount").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let mut record = json!({
        "type": "context.apply_compaction",
        "agentId": agent_id,
        "summary": summary,
        "compactedCount": compacted_count,
    });
    for key in ["tokensBefore", "tokensAfter", "droppedCount"] {
        if let Some(value) = result.get(key).filter(|value| value.is_number()) {
            record[key] = value.clone();
        }
    }
    record["time"] = json!(time);
    vec![record]
}

fn applied_compaction_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    if payload.get("summary").and_then(Value::as_str).is_none()
        || payload
            .get("compactedCount")
            .and_then(Value::as_u64)
            .is_none()
    {
        return Vec::new();
    }
    vec![copied_event(payload, agent_id, time)]
}

fn undo_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    if payload
        .get("count")
        .and_then(Value::as_u64)
        .is_none_or(|count| count == 0)
    {
        return Vec::new();
    }
    vec![copied_event(payload, agent_id, time)]
}

fn append_message_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    if payload.get("message").and_then(Value::as_object).is_none() {
        return Vec::new();
    }
    vec![copied_event(payload, agent_id, time)]
}

fn llm_request_records(payload: &Value, agent_id: &str, time: i64) -> Vec<Value> {
    if payload.get("provider").and_then(Value::as_str).is_none()
        || payload.get("model").and_then(Value::as_str).is_none()
    {
        return Vec::new();
    }
    vec![copied_event(payload, agent_id, time)]
}

fn human_record(payload: &Value, time: i64) -> Vec<Value> {
    let Some(mut object) = payload.as_object().cloned() else {
        return Vec::new();
    };
    object.remove("agent_id");
    object.insert("time".to_string(), json!(time));
    vec![Value::Object(object)]
}

fn copied_event(payload: &Value, agent_id: &str, time: i64) -> Value {
    let mut object = payload.as_object().cloned().unwrap_or_default();
    object.remove("agent_id");
    object
        .entry("agentId".to_string())
        .or_insert_with(|| json!(agent_id));
    object.insert("time".to_string(), json!(time));
    Value::Object(object)
}

fn loop_event(agent_id: &str, event: Value, time: i64) -> Value {
    json!({
        "type": "context.append_loop_event",
        "agentId": agent_id,
        "event": event,
        "time": time,
    })
}

fn usage_record(value: &Value) -> Option<Value> {
    let usage = value.as_object()?;
    let read = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_u64)
            .map(|number| json!(number))
            .unwrap_or_else(|| json!(0))
    };
    Some(json!({
        "inputOther": read("input_tokens"),
        "output": read("output_tokens"),
        "inputCacheRead": read("input_cache_read"),
        "inputCacheCreation": read("input_cache_creation"),
    }))
}

fn is_vacuous_part(part: &Value) -> bool {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => part
            .get("text")
            .and_then(Value::as_str)
            .is_none_or(|text| text.trim().is_empty()),
        Some("think") => {
            part.get("think")
                .and_then(Value::as_str)
                .is_none_or(|think| think.trim().is_empty())
                && part.get("encrypted").is_none()
                && part.get("hidden").and_then(Value::as_bool) != Some(true)
        }
        _ => false,
    }
}

#[derive(Debug)]
pub enum WireReadError {
    NotFound,
    Empty,
    Malformed(String),
    Unsupported(String),
    Io(std::io::Error),
}

impl std::fmt::Display for WireReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireReadError::NotFound => write!(f, "the wire journal does not exist"),
            WireReadError::Empty => write!(f, "the wire journal has no metadata record"),
            WireReadError::Malformed(reason) => {
                write!(f, "the wire journal is malformed: {reason}")
            }
            WireReadError::Unsupported(_) => {
                write!(
                    f,
                    "This session requires a newer version of Kimi Code. Upgrade Kimi Code to the latest version to read this session."
                )
            }
            WireReadError::Io(error) => write!(f, "the wire journal could not be read: {error}"),
        }
    }
}

impl std::error::Error for WireReadError {}

pub fn read_file_lines(path: &Path) -> Result<Vec<wire_tree::WireLine>, WireReadError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(WireReadError::NotFound);
        }
        Err(error) => return Err(WireReadError::Io(error)),
    };
    let mut records = Vec::new();
    let mut declared: Option<String> = None;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| {
            WireReadError::Malformed(format!("line {} is not valid JSON: {error}", index + 1))
        })?;
        let Some(object) = value.as_object() else {
            return Err(WireReadError::Malformed(format!(
                "line {} is not a record object",
                index + 1
            )));
        };
        let Some(record_type) = object.get("type").and_then(Value::as_str) else {
            return Err(WireReadError::Malformed(format!(
                "line {} has no record type",
                index + 1
            )));
        };
        if declared.is_none() {
            if record_type == "metadata" {
                let Some(version) = object.get("protocol_version").and_then(Value::as_str) else {
                    return Err(WireReadError::Malformed(
                        "the metadata record has no protocol_version".to_string(),
                    ));
                };
                if !object.get("created_at").is_some_and(Value::is_number) {
                    return Err(WireReadError::Malformed(
                        "the metadata record has no created_at".to_string(),
                    ));
                }
                if let Some(required) = object.get("min_protocol_version") {
                    let Some(required) = required.as_str().filter(|required| {
                        !required.is_empty()
                            && required.split('.').all(|part| {
                                !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit())
                            })
                    }) else {
                        return Err(WireReadError::Malformed(
                            "the metadata record has an invalid min_protocol_version".to_string(),
                        ));
                    };
                    if compare_wire_versions(WIRE_PROTOCOL_VERSION, required) < 0 {
                        return Err(WireReadError::Unsupported(required.to_string()));
                    }
                }
                declared = Some(version.to_string());
            } else {
                declared = Some("1.4".to_string());
            }
        }
        records.push(wire_tree::WireLine {
            record: value,
            line: index + 1,
        });
    }
    match declared {
        Some(_) => Ok(records),
        None => Err(WireReadError::Empty),
    }
}

pub fn read_file_records(path: &Path) -> Result<Vec<Value>, WireReadError> {
    Ok(read_file_lines(path)?
        .into_iter()
        .filter(|entry| entry.record.get("type").and_then(Value::as_str) != Some("metadata"))
        .map(|entry| entry.record)
        .collect())
}

pub fn records_from_store(
    store: &SqliteSessionStore,
    session_id: &str,
    agent_id: &str,
) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut since = 0u64;
    loop {
        match store.get_wire_events(session_id, since, 4096) {
            Ok(batch) if batch.is_empty() => break,
            Ok(batch) => {
                since = batch[batch.len() - 1].seq;
                rows.extend(batch);
            }
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %error,
                    "could not read wire events for the resume fallback"
                );
                break;
            }
        }
    }
    let mut agent = AgentFile::new(PathBuf::new());
    let mut records = Vec::new();
    let mut last_time = 0i64;
    for row in rows {
        let owner = row
            .payload
            .get("agent_id")
            .and_then(Value::as_str)
            .or_else(|| row.payload.get("agentId").and_then(Value::as_str))
            .unwrap_or(MAIN_AGENT_ID);
        if owner != agent_id {
            continue;
        }
        last_time = row.created_at;
        for mut record in map_event(
            &row.event_type,
            &row.payload,
            agent_id,
            row.created_at,
            &mut agent,
        ) {
            if record.get("type").and_then(Value::as_str).is_none()
                && let Some(object) = record.as_object_mut()
            {
                object.insert("type".to_string(), json!(row.event_type));
            }
            records.push(record);
        }
    }
    records.extend(agent.take_part_records(agent_id, last_time));
    records
}

pub fn resume_history(store: &SqliteSessionStore, session_id: &str) -> Option<Vec<LLMMessage>> {
    let records = resume_journal_records(store, session_id)?;
    fold_history(&records)
        .ok()
        .filter(|history| !history.is_empty())
}

pub fn resume_journal_records(store: &SqliteSessionStore, session_id: &str) -> Option<Vec<Value>> {
    let path = store.wire_journal_path(session_id, MAIN_AGENT_ID)?;
    let mut records: Vec<Value> = match read_file_lines(&path) {
        Ok(records) => records.into_iter().map(|entry| entry.record).collect(),
        Err(WireReadError::NotFound) => return None,
        Err(error) => {
            tracing::warn!(
                %session_id,
                %error,
                path = %path.display(),
                "the v2 wire journal is unusable; falling back to sqlite"
            );
            return None;
        }
    };
    for record in &mut records {
        if let Err(error) = store.rehydrate_payload(record) {
            tracing::warn!(
                %session_id,
                %error,
                path = %path.display(),
                "the v2 wire journal media could not be rehydrated; falling back to sqlite"
            );
            return None;
        }
    }
    Some(records)
}

pub fn has_undo_marker(records: &[Value]) -> bool {
    records.iter().any(|record| {
        matches!(
            record.get("type").and_then(Value::as_str),
            Some("context.undo" | "agent.switched")
        )
    })
}

pub fn history_from_store(
    store: &SqliteSessionStore,
    session_id: &str,
    agent_id: &str,
) -> Option<Vec<LLMMessage>> {
    let records = records_from_store(store, session_id, agent_id);
    if records.is_empty() {
        return None;
    }
    fold_history(&records)
        .ok()
        .filter(|history| !history.is_empty())
}

pub fn turn_prompt_axis(
    store: &SqliteSessionStore,
    session_id: &str,
) -> Option<Vec<(i64, String)>> {
    let records = restorable_session_records(store, session_id);
    let axis: Vec<(i64, String)> = records
        .iter()
        .filter_map(|record| {
            if record.get("type").and_then(Value::as_str) != Some("turn.prompt") {
                return None;
            }
            let turn_id = record.get("turnId").and_then(Value::as_i64)?;
            let text = record
                .get("input")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .unwrap_or_default();
            Some((turn_id, text))
        })
        .collect();
    (!axis.is_empty()).then_some(axis)
}

pub fn journal_line_count(store: &SqliteSessionStore, session_id: &str) -> Option<i64> {
    let path = store.wire_journal_path(session_id, MAIN_AGENT_ID)?;
    read_file_lines(&path)
        .ok()
        .and_then(|entries| entries.last().map(|entry| entry.line as i64))
}

#[derive(Debug, Clone)]
pub struct UndoSwitchPlan {
    pub edge_line: i64,
    pub legacy_undo_line: i64,
    pub branch: String,
    pub base_branch: String,
    pub base_line: i64,
}

pub fn undo_switch_plan(
    store: &SqliteSessionStore,
    session_id: &str,
    count: usize,
) -> Option<UndoSwitchPlan> {
    if count == 0 {
        return None;
    }
    let path = store.wire_journal_path(session_id, MAIN_AGENT_ID)?;
    let entries = read_file_lines(&path).ok()?;
    let last_line = entries.last().map_or(0, |entry| entry.line);
    let tree = wire_tree::parse_tree(&entries, last_line);
    let chain = wire_tree::active_chain(&entries, &tree).ok()?;
    let base_line =
        wire_tree::compute_fork_line(&chain, &tree.paired_legacy_undo_lines, count).ok()? as i64;
    let edge_line = last_line as i64 + 1;
    Some(UndoSwitchPlan {
        edge_line,
        legacy_undo_line: edge_line + 1,
        branch: format!("b{}", tree.edges.len() + 1),
        base_branch: wire_tree::branch_for_line(&tree, base_line as usize),
        base_line,
    })
}

pub fn undo_base_line(store: &SqliteSessionStore, session_id: &str, count: usize) -> Option<i64> {
    undo_switch_plan(store, session_id, count).map(|plan| plan.base_line)
}

pub fn write_journal_file(path: &Path, records: &[Value]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!("{WIRE_FILE_NAME}.tmp"));
    {
        let mut file = std::fs::File::create(&temporary)?;
        write_line(&mut file, &metadata_record())?;
        for record in records {
            write_line(&mut file, record)?;
        }
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

pub(crate) fn real_user_contents(history: &[LLMMessage]) -> Vec<String> {
    history
        .iter()
        .filter(|message| crate::compaction::is_real_user_input(message))
        .map(|message| message.content.clone())
        .collect()
}

#[derive(Debug, Clone)]
pub struct RestorableTurn {
    pub turn_id: Option<i64>,
    pub origin: Option<Value>,
    pub start: usize,
    pub first_message: usize,
    pub last_message: usize,
}

#[derive(Debug, Clone)]
pub struct RestorableFold {
    pub records: Vec<Value>,
    pub history: Vec<LLMMessage>,
    pub turns: Vec<RestorableTurn>,
    pub next_turn_id: i64,
}

pub fn session_records(store: &SqliteSessionStore, session_id: &str) -> Option<Vec<Value>> {
    if let Some(records) = resume_journal_records(store, session_id) {
        let usable = match fold_history(&records) {
            Ok(history) if history.is_empty() => true,
            Ok(history) => {
                if has_undo_marker(&records) {
                    true
                } else {
                    match store.load_session_messages(session_id) {
                        Ok(messages) if !messages.is_empty() => {
                            let table: Vec<LLMMessage> =
                                messages.into_iter().map(|stored| stored.message).collect();
                            real_user_contents(&table) == real_user_contents(&history)
                        }
                        _ => true,
                    }
                }
            }
            Err(_) => false,
        };
        if usable {
            return Some(records);
        }
    }
    let records = records_from_store(store, session_id, MAIN_AGENT_ID);
    (!records.is_empty()).then_some(records)
}

pub fn restorable_session_records(store: &SqliteSessionStore, session_id: &str) -> Vec<Value> {
    session_records(store, session_id)
        .map(|records| restorable_records(&records))
        .unwrap_or_default()
}

pub fn all_agent_records(store: &SqliteSessionStore, session_id: &str) -> Option<Vec<Value>> {
    let agents_dir = store
        .wire_journal_path(session_id, MAIN_AGENT_ID)?
        .parent()?
        .parent()?
        .to_path_buf();
    let entries = std::fs::read_dir(&agents_dir).ok()?;
    let mut merged = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().join(WIRE_FILE_NAME);
        let Ok(mut records) = read_file_lines(&path).map(|entries| {
            entries
                .into_iter()
                .map(|entry| entry.record)
                .collect::<Vec<_>>()
        }) else {
            continue;
        };
        for record in &mut records {
            store.rehydrate_payload(record).ok()?;
        }
        merged.extend(restorable_records(&records));
    }
    if merged.is_empty() {
        return None;
    }
    merged.sort_by_key(|record| record.get("time").and_then(Value::as_i64).unwrap_or(0));
    Some(merged)
}

pub fn has_journal(store: &SqliteSessionStore, session_id: &str) -> bool {
    store
        .wire_journal_path(session_id, MAIN_AGENT_ID)
        .is_some_and(|path| path.exists())
}

pub fn journal_authoritative(store: &SqliteSessionStore, session_id: &str) -> bool {
    let Some(records) = resume_journal_records(store, session_id) else {
        return false;
    };
    let Ok(history) = fold_history(&records) else {
        return false;
    };
    if history.is_empty() {
        return false;
    }
    if has_undo_marker(&records) {
        return true;
    }
    let Ok(messages) = store.load_session_messages(session_id) else {
        return false;
    };
    if messages.is_empty() {
        return true;
    }
    let table: Vec<LLMMessage> = messages.into_iter().map(|stored| stored.message).collect();
    real_user_contents(&table) == real_user_contents(&history)
}

pub fn fold_session(store: &SqliteSessionStore, session_id: &str) -> Option<RestorableFold> {
    fold_restorable(&session_records(store, session_id)?).ok()
}

pub fn active_turn_numbers(
    store: &SqliteSessionStore,
    session_id: &str,
) -> Option<std::collections::HashSet<i64>> {
    let fold = fold_session(store, session_id)?;
    let ids: std::collections::HashSet<i64> =
        fold.turns.iter().filter_map(|turn| turn.turn_id).collect();
    (!ids.is_empty()).then_some(ids)
}

fn try_restorable_records(records: &[Value]) -> Result<Vec<Value>, String> {
    let entries = wire_tree::entries_from_records(records);
    let last_line = entries.last().map_or(0, |entry| entry.line);
    let tree = wire_tree::parse_tree(&entries, last_line);
    for line in &tree.diagnostics.malformed_switch_lines {
        tracing::warn!(
            code = "wire.unknown_record",
            line,
            "Malformed agent.switched record ignored during tree projection"
        );
    }
    for branch in &tree.diagnostics.duplicate_branches {
        tracing::warn!(
            code = "wire.unknown_record",
            branch,
            "Duplicate agent.switched branch '{branch}' ignored during tree projection"
        );
    }
    Ok(wire_tree::restorable_chain(&entries, &tree)?
        .into_iter()
        .map(|entry| entry.record)
        .collect())
}

pub fn restorable_records(records: &[Value]) -> Vec<Value> {
    try_restorable_records(records).unwrap_or_default()
}

pub fn fold_restorable(records: &[Value]) -> Result<RestorableFold, String> {
    let restorable = try_restorable_records(records)?;
    let mut history: Vec<LLMMessage> = Vec::new();
    let mut turns: Vec<RestorableTurn> = Vec::new();
    let mut open: Option<usize> = None;
    let mut next_turn_id = 0i64;
    let mut pending_turn: Option<(Option<i64>, Option<Value>, usize)> = None;
    let mut pending_steer = false;
    for (index, record) in restorable.iter().enumerate() {
        match record.get("type").and_then(Value::as_str) {
            Some("turn.prompt") => {
                let turn_id = record.get("turnId").and_then(Value::as_i64);
                if let Some(turn_id) = turn_id {
                    next_turn_id = next_turn_id.max(turn_id + 1);
                }
                pending_turn = Some((
                    turn_id,
                    record
                        .get("origin")
                        .filter(|value| value.is_object())
                        .cloned(),
                    index,
                ));
            }
            Some("turn.steer") => {
                pending_steer = true;
            }
            Some("turn.ended") => {
                if let Some(turn_id) = record.get("turnId").and_then(Value::as_i64) {
                    next_turn_id = next_turn_id.max(turn_id + 1);
                }
            }
            Some("context.append_message") => {
                if let Some(message) = record.get("message").and_then(history_message) {
                    let starts_turn =
                        !pending_steer && crate::compaction::is_real_user_input(&message);
                    pending_steer = false;
                    if starts_turn {
                        let (turn_id, origin, start) = match pending_turn.take() {
                            Some((turn_id, origin, start)) => (turn_id, origin, start),
                            None => (None, None, index),
                        };
                        let start = injection_floor(&restorable, start);
                        if let Some(last) = turns.last_mut() {
                            last.last_message = history.len();
                        }
                        turns.push(RestorableTurn {
                            turn_id,
                            origin: message.origin.clone().or(origin),
                            start,
                            first_message: history.len(),
                            last_message: history.len(),
                        });
                    }
                    history.push(message);
                }
            }
            Some("context.append_loop_event") => {
                if let Some(event) = record.get("event") {
                    fold_loop_event(&mut history, &mut open, event);
                }
            }
            Some("context.undo") => {
                let count = record.get("count").and_then(Value::as_u64).unwrap_or(0);
                if let Some(cut) = undo_cut(&history, count) {
                    history.truncate(cut);
                    turns.retain(|turn| turn.first_message < cut);
                    if let Some(last) = turns.last_mut() {
                        last.last_message = cut;
                    }
                }
                open = None;
                pending_turn = None;
                pending_steer = false;
            }
            Some("context.clear") => {
                history.clear();
                turns.clear();
                open = None;
                pending_turn = None;
                pending_steer = false;
            }
            Some("context.apply_compaction") => {
                apply_compaction_history(&mut history, record);
                open = None;
                pending_turn = None;
                pending_steer = false;
                turns = rebuild_turns(&history);
            }
            _ => {}
        }
    }
    if let Some(last) = turns.last_mut() {
        last.last_message = history.len();
    }
    Ok(RestorableFold {
        records: restorable,
        history,
        turns,
        next_turn_id,
    })
}

fn injection_floor(records: &[Value], start: usize) -> usize {
    let mut index = start;
    while index > 0 {
        let previous = &records[index - 1];
        if previous.get("type").and_then(Value::as_str) != Some("context.append_message") {
            break;
        }
        let Some(message) = previous.get("message").and_then(history_message) else {
            break;
        };
        if origin_kind(&message) != Some("injection") {
            break;
        }
        index -= 1;
    }
    index
}

fn rebuild_turns(history: &[LLMMessage]) -> Vec<RestorableTurn> {
    let mut turns: Vec<RestorableTurn> = Vec::new();
    for (index, message) in history.iter().enumerate() {
        if crate::compaction::is_real_user_input(message) {
            if let Some(last) = turns.last_mut() {
                last.last_message = index;
            }
            turns.push(RestorableTurn {
                turn_id: None,
                origin: message.origin.clone(),
                start: 0,
                first_message: index,
                last_message: index,
            });
        }
    }
    if let Some(last) = turns.last_mut() {
        last.last_message = history.len();
    }
    turns
}

pub fn fold_history(records: &[Value]) -> Result<Vec<LLMMessage>, String> {
    Ok(fold_restorable(records)?.history)
}

fn fold_loop_event(history: &mut Vec<LLMMessage>, open: &mut Option<usize>, event: &Value) {
    match event.get("type").and_then(Value::as_str) {
        Some("step.begin") => {
            history.push(LLMMessage {
                role: "assistant".to_string(),
                ..Default::default()
            });
            *open = Some(history.len() - 1);
        }
        Some("content.part") => {
            let Some(index) = *open else {
                return;
            };
            if let Some(part) = event.get("part") {
                apply_content_part(&mut history[index], part);
            }
        }
        Some("tool.call") => {
            let Some(index) = *open else {
                return;
            };
            let mut call = ToolCall {
                id: read_wire_string(event, "toolCallId").unwrap_or_default(),
                name: read_wire_string(event, "name").unwrap_or_default(),
                arguments: tool_arguments(event.get("args")),
                extras: None,
                args_parse_error: None,
            };
            if let Some(extras) = event.get("extras").filter(|value| !value.is_null()) {
                call.extras = Some(extras.clone());
            }
            history[index].tool_calls.push(call);
        }
        Some("tool.result") => {
            let tool_call_id = read_wire_string(event, "toolCallId").unwrap_or_default();
            let content = event
                .get("result")
                .and_then(|result| result.get("output"))
                .map(output_text)
                .unwrap_or_default();
            history.push(LLMMessage {
                role: "tool".to_string(),
                content,
                tool_call_id: Some(tool_call_id),
                ..Default::default()
            });
        }
        Some("step.end") => {
            let Some(index) = open.take() else {
                return;
            };
            let vacant = history.get(index).is_some_and(|message| {
                message.content.trim().is_empty()
                    && message.blocks.is_empty()
                    && message.tool_calls.is_empty()
            });
            if vacant {
                history.remove(index);
            }
        }
        _ => {}
    }
}

fn history_message(message: &Value) -> Option<LLMMessage> {
    let role = message.get("role").and_then(Value::as_str)?;
    let mut out = LLMMessage {
        role: role.to_string(),
        ..Default::default()
    };
    match message.get("content") {
        Some(Value::String(text)) => out.content = text.clone(),
        Some(Value::Array(parts)) => {
            for part in parts {
                apply_content_part(&mut out, part);
            }
        }
        _ => {}
    }
    if let Some(calls) = message.get("toolCalls").and_then(Value::as_array) {
        for call in calls {
            let Some(id) = call.get("id").and_then(Value::as_str) else {
                continue;
            };
            let mut tool_call = ToolCall {
                id: id.to_string(),
                name: call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: tool_arguments(call.get("arguments")),
                extras: call.get("extras").filter(|value| !value.is_null()).cloned(),
                args_parse_error: None,
            };
            tool_call.arguments = match tool_call.arguments {
                Value::Null => json!({}),
                other => other,
            };
            out.tool_calls.push(tool_call);
        }
    }
    if let Some(tool_call_id) = message.get("toolCallId").and_then(Value::as_str) {
        out.tool_call_id = Some(tool_call_id.to_string());
    }
    if let Some(origin) = message.get("origin").filter(|value| value.is_object()) {
        out.origin = Some(origin.clone());
    }
    if let Some(tools) = message.get("tools").and_then(Value::as_array) {
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            out.tools.push(kimi_core::types::ToolDescription {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parameters: tool.get("parameters").cloned().unwrap_or(Value::Null),
                deferred: tool.get("deferred").and_then(Value::as_bool),
            });
        }
    }
    Some(out)
}

fn apply_content_part(message: &mut LLMMessage, part: &Value) {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                message.content.push_str(text);
            }
        }
        Some("think") => {
            let Some(think) = part.get("think").and_then(Value::as_str) else {
                return;
            };
            message.blocks.push(ContentBlock::Think {
                think: think.to_string(),
                encrypted: read_wire_string(part, "encrypted"),
                details_index: part
                    .get("detailsIndex")
                    .and_then(Value::as_u64)
                    .map(|value| value as u32),
                reasoning_key: read_wire_string(part, "reasoningKey"),
                details_summary: read_wire_string(part, "detailsSummary"),
            });
        }
        Some("image_url") => {
            if let Some((url, id, name)) = media_part(part, "imageUrl") {
                message
                    .blocks
                    .push(ContentBlock::ImageUrl { url, id, name });
            }
        }
        Some("audio_url") => {
            if let Some((url, id, name)) = media_part(part, "audioUrl") {
                message
                    .blocks
                    .push(ContentBlock::AudioUrl { url, id, name });
            }
        }
        Some("video_url") => {
            if let Some((url, id, name)) = media_part(part, "videoUrl") {
                message
                    .blocks
                    .push(ContentBlock::VideoUrl { url, id, name });
            }
        }
        Some("media_ref") => {
            let Some(file_id) = read_wire_string(part, "file_id") else {
                return;
            };
            let Some(kind) = part.get("kind").and_then(Value::as_str) else {
                return;
            };
            let kind = match kind {
                "image" => MediaKind::Image,
                "video" => MediaKind::Video,
                "audio" => MediaKind::Audio,
                _ => return,
            };
            message
                .blocks
                .push(ContentBlock::MediaRef { file_id, kind });
        }
        _ => {}
    }
}

fn media_part(part: &Value, key: &str) -> Option<(String, Option<String>, Option<String>)> {
    let media = match part.get(key) {
        Some(Value::Object(_)) => part.get(key)?,
        Some(Value::String(_)) => part,
        _ => return None,
    };
    let url = media.get("url").and_then(Value::as_str)?.to_string();
    Some((
        url,
        media.get("id").and_then(Value::as_str).map(str::to_string),
        media
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
    ))
}

fn tool_arguments(arguments: Option<&Value>) -> Value {
    match arguments {
        None | Some(Value::Null) => json!({}),
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
        Some(value) => value.clone(),
    }
}

fn output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .concat(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn undo_cut(history: &[LLMMessage], count: u64) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let mut remaining = count;
    for index in (0..history.len()).rev() {
        let message = &history[index];
        if origin_kind(message) == Some("injection") {
            continue;
        }
        if crate::compaction::is_compaction_summary_message(message) {
            break;
        }
        if crate::compaction::is_real_user_input(message) {
            remaining -= 1;
            if remaining == 0 {
                let mut cut = index;
                while cut > 0 && origin_kind(&history[cut - 1]) == Some("injection") {
                    cut -= 1;
                }
                return Some(cut);
            }
        }
    }
    None
}

fn undo_history(history: &mut Vec<LLMMessage>, count: u64) {
    if let Some(cut) = undo_cut(history, count) {
        history.truncate(cut);
    }
}

fn apply_compaction_history(history: &mut Vec<LLMMessage>, record: &Value) {
    let summary = record
        .get("summary")
        .or_else(|| record.get("contextSummary"))
        .map(summary_text)
        .unwrap_or_default();
    let compacted_count = record
        .get("compactedCount")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let legacy_tail = record.get("keptUserMessageCount").is_none();
    let mut next = if legacy_tail {
        let mut next = vec![compaction_summary_message(&summary)];
        next.extend(history.iter().skip(compacted_count).cloned());
        next
    } else {
        let kept: Vec<LLMMessage> = history
            .iter()
            .filter(|message| crate::compaction::is_real_user_input(message))
            .cloned()
            .collect();
        let mut next = crate::compaction::select_kept_user_messages(&kept);
        next.push(compaction_summary_message(&summary));
        next.push(crate::compaction::compaction_continuation_message());
        next
    };
    *history = std::mem::take(&mut next);
}

fn compaction_summary_message(summary: &str) -> LLMMessage {
    LLMMessage {
        role: "user".to_string(),
        content: crate::compaction::build_compaction_summary_text(summary),
        origin: Some(json!({ "kind": "compaction_summary" })),
        ..Default::default()
    }
}

fn summary_text(summary: &Value) -> String {
    if let Some(text) = summary.as_str() {
        return text.to_string();
    }
    summary
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .concat()
        })
        .unwrap_or_default()
}

fn origin_kind(message: &LLMMessage) -> Option<&str> {
    message
        .origin
        .as_ref()
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
}

fn read_wire_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::replay_builder::fold::fold_wire_records;

    fn fixture() -> (tempfile::TempDir, Arc<SqliteSessionStore>, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap());
        let workspace = store.create_workspace("/work/demo", None).unwrap();
        store
            .create_session_with_workspace("sess-1", Some("wire"), Some(&workspace.id))
            .unwrap();
        (dir, store, workspace.id)
    }

    fn custom(value: Value) -> EngineEvent {
        EngineEvent::Custom(value)
    }

    fn read_records(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn record_types(records: &[Value]) -> Vec<String> {
        records
            .iter()
            .map(|record| record["type"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn turn_events() -> Vec<EngineEvent> {
        vec![
            custom(json!({
                "type": "turn.prompt",
                "turnId": 1,
                "promptId": "prompt-1",
                "input": [{ "type": "text", "text": "hello" }],
                "origin": { "kind": "user" },
            })),
            custom(json!({
                "type": "llm.step.begin",
                "agent_id": "main",
                "turn_id": "turn-1",
                "step": 1,
                "step_id": "turn-1-step-1",
            })),
            custom(json!({
                "type": "llm.delta",
                "agent_id": "main",
                "part": { "type": "text", "text": "Hel" },
            })),
            custom(json!({
                "type": "llm.delta",
                "agent_id": "main",
                "part": { "type": "text", "text": "lo" },
            })),
            custom(json!({
                "type": "tool.call.started",
                "agent_id": "main",
                "turn_id": "turn-1",
                "tool_call_id": "call-1",
                "tool_name": "Read",
                "args": { "path": "a" },
            })),
            custom(json!({
                "type": "tool.native",
                "agent_id": "main",
                "turn_id": "turn-1",
                "tool_call_id": "call-1",
                "tool_name": "Read",
                "arguments": { "path": "a" },
                "content": "body",
                "is_error": false,
                "note": null,
            })),
            custom(json!({
                "type": "llm.step.end",
                "agent_id": "main",
                "turn_id": "turn-1",
                "step": 1,
                "finish_reason": "tool_calls",
                "provider_finish_reason": "tool_calls",
                "raw_finish_reason": "tool_calls",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 4,
                    "input_cache_read": 2,
                    "input_cache_creation": 1,
                },
                "timing": { "firstTokenLatencyMs": 5, "streamDurationMs": 7 },
            })),
            custom(json!({
                "type": "turn.ended",
                "turnId": 1,
                "reason": "completed",
                "durationMs": 9,
                "traceId": "turn-1",
            })),
        ]
    }

    fn run_turn(journal: &WireJournal) {
        for (index, event) in turn_events().iter().enumerate() {
            journal.append("sess-1", event, 1000 + index as i64);
        }
    }

    fn write_turn_to_store(store: &SqliteSessionStore) {
        for (index, event) in turn_events().iter().enumerate() {
            store
                .append_wire_event_json(
                    &format!("evt-{index}"),
                    "sess-1",
                    event.event_type(),
                    &serde_json::to_string(event).unwrap(),
                    false,
                    false,
                    1000 + index as i64,
                )
                .unwrap();
        }
    }

    fn truncate_last_line(path: &std::path::Path) {
        use std::io::Write as _;
        let text = std::fs::read_to_string(path).unwrap();
        let trimmed = text.trim_end();
        let cut = trimmed.len() - 5;
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(&trimmed.as_bytes()[..cut]).unwrap();
    }

    #[test]
    fn the_journal_and_the_sqlite_wire_events_fold_to_the_same_history() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        run_turn(&journal);
        write_turn_to_store(&store);

        let path = journal.journal_path("sess-1", "main").unwrap();
        let journal_records = read_file_records(&path).unwrap();
        let store_records = records_from_store(&store, "sess-1", "main");
        assert_eq!(
            fold_history(&store_records).unwrap(),
            fold_history(&journal_records).unwrap(),
            "the same events must fold identically from both sources"
        );
        let history = store.load_session_history("sess-1").unwrap();
        assert_eq!(history, fold_history(&journal_records).unwrap());
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content, "Hello");
        assert_eq!(history[1].tool_calls[0].id, "call-1");
        assert_eq!(history[1].tool_calls[0].arguments, json!({ "path": "a" }));
        assert_eq!(history[2].role, "tool");
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(history[2].content, "body");
    }

    #[test]
    fn a_session_with_only_sqlite_wire_events_still_resumes() {
        let (dir, store, workspace_id) = fixture();
        write_turn_to_store(&store);
        let path = dir
            .path()
            .join("sessions")
            .join(&workspace_id)
            .join("sess-1")
            .join("agents")
            .join("main")
            .join(WIRE_FILE_NAME);
        assert!(matches!(
            read_file_records(&path),
            Err(WireReadError::NotFound)
        ));
        let history = store.load_session_history("sess-1").unwrap();
        assert_eq!(
            history,
            fold_history(&records_from_store(&store, "sess-1", "main")).unwrap()
        );
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(history[0].content, "hello");
    }

    #[test]
    fn a_truncated_journal_falls_back_to_sqlite() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        run_turn(&journal);
        write_turn_to_store(&store);
        let path = journal.journal_path("sess-1", "main").unwrap();
        truncate_last_line(&path);

        let history = store.load_session_history("sess-1").unwrap();
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(history[1].content, "Hello");
        assert_eq!(history[2].content, "body");
    }

    #[test]
    fn a_corrupt_journal_falls_back_to_sqlite() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        run_turn(&journal);
        write_turn_to_store(&store);
        let path = journal.journal_path("sess-1", "main").unwrap();
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"{not json}\n").unwrap();
        }

        let history = store.load_session_history("sess-1").unwrap();
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(history[0].content, "hello");
    }

    #[test]
    fn a_legacy_session_without_any_wire_source_opens_from_the_messages_table() {
        let (_dir, store, _) = fixture();
        store
            .save_turn(
                "sess-1",
                "turn-1",
                1,
                &[LLMMessage::user("old"), LLMMessage::assistant("reply")],
                None,
                None,
            )
            .unwrap();
        let history = store.load_session_history("sess-1").unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(history[0].content, "old");
        assert_eq!(history[1].content, "reply");
    }

    #[test]
    fn inline_media_is_dehydrated_in_the_journal_and_rehydrated_on_resume() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        let base64 = "A".repeat(5000);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "turn.prompt",
                "turnId": 1,
                "input": [
                    { "type": "text", "text": "look" },
                    {
                        "type": "image_url",
                        "imageUrl": { "url": format!("data:image/png;base64,{base64}") },
                    },
                ],
                "origin": { "kind": "user" },
            })),
            1,
        );

        let path = journal.journal_path("sess-1", "main").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&base64),
            "the journal must not keep inline media"
        );
        assert!(
            text.contains("blobref:image/png;"),
            "the journal must store a blob reference"
        );

        let history = store.load_session_history("sess-1").unwrap();
        assert_eq!(history.len(), 1, "{history:?}");
        assert_eq!(history[0].content, "look");
        assert!(matches!(
            history[0].blocks.first(),
            Some(ContentBlock::ImageUrl { url, .. }) if url.starts_with("data:image/png;base64,")
        ));
    }

    #[test]
    fn export_bundles_every_agent_journal_and_rehydrates_media() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        run_turn(&journal);
        let base64 = "A".repeat(5000);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "turn.prompt",
                "turnId": 2,
                "input": [{
                    "type": "image_url",
                    "imageUrl": { "url": format!("data:image/png;base64,{base64}") },
                }],
                "origin": { "kind": "user" },
            })),
            5000,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "turn.prompt",
                "agent_id": "agent-7",
                "input": [{ "type": "text", "text": "scan" }],
                "origin": { "kind": "system_trigger" },
            })),
            5001,
        );

        let export = store.export_session("sess-1").unwrap().unwrap();
        let ids: Vec<&str> = export
            .agent_wires
            .iter()
            .map(|(agent_id, _)| agent_id.as_str())
            .collect();
        assert_eq!(ids, ["agent-7", "main"]);
        let main_lines = export
            .agent_wires
            .iter()
            .find(|(agent_id, _)| agent_id == "main")
            .map(|(_, lines)| lines.clone())
            .unwrap();
        assert_eq!(export.wire_jsonl, main_lines);
        let main = main_lines.join("\n");
        assert!(main.contains("data:image/png;base64,"));
        assert!(!main.contains("blobref:image/png"));
    }

    #[test]
    fn prompt_task_and_subagent_records_are_journaled() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        for event in [
            json!({
                "type": "prompt.completed",
                "promptId": "p-1",
                "finishedAt": "2026-01-01T00:00:00Z",
                "reason": "completed",
            }),
            json!({
                "type": "subagent.spawned",
                "subagentId": "a-1",
                "subagentName": "worker",
                "parentToolCallId": "c-1",
                "runInBackground": false,
            }),
            json!({
                "type": "task.started",
                "info": {
                    "taskId": "t-1",
                    "kind": "agent",
                    "agentId": "a-1",
                    "status": "running",
                    "startedAt": 1000,
                    "endedAt": null,
                },
            }),
        ] {
            journal.append("sess-1", &custom(event), 1000);
        }
        let records = read_records(&journal.journal_path("sess-1", "main").unwrap());
        assert_eq!(
            record_types(&records),
            vec![
                "metadata",
                "prompt.completed",
                "subagent.spawned",
                "task.started"
            ]
        );
        for record in &records[1..] {
            assert_eq!(record["agentId"], json!("main"), "{record}");
            assert_eq!(record["time"], json!(1000), "{record}");
        }
    }

    #[test]
    fn a_paired_legacy_undo_is_applied_once_by_the_switch_not_twice() {
        let records = vec![
            json!({"type":"turn.prompt","turnId":1,"input":[{"type":"text","text":"one"}]}),
            json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"one"}],"origin":{"kind":"user"}}}),
            json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s1"}}),
            json!({"type":"context.append_loop_event","event":{"type":"content.part","stepUuid":"s1","part":{"type":"text","text":"a1"}}}),
            json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s1"}}),
            json!({"type":"turn.prompt","turnId":2,"input":[{"type":"text","text":"two"}]}),
            json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"two"}],"origin":{"kind":"user"}}}),
            json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s2"}}),
            json!({"type":"context.append_loop_event","event":{"type":"content.part","stepUuid":"s2","part":{"type":"text","text":"a2"}}}),
            json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s2"}}),
            json!({
                "type":"agent.switched",
                "agentId":"main",
                "branch":"b1",
                "base":{"branch":"main","line":5},
                "turns":1,
                "legacyUndoLine":12,
            }),
            json!({"type":"context.undo","count":1}),
            json!({"type":"context.undone","turns":1}),
        ];
        let restorable = restorable_records(&records);
        assert!(
            restorable
                .iter()
                .all(|record| record["type"] != json!("context.undo")),
            "the paired legacy undo must be dropped from the active chain"
        );
        let history = fold_history(&records).unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(history[0].content, "one");
        assert_eq!(history[1].content, "a1");
    }

    #[test]
    fn compaction_folds_into_the_summary_shape_and_drops_older_turns() {
        let records = vec![
            json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"q1"}],"origin":{"kind":"user"}}}),
            json!({"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"a1"}]}}),
            json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"q2"}],"origin":{"kind":"user"}}}),
            json!({"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"a2"}]}}),
            json!({
                "type":"context.apply_compaction",
                "summary":"sum",
                "compactedCount":4,
                "keptUserMessageCount":2,
            }),
        ];
        let history = fold_history(&records).unwrap();
        assert!(
            history.iter().all(|message| message.role != "assistant"),
            "{history:?}"
        );
        assert!(
            history.iter().any(|message| message
                .origin
                .as_ref()
                .and_then(|origin| origin.get("kind"))
                .and_then(Value::as_str)
                == Some("compaction_summary")),
            "{history:?}"
        );
        let contents: Vec<&str> = history
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert!(contents.iter().any(|text| text.contains("q1")));
        assert!(contents.iter().any(|text| text.contains("q2")));
    }

    #[test]
    fn a_fork_materializes_its_own_v2_journal() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        run_turn(&journal);
        write_turn_to_store(&store);
        let source_history = store.load_session_history("sess-1").unwrap();

        store
            .fork_session("sess-1", "sess-fork", None, None)
            .unwrap();
        let path = store.wire_journal_path("sess-fork", "main").unwrap();
        assert!(path.exists(), "the fork must get its own journal");
        let records = read_file_records(&path).unwrap();
        assert_eq!(fold_history(&records).unwrap(), source_history);
        assert_eq!(
            store.load_session_history("sess-fork").unwrap(),
            source_history
        );
    }

    #[test]
    fn a_legacy_delete_style_undo_keeps_the_messages_table_authoritative() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        run_turn(&journal);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "turn.prompt",
                "turnId": 2,
                "input": [{ "type": "text", "text": "second" }],
                "origin": { "kind": "user" },
            })),
            2000,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "llm.step.begin",
                "agent_id": "main",
                "turn_id": "turn-2",
                "step": 1,
                "step_id": "turn-2-step-1",
            })),
            2001,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "llm.delta",
                "agent_id": "main",
                "part": { "type": "text", "text": "reply" },
            })),
            2002,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "llm.step.end",
                "agent_id": "main",
                "turn_id": "turn-2",
                "step": 1,
                "finish_reason": "stop",
            })),
            2003,
        );
        store
            .save_turn(
                "sess-1",
                "turn-1",
                1,
                &[LLMMessage::user("hello"), LLMMessage::assistant("Hello")],
                None,
                None,
            )
            .unwrap();

        let history = store.load_session_history("sess-1").unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].content, "Hello");
    }

    #[test]
    fn the_ws_cursor_uses_sqlite_event_seq_not_journal_lines() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store.clone());
        run_turn(&journal);
        write_turn_to_store(&store);
        journal.append(
            "sess-1",
            &custom(json!({ "type": "tool.progress", "tool_call_id": "call-1" })),
            9000,
        );
        store
            .append_wire_event_json(
                "evt-progress",
                "sess-1",
                "tool.progress",
                &json!({ "type": "tool.progress", "tool_call_id": "call-1" }).to_string(),
                false,
                false,
                9000,
            )
            .unwrap();

        let path = journal.journal_path("sess-1", "main").unwrap();
        let records = read_file_records(&path).unwrap();
        assert_eq!(
            store.latest_wire_event_seq("sess-1").unwrap(),
            9,
            "the ws cursor counts persisted engine events"
        );
        assert_eq!(
            records.len(),
            8,
            "journal lines count emitted v2 records, not engine events"
        );
        assert!(
            records.iter().all(|record| record.get("seq").is_none()),
            "journal records carry no seq field"
        );
    }

    #[test]
    fn the_journal_fold_replays_think_parts_and_undo_records() {
        let records = vec![
            json!({
                "type": "context.append_message",
                "agentId": "main",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "one" },
                        { "type": "media_ref", "file_id": "file-1", "kind": "image" },
                    ],
                    "origin": { "kind": "user" },
                },
                "time": 1,
            }),
            json!({
                "type": "context.append_loop_event",
                "agentId": "main",
                "event": {
                    "type": "step.begin",
                    "uuid": "s1",
                    "turnId": "t1",
                    "step": 1,
                },
                "time": 2,
            }),
            json!({
                "type": "context.append_loop_event",
                "agentId": "main",
                "event": {
                    "type": "content.part",
                    "stepUuid": "s1",
                    "part": { "type": "think", "think": "why" },
                },
                "time": 3,
            }),
            json!({
                "type": "context.append_loop_event",
                "agentId": "main",
                "event": {
                    "type": "content.part",
                    "stepUuid": "s1",
                    "part": { "type": "text", "text": "answer" },
                },
                "time": 4,
            }),
            json!({
                "type": "context.append_loop_event",
                "agentId": "main",
                "event": { "type": "step.end", "uuid": "s1", "finishReason": "stop" },
                "time": 5,
            }),
            json!({
                "type": "context.append_message",
                "agentId": "main",
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "two" }],
                    "origin": { "kind": "user" },
                },
                "time": 6,
            }),
            json!({
                "type": "context.undo",
                "agentId": "main",
                "count": 1,
                "time": 7,
            }),
        ];
        let history = fold_history(&records).unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(history[0].content, "one");
        assert!(matches!(
            history[0].blocks.first(),
            Some(ContentBlock::MediaRef { file_id, .. }) if file_id == "file-1"
        ));
        assert_eq!(history[1].content, "answer");
        assert_eq!(history[1].blocks.len(), 1);
        assert!(
            matches!(&history[1].blocks[0], ContentBlock::Think { think, .. } if think == "why")
        );
    }

    #[test]
    fn a_turn_writes_the_v2_journal_and_folds_back() {
        let (dir, store, workspace_id) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        run_turn(&journal);

        let path = dir
            .path()
            .join("sessions")
            .join(&workspace_id)
            .join("sess-1")
            .join("agents")
            .join("main")
            .join(WIRE_FILE_NAME);
        assert_eq!(journal.journal_path("sess-1", "main"), Some(path.clone()));
        let records = read_records(&path);
        assert_eq!(
            record_types(&records),
            vec![
                "metadata",
                "turn.prompt",
                "context.append_message",
                "context.append_loop_event",
                "context.append_loop_event",
                "context.append_loop_event",
                "context.append_loop_event",
                "context.append_loop_event",
                "turn.ended",
            ]
        );
        assert_eq!(records[0]["protocol_version"], json!(WIRE_PROTOCOL_VERSION));
        assert_eq!(
            records[0]["min_protocol_version"],
            json!(WIRE_MIN_READER_VERSION)
        );
        assert!(records[0]["created_at"].is_number());
        assert_eq!(records[1]["agentId"], json!("main"));
        assert_eq!(records[2]["message"]["id"], json!("prompt-1"));
        assert_eq!(records[2]["message"]["content"][0]["text"], json!("hello"));
        assert_eq!(records[3]["event"]["type"], json!("step.begin"));
        assert_eq!(records[4]["event"]["type"], json!("content.part"));
        assert_eq!(records[4]["event"]["part"]["text"], json!("Hello"));
        assert_eq!(records[5]["event"]["type"], json!("tool.call"));
        assert_eq!(records[5]["event"]["stepUuid"], json!("turn-1-step-1"));
        assert_eq!(records[6]["event"]["type"], json!("tool.result"));
        assert_eq!(records[7]["event"]["type"], json!("step.end"));
        assert_eq!(records[7]["event"]["finishReason"], json!("tool_calls"));
        assert_eq!(records[7]["event"]["usage"]["inputOther"], json!(10));
        assert_eq!(records[7]["event"]["usage"]["inputCacheRead"], json!(2));
        assert_eq!(records[7]["event"]["llmStreamDurationMs"], json!(7));

        let folded = fold_wire_records(&records, None).expect("the journal must fold");
        assert_eq!(folded.replay.len(), 3, "{:?}", folded.replay);
        assert_eq!(folded.replay[0]["type"], json!("message"));
        assert_eq!(folded.replay[0]["turnId"], json!(1));
        assert_eq!(
            folded.replay[0]["message"]["content"][0]["text"],
            json!("hello")
        );
        assert_eq!(folded.replay[1]["message"]["role"], json!("assistant"));
        assert_eq!(
            folded.replay[1]["message"]["content"][0]["text"],
            json!("Hello")
        );
        assert_eq!(
            folded.replay[1]["message"]["toolCalls"][0]["id"],
            json!("call-1")
        );
        assert_eq!(
            folded.replay[1]["message"]["toolCalls"][0]["arguments"],
            json!("{\"path\":\"a\"}")
        );
        assert_eq!(folded.replay[2]["message"]["role"], json!("tool"));
        assert_eq!(folded.replay[2]["message"]["toolCallId"], json!("call-1"));
        assert_eq!(folded.replay[2]["message"]["content"], json!("body"));
    }

    #[test]
    fn a_retried_attempt_does_not_reach_the_journal() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        let events = [
            custom(json!({
                "type": "llm.step.begin",
                "agent_id": "main",
                "turn_id": "turn-1",
                "step": 1,
                "step_id": "turn-1-step-1",
            })),
            custom(json!({
                "type": "llm.delta",
                "agent_id": "main",
                "part": { "type": "text", "text": "draft" },
            })),
            custom(json!({
                "type": "llm.step.retrying",
                "agent_id": "main",
                "turn_id": "turn-1",
                "step": 1,
                "step_id": "turn-1-step-1",
            })),
            custom(json!({
                "type": "llm.delta",
                "agent_id": "main",
                "part": { "type": "text", "text": "final" },
            })),
            custom(json!({
                "type": "llm.step.end",
                "agent_id": "main",
                "turn_id": "turn-1",
                "step": 1,
                "finish_reason": "stop",
                "provider_finish_reason": "completed",
                "raw_finish_reason": "stop",
            })),
        ];
        for event in &events {
            journal.append("sess-1", event, 1);
        }
        let path = journal.journal_path("sess-1", "main").unwrap();
        let records = read_records(&path);
        let folded = fold_wire_records(&records, None).expect("the journal must fold");
        assert_eq!(folded.replay.len(), 1, "{:?}", folded.replay);
        assert_eq!(
            folded.replay[0]["message"]["content"][0]["text"],
            json!("final")
        );
    }

    #[test]
    fn rewrite_replaces_the_file_and_keeps_appending() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        run_turn(&journal);
        let path = journal.journal_path("sess-1", "main").unwrap();
        let original = read_records(&path);

        let kept = vec![original[0].clone(), original[1].clone()];
        journal.rewrite("sess-1", "main", &kept).unwrap();
        assert_eq!(read_records(&path), kept);

        journal.append(
            "sess-1",
            &custom(json!({
                "type": "turn.ended",
                "turnId": 2,
                "reason": "cancelled",
            })),
            2000,
        );
        let rewritten = read_records(&path);
        assert_eq!(rewritten.len(), 3);
        assert_eq!(rewritten[0]["type"], json!("metadata"));
        assert_eq!(rewritten[2]["type"], json!("turn.ended"));
        assert_eq!(rewritten[2]["turnId"], json!(2));
    }

    #[test]
    fn an_undo_record_drops_the_last_turn_from_the_fold() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        run_turn(&journal);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "context.undo",
                "agentId": "main",
                "count": 1,
            })),
            2000,
        );
        let path = journal.journal_path("sess-1", "main").unwrap();
        let records = read_records(&path);
        let undo = records.last().unwrap();
        assert_eq!(undo["type"], json!("context.undo"));
        assert_eq!(undo["agentId"], json!("main"));
        assert_eq!(undo["count"], json!(1));
        assert_eq!(undo["time"], json!(2000));
        let folded = fold_wire_records(&records, None).expect("the journal must fold");
        assert!(folded.replay.is_empty(), "{:?}", folded.replay);
    }

    #[test]
    fn applied_compaction_and_tool_select_records_keep_the_v2_shape() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "context.apply_compaction",
                "agentId": "main",
                "summary": "sum",
                "compactedCount": 3,
                "keptUserMessageCount": 0,
            })),
            1000,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "tool_select.initialized",
                "agentId": "agent-7",
                "tools": [],
            })),
            1001,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "tool_select.tools_changed",
                "agentId": "agent-7",
                "added": [],
                "removed": [],
            })),
            1002,
        );

        let main_records = read_records(&journal.journal_path("sess-1", "main").unwrap());
        assert_eq!(
            record_types(&main_records),
            vec!["metadata", "context.apply_compaction"]
        );
        assert_eq!(main_records[1]["summary"], json!("sum"));
        assert_eq!(main_records[1]["compactedCount"], json!(3));
        assert_eq!(main_records[1]["agentId"], json!("main"));
        assert_eq!(main_records[1]["time"], json!(1000));

        let worker_records = read_records(&journal.journal_path("sess-1", "agent-7").unwrap());
        assert_eq!(
            record_types(&worker_records),
            vec![
                "metadata",
                "tool_select.initialized",
                "tool_select.tools_changed",
            ]
        );
        assert_eq!(worker_records[1]["agentId"], json!("agent-7"));
        assert_eq!(worker_records[1]["time"], json!(1001));
        assert_eq!(worker_records[2]["agentId"], json!("agent-7"));
    }

    #[test]
    fn permission_set_mode_is_a_durable_journal_record() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "permission.set_mode",
                "agentId": "main",
                "mode": "auto",
            })),
            1000,
        );

        let records = read_records(&journal.journal_path("sess-1", "main").unwrap());
        assert_eq!(
            record_types(&records),
            vec!["metadata", "permission.set_mode"]
        );
        assert_eq!(records[1]["agentId"], json!("main"));
        assert_eq!(records[1]["mode"], json!("auto"));
        assert_eq!(records[1]["time"], json!(1000));
        assert!(records[1].get("agent_id").is_none());
    }

    #[test]
    fn todo_and_goal_records_keep_the_v2_shape() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "tools.update_store",
                "agent_id": "main",
                "key": "todo",
                "value": [{ "id": "T1", "parentId": null, "kind": "task", "title": "one", "status": "pending" }],
            })),
            1000,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "goal.create",
                "agent_id": "main",
                "goalId": "goal-1",
                "objective": "ship",
                "createdAt": 1000,
                "updatedAt": 1000,
            })),
            1001,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "goal.update",
                "agent_id": "main",
                "status": "blocked",
                "reason": "no budget",
                "wallClockMs": 5,
            })),
            1002,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "goal.clear",
                "agent_id": "main",
            })),
            1003,
        );

        let records = read_records(&journal.journal_path("sess-1", "main").unwrap());
        assert_eq!(
            record_types(&records),
            vec![
                "metadata",
                "tools.update_store",
                "goal.create",
                "goal.update",
                "goal.clear",
            ]
        );
        assert_eq!(records[1]["agentId"], json!("main"));
        assert_eq!(records[1]["key"], json!("todo"));
        assert_eq!(records[1]["value"][0]["id"], json!("T1"));
        assert!(records[1].get("agent_id").is_none());
        assert_eq!(records[1]["time"], json!(1000));
        assert_eq!(records[2]["goalId"], json!("goal-1"));
        assert_eq!(records[3]["status"], json!("blocked"));
        assert_eq!(records[3]["time"], json!(1002));
        assert!(records[4].get("goalId").is_none());
    }

    #[test]
    fn agent_records_are_registration_only() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        run_turn(&journal);
        let path = journal.journal_path("sess-1", "main").unwrap();
        let before = read_records(&path);
        let baseline = fold_wire_records(&before, None).expect("the journal must fold");

        journal.append(
            "sess-1",
            &custom(json!({
                "type": "agent.turn.started",
                "agent_id": "main",
                "kind": "event",
                "turnId": 1,
            })),
            2100,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "agent.message.appended",
                "agent_id": "main",
                "kind": "event",
                "message": { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
                "meta": { "source": "input" },
            })),
            2101,
        );
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "agent.turn.ended",
                "agent_id": "main",
                "kind": "event",
                "turnId": 1,
                "outcome": "done",
            })),
            2102,
        );

        let after = read_records(&path);
        assert_eq!(after.len(), before.len() + 3);
        for record in &after[before.len()..] {
            assert!(record.get("agentId").is_none(), "{record}");
            assert!(record.get("agent_id").is_none(), "{record}");
            assert_eq!(record["kind"], json!("event"), "{record}");
            assert!(record["time"].is_number(), "{record}");
        }
        assert_eq!(after[before.len()]["turnId"], json!(1));
        assert_eq!(after[before.len() + 1]["message"]["role"], json!("user"));
        assert_eq!(after[before.len() + 2]["outcome"], json!("done"));

        let folded = fold_wire_records(&after, None).expect("the journal must fold");
        assert_eq!(folded.replay, baseline.replay);
        assert_eq!(folded.tool_store, baseline.tool_store);
    }

    #[test]
    fn injected_context_messages_are_written_as_v2_appends() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        run_turn(&journal);
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "context.append_message",
                "agentId": "main",
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "<system-reminder>\nreminder\n</system-reminder>" }],
                    "toolCalls": [],
                    "origin": { "kind": "injection", "variant": "agents_md" },
                },
            })),
            2200,
        );
        let path = journal.journal_path("sess-1", "main").unwrap();
        let records = read_records(&path);
        let appended = records.last().unwrap();
        assert_eq!(appended["type"], json!("context.append_message"));
        assert_eq!(appended["agentId"], json!("main"));
        assert_eq!(appended["time"], json!(2200));
        assert_eq!(appended["message"]["origin"]["kind"], json!("injection"));
        let folded = fold_wire_records(&records, None).expect("the journal must fold");
        let last = folded.replay.last().unwrap();
        assert_eq!(last["message"]["origin"]["kind"], json!("injection"));
        assert!(
            last["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("reminder")
        );
    }

    #[test]
    fn a_subagent_prompt_lands_in_its_own_journal() {
        let (dir, store, workspace_id) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        run_turn(&journal);
        let main_before = read_records(&journal.journal_path("sess-1", "main").unwrap());
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "turn.prompt",
                "agent_id": "agent-7",
                "input": [{ "type": "text", "text": "scan it" }],
                "origin": { "kind": "system_trigger", "name": "subagent" },
            })),
            3000,
        );

        let main_after = read_records(&journal.journal_path("sess-1", "main").unwrap());
        assert_eq!(main_after, main_before, "the main journal must not change");
        assert!(
            main_after
                .iter()
                .all(|record| record["agentId"] != json!("agent-7"))
        );

        let sub_path = dir
            .path()
            .join("sessions")
            .join(&workspace_id)
            .join("sess-1")
            .join("agents")
            .join("agent-7")
            .join(WIRE_FILE_NAME);
        assert_eq!(
            journal.journal_path("sess-1", "agent-7"),
            Some(sub_path.clone())
        );
        let sub_records = read_records(&sub_path);
        assert_eq!(
            record_types(&sub_records),
            vec!["metadata", "turn.prompt", "context.append_message"]
        );
        assert_eq!(sub_records[1]["agentId"], json!("agent-7"));
        assert!(sub_records[1].get("agent_id").is_none());
        assert!(sub_records[1].get("turnId").is_none());
        assert_eq!(sub_records[2]["agentId"], json!("agent-7"));
        assert_eq!(
            sub_records[2]["message"]["content"][0]["text"],
            json!("scan it")
        );
        assert_eq!(
            sub_records[2]["message"]["origin"]["kind"],
            json!("system_trigger")
        );
        let folded = fold_wire_records(&sub_records, None).expect("the journal must fold");
        assert_eq!(folded.replay.len(), 1, "{:?}", folded.replay);
        assert_eq!(
            folded.replay[0]["message"]["content"][0]["text"],
            json!("scan it")
        );
    }

    #[test]
    fn an_llm_request_record_is_registration_only() {
        let (dir, store, _) = fixture();
        let journal = WireJournal::new(dir.path(), store);
        run_turn(&journal);
        let path = journal.journal_path("sess-1", "main").unwrap();
        let before = read_records(&path);
        let baseline = fold_wire_records(&before, None).expect("the journal must fold");
        journal.append(
            "sess-1",
            &custom(json!({
                "type": "llm.request",
                "agent_id": "main",
                "kind": "loop",
                "provider": "openai",
                "model": "kimi-k2",
                "modelAlias": "kimi",
                "thinkingEffort": "high",
                "toolSelect": true,
                "systemPromptHash": "aa",
                "toolsHash": "bb",
                "messageCount": 3,
                "turnStep": "turn-1.1",
            })),
            3100,
        );
        let after = read_records(&path);
        let request = after.last().unwrap();
        assert_eq!(request["type"], json!("llm.request"));
        assert_eq!(request["agentId"], json!("main"));
        assert_eq!(request["provider"], json!("openai"));
        assert_eq!(request["model"], json!("kimi-k2"));
        assert_eq!(request["toolSelect"], json!(true));
        assert_eq!(request["messageCount"], json!(3));
        assert_eq!(request["turnStep"], json!("turn-1.1"));
        assert_eq!(request["time"], json!(3100));
        assert!(request.get("agent_id").is_none());
        let folded = fold_wire_records(&after, None).expect("the journal must fold");
        assert_eq!(folded.replay, baseline.replay);
        assert_eq!(folded.tool_store, baseline.tool_store);
    }

    #[test]
    fn a_tool_schema_message_round_trips_through_the_journal() {
        let records = vec![json!({
            "type": "context.append_message",
            "message": {
                "role": "system",
                "content": [],
                "toolCalls": [],
                "tools": [{
                    "name": "mcp__s__alpha",
                    "description": "alpha tool",
                    "parameters": {"type": "object", "properties": {}},
                }],
                "origin": {"kind": "injection", "variant": "dynamic_tool_schema"},
            },
            "time": 5,
        })];
        let history = fold_history(&records).expect("the schema message must fold");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "system");
        assert_eq!(history[0].tools.len(), 1);
        assert_eq!(history[0].tools[0].name, "mcp__s__alpha");
        assert_eq!(
            history[0].tools[0].parameters,
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn duplicate_branches_are_reported_without_changing_the_active_projection() {
        #[derive(Clone, Default)]
        struct SharedBuffer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let switched = |base_branch: &str, base_line: u64| json!({"type":"agent.switched","branch":"b1","base":{"branch":base_branch,"line":base_line}});
        let records = vec![
            json!({"type":"metadata"}),
            json!({"type":"wire.test.a"}),
            json!({"type":"wire.test.b"}),
            switched("main", 1),
            switched("main", 3),
            json!({"type":"context.undo"}),
            json!({"type":"context.undone"}),
            json!({"type":"wire.test.c"}),
            json!({"type":"wire.test.d"}),
        ];
        let buffer = SharedBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let restored =
            tracing::subscriber::with_default(subscriber, || restorable_records(&records));
        assert_eq!(
            record_types(&restored),
            vec![
                "metadata",
                "wire.test.a",
                "wire.test.b",
                "context.undo",
                "wire.test.c",
                "wire.test.d",
            ]
        );
        let log = String::from_utf8(buffer.0.lock().clone()).unwrap();
        assert!(
            log.contains("Duplicate agent.switched branch 'b1' ignored during tree projection"),
            "{log}"
        );
        assert!(log.contains("wire.unknown_record"), "{log}");
    }

    #[test]
    fn a_newer_protocol_with_a_compatible_reader_is_preserved_without_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIRE_FILE_NAME);
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"metadata\",\"protocol_version\":\"9.9\",\"min_protocol_version\":\"1.5\",\"created_at\":1}\n",
                "{\"type\":\"plan.revision\",\"path\":\"sessions/source/session-1/agents/main/plan/p/v1.md\",\"time\":2}\n",
            ),
        )
        .unwrap();

        let records = read_file_records(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "plan.revision");
        assert_eq!(
            records[0]["path"],
            "sessions/source/session-1/agents/main/plan/p/v1.md"
        );
    }

    #[test]
    fn a_newer_minimum_reader_is_rejected_with_the_v2_upgrade_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIRE_FILE_NAME);
        std::fs::write(
            &path,
            "{\"type\":\"metadata\",\"protocol_version\":\"1.5\",\"min_protocol_version\":\"99.0\",\"created_at\":1}\n",
        )
        .unwrap();

        let error = read_file_records(&path).unwrap_err();
        assert!(matches!(error, WireReadError::Unsupported(_)));
        assert_eq!(
            error.to_string(),
            "This session requires a newer version of Kimi Code. Upgrade Kimi Code to the latest version to read this session."
        );
    }

    #[test]
    fn active_and_restorable_records_follow_the_nested_branch_base_chain() {
        let switched = |branch: &str, base_branch: &str, base_line: u64| json!({"type":"agent.switched","branch":branch,"base":{"branch":base_branch,"line":base_line}});
        let records = vec![
            json!({"type":"metadata"}),
            json!({"type":"wire.test.one"}),
            json!({"type":"wire.test.two"}),
            switched("b1", "main", 2),
            json!({"type":"context.undo"}),
            json!({"type":"context.undone"}),
            json!({"type":"wire.test.three"}),
            switched("b2", "b1", 7),
            json!({"type":"context.undo"}),
            json!({"type":"context.undone"}),
            json!({"type":"wire.test.four"}),
        ];

        let restored = restorable_records(&records);
        assert_eq!(
            record_types(&restored),
            vec![
                "metadata",
                "wire.test.one",
                "context.undo",
                "wire.test.three",
                "context.undo",
                "wire.test.four",
            ]
        );
    }

    #[test]
    fn active_chain_rejects_unknown_and_cyclic_base_branches() {
        let unknown = vec![
            json!({"type":"metadata"}),
            json!({"type":"agent.switched","branch":"b1","base":{"branch":"missing","line":1}}),
        ];
        let error = fold_history(&unknown).unwrap_err();
        assert!(error.contains("unknown branch 'missing'"), "{error}");

        let cyclic = vec![
            json!({"type":"metadata"}),
            json!({"type":"agent.switched","branch":"b1","base":{"branch":"b2","line":1}}),
            json!({"type":"agent.switched","branch":"b2","base":{"branch":"b1","line":1}}),
        ];
        let error = fold_history(&cyclic).unwrap_err();
        assert!(error.contains("cycles at branch"), "{error}");
    }

    #[test]
    fn restorable_records_reinclude_only_ownerless_or_surviving_prompt_injections() {
        let prompt = |id: &str| json!({"type":"context.append_message","message":{"role":"user","id":id,"content":[{"type":"text","text":id}],"origin":{"kind":"user"}}});
        let injection = |variant: &str, owner: Option<&str>| {
            let mut origin = json!({"kind":"injection","variant":variant});
            if let Some(owner) = owner {
                origin["ownerPromptId"] = json!(owner);
            }
            json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":variant}],"origin":origin}})
        };
        let records = vec![
            json!({"type":"metadata"}),
            prompt("p1"),
            injection("before-switch-p1", Some("p1")),
            prompt("p2"),
            injection("removed-p2", Some("p2")),
            json!({"type":"agent.switched","branch":"b1","base":{"branch":"main","line":2},"legacyUndoLine":10}),
            injection("ownerless", None),
            injection("after-switch-p1", Some("p1")),
            json!({"type":"context.undone"}),
            json!({"type":"context.undo"}),
        ];

        let restored = restorable_records(&records);
        let identities: Vec<String> = restored
            .iter()
            .map(|record| {
                if record["type"] == "metadata" {
                    return "metadata".to_string();
                }
                record["message"]["origin"]["variant"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| record["message"]["id"].as_str().map(str::to_string))
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            identities,
            vec![
                "metadata",
                "p1",
                "before-switch-p1",
                "ownerless",
                "after-switch-p1"
            ]
        );
        assert!(restored.iter().all(|record| {
            record["type"] != "agent.switched"
                && record["type"] != "context.undone"
                && record["type"] != "context.undo"
        }));
    }

    #[test]
    fn a_session_without_a_workspace_scope_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap());
        store.create_session("sess-noscope", None).unwrap();
        let journal = WireJournal::new(dir.path(), store);
        journal.append(
            "sess-noscope",
            &custom(json!({ "type": "turn.ended", "turnId": 1, "reason": "completed" })),
            1,
        );
        assert_eq!(journal.journal_path("sess-noscope", "main"), None);
    }
}
