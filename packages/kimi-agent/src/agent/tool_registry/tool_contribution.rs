use parking_lot::Mutex;
use std::sync::{Arc, LazyLock};

use crate::tools::registry::{ToolDefinition, ToolDisclosure, ToolSource};

pub type ToolWhen = Arc<dyn Fn() -> bool + Send + Sync>;
pub type ToolCtor = Arc<dyn Fn() -> ToolDefinition + Send + Sync>;

#[derive(Clone)]
pub struct AgentToolContributionOptions {
    pub name: String,
    pub source: Option<ToolSource>,
    pub disclosure: Option<ToolDisclosure>,
    pub when: Option<ToolWhen>,
    pub required_environment_capabilities: Option<Vec<String>>,
    pub domain: Option<String>,
}

impl AgentToolContributionOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: None,
            disclosure: None,
            when: None,
            required_environment_capabilities: None,
            domain: None,
        }
    }

    pub fn source(mut self, source: ToolSource) -> Self {
        self.source = Some(source);
        self
    }

    pub fn disclosure(mut self, disclosure: ToolDisclosure) -> Self {
        self.disclosure = Some(disclosure);
        self
    }

    pub fn when(mut self, when: ToolWhen) -> Self {
        self.when = Some(when);
        self
    }

    pub fn required_environment_capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.required_environment_capabilities = Some(capabilities);
        self
    }

    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    pub fn resolved_source(&self) -> ToolSource {
        self.source.unwrap_or(ToolSource::Builtin)
    }
}

#[derive(Clone)]
pub struct AgentToolContribution {
    pub id: String,
    pub ctor: ToolCtor,
    pub options: AgentToolContributionOptions,
}

impl AgentToolContribution {
    pub fn new(
        id: impl Into<String>,
        ctor: ToolCtor,
        options: AgentToolContributionOptions,
    ) -> Self {
        Self {
            id: id.into(),
            ctor,
            options,
        }
    }
}

static CONTRIBUTIONS: LazyLock<Mutex<Vec<AgentToolContribution>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

#[cfg(test)]
pub(crate) static CONTRIBUTION_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub fn register_agent_tool_contribution(contribution: AgentToolContribution) {
    CONTRIBUTIONS.lock().push(contribution);
}

pub fn override_agent_tool_contribution(contribution: AgentToolContribution) {
    let mut contributions = CONTRIBUTIONS.lock();
    match contributions
        .iter()
        .position(|existing| existing.id == contribution.id)
    {
        Some(index) => contributions[index] = contribution,
        None => contributions.push(contribution),
    }
}

pub fn agent_tool_contributions() -> Vec<AgentToolContribution> {
    CONTRIBUTIONS.lock().clone()
}

pub fn clear_agent_tool_contributions_for_tests() {
    CONTRIBUTIONS.lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contribution(id: &str, name: &str) -> AgentToolContribution {
        let tool_name = name.to_string();
        AgentToolContribution::new(
            id,
            Arc::new(move || ToolDefinition {
                name: tool_name.clone(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
                source: ToolSource::Builtin,
                disclosure: ToolDisclosure::Inline,
            }),
            AgentToolContributionOptions::new(name),
        )
    }

    #[test]
    fn register_appends_and_override_replaces_in_place() {
        let _guard = CONTRIBUTION_TEST_LOCK.lock();
        clear_agent_tool_contributions_for_tests();
        register_agent_tool_contribution(contribution("a", "A"));
        register_agent_tool_contribution(contribution("b", "B"));
        override_agent_tool_contribution(contribution("a", "A2"));
        override_agent_tool_contribution(contribution("c", "C"));

        let ids: Vec<String> = agent_tool_contributions()
            .into_iter()
            .map(|record| record.id)
            .collect();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(
            agent_tool_contributions()[0].options.name,
            "A2",
            "an override keeps the original position"
        );
        clear_agent_tool_contributions_for_tests();
    }

    #[test]
    fn source_defaults_to_builtin() {
        let options = AgentToolContributionOptions::new("X");
        assert_eq!(options.resolved_source(), ToolSource::Builtin);
        let options = options.source(ToolSource::Mcp);
        assert_eq!(options.resolved_source(), ToolSource::Mcp);
    }
}
