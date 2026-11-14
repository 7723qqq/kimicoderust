use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::agent::tool_registry::tool_contribution::{
    AgentToolContribution, AgentToolContributionOptions, agent_tool_contributions,
};
use crate::tools::registry::{ToolDefinition, ToolRegistry, ToolSource, version_signature};
use crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME;
use crate::tools::tool_policy::{
    ToolActivationPolicy, ToolSource as PolicyToolSource, is_tool_active,
};

use super::tool_activation::{
    AgentToolActivation, IAgentToolActivationService, ToolActivationProfileData,
    ToolActivationSource,
};
use super::tool_availability::{AgentEnvironment, is_tool_available};

struct TrackedRegistration {
    name: String,
    signature: String,
}

pub struct AgentToolActivationService {
    source: Arc<dyn ToolActivationSource + Send + Sync>,
    environment: Arc<dyn AgentEnvironment + Send + Sync>,
    registry: Mutex<ToolRegistry>,
    registrations: Mutex<BTreeMap<String, TrackedRegistration>>,
}

impl AgentToolActivationService {
    pub fn new(
        source: Arc<dyn ToolActivationSource + Send + Sync>,
        environment: Arc<dyn AgentEnvironment + Send + Sync>,
    ) -> Self {
        Self {
            source,
            environment,
            registry: Mutex::new(ToolRegistry::new()),
            registrations: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn registry(&self) -> std::sync::MutexGuard<'_, ToolRegistry> {
        self.registry.lock().expect("tool registry lock")
    }

    pub fn activate_records(&self, records: &[AgentToolContribution]) {
        if records.is_empty() {
            return;
        }
        let data = self.source.profile();
        let policy = ToolActivationPolicy {
            tools: data.active_tool_names.clone(),
            disallowed_tools: data.disallowed_tools.clone(),
        };
        let disclosure_policy = ToolActivationPolicy {
            tools: None,
            disallowed_tools: data.disallowed_tools.clone(),
        };
        let workspace_veto = ToolActivationPolicy {
            tools: None,
            disallowed_tools: self.source.workspace_disabled_tools(),
        };
        for record in records {
            let options: &AgentToolContributionOptions = &record.options;
            let source = options.resolved_source();
            let policy_source = policy_source(source);
            if self
                .registry
                .lock()
                .expect("tool registry lock")
                .resolve(&options.name)
                .is_some()
            {
                continue;
            }
            if !is_tool_active(&workspace_veto, &options.name, policy_source) {
                continue;
            }
            let active_by_profile = if options.name == SELECT_TOOLS_TOOL_NAME {
                is_tool_active(&disclosure_policy, &options.name, policy_source)
            } else {
                is_tool_active(&policy, &options.name, policy_source)
            };
            if !active_by_profile {
                continue;
            }
            if !is_tool_available(options, self.environment.as_ref()) {
                continue;
            }
            let mut tool: ToolDefinition = (record.ctor)();
            tool.source = source;
            tool.disclosure = options.disclosure.unwrap_or_default();
            let signature = version_signature(&tool);
            let name = tool.name.clone();
            self.registry
                .lock()
                .expect("tool registry lock")
                .register(tool);
            self.registrations
                .lock()
                .expect("activation registrations lock")
                .insert(record.id.clone(), TrackedRegistration { name, signature });
        }
    }

    pub fn deactivate_record(&self, record: &AgentToolContribution) {
        let tracked = self
            .registrations
            .lock()
            .expect("activation registrations lock")
            .remove(&record.id);
        let Some(tracked) = tracked else {
            return;
        };
        let mut registry = self.registry.lock().expect("tool registry lock");
        let still_registered = registry
            .resolve(&tracked.name)
            .is_some_and(|tool| version_signature(tool) == tracked.signature);
        if still_registered {
            registry.unregister(&tracked.name);
        }
    }

    pub fn on_contributions_changed(
        &self,
        added: &[AgentToolContribution],
        removed: &[AgentToolContribution],
    ) {
        self.activate_records(added);
        for record in removed {
            self.deactivate_record(record);
        }
    }
}

impl AgentToolActivation for AgentToolActivationService {
    fn activate(&self) {
        self.activate_records(&agent_tool_contributions());
    }
}

#[async_trait]
impl IAgentToolActivationService for AgentToolActivationService {
    async fn activate(&self) {
        AgentToolActivation::activate(self);
    }
}

fn policy_source(source: ToolSource) -> PolicyToolSource {
    match source {
        ToolSource::Mcp => PolicyToolSource::Mcp,
        _ => PolicyToolSource::Builtin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolDisclosure;
    use serde_json::json;

    struct FixedSource {
        profile: ToolActivationProfileData,
        workspace_disabled: Option<Vec<String>>,
    }

    impl ToolActivationSource for FixedSource {
        fn profile(&self) -> ToolActivationProfileData {
            self.profile.clone()
        }

        fn workspace_disabled_tools(&self) -> Option<Vec<String>> {
            self.workspace_disabled.clone()
        }
    }

    struct FixedEnvironment {
        available: bool,
    }

    impl AgentEnvironment for FixedEnvironment {
        fn is_available(&self, _capabilities: &[String]) -> bool {
            self.available
        }

        fn connect_inspectable(&self) -> bool {
            false
        }
    }

    fn contribution(options: AgentToolContributionOptions) -> AgentToolContribution {
        let name = options.name.clone();
        AgentToolContribution::new(
            name.clone(),
            Arc::new(move || ToolDefinition {
                name: name.clone(),
                description: format!("{name} tool"),
                input_schema: json!({ "type": "object" }),
                source: ToolSource::Builtin,
                disclosure: ToolDisclosure::Inline,
            }),
            options,
        )
    }

    fn make_service(
        profile: ToolActivationProfileData,
        workspace_disabled: Option<Vec<String>>,
        available: bool,
    ) -> AgentToolActivationService {
        AgentToolActivationService::new(
            Arc::new(FixedSource {
                profile,
                workspace_disabled,
            }),
            Arc::new(FixedEnvironment { available }),
        )
    }

    fn active_names(service: &AgentToolActivationService) -> Vec<String> {
        service
            .registry()
            .list()
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }

    #[test]
    fn profile_enables_and_workspace_veto_wins() {
        let service = make_service(
            ToolActivationProfileData {
                active_tool_names: Some(vec!["Read".to_string(), "Bash".to_string()]),
                disallowed_tools: Some(vec!["Bash".to_string()]),
            },
            Some(vec!["Read".to_string()]),
            true,
        );
        service.activate_records(&[
            contribution(AgentToolContributionOptions::new("Read")),
            contribution(AgentToolContributionOptions::new("Bash")),
            contribution(AgentToolContributionOptions::new("Grep")),
        ]);
        assert!(active_names(&service).is_empty());
    }

    #[test]
    fn select_tools_ignores_the_profile_allowlist_but_not_the_denylist() {
        let service = make_service(
            ToolActivationProfileData {
                active_tool_names: Some(vec!["Read".to_string()]),
                disallowed_tools: None,
            },
            None,
            true,
        );
        service.activate_records(&[contribution(AgentToolContributionOptions::new(
            SELECT_TOOLS_TOOL_NAME,
        ))]);
        assert_eq!(
            active_names(&service),
            vec![SELECT_TOOLS_TOOL_NAME.to_string()]
        );

        let denied = service_rejecting_select_tools();
        assert!(active_names(&denied).is_empty());
    }

    fn service_rejecting_select_tools() -> AgentToolActivationService {
        make_service(
            ToolActivationProfileData {
                active_tool_names: Some(vec!["Read".to_string()]),
                disallowed_tools: Some(vec![SELECT_TOOLS_TOOL_NAME.to_string()]),
            },
            None,
            true,
        )
    }

    #[test]
    fn reset_contributions_reject_the_mcp_source_against_a_builtin_allowlist() {
        let service = make_service(
            ToolActivationProfileData {
                active_tool_names: Some(vec!["mcp__srv__*".to_string()]),
                disallowed_tools: None,
            },
            None,
            true,
        );
        let mut mcp = AgentToolContributionOptions::new("mcp__srv__tool").source(ToolSource::Mcp);
        mcp.source = Some(ToolSource::Mcp);
        service.activate_records(&[contribution(mcp)]);
        assert_eq!(active_names(&service), vec!["mcp__srv__tool".to_string()]);

        let builtin_named = make_service(
            ToolActivationProfileData {
                active_tool_names: Some(vec!["mcp__srv__*".to_string()]),
                disallowed_tools: None,
            },
            None,
            true,
        );
        builtin_named.activate_records(&[contribution(
            AgentToolContributionOptions::new("mcp__srv__tool").source(ToolSource::Builtin),
        )]);
        assert!(
            active_names(&builtin_named).is_empty(),
            "a non-mcp source never matches an mcp glob"
        );
    }

    #[test]
    fn already_registered_and_unavailable_tools_are_skipped() {
        let service = make_service(ToolActivationProfileData::default(), None, true);
        let record = contribution(AgentToolContributionOptions::new("Read"));
        service.activate_records(std::slice::from_ref(&record));
        service.activate_records(std::slice::from_ref(&record));
        assert_eq!(active_names(&service), vec!["Read".to_string()]);

        let unavailable = make_service(ToolActivationProfileData::default(), None, false);
        unavailable.activate_records(&[contribution(
            AgentToolContributionOptions::new("FetchURL")
                .required_environment_capabilities(vec!["browser".to_string()]),
        )]);
        assert!(active_names(&unavailable).is_empty());
    }

    #[test]
    fn deactivation_removes_only_the_matching_registration() {
        let service = make_service(ToolActivationProfileData::default(), None, true);
        let record = contribution(AgentToolContributionOptions::new("Read"));
        service.activate_records(std::slice::from_ref(&record));
        service.deactivate_record(&record);
        assert!(active_names(&service).is_empty());
        service.deactivate_record(&record);
        assert!(active_names(&service).is_empty());
    }
}
