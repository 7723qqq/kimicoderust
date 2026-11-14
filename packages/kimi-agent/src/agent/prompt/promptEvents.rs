use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::human::llm::message::ContentPart;

use super::message_content::MessageContent;

pub type ClientMetadata = Vec<Map<String, Value>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCompletionReason {
    Completed,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptSubmittedEventStatus {
    Running,
    Queued,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptSubmittedPayloadStatus {
    Running,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCompletedPayload {
    pub agent_id: String,
    pub prompt_id: String,
    pub finished_at: String,
    pub reason: PromptCompletionReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCompletedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub prompt_id: String,
    pub finished_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PromptCompletionReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptAbortedPayload {
    pub agent_id: String,
    pub prompt_id: String,
    pub aborted_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptAbortedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub prompt_id: String,
    pub aborted_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSubmittedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub prompt_id: String,
    pub user_message_id: String,
    pub status: PromptSubmittedEventStatus,
    pub content: Vec<MessageContent>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSteeredEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub active_prompt_id: String,
    pub prompt_ids: Vec<String>,
    pub content: Vec<MessageContent>,
    pub steered_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSteeredPayload {
    pub agent_id: String,
    pub active_prompt_id: String,
    pub prompt_ids: Vec<String>,
    pub content: Vec<ContentPart>,
    pub steered_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptQueuedPayload {
    pub agent_id: String,
    pub prompt_id: String,
    pub content: Vec<ContentPart>,
    pub queue_length: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<ClientMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSubmittedPayload {
    pub agent_id: String,
    pub prompt_id: String,
    pub user_message_id: String,
    pub status: PromptSubmittedPayloadStatus,
    pub content: Vec<ContentPart>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<ClientMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptStartedPayload {
    pub agent_id: String,
    pub prompt_id: String,
}

macro_rules! prompt_event {
    ($name:ident, $payload:ident, $class:ident, $type:literal, $durable:literal, $schema:expr) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: $durable,
                observable: true,
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
            pub const DURABLE: bool = $durable;
            pub const OBSERVABLE: bool = true;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("prompt event is serializable"),
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

prompt_event!(
    PromptCompleted,
    PromptCompletedPayload,
    PROMPT_COMPLETED_CLASS,
    "prompt.completed",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "promptId", "finishedAt", "reason"],
        "properties": {
            "agentId": {"type": "string"},
            "promptId": {"type": "string", "minLength": 1},
            "finishedAt": {"type": "string"},
            "reason": {"enum": ["completed", "failed", "blocked"]}
        }
    }))
);
prompt_event!(
    PromptAborted,
    PromptAbortedPayload,
    PROMPT_ABORTED_CLASS,
    "prompt.aborted",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "promptId", "abortedAt"],
        "properties": {
            "agentId": {"type": "string"},
            "promptId": {"type": "string", "minLength": 1},
            "abortedAt": {"type": "string"}
        }
    }))
);
prompt_event!(
    PromptSteered,
    PromptSteeredPayload,
    PROMPT_STEERED_CLASS,
    "prompt.steered",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "activePromptId", "promptIds", "content", "steeredAt"],
        "properties": {
            "agentId": {"type": "string"},
            "activePromptId": {"type": "string"},
            "promptIds": {"type": "array", "items": {"type": "string"}},
            "content": {},
            "steeredAt": {"type": "string"},
            "messageId": {"type": "string"}
        }
    }))
);
prompt_event!(
    PromptQueued,
    PromptQueuedPayload,
    PROMPT_QUEUED_CLASS,
    "prompt.queued",
    false,
    None
);
prompt_event!(
    PromptSubmitted,
    PromptSubmittedPayload,
    PROMPT_SUBMITTED_CLASS,
    "prompt.submitted",
    false,
    None
);
prompt_event!(
    PromptStarted,
    PromptStartedPayload,
    PROMPT_STARTED_CLASS,
    "prompt.started",
    false,
    None
);

pub fn register_prompt_event_classes() -> Result<(), String> {
    register_event2_class(&PROMPT_COMPLETED_CLASS)?;
    register_event2_class(&PROMPT_ABORTED_CLASS)?;
    register_event2_class(&PROMPT_STEERED_CLASS)?;
    Ok(())
}
