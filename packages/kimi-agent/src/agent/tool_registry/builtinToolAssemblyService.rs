use super::tool_contribution::{AgentToolContribution, agent_tool_contributions};

#[derive(Clone, Default)]
pub struct BuiltinToolAssemblyService {
    contributions: Vec<AgentToolContribution>,
}

impl BuiltinToolAssemblyService {
    pub fn new() -> Self {
        Self {
            contributions: agent_tool_contributions(),
        }
    }

    pub fn contributions(&self) -> &[AgentToolContribution] {
        &self.contributions
    }
}
