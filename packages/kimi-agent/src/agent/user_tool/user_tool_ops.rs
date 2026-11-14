use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey};
use crate::tool::tool_contract::ToolDisclosure;

use super::user_tool::UserToolRegistration;

pub type UserToolModelState = IndexMap<String, UserToolRegistration>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsRegisterUserToolPayload {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<ToolDisclosure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsUnregisterUserToolPayload {
    pub agent_id: String,
    pub name: String,
}

macro_rules! user_tool_event {
    ($name:ident, $payload:ident, $class:ident, $type:literal, $schema:expr) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: true,
                observable: false,
                agent_domain: true,
                schema: $schema,
            })
        });

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;
            pub const DURABLE: bool = true;
            pub const OBSERVABLE: bool = false;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("user tool event is serializable"),
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

user_tool_event!(
    ToolsRegisterUserTool,
    ToolsRegisterUserToolPayload,
    TOOLS_REGISTER_USER_TOOL_CLASS,
    "tools.register_user_tool",
    Some(json!({
        "type": "object",
        "required": ["agentId", "name", "description", "parameters"],
        "properties": {
            "agentId": {"type": "string"},
            "name": {"type": "string"},
            "description": {"type": "string"},
            "parameters": {},
            "disclosure": {}
        }
    }))
);
user_tool_event!(
    ToolsUnregisterUserTool,
    ToolsUnregisterUserToolPayload,
    TOOLS_UNREGISTER_USER_TOOL_CLASS,
    "tools.unregister_user_tool",
    Some(json!({
        "type": "object",
        "required": ["agentId", "name"],
        "properties": {
            "agentId": {"type": "string"},
            "name": {"type": "string"}
        }
    }))
);

fn equal_registration(a: &UserToolRegistration, b: &UserToolRegistration) -> bool {
    a.name == b.name
        && a.description == b.description
        && a.parameters == b.parameters
        && a.disclosure == b.disclosure
}

pub static USER_TOOL_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "userTool".to_string(),
        Arc::new(|| {
            serde_json::to_value(UserToolModelState::default())
                .expect("user tool state is serializable")
        }),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    key.on(
        &TOOLS_REGISTER_USER_TOOL_CLASS,
        Arc::new(|state, event, _context| {
            let mut state = serde_json::from_value::<UserToolModelState>(state).ok()?;
            let payload =
                serde_json::from_value::<ToolsRegisterUserToolPayload>(event.payload.clone())
                    .ok()?;
            let registration = UserToolRegistration {
                name: payload.name,
                description: payload.description,
                parameters: payload.parameters,
                disclosure: payload.disclosure,
            };
            if state
                .get(&registration.name)
                .is_some_and(|existing| equal_registration(existing, &registration))
            {
                return None;
            }
            state.insert(registration.name.clone(), registration);
            serde_json::to_value(state).ok()
        }),
    )
    .expect("user tool registration fold can be registered");
    key.on(
        &TOOLS_UNREGISTER_USER_TOOL_CLASS,
        Arc::new(|state, event, _context| {
            let mut state = serde_json::from_value::<UserToolModelState>(state).ok()?;
            let payload =
                serde_json::from_value::<ToolsUnregisterUserToolPayload>(event.payload.clone())
                    .ok()?;
            if state.shift_remove(&payload.name).is_none() {
                return None;
            }
            serde_json::to_value(state).ok()
        }),
    )
    .expect("user tool unregistration fold can be registered");
    key
});

pub fn user_tool_key() -> Arc<ReplayableStateKey> {
    USER_TOOL_KEY.clone()
}
