use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::human::agent::origin::{
    BundledSkillActivation, PromptFileAttachment, annotate_bundled_skill_parts,
    is_skill_activation_part,
};
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::ContentPart;
use crate::human::llm::usage::TokenUsage;
use crate::llm::media_resolver::parse_daemon_file_url;

pub use crate::session::cache_observation::{
    CacheBreakCause, CacheBreakObservation, CacheBreakSnapshot, CacheObservation,
};

pub type PromptOrigin = Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInterruptReason {
    UserCancelled,
    Aborted,
    MaxSteps,
    Error,
    Filtered,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPromptAttachmentFile {
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TurnPromptAttachment {
    #[serde(rename_all = "camelCase")]
    Image {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Video {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Audio {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    File(TurnPromptAttachmentFile),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedPayload {
    pub agent_id: String,
    pub turn_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    pub origin: PromptOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_attachments: Option<Vec<TurnPromptAttachment>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStepStartedPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub step: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStepCompletedPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub step: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_break: Option<CacheBreakObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_first_token_latency_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_stream_duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_request_build_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_server_first_token_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_server_decode_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_client_consume_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_client_blocked_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_first_token_offset_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_last_token_offset_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_window_on_frame_clock: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStepInterruptedPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub step: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStepRetryingPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub step: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    pub error_name: String,
    pub error_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantDeltaPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingDeltaPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDeltaPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_part: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<u32>,
}

macro_rules! agent_event {
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
                    serde_json::to_value(&payload).expect("turn event is serializable"),
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

agent_event!(TurnStarted, TurnStartedPayload, TURN_STARTED_CLASS, "turn.started", false, None);
agent_event!(TurnStepStarted, TurnStepStartedPayload, TURN_STEP_STARTED_CLASS, "turn.step.started", false, None);
agent_event!(TurnStepCompleted, TurnStepCompletedPayload, TURN_STEP_COMPLETED_CLASS, "turn.step.completed", false, None);
agent_event!(
    TurnStepInterrupted,
    TurnStepInterruptedPayload,
    TURN_STEP_INTERRUPTED_CLASS,
    "turn.step.interrupted",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "turnId", "step", "reason"],
        "properties": {
            "agentId": {"type": "string"},
            "turnId": {"type": "number"},
            "step": {"type": "number"},
            "stepId": {"type": "string"},
            "reason": {"type": "string"},
            "message": {"type": "string"}
        }
    }))
);
agent_event!(
    TurnStepRetrying,
    TurnStepRetryingPayload,
    TURN_STEP_RETRYING_CLASS,
    "turn.step.retrying",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "turnId", "step", "failedAttempt", "nextAttempt", "maxAttempts", "delayMs", "errorName", "errorMessage"],
        "properties": {
            "agentId": {"type": "string"},
            "turnId": {"type": "number"},
            "step": {"type": "number"},
            "stepId": {"type": "string"},
            "failedAttempt": {"type": "number"},
            "nextAttempt": {"type": "number"},
            "maxAttempts": {"type": "number"},
            "delayMs": {"type": "number"},
            "errorName": {"type": "string"},
            "errorMessage": {"type": "string"},
            "statusCode": {"type": "number"}
        }
    }))
);
agent_event!(AssistantDelta, AssistantDeltaPayload, ASSISTANT_DELTA_CLASS, "assistant.delta", false, None);
agent_event!(ThinkingDelta, ThinkingDeltaPayload, THINKING_DELTA_CLASS, "thinking.delta", false, None);
agent_event!(ToolCallDelta, ToolCallDeltaPayload, TOOL_CALL_DELTA_CLASS, "tool.call.delta", false, None);

pub fn register_turn_event_classes() -> Result<(), String> {
    register_event2_class(&TURN_STEP_INTERRUPTED_CLASS)?;
    register_event2_class(&TURN_STEP_RETRYING_CLASS)?;
    Ok(())
}

