use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};

use crate::_base::errors::serialize::KimiErrorPayload;
use crate::app::event::event2::{Event2, Event2Class};
use crate::mcp_core::config_schema::McpTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerStatusValue {
    Pending,
    Connected,
    Failed,
    Disabled,
    NeedsAuth,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusPayload {
    pub name: String,
    pub transport: McpTransport,
    pub status: McpServerStatusValue,
    pub tool_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusEventPayload {
    pub agent_id: String,
    pub server: McpServerStatusPayload,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolListUpdatedPayload {
    pub agent_id: String,
    pub reason: ToolListUpdatedReason,
    pub server_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentErrorEventPayload {
    #[serde(flatten)]
    pub error: KimiErrorPayload,
    pub agent_id: String,
}

macro_rules! mcp_event {
    ($name:ident, $payload:ty, $class:ident, $type:literal) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: false,
                observable: true,
                agent_domain: true,
                schema: None,
            })
        });

        #[derive(Debug, Clone)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("MCP event is serializable"),
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

mcp_event!(
    McpServerStatus,
    McpServerStatusEventPayload,
    MCP_SERVER_STATUS_CLASS,
    "mcp.server.status"
);
mcp_event!(
    ToolListUpdated,
    ToolListUpdatedPayload,
    TOOL_LIST_UPDATED_CLASS,
    "tool.list.updated"
);
mcp_event!(
    AgentErrorEvent,
    AgentErrorEventPayload,
    AGENT_ERROR_EVENT_CLASS,
    "error"
);
