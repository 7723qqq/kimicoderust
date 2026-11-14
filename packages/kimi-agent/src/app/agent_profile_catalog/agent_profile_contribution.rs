use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::_base::di::collection::{CollectionToken, collection};

use super::agent_profile_catalog::AgentProfile;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedAgentFile {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Default)]
pub struct AgentProfileContribution {
    pub profiles: Vec<AgentProfile>,
    pub skipped: Option<Vec<SkippedAgentFile>>,
    pub scanned_roots: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct AgentProfileContributionRecord {
    pub source_id: String,
    pub priority: Option<i64>,
    pub workspace_key: Option<String>,
    pub context_key: Option<String>,
    pub contribution: AgentProfileContribution,
}

pub static AGENT_PROFILE_CONTRIBUTION: LazyLock<
    CollectionToken<AgentProfileContributionRecord>,
> = LazyLock::new(|| collection("agent-profile", Default::default()));

pub struct AgentProfileSourcePriority {
    pub builtin: i64,
    pub plugin: i64,
    pub user: i64,
    pub extra: i64,
    pub workspace: i64,
    pub explicit: i64,
}

pub const AGENT_PROFILE_SOURCE_PRIORITY: AgentProfileSourcePriority = AgentProfileSourcePriority {
    builtin: 0,
    plugin: 5,
    user: 10,
    extra: 20,
    workspace: 30,
    explicit: 40,
};
