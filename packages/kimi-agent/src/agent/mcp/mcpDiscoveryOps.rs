use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};
use crate::mcp_core::types::MCPToolDefinition;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCollision {
    pub qualified: String,
    pub tool_name: String,
    pub collides_with: McpToolCollisionTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpToolCollisionTarget {
    SameServer {
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    OtherServer {
        #[serde(rename = "serverName")]
        server_name: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpDiscoveryState {
    pub seen: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolsDiscoveredPayload {
    pub agent_id: String,
    pub server_name: String,
    pub hash: String,
    pub tools: Vec<MCPToolDefinition>,
    pub enabled_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collisions: Option<Vec<McpToolCollision>>,
}

pub static MCP_TOOLS_DISCOVERED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "mcp.tools_discovered".to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "serverName", "hash", "tools", "enabledNames"],
            "properties": {
                "agentId": {"type": "string"},
                "serverName": {"type": "string"},
                "hash": {"type": "string"},
                "tools": {},
                "enabledNames": {"type": "array", "items": {"type": "string"}},
                "collisions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["qualified", "toolName", "collidesWith"],
                        "properties": {
                            "qualified": {"type": "string"},
                            "toolName": {"type": "string"},
                            "collidesWith": {"anyOf": [
                                {
                                    "type": "object",
                                    "required": ["kind", "toolName"],
                                    "properties": {
                                        "kind": {"const": "same_server"},
                                        "toolName": {"type": "string"},
                                    },
                                },
                                {
                                    "type": "object",
                                    "required": ["kind", "serverName"],
                                    "properties": {
                                        "kind": {"const": "other_server"},
                                        "serverName": {"type": "string"},
                                    },
                                },
                            ]},
                        },
                    },
                }
            }
        })),
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolsDiscovered {
    event: Event2,
    pub payload: McpToolsDiscoveredPayload,
}

impl McpToolsDiscovered {
    pub const TYPE: &'static str = "mcp.tools_discovered";

    pub fn new(payload: McpToolsDiscoveredPayload) -> Self {
        let event = Event2::new(
            &MCP_TOOLS_DISCOVERED_CLASS,
            serde_json::to_value(&payload).expect("MCP discovery event is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        MCP_TOOLS_DISCOVERED_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for McpToolsDiscovered {
    type Target = McpToolsDiscoveredPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<McpToolsDiscovered> for Event2 {
    fn from(event: McpToolsDiscovered) -> Self {
        event.event
    }
}

pub static MCP_DISCOVERY_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "mcp.discovery".to_string(),
        Arc::new(|| serde_json::to_value(McpDiscoveryState::default()).unwrap()),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    key.on(
        &MCP_TOOLS_DISCOVERED_CLASS,
        Arc::new(|state, event, _context| {
            let mut state = serde_json::from_value::<McpDiscoveryState>(state).ok()?;
            let server_name = event.payload.get("serverName")?.as_str()?;
            let hash = event.payload.get("hash")?.as_str()?;
            let key = format!("{server_name}\n{hash}");
            if state.seen.contains(&key) {
                return None;
            }
            state.seen.push(key);
            if state.seen.len() > MAX_DISCOVERY_SEEN_KEYS {
                state.seen = state.seen[state.seen.len() - MAX_DISCOVERY_SEEN_KEYS..].to_vec();
            }
            serde_json::to_value(state).ok()
        }),
    )
    .expect("MCP discovery fold registration is valid");
    Arc::new(key)
});

pub fn mcp_discovery_key() -> Arc<ReplayableStateKey> {
    MCP_DISCOVERY_KEY.clone()
}

const MAX_DISCOVERY_SEEN_KEYS: usize = 500;
