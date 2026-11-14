use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::event::event2::{Event2, Event2Class};
use crate::human::llm::thinking::ThinkingEffort;
use crate::state::state::{ReplayableOptions, ReplayableStateKey, define_state};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequestToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmRequestTraceState {
    pub seen_tools_hashes: Vec<String>,
}

fn llm_tool_entry_schema() -> Value {
    json!({
        "type": "object",
        "required": ["name", "description", "parameters"],
        "properties": {
            "name": {"type": "string"},
            "description": {"type": "string"},
            "parameters": {"type": "object"},
        },
    })
}

fn llm_tools_snapshot_schema() -> Value {
    json!({
        "type": "object",
        "required": ["agentId", "hash", "tools"],
        "properties": {
            "agentId": {"type": "string"},
            "hash": {"type": "string"},
            "tools": {"type": "array", "items": llm_tool_entry_schema()},
        },
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmToolsSnapshotPayload {
    pub agent_id: String,
    pub hash: String,
    pub tools: Vec<LlmRequestToolSchema>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmToolsSnapshot {
    event: Event2,
    pub payload: LlmToolsSnapshotPayload,
}

pub static LLM_TOOLS_SNAPSHOT_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: LlmToolsSnapshot::TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(llm_tools_snapshot_schema()),
    })
});

impl LlmToolsSnapshot {
    pub const TYPE: &'static str = "llm.tools_snapshot";
    pub const DURABLE: bool = true;

    pub fn new(payload: LlmToolsSnapshotPayload) -> Self {
        let event = Event2::new(
            &LLM_TOOLS_SNAPSHOT_CLASS,
            serde_json::to_value(&payload).expect("llm tools snapshot is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        LLM_TOOLS_SNAPSHOT_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for LlmToolsSnapshot {
    type Target = LlmToolsSnapshotPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<LlmToolsSnapshot> for Event2 {
    fn from(event: LlmToolsSnapshot) -> Self {
        event.event
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmRequestKind {
    Loop,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LlmRequestProjection {
    Strict,
    MediaDegraded,
    MediaStripped,
    StrictMediaDegraded,
    StrictMediaStripped,
}

impl LlmRequestProjection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::MediaDegraded => "media-degraded",
            Self::MediaStripped => "media-stripped",
            Self::StrictMediaDegraded => "strict-media-degraded",
            Self::StrictMediaStripped => "strict-media-stripped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmRequestPayload {
    pub agent_id: String,
    pub kind: LlmRequestKind,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<ThinkingEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_keep: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta_api: Option<bool>,
    pub tool_select: bool,
    pub system_prompt_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub tools_hash: String,
    pub message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<LlmRequestProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_count: Option<usize>,
}

fn llm_request_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "agentId", "kind", "provider", "model", "toolSelect", "systemPromptHash",
            "toolsHash", "messageCount"
        ],
        "properties": {
            "agentId": {"type": "string"},
            "kind": {"enum": ["loop", "compaction"]},
            "provider": {"type": "string"},
            "model": {"type": "string"},
            "modelAlias": {"type": "string"},
            "thinkingEffort": {},
            "thinkingKeep": {"type": "string"},
            "temperature": {"type": "number"},
            "topP": {"type": "number"},
            "maxTokens": {"type": "number"},
            "betaApi": {"type": "boolean"},
            "toolSelect": {"type": "boolean"},
            "systemPromptHash": {"type": "string"},
            "systemPrompt": {"type": "string"},
            "toolsHash": {"type": "string"},
            "messageCount": {"type": "number"},
            "turnStep": {"type": "string"},
            "attempt": {"type": "string"},
            "projection": {"enum": [
                "strict", "media-degraded", "media-stripped", "strict-media-degraded",
                "strict-media-stripped"
            ]},
            "droppedCount": {"type": "number"},
        },
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    event: Event2,
    pub payload: LlmRequestPayload,
}

pub static LLM_REQUEST_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: LlmRequest::TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(llm_request_schema()),
    })
});

impl LlmRequest {
    pub const TYPE: &'static str = "llm.request";
    pub const DURABLE: bool = true;

    pub fn new(payload: LlmRequestPayload) -> Self {
        let event = Event2::new(
            &LLM_REQUEST_CLASS,
            serde_json::to_value(&payload).expect("llm request is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        LLM_REQUEST_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for LlmRequest {
    type Target = LlmRequestPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<LlmRequest> for Event2 {
    fn from(event: LlmRequest) -> Self {
        event.event
    }
}

pub static LLM_REQUEST_TRACE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = define_state(
        "llm.requestTrace",
        Arc::new(|| json!({"seenToolsHashes": []})),
    )
    .replayable(ReplayableOptions {
        schema: json!({}),
        durable: None,
        blobs: None,
        mutable_replay: None,
    });
    key.on(&LLM_TOOLS_SNAPSHOT_CLASS, Arc::new(|state, event, _ctx| {
        let hash = event.payload.get("hash").and_then(Value::as_str)?;
        let mut state = state;
        let hashes = state
            .get("seenToolsHashes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if hashes.iter().any(|entry| entry.as_str() == Some(hash)) {
            return None;
        }
        let mut hashes = hashes;
        hashes.push(Value::String(hash.to_string()));
        state
            .as_object_mut()
            .expect("llm request trace state is an object")
            .insert("seenToolsHashes".to_string(), Value::Array(hashes));
        Some(state)
    }))
    .expect("llm tools snapshot fold can be registered");
    key.on(&LLM_REQUEST_CLASS, Arc::new(|_state, _event, _ctx| None))
        .expect("llm request fold can be registered");
    key
});

pub fn llm_request_trace_key() -> Arc<ReplayableStateKey> {
    LLM_REQUEST_TRACE_KEY.clone()
}
