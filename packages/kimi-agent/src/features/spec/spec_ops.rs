use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::storage::state_store::{StateStore, StateWriteOutcome};

pub const SPEC_STATE_KEY: &str = "spec";
pub const SPEC_MODE_ENTER_TYPE: &str = "spec_mode.enter";
pub const SPEC_MODE_CANCEL_TYPE: &str = "spec_mode.cancel";
pub const SPEC_MODE_EXIT_TYPE: &str = "spec_mode.exit";
pub const SPEC_REVISION_TYPE: &str = "spec.revision";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecTransition {
    Cancel,
    Exit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecState {
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_count: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition: Option<SpecTransition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecModeEnter {
    pub agent_id: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecModeTransition {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecRevision {
    pub agent_id: String,
    pub id: String,
    pub version: u64,
    pub key: String,
    pub sha256: String,
    pub bytes: usize,
}

pub fn apply_enter(state: &mut SpecState, event: &SpecModeEnter) {
    if !(state.active && state.id.as_deref() == Some(&event.id)) {
        state.active = true;
        state.id = Some(event.id.clone());
    }
}

pub fn apply_transition(state: &mut SpecState, transition: SpecTransition) {
    if state.active {
        state.active = false;
        state.id = None;
        state.last_transition = Some(transition);
    }
}

pub fn apply_revision(state: &mut SpecState, event: &SpecRevision) {
    state
        .revision_count
        .get_or_insert_with(BTreeMap::new)
        .insert(event.id.clone(), event.version);
}

impl StateStore {
    pub(crate) fn apply_spec_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let active = value
            .get("active")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                "State write error: [-32003] invalid spec state value: expected { active: boolean }"
                    .to_string()
            })?;
        if !active {
            let mut stored = json!({ "active": false });
            if let Some(transition) = value.get("lastTransition").and_then(Value::as_str) {
                stored["lastTransition"] = json!(transition);
            }
            return Ok(StateWriteOutcome {
                stored: stored.clone(),
                response: stored,
            });
        }
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                "State write error: [-32003] invalid spec state value: entering spec mode needs a non-empty id"
                    .to_string()
            })?;
        let stored = json!({ "active": true, "id": id });
        Ok(StateWriteOutcome {
            stored: stored.clone(),
            response: stored,
        })
    }
}
