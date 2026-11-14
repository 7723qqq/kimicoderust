use serde::{Deserialize, Serialize};

pub const TOWER_STATE_KEY: &str = "tower";
pub const TOWER_OWNER_STATE_KEY: &str = "tower.owner";
pub const TOWER_MODE_ENTER_TYPE: &str = "tower_mode.enter";
pub const TOWER_MODE_EXIT_TYPE: &str = "tower_mode.exit";
pub const TOWER_INBOX_SENT_TYPE: &str = "tower.inbox.sent";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerModeEnter {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerModeExit {
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerInboxSent {
    pub from: String,
    pub to: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerModeState {
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub fn apply_tower_mode_enter(state: &mut TowerModeState, event: &TowerModeEnter) -> bool {
    state.active = true;
    state.owner = event.session_id.clone();
    true
}

pub fn apply_tower_mode_exit(state: &mut TowerModeState, _event: &TowerModeExit) -> bool {
    state.active = false;
    state.owner = None;
    false
}
