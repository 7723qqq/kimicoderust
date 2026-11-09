//! MCP status → host event bridge.
//!
//! v2's `AgentMcpService.handleMcpServerStatusChange`
//! (`agent/mcp/mcpService.ts:212-249`) turns every connection-manager status
//! transition into two host events: `mcp.server.status` (the roster entry) and —
//! when the server's tools enter or leave the agent's tool list —
//! `tool.list.updated`. The napi addon and the HTTP server deliver those events
//! through different sinks (a JS callback vs. a session event lane), so the
//! mapping lives here and each transport only supplies an emitter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::manager::{McpManager, McpServerEntry, McpStatusListener, McpStatusSubscription};

/// One public MCP entry as the host-facing `mcp.server.status` event (v2
/// `McpServerStatus`, agent-core-v2 `agent/mcp/mcpEvents.ts:13-23`). `error` is
/// omitted when absent so the payload carries the protocol's optional field
/// rather than a `null` the zod schema would reject.
pub fn mcp_status_event(entry: &McpServerEntry) -> Value {
    let mut server = json!({
        "name": entry.name,
        "transport": entry.transport,
        "status": entry.status,
        "toolCount": entry.tool_count,
    });
    if let Some(error) = &entry.error {
        server["error"] = json!(error);
    }
    json!({ "type": "mcp.server.status", "server": server })
}

/// One `tool.list.updated` event (v2 `ToolListUpdated`, `agent/mcp/mcpEvents.ts`).
/// Emitted when a server's tools enter or leave the agent's tool list so a host
/// can refresh its tool surface without polling the roster.
pub fn tool_list_updated_event(reason: &str, server_name: &str) -> Value {
    json!({
        "type": "tool.list.updated",
        "reason": reason,
        "serverName": server_name,
    })
}

/// v2 `handleMcpServerStatusChange` (mcpService.ts:212-249): a server that
/// becomes connected — or needs auth, which registers the auth tool — publishes
/// `mcp.connected`; disabling a server that had contributed tools publishes
/// `mcp.disconnected`. `pending` / `failed` / `removed` carry no tool-list
/// change (v2 dispatches `McpServerFailed` for failures, which this protocol
/// does not declare).
fn tool_list_reason(previous: Option<&str>, status: &str) -> Option<&'static str> {
    match status {
        "connected" | "needs-auth" => Some("mcp.connected"),
        "disabled" if matches!(previous, Some("connected") | Some("needs-auth")) => {
            Some("mcp.disconnected")
        }
        _ => None,
    }
}

/// Where a bridge delivers its events: a JS callback (napi) or a session lane
/// publish (HTTP server).
pub type McpEventSink = Arc<dyn Fn(Value) + Send + Sync>;

/// Owns one status subscription. Dropping it unsubscribes from the manager, so
/// a disposed session stops receiving transitions and stops holding the sink
/// alive.
pub struct McpStatusBridge {
    manager: Arc<McpManager>,
    subscription: McpStatusSubscription,
}

impl Drop for McpStatusBridge {
    fn drop(&mut self) {
        self.manager.unsubscribe_status(self.subscription);
    }
}

/// Subscribe `emit` to `manager`'s status transitions and replay the current
/// roster once, matching v2's attach (`attachMcpTools` walks `list()` through
/// the handler, then registers `onStatusChange`). The replay runs on a task
/// because the caller installs the bridge only after this call returns; a
/// transition landing in that gap emits one duplicate status, which is
/// idempotent.
pub fn attach(manager: &Arc<McpManager>, emit: McpEventSink) -> Arc<McpStatusBridge> {
    let seen: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let listener: McpStatusListener = Box::new({
        let emit = emit.clone();
        let seen = seen.clone();
        move |entry| emit_entry(&emit, &seen, &entry)
    });
    let subscription = manager.on_status_change(listener);
    let replay = manager.clone();
    tokio::spawn(async move {
        for entry in replay.server_entries().await {
            emit_entry(&emit, &seen, &entry);
        }
    });
    Arc::new(McpStatusBridge {
        manager: manager.clone(),
        subscription,
    })
}

/// The previous status starts empty, so the replay's first `connected` entry
/// still announces its tools.
fn emit_entry(emit: &McpEventSink, seen: &Mutex<HashMap<String, String>>, entry: &McpServerEntry) {
    emit(mcp_status_event(entry));
    let previous = {
        let mut seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        seen.insert(entry.name.clone(), entry.status.clone())
    };
    if let Some(reason) = tool_list_reason(previous.as_deref(), &entry.status) {
        emit(tool_list_updated_event(reason, &entry.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_updates_follow_v2s_status_handler() {
        // mcpService.ts:212-249: connected and needs-auth register tools,
        // disabling a server that had tools removes them, and the remaining
        // transitions leave the tool list alone.
        assert_eq!(tool_list_reason(None, "connected"), Some("mcp.connected"));
        assert_eq!(
            tool_list_reason(Some("connected"), "connected"),
            Some("mcp.connected")
        );
        assert_eq!(tool_list_reason(None, "needs-auth"), Some("mcp.connected"));
        assert_eq!(
            tool_list_reason(Some("connected"), "disabled"),
            Some("mcp.disconnected")
        );
        assert_eq!(
            tool_list_reason(Some("needs-auth"), "disabled"),
            Some("mcp.disconnected")
        );
        assert_eq!(tool_list_reason(None, "disabled"), None);
        assert_eq!(tool_list_reason(Some("connected"), "pending"), None);
        assert_eq!(tool_list_reason(Some("connected"), "failed"), None);
        assert_eq!(tool_list_reason(Some("connected"), "removed"), None);
    }

    #[test]
    fn tool_list_updated_event_matches_the_protocol_shape() {
        assert_eq!(
            tool_list_updated_event("mcp.connected", "github"),
            json!({
                "type": "tool.list.updated",
                "reason": "mcp.connected",
                "serverName": "github",
            })
        );
    }
}
