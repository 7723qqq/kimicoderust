use super::tool_contribution::{AgentToolContribution, agent_tool_contributions};

pub struct BuiltinToolAssembly {
    contributions: Vec<AgentToolContribution>,
}

impl BuiltinToolAssembly {
    pub fn create() -> Self {
        Self {
            contributions: agent_tool_contributions(),
        }
    }

    pub fn from_records(contributions: Vec<AgentToolContribution>) -> Self {
        Self { contributions }
    }

    pub fn contributions(&self) -> &[AgentToolContribution] {
        &self.contributions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool_registry::tool_contribution::{
        AgentToolContributionOptions, CONTRIBUTION_TEST_LOCK,
        clear_agent_tool_contributions_for_tests, register_agent_tool_contribution,
    };
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn assembly_copies_every_registered_contribution() {
        let _guard = CONTRIBUTION_TEST_LOCK.lock();
        clear_agent_tool_contributions_for_tests();
        for name in ["A", "B"] {
            let tool_name = name.to_string();
            register_agent_tool_contribution(AgentToolContribution::new(
                name,
                Arc::new(move || crate::tools::registry::ToolDefinition {
                    name: tool_name.clone(),
                    description: String::new(),
                    input_schema: json!({ "type": "object" }),
                    source: crate::tools::registry::ToolSource::Builtin,
                    disclosure: crate::tools::registry::ToolDisclosure::Inline,
                }),
                AgentToolContributionOptions::new(name),
            ));
        }

        let assembly = BuiltinToolAssembly::create();
        let names: Vec<&str> = assembly
            .contributions()
            .iter()
            .map(|record| record.options.name.as_str())
            .collect();
        assert_eq!(names, vec!["A", "B"]);
        clear_agent_tool_contributions_for_tests();
    }
}
