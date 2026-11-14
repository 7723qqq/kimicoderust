use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::app::agent_profile_catalog::agent_profile_catalog::EnvironmentDisclosureSnapshot;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

use super::errors::ProfileErrors;
use super::profile::{ProfileError, ThinkingEffort};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileModelState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    pub thinking_level: String,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_disclosure: Option<EnvironmentDisclosureSnapshot>,
    pub render_generation: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Vec<String>>,
}

impl Default for ProfileModelState {
    fn default() -> Self {
        Self {
            model_alias: None,
            profile_name: None,
            thinking_level: "off".to_string(),
            system_prompt: String::new(),
            environment_disclosure: None,
            render_generation: 0,
            agents_md_paths: None,
            disallowed_tools: None,
            subagents: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileBindPayload {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    pub thinking_effort: ThinkingEffort,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_disclosure: Option<EnvironmentDisclosureSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_generation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tool_names: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigUpdatePayload {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<ThinkingEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_disclosure: Option<EnvironmentDisclosureSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_generation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsSetActiveToolsPayload {
    pub agent_id: String,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsResetActiveToolsPayload {
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarningIssuedPayload {
    pub agent_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

macro_rules! profile_event {
    ($name:ident, $payload:ty, $class:ident, $type:literal, $durable:literal, $observable:literal, $schema:expr) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: $durable,
                observable: $observable,
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

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("profile event is serializable"),
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

profile_event!(
    ProfileBind,
    ProfileBindPayload,
    PROFILE_BIND_CLASS,
    "profile.bind",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "thinkingEffort", "systemPrompt", "disallowedTools"],
        "properties": {
            "agentId": {"type": "string"},
            "modelAlias": {"type": "string"},
            "profileName": {"type": "string"},
            "thinkingEffort": {},
            "systemPrompt": {"type": "string"},
            "environmentDisclosure": {},
            "renderGeneration": {"type": "number"},
            "agentsMdPaths": {"type": "array", "items": {"type": "string"}},
            "activeToolNames": {"type": "array", "items": {"type": "string"}},
            "disallowedTools": {"type": "array", "items": {"type": "string"}},
            "subagents": {"type": "array", "items": {"type": "string"}}
        }
    }))
);
profile_event!(
    ConfigUpdate,
    ConfigUpdatePayload,
    CONFIG_UPDATE_CLASS,
    "config.update",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId"],
        "properties": {
            "agentId": {"type": "string"},
            "modelAlias": {"type": "string"},
            "profileName": {"type": "string"},
            "thinkingEffort": {},
            "thinkingLevel": {},
            "systemPrompt": {"type": "string"},
            "environmentDisclosure": {},
            "renderGeneration": {"type": "number"},
            "agentsMdPaths": {"type": "array", "items": {"type": "string"}},
            "disallowedTools": {"type": "array", "items": {"type": "string"}}
        }
    }))
);
profile_event!(
    ToolsSetActiveTools,
    ToolsSetActiveToolsPayload,
    TOOLS_SET_ACTIVE_TOOLS_CLASS,
    "tools.set_active_tools",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "names"],
        "properties": {
            "agentId": {"type": "string"},
            "names": {"type": "array", "items": {"type": "string"}}
        }
    }))
);
profile_event!(
    ToolsResetActiveTools,
    ToolsResetActiveToolsPayload,
    TOOLS_RESET_ACTIVE_TOOLS_CLASS,
    "tools.reset_active_tools",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId"],
        "properties": {"agentId": {"type": "string"}}
    }))
);
profile_event!(WarningIssued, WarningIssuedPayload, WARNING_ISSUED_CLASS, "warning", false, true, None);

pub fn register_profile_event_classes() -> Result<(), String> {
    for class in [
        &PROFILE_BIND_CLASS,
        &CONFIG_UPDATE_CLASS,
        &TOOLS_SET_ACTIVE_TOOLS_CLASS,
        &TOOLS_RESET_ACTIVE_TOOLS_CLASS,
        &WARNING_ISSUED_CLASS,
    ] {
        register_event2_class(class)?;
    }
    Ok(())
}

