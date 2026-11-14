use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};
use crate::environment::environment::EnvironmentBinding;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

pub const ENVIRONMENT_SET_BINDING_EVENT: &str = "environment.set_binding";

pub static ENVIRONMENT_SET_BINDING_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: ENVIRONMENT_SET_BINDING_EVENT.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "environmentId"],
            "properties": {
                "agentId": {"type": "string"},
                "environmentId": {"type": "string"},
                "cwd": {"type": "string"},
            },
        })),
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentSetBinding {
    pub agent_id: String,
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl EnvironmentSetBinding {
    pub const TYPE: &'static str = ENVIRONMENT_SET_BINDING_EVENT;

    pub fn class() -> Arc<Event2Class> {
        ENVIRONMENT_SET_BINDING_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &ENVIRONMENT_SET_BINDING_CLASS,
            serde_json::to_value(self).expect("environment binding event is serializable"),
            None,
        )
    }
}

impl From<EnvironmentSetBinding> for Event2 {
    fn from(event: EnvironmentSetBinding) -> Self {
        event.to_event2()
    }
}

pub static ENVIRONMENT_BINDING_REPLAYABLE_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        let mut key = ReplayableStateKey::new(
            "environmentBinding".to_string(),
            Arc::new(|| Value::Null),
            ReplayableOptions {
                schema: json!({}),
                durable: None,
                blobs: None,
                mutable_replay: None,
            },
        );
        key.on(&ENVIRONMENT_SET_BINDING_CLASS, Arc::new(|_state, event, _ctx| {
            let environment_id = event.payload.get("environmentId")?.as_str()?.to_string();
            let cwd = event
                .payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string);
            serde_json::to_value(EnvironmentBinding {
                environment_id,
                cwd,
            })
            .ok()
        }))
        .expect("environment binding fold can be registered");
        Arc::new(key)
    });

pub fn environment_binding_replayable_key() -> Arc<ReplayableStateKey> {
    ENVIRONMENT_BINDING_REPLAYABLE_KEY.clone()
}
