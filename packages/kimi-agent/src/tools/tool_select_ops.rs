use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::tools::registry::ToolDisclosure;

pub const TOOL_SELECT_INITIALIZED: &str = "tool_select.initialized";
pub const TOOL_SELECT_TOOLS_CHANGED: &str = "tool_select.tools_changed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolSelectOrigin {
    Initial,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolSelectSource {
    Builtin,
    User,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectTool {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
    pub source: ToolSelectSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<ToolDisclosure>,
    pub origin: ToolSelectOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectChange {
    pub added: Vec<ToolSelectTool>,
    pub removed: Vec<ToolSelectTool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectModelState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<Vec<ToolSelectTool>>,
    pub current: Vec<ToolSelectTool>,
    pub changes: Vec<ToolSelectChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSelectInitialized {
    pub agent_id: String,
    pub tools: Vec<ToolSelectTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSelectToolsChanged {
    pub agent_id: String,
    pub added: Vec<ToolSelectTool>,
    pub removed: Vec<ToolSelectTool>,
}

impl ToolSelectModelState {
    pub fn initialize(&mut self, tools: Vec<ToolSelectTool>) {
        if self.initial.is_some() {
            return;
        }
        self.initial = Some(tools.clone());
        self.current = tools;
    }

    pub fn change(&mut self, added: Vec<ToolSelectTool>, removed: Vec<ToolSelectTool>) {
        let removed_names = removed
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        self.current
            .retain(|tool| !removed_names.contains(tool.name.as_str()));
        self.current.extend(added.iter().cloned());
        self.current
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.changes.push(ToolSelectChange { added, removed });
    }

    pub fn apply_wire_event(
        &mut self,
        event_type: &str,
        payload: Value,
    ) -> Result<bool, serde_json::Error> {
        match event_type {
            TOOL_SELECT_INITIALIZED => {
                let event = serde_json::from_value::<ToolSelectInitialized>(payload)?;
                self.initialize(event.tools);
                Ok(true)
            }
            TOOL_SELECT_TOOLS_CHANGED => {
                let event = serde_json::from_value::<ToolSelectToolsChanged>(payload)?;
                self.change(event.added, event.removed);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

pub fn tool_select_tool_from_info(
    info: &crate::turn_loop::types::ToolInfo,
    source: ToolSelectSource,
    disclosure: Option<ToolDisclosure>,
) -> ToolSelectTool {
    ToolSelectTool {
        name: info.name.clone(),
        description: info.description.clone(),
        parameters: if info.input_schema.is_null() {
            serde_json::json!({"type": "object", "properties": {}})
                .as_object()
                .cloned()
                .unwrap_or_default()
        } else {
            info.input_schema.as_object().cloned().unwrap_or_default()
        },
        source,
        disclosure,
        origin: ToolSelectOrigin::Initial,
    }
}

pub fn tool_select_tool_from_definition(
    definition: &crate::tools::registry::ToolDefinition,
) -> ToolSelectTool {
    ToolSelectTool {
        name: definition.name.clone(),
        description: definition.description.clone(),
        parameters: if definition.input_schema.is_null() {
            serde_json::json!({"type": "object", "properties": {}})
                .as_object()
                .cloned()
                .unwrap_or_default()
        } else {
            definition.input_schema.as_object().cloned().unwrap_or_default()
        },
        source: match definition.source {
            crate::tools::registry::ToolSource::Builtin => ToolSelectSource::Builtin,
            crate::tools::registry::ToolSource::Mcp => ToolSelectSource::Mcp,
            crate::tools::registry::ToolSource::Host
            | crate::tools::registry::ToolSource::Plugin => ToolSelectSource::User,
        },
        disclosure: Some(definition.disclosure),
        origin: ToolSelectOrigin::Initial,
    }
}

pub fn tool_select_schema_signature(tool: &ToolSelectTool) -> String {
    serde_json::to_string(&serde_json::json!([
        tool.name.as_str(),
        tool.description.as_str(),
        crate::tools::registry::canonical_value(&Value::Object(tool.parameters.clone())),
    ]))
    .unwrap_or_default()
}

pub fn tool_description_schema_signature(tool: &kimi_core::types::ToolDescription) -> String {
    serde_json::to_string(&serde_json::json!([
        tool.name.as_str(),
        tool.description.as_str(),
        crate::tools::registry::canonical_value(&Value::Object(tool.parameters.clone())),
    ]))
    .unwrap_or_default()
}

pub fn tool_select_version_signature(tool: &ToolSelectTool) -> String {
    serde_json::to_string(&serde_json::json!([
        tool_select_schema_signature(tool),
        tool.source,
        tool.disclosure,
    ]))
    .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSelectPendingUpdate {
    pub name: String,
    pub signature: String,
}

#[derive(Debug, Clone)]
pub struct ToolSelectReconcilePlan {
    pub state: ToolSelectModelState,
    pub initialized: Option<ToolSelectInitialized>,
    pub changed: Option<ToolSelectToolsChanged>,
    pub updates: Vec<ToolSelectPendingUpdate>,
}

pub fn plan_tool_select_reconcile(
    state: &ToolSelectModelState,
    agent_id: &str,
    active: &[ToolSelectTool],
    live: &[ToolSelectTool],
    notified: &std::collections::HashSet<String>,
) -> ToolSelectReconcilePlan {
    let normalize = |tools: &[ToolSelectTool]| {
        let mut normalized = tools
            .iter()
            .map(|tool| ToolSelectTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                source: tool.source,
                disclosure: tool.disclosure,
                origin: ToolSelectOrigin::Initial,
            })
            .collect::<Vec<_>>();
        normalized.sort_by(|left, right| left.name.cmp(&right.name));
        normalized
    };
    let active = normalize(active);
    let live = normalize(live);
    let mut next = state.clone();
    let mut initialized = None;
    let mut changed = None;
    if next.initial.is_none() {
        let event = ToolSelectInitialized {
            agent_id: agent_id.to_string(),
            tools: active.clone(),
        };
        next.initialize(event.tools.clone());
        initialized = Some(event);
    } else {
        let current_names = next
            .current
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        let active_names = active
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        let removed = next
            .current
            .iter()
            .filter(|tool| !active_names.contains(tool.name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let added = active
            .iter()
            .filter(|tool| !current_names.contains(tool.name.as_str()))
            .map(|tool| ToolSelectTool {
                origin: ToolSelectOrigin::Runtime,
                ..tool.clone()
            })
            .collect::<Vec<_>>();
        if !added.is_empty() || !removed.is_empty() {
            let event = ToolSelectToolsChanged {
                agent_id: agent_id.to_string(),
                added,
                removed,
            };
            next.change(event.added.clone(), event.removed.clone());
            changed = Some(event);
        }
    }
    let live_by_name = live
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect::<std::collections::HashMap<_, _>>();
    let mut updates = Vec::new();
    for current in &next.current {
        let Some(live) = live_by_name.get(current.name.as_str()) else {
            continue;
        };
        let signature = tool_select_version_signature(live);
        if signature == tool_select_version_signature(current) || notified.contains(&signature) {
            continue;
        }
        updates.push(ToolSelectPendingUpdate {
            name: current.name.clone(),
            signature,
        });
    }
    ToolSelectReconcilePlan {
        state: next,
        initialized,
        changed,
        updates,
    }
}

pub fn persist_tool_select_payload<T: Serialize>(
    store: &crate::session::sqlite_store::SqliteSessionStore,
    session_id: &str,
    event_type: &str,
    event: &T,
) -> Result<u64, String> {
    let payload = serde_json::to_value(event).map_err(|error| error.to_string())?;
    store
        .append_wire_event(&crate::native::event_store::RawWireEvent {
            id: ulid::Ulid::new().to_string(),
            session_id: session_id.to_string(),
            event_type: event_type.to_string(),
            payload,
            is_checkpoint: false,
            is_compaction: false,
            created_at: chrono::Utc::now().timestamp_millis(),
        })
        .map_err(|error| error.to_string())
}

pub fn persist_tool_select_initialized(
    store: &crate::session::sqlite_store::SqliteSessionStore,
    session_id: &str,
    event: &ToolSelectInitialized,
) -> Result<u64, String> {
    persist_tool_select_payload(store, session_id, TOOL_SELECT_INITIALIZED, event)
}

pub fn persist_tool_select_tools_changed(
    store: &crate::session::sqlite_store::SqliteSessionStore,
    session_id: &str,
    event: &ToolSelectToolsChanged,
) -> Result<u64, String> {
    persist_tool_select_payload(store, session_id, TOOL_SELECT_TOOLS_CHANGED, event)
}

pub fn restore_tool_select_state(
    store: &crate::session::sqlite_store::SqliteSessionStore,
    session_id: &str,
    agent_id: &str,
) -> Result<ToolSelectModelState, String> {
    let records = crate::session::wire_journal::restorable_session_records(store, session_id);
    let mut state = ToolSelectModelState::default();
    for record in records {
        if record.get("agentId").and_then(Value::as_str) != Some(agent_id) {
            continue;
        }
        let Some(event_type) = record
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        state
            .apply_wire_event(&event_type, record)
            .map_err(|error| error.to_string())?;
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolDisclosure;

    fn tool(name: &str, origin: ToolSelectOrigin) -> ToolSelectTool {
        ToolSelectTool {
            name: name.to_string(),
            description: format!("{name} description"),
            parameters: serde_json::json!({"type": "object"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            source: ToolSelectSource::Mcp,
            disclosure: Some(ToolDisclosure::Deferred),
            origin,
        }
    }

    #[test]
    fn initialization_is_first_write_wins_and_preserves_event_order() {
        let mut state = ToolSelectModelState::default();
        assert!(state.initial.is_none());
        assert!(state.current.is_empty());
        assert!(state.changes.is_empty());

        state.initialize(vec![
            tool("mcp__s__beta", ToolSelectOrigin::Initial),
            tool("mcp__s__alpha", ToolSelectOrigin::Initial),
        ]);
        state.initialize(vec![tool("mcp__s__later", ToolSelectOrigin::Initial)]);

        fn names(tools: &[ToolSelectTool]) -> Vec<&str> {
            tools.iter().map(|tool| tool.name.as_str()).collect()
        }
        assert_eq!(
            names(state.initial.as_ref().unwrap()),
            vec!["mcp__s__beta", "mcp__s__alpha"]
        );
        assert_eq!(names(&state.current), vec!["mcp__s__beta", "mcp__s__alpha"]);
        assert!(state.changes.is_empty());
    }

    #[test]
    fn a_change_removes_then_appends_and_sorts_current_by_name() {
        let mut state = ToolSelectModelState::default();
        state.initialize(vec![
            tool("mcp__s__alpha", ToolSelectOrigin::Initial),
            tool("mcp__s__beta", ToolSelectOrigin::Initial),
        ]);

        state.change(
            vec![tool("mcp__s__aardvark", ToolSelectOrigin::Runtime)],
            vec![tool("mcp__s__beta", ToolSelectOrigin::Initial)],
        );

        assert_eq!(
            state
                .current
                .iter()
                .map(|tool| (tool.name.as_str(), tool.origin))
                .collect::<Vec<_>>(),
            vec![
                ("mcp__s__aardvark", ToolSelectOrigin::Runtime),
                ("mcp__s__alpha", ToolSelectOrigin::Initial),
            ]
        );
        assert_eq!(state.changes.len(), 1);
        assert_eq!(state.changes[0].added[0].name, "mcp__s__aardvark");
        assert_eq!(state.changes[0].removed[0].name, "mcp__s__beta");
    }

    #[test]
    fn durable_events_round_trip_with_v2_field_names_and_replay_in_order() {
        let initialized = ToolSelectInitialized {
            agent_id: "main".to_string(),
            tools: vec![tool("mcp__s__alpha", ToolSelectOrigin::Initial)],
        };
        let payload = serde_json::to_value(&initialized).unwrap();
        assert_eq!(payload["agentId"], "main");
        assert_eq!(payload["tools"][0]["source"], "mcp");
        assert_eq!(payload["tools"][0]["disclosure"], "deferred");
        assert_eq!(payload["tools"][0]["origin"], "initial");
        assert!(payload["agent_id"].is_null());

        let changed = ToolSelectToolsChanged {
            agent_id: "main".to_string(),
            added: vec![tool("mcp__s__beta", ToolSelectOrigin::Runtime)],
            removed: vec![tool("mcp__s__alpha", ToolSelectOrigin::Initial)],
        };
        let changed_payload = serde_json::to_value(&changed).unwrap();
        let mut restored = ToolSelectModelState::default();
        assert!(
            restored
                .apply_wire_event("tool_select.initialized", payload)
                .unwrap()
        );
        assert!(
            restored
                .apply_wire_event("tool_select.tools_changed", changed_payload)
                .unwrap()
        );
        assert!(
            !restored
                .apply_wire_event("message.user", serde_json::json!({}))
                .unwrap()
        );

        assert_eq!(restored.current.len(), 1);
        assert_eq!(restored.current[0].name, "mcp__s__beta");
        assert_eq!(restored.current[0].origin, ToolSelectOrigin::Runtime);
        assert_eq!(restored.changes.len(), 1);
        assert_eq!(restored.initial.as_ref().unwrap()[0].name, "mcp__s__alpha");
    }

    #[test]
    fn reconcile_initializes_then_reports_a_pending_update_once_per_signature() {
        let active = vec![tool("mcp__s__alpha", ToolSelectOrigin::Runtime)];
        let mut updated = tool("mcp__s__alpha", ToolSelectOrigin::Runtime);
        updated.description = "updated".to_string();
        let live = vec![updated];
        let notified = std::collections::HashSet::new();

        let plan = plan_tool_select_reconcile(
            &ToolSelectModelState::default(),
            "main",
            &active,
            &live,
            &notified,
        );
        let initialized = plan.initialized.as_ref().unwrap();
        assert_eq!(initialized.agent_id, "main");
        assert_eq!(initialized.tools[0].origin, ToolSelectOrigin::Initial);
        assert!(plan.changed.is_none());
        assert_eq!(plan.updates.len(), 1);
        assert_eq!(plan.updates[0].name, "mcp__s__alpha");

        let notified = plan
            .updates
            .iter()
            .map(|update| update.signature.clone())
            .collect::<std::collections::HashSet<_>>();
        let repeated = plan_tool_select_reconcile(&plan.state, "main", &active, &live, &notified);
        assert!(repeated.initialized.is_none());
        assert!(repeated.changed.is_none());
        assert!(repeated.updates.is_empty());
    }

    #[test]
    fn reconcile_marks_only_post_initial_additions_as_runtime() {
        let initial = vec![
            tool("mcp__s__alpha", ToolSelectOrigin::Initial),
            tool("mcp__s__beta", ToolSelectOrigin::Initial),
        ];
        let mut state = ToolSelectModelState::default();
        state.initialize(initial.clone());
        let active = vec![
            tool("mcp__s__beta", ToolSelectOrigin::Initial),
            tool("mcp__s__gamma", ToolSelectOrigin::Initial),
        ];

        let plan = plan_tool_select_reconcile(
            &state,
            "main",
            &active,
            &active,
            &std::collections::HashSet::new(),
        );
        let changed = plan.changed.as_ref().unwrap();
        assert!(plan.initialized.is_none());
        assert_eq!(changed.added.len(), 1);
        assert_eq!(changed.added[0].name, "mcp__s__gamma");
        assert_eq!(changed.added[0].origin, ToolSelectOrigin::Runtime);
        assert_eq!(changed.removed.len(), 1);
        assert_eq!(changed.removed[0].name, "mcp__s__alpha");
        assert_eq!(plan.state.changes.len(), 1);
        assert_eq!(
            plan.state
                .current
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["mcp__s__beta", "mcp__s__gamma"]
        );
        assert!(plan.updates.is_empty());
    }

    #[test]
    fn version_signatures_ignore_schema_key_order_but_keep_disclosure_presence() {
        let mut left = tool("mcp__s__alpha", ToolSelectOrigin::Initial);
        left.parameters = serde_json::json!({"a": 1, "b": {"x": 2, "y": 3}})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut reordered = left.clone();
        reordered.parameters = serde_json::json!({"b": {"y": 3, "x": 2}, "a": 1})
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            tool_select_schema_signature(&left),
            tool_select_schema_signature(&reordered)
        );

        left.disclosure = None;
        reordered.disclosure = Some(ToolDisclosure::Inline);
        assert_ne!(
            tool_select_version_signature(&left),
            tool_select_version_signature(&reordered)
        );
    }

    #[test]
    fn sqlite_restore_replays_only_the_requested_agents_durable_events() {
        let store = crate::session::sqlite_store::SqliteSessionStore::in_memory().unwrap();
        store.create_session("session-a", None).unwrap();
        store.create_session("session-b", None).unwrap();
        persist_tool_select_initialized(
            &store,
            "session-a",
            &ToolSelectInitialized {
                agent_id: "worker".to_string(),
                tools: vec![tool("mcp__s__worker", ToolSelectOrigin::Initial)],
            },
        )
        .unwrap();
        persist_tool_select_initialized(
            &store,
            "session-a",
            &ToolSelectInitialized {
                agent_id: "main".to_string(),
                tools: vec![tool("mcp__s__alpha", ToolSelectOrigin::Initial)],
            },
        )
        .unwrap();
        persist_tool_select_tools_changed(
            &store,
            "session-a",
            &ToolSelectToolsChanged {
                agent_id: "main".to_string(),
                added: vec![tool("mcp__s__beta", ToolSelectOrigin::Runtime)],
                removed: Vec::new(),
            },
        )
        .unwrap();
        persist_tool_select_tools_changed(
            &store,
            "session-b",
            &ToolSelectToolsChanged {
                agent_id: "main".to_string(),
                added: vec![tool("mcp__s__other", ToolSelectOrigin::Runtime)],
                removed: Vec::new(),
            },
        )
        .unwrap();

        let restored = restore_tool_select_state(&store, "session-a", "main").unwrap();
        assert_eq!(
            restored
                .current
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["mcp__s__alpha", "mcp__s__beta"]
        );
        assert_eq!(restored.changes.len(), 1);
        assert_eq!(restored.initial.as_ref().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn native_toolset_reconciliation_survives_reconstruction() {
        let workspace = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            crate::session::sqlite_store::SqliteSessionStore::in_memory().unwrap(),
        );
        store.create_session("session-a", None).unwrap();
        let make_toolset = || {
            crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
                .unwrap()
                .with_session_id("session-a")
                .with_session_store(store.clone())
        };
        let initial = vec![tool("mcp__s__alpha", ToolSelectOrigin::Initial)];
        let first = make_toolset();
        let persisted_seq = std::sync::Arc::new(std::sync::Mutex::new(0));
        first.set_tool_select_persist_listener(std::sync::Arc::new({
            let persisted_seq = persisted_seq.clone();
            move |_, seq| *persisted_seq.lock().unwrap() = seq
        }));
        let initialized = first
            .reconcile_tool_select(&initial, &initial)
            .await
            .unwrap();
        assert!(initialized.initialized.is_some());
        assert!(initialized.changed.is_none());

        let active = vec![
            tool("mcp__s__alpha", ToolSelectOrigin::Initial),
            tool("mcp__s__beta", ToolSelectOrigin::Initial),
        ];
        let changed = first.reconcile_tool_select(&active, &active).await.unwrap();
        assert_eq!(changed.changed.as_ref().unwrap().added.len(), 1);
        assert_eq!(
            changed.changed.as_ref().unwrap().added[0].origin,
            ToolSelectOrigin::Runtime
        );
        assert_eq!(*persisted_seq.lock().unwrap(), 2);

        let restored = make_toolset()
            .reconcile_tool_select(&active, &active)
            .await
            .unwrap();
        assert!(restored.initialized.is_none());
        assert!(restored.changed.is_none());
        assert_eq!(restored.state.initial.as_ref().unwrap().len(), 1);
        assert_eq!(restored.state.current.len(), 2);
        assert_eq!(restored.state.changes.len(), 1);
        assert_eq!(
            store
                .get_wire_events("session-a", 0, 10)
                .unwrap()
                .iter()
                .filter(|event| event.event_type.starts_with("tool_select."))
                .count(),
            2
        );
    }

    #[test]
    fn registry_snapshots_normalize_parameters_source_disclosure_and_origin() {
        let info = crate::turn_loop::types::ToolInfo {
            name: "host_tool".to_string(),
            description: "host".to_string(),
            input_schema: serde_json::Value::Null,
        };
        let host = tool_select_tool_from_info(&info, ToolSelectSource::User, None);
        assert_eq!(host.source, ToolSelectSource::User);
        assert_eq!(host.disclosure, None);
        assert_eq!(host.origin, ToolSelectOrigin::Initial);
        assert_eq!(
            host.parameters,
            serde_json::json!({"type": "object", "properties": {}})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );

        let definition = crate::tools::registry::ToolDefinition {
            name: "mcp__s__alpha".to_string(),
            description: "mcp".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            source: crate::tools::registry::ToolSource::Mcp,
            disclosure: ToolDisclosure::Deferred,
        };
        let mcp = tool_select_tool_from_definition(&definition);
        assert_eq!(mcp.source, ToolSelectSource::Mcp);
        assert_eq!(mcp.disclosure, Some(ToolDisclosure::Deferred));
        assert_eq!(mcp.origin, ToolSelectOrigin::Initial);
    }

    #[test]
    fn closed_disclosure_uses_the_pinned_initial_table_without_runtime_entries() {
        let workspace = tempfile::tempdir().unwrap();
        let toolset =
            crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None).unwrap();
        let mut pinned = tool("Read", ToolSelectOrigin::Initial);
        pinned.source = ToolSelectSource::Builtin;
        pinned.disclosure = None;
        pinned.description = "old".to_string();
        let runtime = tool("mcp__s__runtime", ToolSelectOrigin::Runtime);
        let mut state = ToolSelectModelState::default();
        state.initialize(vec![pinned.clone()]);
        state.change(vec![runtime.clone()], Vec::new());
        let current = crate::turn_loop::types::ToolInfo {
            name: "Read".to_string(),
            description: "new".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {"new": {}}}),
        };
        let runtime_info = crate::turn_loop::types::ToolInfo {
            name: runtime.name.clone(),
            description: runtime.description.clone(),
            input_schema: serde_json::Value::Object(runtime.parameters.clone()),
        };

        let shaped = toolset.shape_tool_table_with_state(vec![current, runtime_info], &state);
        assert_eq!(shaped.len(), 1);
        assert_eq!(shaped[0].name, "Read");
        assert_eq!(shaped[0].description, "old");
        assert!(shaped[0].input_schema["properties"].get("new").is_none());
    }

    #[test]
    fn open_disclosure_keeps_select_tools_even_when_nothing_is_loadable() {
        let workspace = tempfile::tempdir().unwrap();
        let toolset = crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
            .unwrap()
            .with_model_capabilities(Some(vec![
                "tool_use".to_string(),
                "dynamically_loaded_tools".to_string(),
            ]))
            .with_tool_select_enabled(true);
        let mut read = tool("Read", ToolSelectOrigin::Initial);
        read.source = ToolSelectSource::Builtin;
        read.disclosure = None;
        let mut select = tool(
            crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME,
            ToolSelectOrigin::Initial,
        );
        select.source = ToolSelectSource::Builtin;
        select.disclosure = None;
        let mut state = ToolSelectModelState::default();
        state.initialize(vec![read.clone(), select]);
        let read_info = crate::turn_loop::types::ToolInfo {
            name: read.name,
            description: read.description,
            input_schema: serde_json::Value::Object(read.parameters),
        };

        let shaped = toolset.shape_tool_table_with_state(vec![read_info], &state);
        assert_eq!(
            shaped
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Read", "select_tools"]
        );
    }

    #[tokio::test]
    async fn pending_updates_are_queued_once_with_the_v2_reminder_text() {
        let workspace = tempfile::tempdir().unwrap();
        let toolset =
            crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None).unwrap();
        let active = vec![tool("mcp__s__alpha", ToolSelectOrigin::Initial)];
        let mut updated = tool("mcp__s__alpha", ToolSelectOrigin::Initial);
        updated.description = "updated".to_string();
        let live = vec![updated];

        toolset.reconcile_tool_select(&active, &live).await.unwrap();
        assert_eq!(
            toolset.take_tool_update_pending_reminders(),
            vec![
                "Tool \"mcp__s__alpha\" has an update, but the update is not applied in this session. Continue using the currently registered version.".to_string()
            ]
        );
        assert!(toolset.take_tool_update_pending_reminders().is_empty());

        toolset.reconcile_tool_select(&active, &live).await.unwrap();
        assert!(toolset.take_tool_update_pending_reminders().is_empty());
    }

    #[tokio::test]
    async fn announcements_resume_from_the_last_structured_disclosure_revision() {
        let workspace = tempfile::tempdir().unwrap();
        let toolset = crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
            .unwrap()
            .with_tool_select_enabled(true);
        let alpha = tool("mcp__s__alpha", ToolSelectOrigin::Initial);
        toolset
            .reconcile_tool_select(std::slice::from_ref(&alpha), std::slice::from_ref(&alpha))
            .await
            .unwrap();

        let first = toolset
            .take_disclosure_announcement(&[])
            .expect("the feature flag alone enables announcements");
        assert!(first.contains("<tools_added>\nmcp__s__alpha\n</tools_added>"));
        let first_disclosure = toolset
            .take_last_announcement_disclosure()
            .expect("a snapshot carries structured disclosure");
        assert_eq!(first_disclosure["kind"], "delta");
        assert_eq!(first_disclosure["revision"], 0);
        assert_eq!(first_disclosure["added"][0]["name"], "mcp__s__alpha");

        let beta = tool("mcp__s__beta", ToolSelectOrigin::Initial);
        let active = vec![alpha, beta];
        toolset
            .reconcile_tool_select(&active, &active)
            .await
            .unwrap();
        let mut first_message = crate::injection::injection_message_named(
            crate::injection::wrap_system_reminder(&first),
            crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
        );
        first_message
            .origin
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("disclosure".to_string(), first_disclosure);
        let second = toolset
            .take_disclosure_announcement(&[first_message.clone()])
            .expect("the next durable change produces a delta");
        assert!(second.contains("<tools_added>\nmcp__s__beta\n</tools_added>"));
        assert!(!second.contains("<tools_removed>"));
        let second_disclosure = toolset
            .take_last_announcement_disclosure()
            .expect("a delta carries structured disclosure");
        assert_eq!(second_disclosure["kind"], "delta");
        assert_eq!(second_disclosure["revision"], 1);
        assert_eq!(second_disclosure["added"][0]["name"], "mcp__s__beta");
        let mut second_message = crate::injection::injection_message_named(
            crate::injection::wrap_system_reminder(&second),
            crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
        );
        second_message
            .origin
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("disclosure".to_string(), second_disclosure);
        assert!(
            toolset
                .take_disclosure_announcement(&[first_message, second_message])
                .is_none()
        );
    }

    #[test]
    fn an_added_announcement_does_not_mark_a_tool_as_loaded() {
        let workspace = tempfile::tempdir().unwrap();
        let toolset = crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
            .unwrap()
            .with_model_capabilities(Some(vec![
                "tool_use".to_string(),
                "dynamically_loaded_tools".to_string(),
            ]))
            .with_tool_select_enabled(true);
        let history = vec![crate::injection::injection_message_named(
            crate::injection::wrap_system_reminder("<tools_added>\nmcp__s__alpha\n</tools_added>"),
            crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
        )];

        toolset.seed_loaded_tools_from_history(&history);
        assert!(!toolset.is_dynamic_tool_loaded("mcp__s__alpha"));
    }

    #[tokio::test]
    async fn a_runtime_inline_tool_is_loadable_through_the_durable_registry() {
        let workspace = tempfile::tempdir().unwrap();
        let toolset = crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
            .unwrap()
            .with_model_capabilities(Some(vec![
                "tool_use".to_string(),
                "dynamically_loaded_tools".to_string(),
            ]))
            .with_tool_select_enabled(true);
        toolset.reconcile_tool_select(&[], &[]).await.unwrap();
        let runtime = tool("mcp__s__runtime", ToolSelectOrigin::Initial);
        let runtime = crate::tools::tool_select_ops::ToolSelectTool {
            disclosure: Some(ToolDisclosure::Inline),
            ..runtime
        };
        toolset
            .reconcile_tool_select(
                std::slice::from_ref(&runtime),
                std::slice::from_ref(&runtime),
            )
            .await
            .unwrap();

        let loaded = toolset
            .execute_tool(
                crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME,
                &serde_json::json!({"names": ["mcp__s__runtime"]}),
            )
            .await
            .unwrap();
        assert!(!loaded.is_error, "{}", loaded.content);
        assert!(loaded.content.contains("Loaded: mcp__s__runtime"));
    }

    #[test]
    fn selection_environment_tracks_full_tool_definitions_not_only_names() {
        let alpha = tool("mcp__s__alpha", ToolSelectOrigin::Initial);
        let beta = tool("mcp__s__beta", ToolSelectOrigin::Runtime);
        let first = crate::tools::select_tools::selection_environment_for_tools(
            None,
            &[alpha.clone(), beta.clone()],
        );
        let reordered = crate::tools::select_tools::selection_environment_for_tools(
            None,
            &[beta.clone(), alpha.clone()],
        );
        assert_eq!(first, reordered);

        let changed = crate::tools::tool_select_ops::ToolSelectTool {
            description: "changed".to_string(),
            ..alpha.clone()
        };
        assert_ne!(
            first,
            crate::tools::select_tools::selection_environment_for_tools(
                None,
                &[changed, beta.clone()]
            )
        );
        let changed_origin = crate::tools::tool_select_ops::ToolSelectTool {
            origin: ToolSelectOrigin::Runtime,
            ..alpha
        };
        assert_ne!(
            first,
            crate::tools::select_tools::selection_environment_for_tools(
                None,
                &[changed_origin, beta]
            )
        );
    }

    #[tokio::test]
    async fn retrieval_emits_announce_and_search_telemetry_with_v2_counts() {
        struct TelemetryHost {
            events: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        }
        impl crate::callbacks::HostCallbacks for TelemetryHost {
            fn llm_chat(
                &self,
                _: crate::rpc::types::LlmChatRequest,
            ) -> crate::rpc::types::BoxFuture<
                'static,
                Result<crate::rpc::types::LlmChatResponse, String>,
            > {
                Box::pin(async { Err("unused".to_string()) })
            }
            fn execute_tool(
                &self,
                _: crate::rpc::types::ToolExecuteRequest,
            ) -> crate::rpc::types::BoxFuture<
                'static,
                Result<crate::rpc::types::ToolExecuteResponse, String>,
            > {
                Box::pin(async { Err("unused".to_string()) })
            }
            fn check_permission(
                &self,
                _: crate::rpc::types::PermissionCheckRequest,
            ) -> crate::rpc::types::BoxFuture<
                'static,
                Result<crate::rpc::types::PermissionDecision, String>,
            > {
                Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
            }
            fn telemetry(&self, event: serde_json::Value) {
                self.events.lock().unwrap().push(event);
            }
        }

        let workspace = tempfile::tempdir().unwrap();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let toolset = crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(std::sync::Arc::new(TelemetryHost {
                events: events.clone(),
            }))
            .with_model_capabilities(Some(vec![
                "tool_use".to_string(),
                "dynamically_loaded_tools".to_string(),
            ]))
            .with_tool_select_enabled(true);
        let tools = (0..20)
            .map(|index| {
                let mut tool = tool(
                    &format!("mcp__s__tool_{index:02}"),
                    ToolSelectOrigin::Initial,
                );
                tool.description = format!("capability number {index}");
                tool
            })
            .collect::<Vec<_>>();
        toolset.reconcile_tool_select(&tools, &tools).await.unwrap();

        let history = vec![crate::turn_loop::types::LLMMessage::user("capability")];
        assert!(toolset.take_disclosure_announcement(&history).is_some());
        let searched = toolset
            .execute_tool(
                crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME,
                &serde_json::json!({"query": "😀"}),
            )
            .await
            .unwrap();
        assert!(!searched.is_error);

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "tool_retrieval");
        assert_eq!(events[0]["mode"], "announce");
        assert_eq!(events[0]["catalog_size"], 20);
        assert_eq!(events[0]["candidate_count"], 20);
        assert_eq!(events[0]["selected_count"], 10);
        assert_eq!(events[0]["query_chars"], 10);
        assert_eq!(events[0]["matched"], true);
        assert_eq!(events[1]["event"], "tool_retrieval");
        assert_eq!(events[1]["mode"], "search");
        assert_eq!(events[1]["catalog_size"], 20);
        assert_eq!(events[1]["candidate_count"], 20);
        assert_eq!(events[1]["selected_count"], 10);
        assert_eq!(events[1]["query_chars"], 2);
        assert_eq!(events[1]["matched"], true);
    }

    #[tokio::test]
    async fn runtime_inline_calls_are_intercepted_until_selection_lands() {
        let workspace = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
        let toolset = crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
            .unwrap()
            .with_mcp(manager.clone())
            .with_model_capabilities(Some(vec![
                "tool_use".to_string(),
                "dynamically_loaded_tools".to_string(),
            ]))
            .with_tool_select_enabled(true);
        toolset.reconcile_tool_select(&[], &[]).await.unwrap();
        manager
            .add_client(crate::mcp::McpClient::mock("runtime"))
            .await;
        let active = manager
            .active_definitions_blocking()
            .iter()
            .map(crate::tools::tool_select_ops::tool_select_tool_from_definition)
            .collect::<Vec<_>>();
        toolset
            .reconcile_tool_select(&active, &active)
            .await
            .unwrap();
        let name = "mcp__runtime__runtime_sample_tool";

        let before = toolset
            .execute_tool(name, &serde_json::json!({}))
            .await
            .unwrap();
        assert!(before.is_error);
        assert!(before.content.contains("is available but not loaded"));
        let selected = toolset
            .execute_tool(
                crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME,
                &serde_json::json!({"names": [name]}),
            )
            .await
            .unwrap();
        assert!(!selected.is_error, "{}", selected.content);
        let after = toolset
            .execute_tool(name, &serde_json::json!({}))
            .await
            .unwrap();
        assert!(!after.is_error, "{}", after.content);
    }

    #[tokio::test]
    async fn announcement_boundaries_reset_selection_failures_for_the_latest_environment() {
        let workspace = tempfile::tempdir().unwrap();
        let toolset = crate::tools::NativeToolset::new(workspace.path().to_str().unwrap(), None)
            .unwrap()
            .with_model_capabilities(Some(vec![
                "tool_use".to_string(),
                "dynamically_loaded_tools".to_string(),
            ]))
            .with_tool_select_enabled(true);
        let alpha = tool("mcp__s__alpha", ToolSelectOrigin::Initial);
        toolset
            .reconcile_tool_select(std::slice::from_ref(&alpha), std::slice::from_ref(&alpha))
            .await
            .unwrap();
        let fail = || async {
            toolset
                .execute_tool(
                    crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME,
                    &serde_json::json!({"names": ["mcp__s__unknown"]}),
                )
                .await
                .unwrap()
        };
        assert!(fail().await.content.contains("Unknown tool"));

        let beta = tool("mcp__s__beta", ToolSelectOrigin::Initial);
        let both = vec![alpha.clone(), beta];
        toolset.reconcile_tool_select(&both, &both).await.unwrap();
        assert!(toolset.take_disclosure_announcement(&[]).is_some());
        toolset
            .reconcile_tool_select(std::slice::from_ref(&alpha), std::slice::from_ref(&alpha))
            .await
            .unwrap();

        let repeated = fail().await;
        assert!(
            !repeated
                .content
                .contains("has already failed selection 2 times")
        );
        assert!(
            !repeated
                .content
                .contains("2 consecutive select_tools calls failed")
        );
    }

    #[test]
    fn every_change_appends_one_revision_even_when_names_repeat() {
        let mut state = ToolSelectModelState::default();
        state.initialize(vec![tool("mcp__s__alpha", ToolSelectOrigin::Initial)]);
        state.change(
            vec![tool("mcp__s__alpha", ToolSelectOrigin::Runtime)],
            vec![tool("mcp__s__alpha", ToolSelectOrigin::Initial)],
        );
        state.change(
            vec![tool("mcp__s__alpha", ToolSelectOrigin::Runtime)],
            vec![tool("mcp__s__alpha", ToolSelectOrigin::Runtime)],
        );

        assert_eq!(state.changes.len(), 2);
        assert_eq!(state.current.len(), 1);
        assert_eq!(state.current[0].description, "mcp__s__alpha description");
        assert_eq!(state.initial.as_ref().unwrap().len(), 1);
    }
}
