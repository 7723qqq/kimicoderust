use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Approval,
    Question,
    UserTool,
}

impl InteractionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Question => "question",
            Self::UserTool => "user_tool",
        }
    }
}

impl PartialEq<&str> for InteractionKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InteractionTagValue {
    String(String),
    Number(f64),
}

impl From<String> for InteractionTagValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for InteractionTagValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<f64> for InteractionTagValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<i64> for InteractionTagValue {
    fn from(value: i64) -> Self {
        Self::Number(value as f64)
    }
}

impl From<u64> for InteractionTagValue {
    fn from(value: u64) -> Self {
        Self::Number(value as f64)
    }
}

pub type InteractionTags = HashMap<String, InteractionTagValue>;

pub const INTERACTION_TAG_AGENT_ID: &str = "agentId";
pub const INTERACTION_TAG_SESSION_ID: &str = "sessionId";
pub const INTERACTION_TAG_TURN_ID: &str = "turnId";
pub const INTERACTION_TAG_TOOL_CALL_ID: &str = "toolCallId";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionRequest<TPayload = Value> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: InteractionKind,
    pub payload: TPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<InteractionTags>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interaction<TPayload = Value> {
    pub id: String,
    pub kind: InteractionKind,
    pub payload: TPayload,
    pub tags: InteractionTags,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionCancellationReason {
    TurnEnded,
    AgentClosed,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionCancellation {
    pub cancelled: bool,
    pub reason: InteractionCancellationReason,
}

pub fn is_interaction_cancellation(response: &Value) -> bool {
    response.get("cancelled").and_then(Value::as_bool) == Some(true)
        && response.get("reason").and_then(Value::as_str).is_some_and(|reason| {
            matches!(reason, "turn_ended" | "agent_closed" | "unsupported")
        })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionResolution {
    pub id: String,
    pub response: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionPendingChangedEvent {
    pub pending: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InteractionQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<InteractionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<InteractionTags>,
}
