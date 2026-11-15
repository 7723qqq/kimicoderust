use std::sync::Arc;

use parking_lot::Mutex;

use crate::agent::state::errors::StateError;
use crate::agent::state::listeners::{ListenerRegistration, Listeners};

use super::agent_command::{AgentCommand, AgentCommandError, AgentCommandInfo};
use super::command_contribution::{CommandContributions, CommandRunContext};

pub struct AgentCommandService {
    contributions: Arc<CommandContributions>,
    changed: Arc<Listeners<()>>,
    _bridge: Mutex<Option<ListenerRegistration<()>>>,
}

impl AgentCommandService {
    pub fn new(contributions: Arc<CommandContributions>) -> Self {
        let changed = Arc::new(Listeners::default());
        let bridge = contributions.subscribe_changes({
            let changed = changed.clone();
            move |_| changed.fire(&())
        });
        Self {
            contributions,
            changed,
            _bridge: Mutex::new(Some(bridge)),
        }
    }

    pub fn on_did_change(&self) -> Arc<Listeners<()>> {
        self.changed.clone()
    }

    fn find(&self, name: &str) -> Option<super::command_contribution::CommandRecord> {
        let mut found = None;
        for record in self.contributions.records() {
            if record.value.name == name {
                found = Some(record);
            }
        }
        found
    }

    pub fn list_commands(&self) -> Vec<AgentCommandInfo> {
        let mut by_name: Vec<AgentCommandInfo> = Vec::new();
        for record in self.contributions.records() {
            let info = AgentCommandInfo {
                name: record.value.name.clone(),
                description: record.value.description.clone(),
                source: record.provider.clone(),
            };
            match by_name.iter_mut().find(|item| item.name == info.name) {
                Some(existing) => *existing = info,
                None => by_name.push(info),
            }
        }
        by_name
    }

    pub fn run_command(&self, name: &str, args: Option<&str>) -> Result<(), AgentCommandError> {
        let record = self
            .find(name)
            .ok_or_else(|| AgentCommandError::unknown_command(name))?;
        let context = CommandRunContext {
            args: args.unwrap_or("").to_string(),
        };
        (record.value.run)(&context)
    }

    pub fn subscribe_changes<F>(&self, listener: F) -> ListenerRegistration<()>
    where
        F: Fn(&()) -> Result<(), StateError> + Send + Sync + 'static,
    {
        self.changed.add(listener)
    }
}

impl AgentCommand for AgentCommandService {
    fn list(&self) -> Vec<AgentCommandInfo> {
        self.list_commands()
    }

    fn run(&self, name: &str, args: &str) -> Result<(), AgentCommandError> {
        self.run_command(name, Some(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as ParkingMutex;

    fn contribution(
        name: &str,
        description: Option<&str>,
    ) -> super::super::command_contribution::CommandContribution {
        super::super::command_contribution::CommandContribution {
            name: name.to_string(),
            description: description.map(str::to_string),
            run: Arc::new(|_| Ok(())),
        }
    }

    #[test]
    fn list_dedupes_by_name_with_the_last_contribution_winning() {
        let contributions = Arc::new(CommandContributions::new());
        contributions.provide("first", contribution("compact", Some("old")));
        contributions.provide("first", contribution("goal", None));
        contributions.provide("second", contribution("compact", Some("new")));

        let service = AgentCommandService::new(contributions);
        let listed = service.list_commands();
        assert_eq!(
            listed
                .iter()
                .map(|info| info.name.as_str())
                .collect::<Vec<_>>(),
            vec!["compact", "goal"]
        );
        assert_eq!(listed[0].description.as_deref(), Some("new"));
        assert_eq!(listed[0].source, "second");
        assert_eq!(listed[1].description, None);
    }

    #[test]
    fn run_dispatches_to_the_last_matching_contribution() {
        let contributions = Arc::new(CommandContributions::new());
        let seen = Arc::new(ParkingMutex::new(Vec::new()));
        let sink = seen.clone();
        contributions.provide(
            "first",
            super::super::command_contribution::CommandContribution {
                name: "goal".to_string(),
                description: None,
                run: Arc::new(move |context| {
                    sink.lock().push(context.args.clone());
                    Ok(())
                }),
            },
        );

        let service = AgentCommandService::new(contributions);
        service.run_command("goal", Some("status")).unwrap();
        assert_eq!(*seen.lock(), vec!["status".to_string()]);
        service.run_command("goal", None).unwrap();
        assert_eq!(*seen.lock(), vec!["status".to_string(), String::new()]);
    }

    #[test]
    fn unknown_command_reports_request_invalid_with_the_v2_message() {
        let service = AgentCommandService::new(Arc::new(CommandContributions::new()));
        let error = service.run_command("missing", None).unwrap_err();
        assert_eq!(error.code, "request.invalid");
        assert_eq!(error.message, "Unknown command \"missing\"");
    }

    #[test]
    fn contribution_changes_notify_service_subscribers() {
        let contributions = Arc::new(CommandContributions::new());
        let service = AgentCommandService::new(contributions.clone());
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = fired.clone();
        let _subscription = service.subscribe_changes(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        contributions.provide("first", contribution("compact", None));
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);

        contributions.remove_provider("first");
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 2);

        contributions.remove_provider("absent");
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
