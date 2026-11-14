use serde_json::{Value, json};

use crate::agent::permission_mode::permission_mode_ops::{
    AGENT_STATUS_UPDATED_EVENT, PERMISSION_MODE_CONFIGURED_KEY_NAME, PERMISSION_MODE_KEY_NAME,
    PERMISSION_SET_MODE_EVENT, parse_mode, permission_mode_configured_key, permission_mode_key,
    permission_set_mode_event, permission_status_event, record_mode, recorded_mode,
};
use crate::permission::PermissionMode;
use crate::session::sqlite_store::SqliteSessionStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionModeFold {
    pub mode: PermissionMode,
    pub configured: bool,
}

pub fn fold_permission_mode(
    store: &SqliteSessionStore,
    session_id: &str,
    agent_id: &str,
) -> PermissionModeFold {
    let recorded = recorded_mode(store, session_id, agent_id);
    PermissionModeFold {
        configured: recorded.is_some(),
        mode: recorded.unwrap_or(PermissionMode::Manual),
    }
}

pub fn recorded_configured(store: &SqliteSessionStore, session_id: &str, agent_id: &str) -> bool {
    recorded_mode(store, session_id, agent_id).is_some()
}

pub fn record_mode_for_agents(
    store: &SqliteSessionStore,
    session_id: &str,
    agent_ids: &[String],
    mode: PermissionMode,
    now_ms: i64,
) -> Vec<String> {
    let mut written = Vec::new();
    for agent_id in agent_ids {
        match record_mode(store, session_id, agent_id, mode, now_ms) {
            Ok(true) => written.push(agent_id.clone()),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %agent_id,
                    %error,
                    "failed to persist the durable permission.set_mode record"
                );
            }
        }
    }
    written
}

pub fn permission_set_mode_records(store: &SqliteSessionStore, session_id: &str) -> Vec<Value> {
    store
        .wire_event_payloads(session_id, PERMISSION_SET_MODE_EVENT)
        .unwrap_or_default()
}

pub fn permission_set_mode_payload(agent_id: &str, mode: PermissionMode) -> Value {
    permission_set_mode_event(agent_id, mode)
}

pub fn permission_status_payload(agent_id: &str, mode: PermissionMode) -> Value {
    permission_status_event(agent_id, mode)
}

pub fn replayable_keys() -> (
    std::sync::Arc<crate::state::state::ReplayableStateKey>,
    std::sync::Arc<crate::state::state::ReplayableStateKey>,
) {
    (permission_mode_key(), permission_mode_configured_key())
}

pub fn parse_recorded_mode(mode: &str) -> Option<PermissionMode> {
    parse_mode(mode)
}

pub fn event_names() -> (&'static str, &'static str) {
    (PERMISSION_SET_MODE_EVENT, AGENT_STATUS_UPDATED_EVENT)
}

pub fn key_names() -> (&'static str, &'static str) {
    (
        PERMISSION_MODE_KEY_NAME,
        PERMISSION_MODE_CONFIGURED_KEY_NAME,
    )
}

pub fn configured_key_default() -> Value {
    json!(false)
}
