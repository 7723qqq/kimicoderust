use std::collections::BTreeMap;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

pub use crate::injection::change_notifier::*;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentModelPoolSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    pub models: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentChangesInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_subagent_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_model_pool: Option<Option<SubagentModelPoolSnapshot>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeliefsSeed {
    pub agents_md: String,
    pub agents_md_paths: Vec<String>,
    pub skills_listing: String,
}

pub trait IAgentChangeNotifierService: Send + Sync {
    fn notify_agents_md_changes(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;

    fn notify_skill_changes(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;

    fn notify_subagent_changes(
        &self,
        input: Option<SubagentChangesInput>,
    ) -> BoxFuture<'static, Result<(), WireAsyncError>>;

    fn notify_workspace_dirs_changes(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;

    fn seed_beliefs(&self, seed: BeliefsSeed) -> BoxFuture<'static, Result<(), WireAsyncError>>;

    fn mute(&self);

    fn unmute(&self);
}
