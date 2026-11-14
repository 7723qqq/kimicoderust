use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::task::task::{AgentTaskInfo, IAgentTaskService};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::features::tower::protocol::store::{
    TowerActiveAgentIds, TowerMergeResult, TowerMergeStatus, TowerMissionRuntimeOptions,
};
use crate::features::tower::protocol::types::TowerMissionStatus;
use crate::features::tower::tools::plan::planTool::IAgentTowerService;
use crate::features::tower::tools::support::{TOWER_MAIN_AGENT_ONLY, new_tower_store, run_tower_tool};
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::merge::{TowerMergeToolInput, tower_merge_tool_input_schema};

pub const TOWER_MERGE_TOOL_NAME: &str = "TowerMerge";

const TEARDOWN_HINT: &str = "Every mission is now merged or abandoned — ready for TowerTeardown (branches and .tower/comms/ are kept; dirty worktrees are protected).";

#[derive(Clone)]
pub struct TowerMergeTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
    tasks: Arc<dyn IAgentTaskService>,
    tower: Arc<dyn IAgentTowerService>,
}

impl TowerMergeTool {
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
        TOWER_MERGE_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "write"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("merge.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_merge_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerMerge input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerMergeToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerMerge: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerMergeToolInput) -> ToolExecution {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return ToolExecution::Error(ExecutableToolResult::error(TOWER_MAIN_AGENT_ONLY));
        }
        let description = format!("Merging tower branch: {}", args.branch);
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_MERGE_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerMergeToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let tasks = self.tasks.clone();
            let runtime = TowerMissionRuntimeOptions {
                active_agent_ids: Some(TowerActiveAgentIds::Provider(Arc::new(move || {
                    active_agent_ids(tasks.as_ref())
                }))),
            };
            let result = {
                let _lease = self.tower.acquire_branch_lease(&args.branch)?;
                store.merge(&args.branch, runtime).await?
            };
            let TowerMergeResult {
                status,
                merge_commit,
                conflicts_with,
                evaluated_branch_tip,
                current_branch_tip,
                noop,
            } = result;
            if status == TowerMergeStatus::Advanced {
                if noop {
                    return Ok(ExecutableToolResult::error(format!(
                        "SURVEY NOT CLOSED: {} advanced from the evaluated zero-diff tip {} to {} before the mission could be closed — no git merge was performed and the mission was not marked merged; re-evaluate the new tip",
                        args.branch, evaluated_branch_tip, current_branch_tip
                    )));
                }
                return Ok(ExecutableToolResult::error(
                    [
                        format!(
                            "PARTIAL MERGE: only the evaluated commit {} of {} was merged (merge commit {}).",
                            evaluated_branch_tip, args.branch, merge_commit
                        ),
                        format!(
                            "{} advanced during the merge to {}; that newer tip and any commits after {} were NOT merged.",
                            args.branch, current_branch_tip, evaluated_branch_tip
                        ),
                        "The mission was not marked merged. Review the new exact tip, then retry TowerMerge; an artifact mission instead needs fresh explicit human acceptance on that tip. Do not treat the partial merge as a completed mission merge.".to_string(),
                    ]
                    .join("\n"),
                ));
            }
            let after = store.load().await?;
            let all_closed = !after.missions.is_empty()
                && after.missions.iter().all(|mission| {
                    mission.status == TowerMissionStatus::Merged
                        || mission.status == TowerMissionStatus::Abandoned
                });
            if status == TowerMergeStatus::Noop {
                let mut lines = vec![format!(
                    "{} is a read-only survey with a zero-diff branch — mission marked merged, no git merge needed.",
                    args.branch
                )];
                lines.push(if all_closed {
                    TEARDOWN_HINT.to_string()
                } else {
                    "Continue with the remaining missions in Dependency Flow order.".to_string()
                });
                return Ok(ExecutableToolResult {
                    output: lines.join("\n").into(),
                    ..Default::default()
                });
            }
            let short_commit = if merge_commit.len() >= 7 {
                &merge_commit[..7]
            } else {
                &merge_commit
            };
            let mut lines = vec![
                format!("merged {} (merge commit {short_commit})", args.branch),
                format!("full commit: {merge_commit}"),
            ];
            if !conflicts_with.is_empty() {
                lines.push(String::new());
                lines.push(
                    "These unmerged branches changed the same files and now likely conflict with the base:"
                        .to_string(),
                );
                for conflict in conflicts_with {
                    lines.push(format!(
                        "- {}: {}",
                        conflict.branch,
                        conflict.files.join(", ")
                    ));
                }
                lines.push("For each affected mission, follow the next structured refusal: TowerRebase while its worker is idle (or ask the worker to rebase mid-turn). Conflict-free rebase preserves build review binding; artifact missions instead require fresh explicit human acceptance after the tip moves. Real conflicts must be resolved before another merge.".to_string());
            } else if all_closed {
                lines.push(format!("The mission is now marked merged. {TEARDOWN_HINT}"));
            } else {
                lines.push("The mission is now marked merged. Continue with the remaining missions in Dependency Flow order.".to_string());
            }
            Ok(ExecutableToolResult {
                output: lines.join("\n").into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerMergeTool {
    fn name(&self) -> &str {
        TowerMergeTool::name(self)
    }

    fn description(&self) -> &str {
        TowerMergeTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerMergeTool::parameters(self)
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
