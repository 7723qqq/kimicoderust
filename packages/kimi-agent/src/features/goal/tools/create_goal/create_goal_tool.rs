use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use serde_json::{Map, Value, json};

use super::createGoal::{CreateGoalInput, create_goal_input_schema};
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tools::main_agent_only::{GOAL_MAIN_AGENT_ONLY, main_agent_only_execution};
use crate::features::goal::goal_service::AgentGoalService;
use crate::features::goal::tools::serialize::goal_for_model;
use crate::features::goal::types::GoalActor;
use crate::permission::PermissionMode;
use crate::tool::tool_input_display::{GoalStartMode, ToolInputDisplay};
use crate::wire::record::WireAsyncError;

pub const CREATE_GOAL_DESCRIPTION: &str = include_str!("create-goal.md");

#[derive(Clone)]
pub struct CreateGoalTool {
    goal: Arc<AgentGoalService>,
    scope_context: Arc<dyn IAgentScopeContext>,
    permission_mode: Arc<dyn IAgentPermissionModeService>,
}

impl CreateGoalTool {
    pub fn new(
        goal: Arc<AgentGoalService>,
        scope_context: Arc<dyn IAgentScopeContext>,
        permission_mode: Arc<dyn IAgentPermissionModeService>,
    ) -> Self {
        Self {
            goal,
            scope_context,
            permission_mode,
        }
    }

    pub fn name(&self) -> &'static str {
        "CreateGoal"
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        CREATE_GOAL_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        create_goal_input_schema()
            .as_object()
            .cloned()
            .expect("CreateGoal input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<CreateGoalInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for CreateGoal: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &CreateGoalInput) -> ToolExecution {
        let agent_id = self.scope_context.agent_id();
        if let Some(denied) = main_agent_only_execution(&agent_id, GOAL_MAIN_AGENT_ONLY) {
            return ToolExecution::Error(ExecutableToolResult::error(denied.content));
        }
        let goal_at_resolution = match self.goal.get_goal() {
            Ok(result) => result.goal,
            Err(error) => return ToolExecution::Error(ExecutableToolResult::error(error)),
        };
        let display = self.resolve_goal_start_display(args);
        let goal = self.goal.clone();
        let input = args.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display,
            description: Some("Creating a goal".to_string()),
            stop_batch_after_this: None,
            approval_rule: "CreateGoal".to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let goal = goal.clone();
                let input = input.clone();
                let goal_at_resolution = goal_at_resolution.clone();
                Box::pin(async move {
                    let current_goal = goal.get_goal().map_err(wire_error)?.goal;
                    let resolution_id = goal_at_resolution
                        .as_ref()
                        .map(|snapshot| snapshot.goal_id.clone());
                    let current_id = current_goal
                        .as_ref()
                        .map(|snapshot| snapshot.goal_id.clone());
                    if current_id != resolution_id {
                        let targets_current = current_goal.as_ref().is_some_and(|snapshot| {
                            goal.is_goal_tool_target(context.turn_id, &snapshot.goal_id)
                                .unwrap_or(false)
                        });
                        if !targets_current {
                            return Ok(ExecutableToolResult {
                                output: "Goal not created: the current goal changed.".into(),
                                ..Default::default()
                            });
                        }
                    }
                    let snapshot = goal
                        .create_goal(input, GoalActor::Model)
                        .await
                        .map_err(wire_error)?;
                    let value = json!({ "goal": goal_for_model(&snapshot) });
                    Ok(ExecutableToolResult {
                        output: serde_json::to_string_pretty(&value)
                            .unwrap_or_default()
                            .into(),
                        is_error: Some(false),
                        ..Default::default()
                    })
                })
            }),
        })
    }

    fn resolve_goal_start_display(&self, args: &CreateGoalInput) -> Option<ToolInputDisplay> {
        let mode = self.permission_mode.mode();
        if mode == PermissionMode::Auto {
            return None;
        }
        Some(ToolInputDisplay::GoalStart {
            objective: args.objective.clone(),
            completion_criterion: args.completion_criterion.clone(),
            mode: match mode {
                PermissionMode::Yolo => GoalStartMode::Yolo,
                _ => GoalStartMode::Manual,
            },
        })
    }
}

fn wire_error(message: String) -> WireAsyncError {
    Box::new(Error2::new(INTERNAL_CODE, message))
}

impl ExecutableTool for CreateGoalTool {
    fn name(&self) -> &str {
        CreateGoalTool::name(self)
    }

    fn description(&self) -> &str {
        CreateGoalTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        CreateGoalTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
