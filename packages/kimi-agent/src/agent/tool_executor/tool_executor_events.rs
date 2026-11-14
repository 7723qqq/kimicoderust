use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::event::event2::{Event2, Event2Class};
use crate::tool::tool_input_display::ToolInputDisplay;

pub use crate::events::types::EngineEvent;
pub use crate::mcp::status::tool_list_updated_event;
pub use crate::human::tool::executor::ToolUpdate;

macro_rules! observable_agent_event {
    ($name:ident, $payload:ident, $type:literal) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;
            pub const OBSERVABLE: bool = true;

            pub fn new(payload: $payload) -> Self {
                let class = Self::class();
                let event = Event2::new(
                    &class,
                    serde_json::to_value(&payload).expect("tool executor event is serializable"),
                    None,
                );
                Self { event, payload }
            }

            pub fn class() -> Arc<Event2Class> {
                static CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
                    Arc::new(Event2Class {
                        type_: $type.to_string(),
                        durable: false,
                        observable: true,
                        agent_domain: true,
                        schema: None,
                    })
                });
                CLASS.clone()
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallStartedPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub name: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolInputDisplay>,
}

observable_agent_event!(ToolCallStarted, ToolCallStartedPayload, "tool.call.started");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProgressPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub update: ToolUpdate,
}

observable_agent_event!(ToolProgress, ToolProgressPayload, "tool.progress");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEventPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<bool>,
}

observable_agent_event!(ToolResultEvent, ToolResultEventPayload, "tool.result");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDeltaEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_part: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallStartedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub name: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolInputDisplay>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProgressEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub update: ToolUpdate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub command_id: String,
    pub update: ToolUpdate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellStartedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub command_id: String,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellCompletedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub command_id: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolListUpdatedReason {
    #[serde(rename = "mcp.connected")]
    McpConnected,
    #[serde(rename = "mcp.disconnected")]
    McpDisconnected,
    #[serde(rename = "mcp.failed")]
    McpFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolListUpdatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub reason: ToolListUpdatedReason,
    pub server_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatusEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub server: Value,
}
