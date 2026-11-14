use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};

use async_trait::async_trait;

use crate::app::event::event2::{Event2, Event2Class};
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait IAgentPluginCommandService: Send + Sync {
    async fn activate(&self, payload: ActivatePluginCommandPayload) -> Result<(), WireAsyncError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivatePluginCommandPayload {
    pub plugin_id: String,
    pub command_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "user-slash")]
pub enum PluginCommandTrigger {
    UserSlash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCommandActivatedPayload {
    pub agent_id: String,
    pub activation_id: String,
    pub plugin_id: String,
    pub command_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_args: Option<String>,
    pub trigger: PluginCommandTrigger,
}

pub static PLUGIN_COMMAND_ACTIVATED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "plugin_command.activated".to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCommandActivated {
    pub agent_id: String,
    pub activation_id: String,
    pub plugin_id: String,
    pub command_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_args: Option<String>,
    pub trigger: PluginCommandTrigger,
}

impl PluginCommandActivated {
    pub const TYPE: &'static str = "plugin_command.activated";

    pub fn class() -> Arc<Event2Class> {
        PLUGIN_COMMAND_ACTIVATED_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &PLUGIN_COMMAND_ACTIVATED_CLASS,
            serde_json::to_value(self).expect("plugin command event is serializable"),
            None,
        )
    }
}

impl From<PluginCommandActivated> for Event2 {
    fn from(event: PluginCommandActivated) -> Self {
        event.to_event2()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCommandActivatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub activation_id: String,
    pub plugin_id: String,
    pub command_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_args: Option<String>,
    pub trigger: PluginCommandTrigger,
}

impl PluginCommandActivatedEvent {
    pub fn new(
        activation_id: String,
        plugin_id: String,
        command_name: String,
        command_args: Option<String>,
    ) -> Self {
        Self {
            type_: PluginCommandActivated::TYPE.to_string(),
            activation_id,
            plugin_id,
            command_name,
            command_args,
            trigger: PluginCommandTrigger::UserSlash,
        }
    }
}
