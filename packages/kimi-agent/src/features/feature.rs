#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureContribution {
    SessionModel(String),
    AgentModel(String),
    Config { domain: String },
    Service {
        scope: String,
        id: String,
        activation: Option<String>,
    },
    Tool {
        id: String,
        name: String,
        domain: String,
    },
    Command(String),
    Profiles {
        source_id: String,
        priority: i32,
        profiles: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContributionHandle {
    pub index: usize,
}

pub struct Feature {
    name: String,
    contributions: Vec<FeatureContribution>,
    dispose: Vec<Box<dyn FnOnce() + Send>>,
}

impl Feature {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            contributions: Vec::new(),
            dispose: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn contributions(&self) -> &[FeatureContribution] {
        &self.contributions
    }

    pub fn contribute(&mut self, contribution: FeatureContribution) -> ContributionHandle {
        let index = self.contributions.len();
        self.contributions.push(contribution);
        ContributionHandle { index }
    }

    pub fn contribute_session_model(&mut self, definition: impl Into<String>) -> ContributionHandle {
        self.contribute(FeatureContribution::SessionModel(definition.into()))
    }

    pub fn contribute_agent_model(&mut self, definition: impl Into<String>) -> ContributionHandle {
        self.contribute(FeatureContribution::AgentModel(definition.into()))
    }

    pub fn contribute_config(&mut self, domain: impl Into<String>) -> ContributionHandle {
        self.contribute(FeatureContribution::Config {
            domain: domain.into(),
        })
    }

    pub fn contribute_service(
        &mut self,
        scope: impl Into<String>,
        id: impl Into<String>,
        activation: Option<String>,
    ) -> ContributionHandle {
        self.contribute(FeatureContribution::Service {
            scope: scope.into(),
            id: id.into(),
            activation,
        })
    }

    pub fn contribute_agent_service(
        &mut self,
        id: impl Into<String>,
        activation: Option<String>,
    ) -> ContributionHandle {
        self.contribute_service("agent", id, activation)
    }

    pub fn contribute_tool(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        domain: impl Into<String>,
    ) -> ContributionHandle {
        self.contribute(FeatureContribution::Tool {
            id: id.into(),
            name: name.into(),
            domain: domain.into(),
        })
    }

    pub fn contribute_command(&mut self, contribution: impl Into<String>) -> ContributionHandle {
        self.contribute(FeatureContribution::Command(contribution.into()))
    }

    pub fn contribute_profiles(
        &mut self,
        profiles: Vec<String>,
        source_id: Option<String>,
        priority: Option<i32>,
    ) -> ContributionHandle {
        self.contribute(FeatureContribution::Profiles {
            source_id: source_id.unwrap_or_else(|| format!("feature:{}", self.name)),
            priority: priority.unwrap_or(0),
            profiles,
        })
    }

    pub fn on_dispose(&mut self, dispose: impl FnOnce() + Send + 'static) {
        self.dispose.push(Box::new(dispose));
    }
}

impl Drop for Feature {
    fn drop(&mut self) {
        for dispose in self.dispose.drain(..) {
            dispose();
        }
    }
}
