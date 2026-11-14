use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use serde_json::{Map, Value};

use super::getGoal::{GetGoalToolInput, get_goal_input_schema};
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tools::main_agent_only::{GOAL_MAIN_AGENT_ONLY, main_agent_only_execution};
use crate::features::goal::goal_service::AgentGoalService;
use crate::features::goal::tools::serialize::goal_result_for_model;
use crate::wire::record::WireAsyncError;

pub const GET_GOAL_DESCRIPTION: &str = include_str!("get-goal.md");

#[derive(Clone)]
pub struct GetGoalTool {
    goal: Arc<AgentGoalService>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl GetGoalTool {
    pub fn new(goal: Arc<AgentGoalService>, scope_context: Arc<dyn IAgentScopeContext>) -> Self {
        Self {
            goal,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        "GetGoal"
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        GET_GOAL_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        get_goal_input_schema()
            .as_object()
            .cloned()
            .expect("GetGoal input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<GetGoalToolInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for GetGoal: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, _args: &GetGoalToolInput) -> ToolExecution {
        let agent_id = self.scope_context.agent_id();
        if let Some(denied) = main_agent_only_execution(&agent_id, GOAL_MAIN_AGENT_ONLY) {
            return ToolExecution::Error(ExecutableToolResult::error(denied.content));
        }
        let goal = self.goal.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some("Reading the current goal".to_string()),
            stop_batch_after_this: None,
            approval_rule: "GetGoal".to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let goal = goal.clone();
                Box::pin(async move {
                    let result = goal.get_goal().map_err(wire_error)?;
                    let value = goal_result_for_model(&result);
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
}

fn wire_error(message: String) -> WireAsyncError {
    Box::new(Error2::new(INTERNAL_CODE, message))
}

impl ExecutableTool for GetGoalTool {
    fn name(&self) -> &str {
        GetGoalTool::name(self)
    }

    fn description(&self) -> &str {
        GetGoalTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        GetGoalTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