pub fn turn_prompt_text(input: &[ContentPart], origin: Option<&PromptOrigin>) -> Option<String> {
    let activations = origin
        .filter(|origin| origin.get("kind").and_then(Value::as_str) == Some("user"))
        .and_then(|origin| origin.get("skillActivations"))
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<BundledSkillActivation>>(value).ok())
        .unwrap_or_default();
    let text = annotate_bundled_skill_parts(input, &activations)
        .into_iter()
        .filter(|part| matches!(part, ContentPart::Text { .. }) && !is_skill_activation_part(part))
        .map(|part| match part {
            ContentPart::Text { text, .. } => text,
            _ => unreachable!(),
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn prompt_media_file_id(url: &str, id: Option<&str>) -> Option<String> {
    let parsed = parse_daemon_file_url(url);
    match id {
        Some(id) if parsed == Some(id) => Some(id.to_string()),
        Some(_) => None,
        None => parsed.map(ToString::to_string),
    }
}

pub fn turn_prompt_attachments(
    input: &[ContentPart],
    origin: Option<&PromptOrigin>,
) -> Option<Vec<TurnPromptAttachment>> {
    let mut attachments = Vec::new();
    for part in input {
        match part {
            ContentPart::ImageUrl { image_url } => {
                if let Some(file_id) = prompt_media_file_id(&image_url.url, image_url.id.as_deref()) {
                    attachments.push(TurnPromptAttachment::Image {
                        file_id,
                        name: image_url.name.clone(),
                    });
                }
            }
            ContentPart::VideoUrl { video_url } => {
                if let Some(file_id) = prompt_media_file_id(&video_url.url, video_url.id.as_deref()) {
                    attachments.push(TurnPromptAttachment::Video {
                        file_id,
                        name: video_url.name.clone(),
                    });
                }
            }
            ContentPart::AudioUrl { audio_url } => {
                if let Some(file_id) = prompt_media_file_id(&audio_url.url, audio_url.id.as_deref()) {
                    attachments.push(TurnPromptAttachment::Audio {
                        file_id,
                        name: None,
                    });
                }
            }
            _ => {}
        }
    }
    if origin.is_some_and(|origin| {
        matches!(
            origin.get("kind").and_then(Value::as_str),
            Some("user" | "skill_activation")
        )
    }) {
        let files = origin
            .and_then(|origin| origin.get("attachments"))
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<PromptFileAttachment>>(value).ok())
            .unwrap_or_default();
        attachments.extend(files.into_iter().map(|file| {
            TurnPromptAttachment::File(TurnPromptAttachmentFile {
                name: file.name,
                media_type: file.media_type,
                size: file.size,
                path: file.path,
            })
        }));
    }
    (!attachments.is_empty()).then_some(attachments)
}

pub fn is_displayable_prompt_origin(origin: &PromptOrigin) -> bool {
    let kind = origin.get("kind").and_then(Value::as_str);
    if kind == Some("user") {
        return true;
    }
    if kind == Some("system_trigger") && origin.get("name").and_then(Value::as_str) == Some("subagent") {
        return true;
    }
    matches!(kind, Some("skill_activation" | "plugin_command"))
        && origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypedTurnEvent<T> {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(flatten)]
    pub payload: T,
}

pub type TurnStartedEvent = TypedTurnEvent<TurnStartedPayload>;
pub type TurnStepStartedEvent = TypedTurnEvent<TurnStepStartedPayload>;
pub type TurnStepCompletedEvent = TypedTurnEvent<TurnStepCompletedPayload>;
pub type TurnStepRetryingEvent = TypedTurnEvent<TurnStepRetryingPayload>;
pub type TurnStepInterruptedEvent = TypedTurnEvent<TurnStepInterruptedPayload>;
pub type AssistantDeltaEvent = TypedTurnEvent<AssistantDeltaPayload>;
pub type ThinkingDeltaEvent = TypedTurnEvent<ThinkingDeltaPayload>;
