use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::features::tower::tools::support::{
    TOWER_MAIN_AGENT_ONLY, TOWER_MODE_USER_ENABLED_ONLY, new_tower_store, run_tower_tool,
};
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::plan::{TowerPlanToolInput, tower_plan_tool_input_schema};

pub const TOWER_PLAN_TOOL_NAME: &str = "TowerPlan";

pub trait IAgentTowerService: Send + Sync {
    fn is_active(&self) -> bool;
    fn acquire_branch_lease(&self, branch: &str) -> Result<Box<dyn Send + Sync + '_>, String>;
}

#[derive(Clone)]
pub struct TowerPlanTool {
    session_context: Arc<dyn ISessionContext>,
    tower: Arc<dyn IAgentTowerService>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl TowerPlanTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        tower: Arc<dyn IAgentTowerService>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            session_context,
            tower,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_PLAN_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("plan.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_plan_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerPlan input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerPlanToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerPlan: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerPlanToolInput) -> ToolExecution {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return ToolExecution::Error(ExecutableToolResult::error(TOWER_MAIN_AGENT_ONLY));
        }
        let description = format!("Planning {} tower mission(s)", args.missions.len());
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_PLAN_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerPlanToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            if !self.tower.is_active() {
                return Ok(ExecutableToolResult::error(TOWER_MODE_USER_ENABLED_ONLY));
            }
            let store = new_tower_store(&self.session_context.cwd());
            let missions = store.plan(&args.missions).await?;
            let rows = missions
                .iter()
                .map(|mission| {
                    format!(
                        "| {} | {} | {} | {} | {} | {} |",
                        mission.id,
                        mission.title,
                        mission.kind.as_str(),
                        mission.branch,
                        mission.worktree,
                        mission.scope.join(", ")
                    )
                })
                .collect::<Vec<_>>();
            let mut lines = vec![
                format!("planned {} mission(s):", missions.len()),
                String::new(),
                "| ID | Mission | Kind | Branch | Worktree | Scope |".to_string(),
                "| -- | ------- | ---- | ------ | -------- | ----- |".to_string(),
            ];
            lines.extend(rows);
            lines.push(String::new());
            lines.push("Next: TowerSpawn one worker per mission (workers get their worktree path and mission briefing automatically). Build missions also need independent reviewers. Artifact missions instead present their key artifacts for explicit human acceptance of the entire committed tip before TowerMerge. Survey missions need no reviewer — they close with a zero-diff TowerMerge.".to_string());
            Ok(ExecutableToolResult {
                output: lines.join("\n").into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerPlanTool {
    fn name(&self) -> &str {
        TowerPlanTool::name(self)
    }

    fn description(&self) -> &str {
        TowerPlanTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerPlanTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
