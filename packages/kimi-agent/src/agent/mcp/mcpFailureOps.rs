use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::event::event2::{Event2, Event2Class};
use crate::mcp_core::config_schema::McpTransport;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerFailedPayload {
    pub agent_id: String,
    pub server_name: String,
    pub transport: McpTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerFailureResolution {
    Recovered,
    NeedsAuth,
    Disabled,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerFailureResolvedPayload {
    pub agent_id: String,
    pub server_name: String,
    pub transport: McpTransport,
    pub resolution: McpServerFailureResolution,
}

pub static MCP_SERVER_FAILED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "mcp.server_failed".to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "serverName", "transport"],
            "properties": {
                "agentId": {"type": "string"},
                "serverName": {"type": "string"},
                "transport": {"enum": ["stdio", "http", "sse"]},
                "error": {"type": "string"},
                "nextRetryAt": {"type": "number"}
            }
        })),
    })
});

pub static MCP_SERVER_FAILURE_RESOLVED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "mcp.server_failure_resolved".to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "serverName", "transport", "resolution"],
            "properties": {
                "agentId": {"type": "string"},
                "serverName": {"type": "string"},
                "transport": {"enum": ["stdio", "http", "sse"]},
                "resolution": {"enum": ["recovered", "needs-auth", "disabled", "removed"]}
            }
        })),
    })
});

macro_rules! mcp_failure_event {
    ($name:ident, $payload:ty, $class:ident, $type:literal) => {
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
                    serde_json::to_value(&payload).expect("MCP failure event is serializable"),
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

mcp_failure_event!(
    McpServerFailed,
    McpServerFailedPayload,
    MCP_SERVER_FAILED_CLASS,
    "mcp.server_failed"
);
mcp_failure_event!(
    McpServerFailureResolved,
    McpServerFailureResolvedPayload,
    MCP_SERVER_FAILURE_RESOLVED_CLASS,
    "mcp.server_failure_resolved"
);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpFailureState {
    pub open: Vec<String>,
    pub retry_at: HashMap<String, i64>,
}

pub static MCP_FAILURE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "mcp.failures".to_string(),
        Arc::new(|| serde_json::to_value(McpFailureState::default()).unwrap()),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    key.on(
        &MCP_SERVER_FAILED_CLASS,
        Arc::new(|state, event, _context| {
            let mut state = state;
            let server_name = event.payload.get("serverName")?.as_str()?.to_string();
            let mut changed = false;
            {
                let open = state.get_mut("open")?.as_array_mut()?;
                if !open
                    .iter()
                    .any(|entry| entry.as_str() == Some(server_name.as_str()))
                {
                    open.push(json!(server_name.clone()));
                    changed = true;
                }
            }
            if let Some(next_retry_at) = event.payload.get("nextRetryAt") {
                state
                    .get_mut("retryAt")?
                    .as_object_mut()?
                    .insert(server_name, next_retry_at.clone());
                changed = true;
            }
            changed.then_some(state)
        }),
    )
    .expect("MCP failure fold registration is valid");
    key.on(
        &MCP_SERVER_FAILURE_RESOLVED_CLASS,
        Arc::new(|state, event, _context| {
            let mut state = state;
            let server_name = event.payload.get("serverName")?.as_str()?;
            state
                .get_mut("open")?
                .as_array_mut()?
                .retain(|entry| entry.as_str() != Some(server_name));
            if state
                .get("retryAt")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|retry_at| retry_at.contains_key(server_name))
            {
                state
                    .get_mut("retryAt")?
                    .as_object_mut()?
                    .remove(server_name);
            }
            Some(state)
        }),
    )
    .expect("MCP failure resolution fold registration is valid");
    Arc::new(key)
});

pub fn mcp_failure_key() -> Arc<ReplayableStateKey> {
    MCP_FAILURE_KEY.clone()
}
