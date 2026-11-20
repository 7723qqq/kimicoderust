use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::task::task::{AgentTaskInfo, IAgentTaskService};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::features::tower::protocol::paths::TOWER_NAME;
use crate::features::tower::protocol::store::{
    TowerActiveAgentIds, TowerMissionRuntimeOptions, TowerRebaseMissionStatus,
};
use crate::features::tower::protocol::types::TowerMissionKind;
use crate::features::tower::tools::plan::planTool::IAgentTowerService;
use crate::features::tower::tools::support::{TOWER_MAIN_AGENT_ONLY, new_tower_store, run_tower_tool};
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::rebase::{TowerRebaseToolInput, tower_rebase_tool_input_schema};

pub const TOWER_REBASE_TOOL_NAME: &str = "TowerRebase";

#[derive(Clone)]
pub struct TowerRebaseTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
    tasks: Arc<dyn IAgentTaskService>,
    tower: Arc<dyn IAgentTowerService>,
}

impl TowerRebaseTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
        tasks: Arc<dyn IAgentTaskService>,
        tower: Arc<dyn IAgentTowerService>,
    ) -> Self {
        Self {
            session_context,
            scope_context,
            tasks,
            tower,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_REBASE_TOOL_NAME
    }

    pub fn description(&self) -> &'static str {
        include_str!("rebase.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_rebase_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerRebase input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerRebaseToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerRebase: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerRebaseToolInput) -> ToolExecution {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return ToolExecution::Error(ExecutableToolResult::error(TOWER_MAIN_AGENT_ONLY));
        }
        let description = format!("Rebasing tower mission onto base: {}", args.mission);
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_REBASE_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerRebaseToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let state = store.load().await?;
            let lookup = args.mission.trim();
            let Some(mission) = state
                .missions
                .iter()
                .find(|mission| mission.id == lookup)
                .cloned()
            else {
                let known = state
                    .missions
                    .iter()
                    .map(|mission| mission.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let known = if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known
                };
                return Ok(ExecutableToolResult::error(format!(
                    "unknown mission \"{}\" — known: {known}",
                    args.mission
                )));
            };
            let tasks = self.tasks.clone();
            let runtime = TowerMissionRuntimeOptions {
                active_agent_ids: Some(TowerActiveAgentIds::Provider(Arc::new(move || {
                    active_agent_ids(tasks.as_ref())
                }))),
            };
            let _lease = self.tower.acquire_branch_lease(&mission.branch)?;
            let result = store
                .rebase_mission(TOWER_NAME, &mission.id, runtime)
                .await?;
            if result.status == TowerRebaseMissionStatus::UpToDate {
                return Ok(ExecutableToolResult {
                    output: format!(
                        "mission {} ({}) is already up to date with base \"{}\" — retry TowerMerge; if it was refused for another reason, that refusal tells you what is missing",
                        mission.id, mission.branch, state.base
                    )
                    .into(),
                    ..Default::default()
                });
            }
            if result.status == TowerRebaseMissionStatus::Conflict {
                let files = if result.files.is_empty() {
                    "(none reported)".to_string()
                } else {
                    result.files.join(", ")
                };
                let next = if mission.kind == TowerMissionKind::Artifact {
                    "resume the worker in the background (foreground only when background task tools are unavailable) so it resolves the conflicts in its worktree; the moved tip then needs fresh explicit human acceptance."
                } else {
                    "resume the worker in the background (foreground only when background task tools are unavailable) so it resolves the conflicts in its worktree and requests a fresh review."
                };
                return Ok(ExecutableToolResult {
                    output: [
                        format!(
                            "rebase of mission {} ({}) onto \"{}\" conflicted and was aborted — the mission is marked blocked.",
                            mission.id, mission.branch, state.base
                        ),
                        format!("conflicted files: {files}"),
                        next.to_string(),
                    ]
                    .join("\n")
                    .into(),
                    ..Default::default()
                });
            }
            let next = if mission.kind == TowerMissionKind::Artifact {
                "The pre-rebase human acceptance does not carry over — have the worker run TowerComplete with the current report and key_artifacts at the rebased tip, present that new request for fresh explicit human acceptance of the entire tip, record it with TowerReview, then retry TowerMerge."
            } else {
                "The pre-rebase review keeps its exact-tip binding, but merge approval still requires a clean review with merge verdict \"merge\" — retry TowerMerge."
            };
            Ok(ExecutableToolResult {
                output: [
                    format!(
                        "rebased mission {} ({}) onto \"{}\": {} → {}.",
                        mission.id,
                        mission.branch,
                        state.base,
                        short_commit(result.from_commit.as_deref()),
                        short_commit(result.to_commit.as_deref())
                    ),
                    next.to_string(),
                ]
                .join("\n")
                .into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerRebaseTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Write)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        TowerRebaseTool::name(self)
    }

    fn description(&self) -> &str {
        TowerRebaseTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerRebaseTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}

fn active_agent_ids(tasks: &dyn IAgentTaskService) -> HashSet<String> {
    tasks
        .list(Some(true), None)
        .into_iter()
        .filter_map(|task| match task {
            AgentTaskInfo::Agent(info) => info.agent_id,
            _ => None,
        })
        .collect()
}

fn short_commit(commit: Option<&str>) -> String {
    commit
        .map(|commit| commit.chars().take(7).collect())
        .unwrap_or_default()
}
