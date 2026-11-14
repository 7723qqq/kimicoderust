use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey};
use crate::swarm::mode::SwarmModeTrigger;

pub const SWARM_STATE_KEY: &str = "swarm";
pub const SWARM_MODE_ENTER_TYPE: &str = "swarm_mode.enter";
pub const SWARM_MODE_EXIT_TYPE: &str = "swarm_mode.exit";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmModeEnterPayload {
    pub agent_id: String,
    pub trigger: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmModeExitPayload {
    pub agent_id: String,
}

pub static SWARM_MODE_ENTER_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: SWARM_MODE_ENTER_TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "trigger"],
            "properties": {
                "agentId": {"type": "string"},
                "trigger": {"type": "string"},
            },
        })),
    })
});

pub static SWARM_MODE_EXIT_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: SWARM_MODE_EXIT_TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId"],
            "properties": {
                "agentId": {"type": "string"},
            },
        })),
    })
});

pub fn swarm_key() -> Arc<ReplayableStateKey> {
    static KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
        Arc::new(ReplayableStateKey::new(
            SWARM_STATE_KEY.to_string(),
            Arc::new(|| Value::Null),
            ReplayableOptions {
                schema: json!({}),
                durable: Some(true),
                blobs: None,
                mutable_replay: None,
            },
        ))
    });
    KEY.clone()
}

pub fn swarm_mode_enter_event(agent_id: &str, trigger: SwarmModeTrigger) -> Event2 {
    Event2::new(
        &SWARM_MODE_ENTER_CLASS,
        json!({ "agentId": agent_id, "trigger": trigger.as_str() }),
        None,
    )
}

pub fn swarm_mode_exit_event(agent_id: &str) -> Event2 {
    Event2::new(
        &SWARM_MODE_EXIT_CLASS,
        json!({ "agentId": agent_id }),
        None,
    )
}

pub fn swarm_mode_status_event(agent_id: &str, active: bool) -> Value {
    json!({
        "type": "agent.status.updated",
        "agentId": agent_id,
        "swarmMode": active,
    })
}
