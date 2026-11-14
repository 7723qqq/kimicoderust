use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::features::tower::protocol::paths::{MISSIONS_DIR, mission_file_name};
use crate::features::tower::protocol::store::{
    TowerMissionPatch, TowerStore, TowerTaskDrop, TowerUpdateMissionOptions,
};
use crate::features::tower::protocol::types::TowerMission;
use crate::features::tower::tools::support::{new_tower_store, run_tower_tool};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::mission::{TowerMissionToolInput, tower_mission_tool_input_schema};

pub const TOWER_MISSION_TOOL_NAME: &str = "TowerMission";

#[derive(Clone)]
pub struct TowerMissionTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl TowerMissionTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            session_context,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_MISSION_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("mission.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_mission_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerMission input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerMissionToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerMission: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerMissionToolInput) -> ToolExecution {
        let has_patch = args.status.is_some()
            || args.note.is_some()
            || args.blocker.is_some()
            || args.clear_blockers.is_some()
            || args.task_done.is_some()
            || args.task_drop.is_some()
            || args.scope.is_some();
        let description = if has_patch {
            format!("Updating tower mission {}", args.id)
        } else {
            format!("Reading tower mission {}", args.id)
        };
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_MISSION_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args, has_patch).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerMissionToolInput, has_patch: bool) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let state = store.load().await?;
            let caller = store.resolve_caller_name(&state, &self.scope_context.agent_id())?;
            if !has_patch {
                let Some(mission) = state
                    .missions
                    .iter()
                    .find(|mission| mission.id == args.id)
                else {
                    let known = state
                        .missions
                        .iter()
                        .map(|mission| mission.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let known = if known.is_empty() {
                        "(none planned yet)".to_string()
                    } else {
                        known
                    };
                    return Ok(ExecutableToolResult::error(format!(
                        "unknown mission \"{}\" — known missions: {known}",
                        args.id
                    )));
                };
                return Ok(ExecutableToolResult {
                    output: render_mission(&store, mission).await?.into(),
                    ..Default::default()
                });
            }
            let TowerMissionToolInput {
                id,
                status,
                note,
                blocker,
                clear_blockers,
                task_done,
                task_drop,
                task_drop_reason,
                scope,
            } = args;
            let patch = TowerMissionPatch {
                status,
                note,
                blocker,
                clear_blockers,
                task_done,
                task_drop: task_drop.map(|text| TowerTaskDrop {
                    text,
                    reason: task_drop_reason,
                }),
                owner: None,
                scope,
                spawn_base: None,
            };
            let mission = store
                .update_mission(
                    &caller,
                    &id,
                    patch,
                    TowerUpdateMissionOptions::default(),
                )
                .await?;
            let open_tasks = mission
                .tasks
                .iter()
                .filter(|task| !task.done && !task.dropped)
                .count();
            let content = render_mission(&store, &mission).await?;
            Ok(ExecutableToolResult {
                output: format!(
                    "mission {} updated — status: {}, open tasks: {open_tasks}, blockers: {}\n\n{content}",
                    mission.id,
                    mission.status.as_str(),
                    mission.blockers.len()
                )
                .into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerMissionTool {
    fn name(&self) -> &str {
        TowerMissionTool::name(self)
    }

    fn description(&self) -> &str {
        TowerMissionTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerMissionTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}

async fn render_mission(store: &TowerStore, mission: &TowerMission) -> Result<String, String> {
    let path = store.abs(&format!(
        "{MISSIONS_DIR}/{}",
        mission_file_name(&mission.id, &mission.slug)
    ));
    tokio::fs::read_to_string(path)
        .await
        .map_err(|error| error.to_string())
}
