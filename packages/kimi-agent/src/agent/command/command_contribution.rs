use std::sync::Arc;

use crate::agent::state::listeners::{ListenerRegistration, Listeners};

use super::agent_command::AgentCommandError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandRunContext {
    pub args: String,
}

pub type CommandRun =
    Arc<dyn Fn(&CommandRunContext) -> Result<(), AgentCommandError> + Send + Sync>;

#[derive(Clone)]
pub struct CommandContribution {
    pub name: String,
    pub description: Option<String>,
    pub run: CommandRun,
}

impl CommandContribution {
    pub fn new(name: impl Into<String>, run: CommandRun) -> Self {
        Self {
            name: name.into(),
            description: None,
            run,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

#[derive(Clone)]
pub struct CommandRecord {
    pub provider: String,
    pub value: CommandContribution,
}

#[derive(Default)]
pub struct CommandContributions {
    records: parking_lot::Mutex<Vec<CommandRecord>>,
    changed: Arc<Listeners<()>>,
}

impl CommandContributions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn provide(&self, provider: impl Into<String>, contribution: CommandContribution) {
        self.records.lock().push(CommandRecord {
            provider: provider.into(),
            value: contribution,
        });
        let _ = self.changed.fire(&());
    }

    pub fn remove_provider(&self, provider: &str) {
        let mut records = self.records.lock();
        let before = records.len();
        records.retain(|record| record.provider != provider);
        let removed = records.len() != before;
        drop(records);
        if removed {
            let _ = self.changed.fire(&());
        }
    }

    pub fn records(&self) -> Vec<CommandRecord> {
        self.records.lock().clone()
    }

    pub fn on_did_change(&self) -> Arc<Listeners<()>> {
        self.changed.clone()
    }

    pub fn subscribe_changes<F>(&self, listener: F) -> ListenerRegistration<()>
    where
        F: Fn(&()) -> Result<(), crate::agent::state::errors::StateError>
            + Send
            + Sync
            + 'static,
    {
        self.changed.add(listener)
    }
}