fn string_array_equal(left: Option<&[String]>, right: Option<&[String]>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

pub fn config_update_thinking_level(
    event: &ConfigUpdatePayload,
) -> Result<Option<ThinkingEffort>, ProfileError> {
    match (&event.thinking_effort, &event.thinking_level) {
        (Some(effort), Some(level)) if effort != level => Err(ProfileError::new(
            ProfileErrors::THINKING_ALIAS_CONFLICT,
            format!(
                "config.update has conflicting thinkingEffort ({effort}) and legacy thinkingLevel ({level})"
            ),
            Some(json!({
                "type": "config.update",
                "thinkingEffort": effort,
                "thinkingLevel": level
            })),
        )),
        (Some(effort), Some(_)) => Ok(Some(effort.clone())),
        (Some(effort), None) => Ok(Some(effort.clone())),
        (None, level) => Ok(level.clone()),
    }
}

pub fn fold_profile_state(
    current: Value,
    event: &Event2,
) -> Result<Option<Value>, ProfileError> {
    let state = serde_json::from_value::<ProfileModelState>(current).unwrap_or_default();
    match event.type_.as_str() {
        "profile.bind" => {
            let event = serde_json::from_value::<ProfileBindPayload>(event.payload.clone())
                .map_err(|error| ProfileError::new(ProfileErrors::MODEL_CONFIG_INVALID, error.to_string(), None))?;
            let next = ProfileModelState {
                model_alias: event.model_alias.or(state.model_alias),
                profile_name: event.profile_name.or(state.profile_name),
                thinking_level: event.thinking_effort,
                system_prompt: event.system_prompt,
                environment_disclosure: event.environment_disclosure,
                render_generation: event.render_generation.unwrap_or(state.render_generation + 1),
                agents_md_paths: event.agents_md_paths.or(state.agents_md_paths),
                disallowed_tools: Some(event.disallowed_tools),
                subagents: event.subagents,
            };
            Ok(serde_json::to_value(next).ok())
        }
        "config.update" => {
            let event = serde_json::from_value::<ConfigUpdatePayload>(event.payload.clone())
                .map_err(|error| ProfileError::new(ProfileErrors::MODEL_CONFIG_INVALID, error.to_string(), None))?;
            let mut next = state.clone();
            if let Some(alias) = event.model_alias.filter(|alias| Some(alias) != state.model_alias.as_ref()) {
                next.model_alias = Some(alias);
            }
            if let Some(name) = event.profile_name.filter(|name| Some(name) != state.profile_name.as_ref()) {
                next.profile_name = Some(name);
            }
            if let Some(level) = config_update_thinking_level(&event)?
                .filter(|level| *level != state.thinking_level)
            {
                next.thinking_level = level;
            }
            if let Some(prompt) = event.system_prompt.clone().filter(|prompt| {
                *prompt != state.system_prompt
                    || event.environment_disclosure.is_some()
                    || event.render_generation.is_some()
            }) {
                next.system_prompt = prompt;
                next.environment_disclosure = event.environment_disclosure.clone();
                next.render_generation = event
                    .render_generation
                    .unwrap_or(state.render_generation + 1);
            }
            if let Some(paths) = event.agents_md_paths.clone().filter(|paths| {
                !string_array_equal(Some(paths), state.agents_md_paths.as_deref())
            }) {
                next.agents_md_paths = Some(paths);
            }
            if let Some(tools) = event.disallowed_tools.clone().filter(|tools| {
                !string_array_equal(Some(tools), state.disallowed_tools.as_deref())
            }) {
                next.disallowed_tools = Some(tools);
            }
            if next == state {
                return Ok(None);
            }
            Ok(serde_json::to_value(next).ok())
        }
        _ => Ok(None),
    }
}

pub static PROFILE_REPLAYABLE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "profile".to_string(),
        Arc::new(|| serde_json::to_value(ProfileModelState::default).unwrap_or(Value::Null)),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    for class in [&PROFILE_BIND_CLASS, &CONFIG_UPDATE_CLASS] {
        key.on(
            class,
            Arc::new(|state, event, _| {
                fold_profile_state(state, event)
                    .unwrap_or_else(|error| std::panic::panic_any(error))
            }),
        )
        .unwrap_or_else(|error| panic!("failed to register profile state fold: {error}"));
    }
    Arc::new(key)
});

pub fn profile_replayable_key() -> Arc<ReplayableStateKey> {
    PROFILE_REPLAYABLE_KEY.clone()
}

pub type ActiveToolsState = Option<Vec<String>>;

pub fn fold_profile_active_tools_state(current: Value, event: &Event2) -> Option<Value> {
    let state = serde_json::from_value::<ActiveToolsState>(current).ok().flatten();
    match event.type_.as_str() {
        "tools.set_active_tools" => {
            let event = serde_json::from_value::<ToolsSetActiveToolsPayload>(event.payload.clone()).ok()?;
            if state.as_deref() == Some(event.names.as_slice()) {
                return None;
            }
            serde_json::to_value(Some(event.names)).ok()
        }
        "tools.reset_active_tools" => {
            if state.is_none() {
                return None;
            }
            Some(Value::Null)
        }
        "profile.bind" => {
            let event = serde_json::from_value::<ProfileBindPayload>(event.payload.clone()).ok()?;
            match event.active_tool_names {
                Some(names) => serde_json::to_value(Some(names)).ok(),
                None if state.is_some() => Some(Value::Null),
                None => None,
            }
        }
        _ => None,
    }
}

pub static PROFILE_ACTIVE_TOOLS_REPLAYABLE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "profile.activeTools".to_string(),
        Arc::new(|| Value::Null),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    for class in [
        &TOOLS_SET_ACTIVE_TOOLS_CLASS,
        &TOOLS_RESET_ACTIVE_TOOLS_CLASS,
        &PROFILE_BIND_CLASS,
    ] {
        key.on(class, Arc::new(|state, event, _| fold_profile_active_tools_state(state, event)))
            .unwrap_or_else(|error| panic!("failed to register active tools state fold: {error}"));
    }
    Arc::new(key)
});

pub fn profile_active_tools_replayable_key() -> Arc<ReplayableStateKey> {
    PROFILE_ACTIVE_TOOLS_REPLAYABLE_KEY.clone()
}
