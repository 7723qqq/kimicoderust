use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::app::session_manager::session_manager::ISessionManager;
use crate::features::tower::protocol::store::{TowerReanchorMissionDigest, TowerReanchorRelation};
use crate::features::tower::tools::plan::planTool::IAgentTowerService;
use crate::features::tower::tools::support::{
    TOWER_MAIN_AGENT_ONLY, TOWER_MODE_USER_ENABLED_ONLY, new_tower_store, run_tower_tool,
};
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::init::{TowerInitToolInput, tower_init_tool_input_schema};

pub const TOWER_INIT_TOOL_NAME: &str = "TowerInit";

#[derive(Clone)]
pub struct TowerInitTool {
    session_context: Arc<dyn ISessionContext>,
    tower: Arc<dyn IAgentTowerService>,
    sessions: Arc<dyn ISessionManager>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl TowerInitTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        tower: Arc<dyn IAgentTowerService>,
        sessions: Arc<dyn ISessionManager>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            session_context,
            tower,
            sessions,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_INIT_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("init.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_init_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerInit input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerInitToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerInit: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerInitToolInput) -> ToolExecution {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return ToolExecution::Error(ExecutableToolResult::error(TOWER_MAIN_AGENT_ONLY));
        }
        if !self.tower.is_active() {
            return ToolExecution::Error(ExecutableToolResult::error(TOWER_MODE_USER_ENABLED_ONLY));
        }
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some("Initializing tower workspace".to_string()),
            stop_batch_after_this: None,
            approval_rule: TOWER_INIT_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerInitToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let session_id = self.session_context.session_id();
            let prior_owner = store
                .load()
                .await
                .ok()
                .and_then(|state| state.session_id);
            if let Some(prior_owner) = &prior_owner {
                if prior_owner != &session_id && self.sessions.get(prior_owner).is_some() {
                    return Err(format!(
                        "tower workspace is owned by a live session ({prior_owner}) — adopting it would retire that session's roster. Use the tower from that session, or close it first."
                    ));
                }
            }
            let result = store.init(Some(session_id), args.base).await?;
            let mut lines = vec![
                if result.created {
                    "tower workspace initialized".to_string()
                } else {
                    "tower workspace already initialized — existing state preserved".to_string()
                },
                format!("base branch: {}", result.base),
            ];
            if let Some(rebased_from) = &result.rebased_from {
                lines.push(format!(
                    "base re-anchored from \"{rebased_from}\" to \"{}\" — open missions now evaluate against \"{}\"",
                    result.base, result.base
                ));
            }
            if let Some(digest) = &result.rebase_digest {
                lines.extend(digest.missions.iter().map(reanchor_mission_line));
            }
            if result.checkout != result.base {
                lines.push(if result.checkout == "HEAD" {
                    format!(
                        "note: the main checkout is in a detached HEAD state — merges stay blocked until the base is checked out (git checkout {})",
                        result.base
                    )
                } else {
                    format!(
                        "note: the main checkout is on \"{}\", not base \"{}\" — merges stay blocked until it is switched over (git checkout {})",
                        result.checkout, result.base, result.base
                    )
                });
            }
            lines.push(
                "workspace: .tower/ (comms under .tower/comms/, worktrees under .tower/worktrees/)"
                    .to_string(),
            );
            if !result.open_missions.is_empty() {
                lines.push(format!(
                    "carried-over open missions: {} — their scopes are still reserved. Continue them (TowerSpawn fresh workers), or — when they belong to an unrelated earlier task — abandon them first (TowerMission status=abandoned) so a new plan can use those files.",
                    result.open_missions.join(", ")
                ));
            }
            if !result.retired_agents.is_empty() {
                lines.push(format!(
                    "adopted from a previous session — retired its stale roster entries: {}. Their agents belong to the dead session and cannot be resumed; missions and worktrees are preserved — TowerSpawn fresh workers to continue them.",
                    result.retired_agents.join(", ")
                ));
            }
            lines.push(String::new());
            lines.push("Tower mode is active and the tower tool set is enabled.".to_string());
            lines.push(
                "Next: split the work with TowerPlan (one mission per disjoint file scope), then TowerSpawn a worker per mission. Assign independent reviewers for their branches, and merge with TowerMerge only after a clean review with merge verdict \"merge\"."
                    .to_string(),
            );
            Ok(ExecutableToolResult {
                output: lines.join("\n").into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerInitTool {
    fn name(&self) -> &str {
        TowerInitTool::name(self)
    }

    fn description(&self) -> &str {
        TowerInitTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerInitTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}

fn reanchor_mission_line(mission: &TowerReanchorMissionDigest) -> String {
    let prefix = format!(
        "mission {} ({}): {}",
        mission.id,
        mission.branch,
        mission.relation.as_str()
    );
    match mission.relation {
        TowerReanchorRelation::UpToDate => format!(
            "{prefix} — the branch already contains the new base tip; no rebase is required by this re-anchor"
        ),
        TowerReanchorRelation::Stale => format!(
            "{prefix} — the new base is not an ancestor of this branch tip; run TowerRebase(mission=\"{}\") before merging",
            mission.id
        ),
        TowerReanchorRelation::NoBranch => format!(
            "{prefix} — no branch exists yet; spawn first, then evaluate its relation before merging"
        ),
    }
}
