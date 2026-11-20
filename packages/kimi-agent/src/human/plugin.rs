use std::sync::Arc;

use crate::human::agent::machine::AgentEmitted;
use crate::human::agent::turn::{SystemEntry, UserEntry, create_system_entry, create_user_entry};
use crate::human::llm::message::{SystemMessage, UserMessage};
use crate::human::tool::tool::ToolDefinition;

#[derive(Clone)]
pub enum PluginMessage {
    System(SystemMessage),
    User(UserMessage),
}

#[derive(Clone)]
pub enum PluginEntry {
    System(SystemEntry),
    User(UserEntry),
}

#[derive(Clone)]
pub enum AgentInputEvent {
    Notify { entry: UserEntry },
    Remind { key: String, entry: PluginEntry },
}

#[derive(Clone)]
pub struct AgentPluginTarget {
    pub kind: String,
    pub on: Arc<dyn Fn(String, Arc<dyn Fn(AgentEmitted) + Send + Sync>) + Send + Sync>,
    pub notify: Arc<dyn Fn(UserMessage) + Send + Sync>,
    pub remind: Arc<dyn Fn(String, PluginMessage) + Send + Sync>,
}

pub type PluginTarget = AgentPluginTarget;

pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
    fn tools(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
    fn connect(&self, _target: PluginTarget) {}
}

pub fn collect_plugin_tools(plugins: &[Arc<dyn Plugin>]) -> Vec<ToolDefinition> {
    plugins
        .iter()
        .flat_map(|plugin| plugin.tools())
        .collect()
}

pub trait AgentPluginSource: Send + Sync {
    fn on(&self, type_: String, handler: Arc<dyn Fn(AgentEmitted) + Send + Sync>);
    fn send(&self, event: AgentInputEvent);
}

pub fn connect_plugins(actor: Arc<dyn AgentPluginSource>, plugins: &[Arc<dyn Plugin>]) {
    let target = AgentPluginTarget {
        kind: "agent".to_string(),
        on: {
            let actor = actor.clone();
            Arc::new(move |type_, handler| actor.on(type_, handler))
        },
        notify: {
            let actor = actor.clone();
            Arc::new(move |message| {
                actor.send(AgentInputEvent::Notify {
                    entry: UserEntry {
                        message,
                        meta: Default::default(),
                    },
                });
            })
        },
        remind: {
            let actor = actor.clone();
            Arc::new(move |key, message| {
                let entry = match message {
                    PluginMessage::System(message) => {
                        PluginEntry::System(create_system_entry(message, Default::default()))
                    }
                    PluginMessage::User(message) => PluginEntry::User(create_user_entry(message, Default::default())),
                };
                actor.send(AgentInputEvent::Remind { key, entry });
            })
        },
    };
    for plugin in plugins {
        plugin.connect(target.clone());
    }
}
