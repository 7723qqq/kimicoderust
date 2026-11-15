use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};

use super::loop_event_fold::LoopRecordedEvent;
use super::types::ContextMessage;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextAppendMessagePayload {
    pub agent_id: String,
    pub message: ContextMessage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextAppendLoopEventPayload {
    pub agent_id: String,
    pub event: LoopRecordedEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextClearPayload {
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCompactionWireLines {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextApplyCompactionKind {
    Summary {
        summary: String,
        #[serde(rename = "compactedCount")]
        compacted_count: usize,
        #[serde(rename = "contextSummary", default, skip_serializing_if = "Option::is_none")]
        context_summary: Option<String>,
    },
    ContextSummary {
        #[serde(rename = "contextSummary")]
        context_summary: String,
        #[serde(rename = "compactedCount")]
        compacted_count: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    Legacy {
        summary: ContextMessage,
        count: usize,
        #[serde(rename = "compactedCount", default, skip_serializing_if = "Option::is_none")]
        compacted_count: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextApplyCompactionPayload {
    pub agent_id: String,
    #[serde(flatten)]
    pub compaction: ContextApplyCompactionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_user_message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_head_user_message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appended_injection_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_tail: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_lines: Option<ContextCompactionWireLines>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUndoPayload {
    pub agent_id: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSplicedPayload {
    pub agent_id: String,
    pub start: usize,
    pub delete_count: usize,
    pub messages: Vec<ContextMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

macro_rules! context_event {
    ($name:ident, $payload:ty, $class:ident, $type:literal, $durable:literal, $observable:literal, $schema:expr) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: $durable,
                observable: $observable,
                agent_domain: true,
                schema: $schema,
            })
        });

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("context event is serializable"),
                    None,
                );
                Self { event, payload }
            }

            pub fn class() -> Arc<Event2Class> {
                $class.clone()
            }

            pub fn as_event(&self) -> &Event2 {
                &self.event
            }
        }

        impl Deref for $name {
            type Target = $payload;

            fn deref(&self) -> &Self::Target {
                &self.payload
            }
        }

        impl From<$name> for Event2 {
            fn from(event: $name) -> Self {
                event.event
            }
        }
    };
}

context_event!(
    ContextAppendMessage,
    ContextAppendMessagePayload,
    CONTEXT_APPEND_MESSAGE_CLASS,
    "context.append_message",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "message"],
        "properties": {"agentId": {"type": "string"}, "message": {}}
    }))
);
context_event!(
    ContextAppendLoopEvent,
    ContextAppendLoopEventPayload,
    CONTEXT_APPEND_LOOP_EVENT_CLASS,
    "context.append_loop_event",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "event"],
        "properties": {"agentId": {"type": "string"}, "event": {}}
    }))
);
context_event!(
    ContextClear,
    ContextClearPayload,
    CONTEXT_CLEAR_CLASS,
    "context.clear",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId"],
        "properties": {"agentId": {"type": "string"}}
    }))
);
fn context_apply_compaction_schema() -> Value {
    let base = json!({
        "type": "object",
        "properties": {
            "agentId": {"type": "string"},
            "tokensBefore": {"type": "number"},
            "tokensAfter": {"type": "number"},
            "summaryOutputTokens": {"type": "number"},
            "keptUserMessageCount": {"type": "number"},
            "keptHeadUserMessageCount": {"type": "number"},
            "appendedInjectionCount": {"type": "number"},
            "droppedCount": {"type": "number"},
            "legacyTail": {"type": "boolean"},
            "wireLines": {
                "type": "object",
                "required": ["start", "end"],
                "properties": {
                    "start": {"type": "integer", "minimum": 0},
                    "end": {"type": "integer", "minimum": 0},
                },
            },
        },
    });
    json!({
        "allOf": [
            base,
            {"anyOf": [
                {
                    "required": ["agentId", "summary", "compactedCount"],
                    "properties": {
                        "summary": {"type": "string"},
                        "compactedCount": {"type": "number"},
                        "contextSummary": {"type": "string"},
                    },
                },
                {
                    "required": ["agentId", "contextSummary", "compactedCount"],
                    "properties": {
                        "contextSummary": {"type": "string"},
                        "compactedCount": {"type": "number"},
                        "summary": {"type": "string"},
                    },
                },
                {
                    "required": ["agentId", "summary", "count"],
                    "properties": {
                        "summary": {},
                        "count": {"type": "number"},
                        "compactedCount": {"type": "number"},
                    },
                },
            ]},
        ],
    })
}

context_event!(
    ContextApplyCompaction,
    ContextApplyCompactionPayload,
    CONTEXT_APPLY_COMPACTION_CLASS,
    "context.apply_compaction",
    true,
    false,
    Some(context_apply_compaction_schema())
);
context_event!(
    ContextUndo,
    ContextUndoPayload,
    CONTEXT_UNDO_CLASS,
    "context.undo",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "count"],
        "properties": {
            "agentId": {"type": "string"},
            "count": {"type": "integer", "exclusiveMinimum": 0, "maximum": 9007199254740991u64}
        }
    }))
);
context_event!(
    ContextSpliced,
    ContextSplicedPayload,
    CONTEXT_SPLICED_CLASS,
    "context.spliced",
    false,
    true,
    None::<Value>
);
