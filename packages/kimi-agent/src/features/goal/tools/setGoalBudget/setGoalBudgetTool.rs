use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use serde_json::{Map, Value};

use super::setGoalBudget::{SetGoalBudgetToolInput, set_goal_budget_input_schema};
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolOutput, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::agent::tools::main_agent_only::{GOAL_MAIN_AGENT_ONLY, main_agent_only_execution};
use crate::features::goal::goal_service::AgentGoalService;
use crate::features::goal::types::{GoalActor, GoalBudgetLimits, GoalSnapshot};
use crate::wire::record::WireAsyncError;

pub const SET_GOAL_BUDGET_DESCRIPTION: &str = include_str!("set-goal-budget.md");
pub const MIN_REASONABLE_TIME_BUDGET_MS: f64 = 1_000.0;

#[derive(Clone)]
pub struct SetGoalBudgetTool {
    goal: Arc<AgentGoalService>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl SetGoalBudgetTool {
    pub fn new(goal: Arc<AgentGoalService>, scope_context: Arc<dyn IAgentScopeContext>) -> Self {
        Self {
            goal,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        "SetGoalBudget"
    }

    pub fn description(&self) -> &'static str {
        SET_GOAL_BUDGET_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        set_goal_budget_input_schema()
            .as_object()
            .cloned()
            .expect("SetGoalBudget input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<SetGoalBudgetToolInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for SetGoalBudget: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &SetGoalBudgetToolInput) -> ToolExecution {
        let agent_id = self.scope_context.agent_id();
        if let Some(denied) = main_agent_only_execution(&agent_id, GOAL_MAIN_AGENT_ONLY) {
            return ToolExecution::Error(ExecutableToolResult::error(denied.content));
        }
        let normalized_args = normalize_budget_input(args);
        let budget = budget_limits_from_input(&normalized_args);
        let goal_at_resolution = match self.goal.get_goal() {
            Ok(result) => result.goal,
            Err(error) => return ToolExecution::Error(ExecutableToolResult::error(error)),
        };
        let over_budget_after_set = budget.as_ref().is_some_and(|budget| {
            goal_at_resolution
                .as_ref()
                .is_some_and(|goal| would_exceed_budget(goal, budget))
        });
        let budget_label = format_budget(normalized_args.value, &normalized_args.unit);

        let goal = self.goal.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(format!("Setting goal budget: {budget_label}")),
            stop_batch_after_this: Some(over_budget_after_set),
            approval_rule: "SetGoalBudget".to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let goal = goal.clone();
                let budget = budget.clone();
                let goal_at_resolution = goal_at_resolution.clone();
                let budget_label = budget_label.clone();
                Box::pin(async move {
                    let current_goal = goal.get_goal().map_err(wire_error)?.goal;
                    let Some(current_goal) = current_goal else {
                        return Ok(output_result("Goal budget not set: no current goal."));
                    };
                    let changed = goal_at_resolution
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.goal_id != current_goal.goal_id);
                    if changed
                        && !goal
                            .is_goal_tool_target(context.turn_id, &current_goal.goal_id)
                            .unwrap_or(false)
                    {
                        return Ok(output_result(
                            "Goal budget not set: the current goal changed.",
                        ));
                    }
                    let Some(budget) = budget else {
                        return Ok(output_result(format!(
                            "Goal budget not set: {budget_label} is not a reasonable goal budget."
                        )));
                    };
                    let snapshot = goal
                        .set_budget_limits(budget, GoalActor::Model)
                        .await
                        .map_err(wire_error)?;
                    if snapshot.budget.over_budget {
                        return Ok(ExecutableToolResult {
                            output: format!(
                                "Goal budget set: {budget_label}. The goal has already reached this budget and will stop now."
                            )
                            .into(),
                            stop_turn: Some(true),
                            ..Default::default()
                        });
                    }
                    Ok(output_result(format!("Goal budget set: {budget_label}.")))
                })
            }),
        })
    }
}

fn output_result(output: impl Into<ExecutableToolOutput>) -> ExecutableToolResult {
    ExecutableToolResult {
        output: output.into(),
        ..Default::default()
    }
}

fn normalize_budget_input(input: &SetGoalBudgetToolInput) -> SetGoalBudgetToolInput {
    match input.unit.as_str() {
        "turns" | "tokens" => SetGoalBudgetToolInput {
            value: input.value.round().max(1.0),
            unit: input.unit.clone(),
        },
        _ => input.clone(),
    }
}

fn budget_limits_from_input(input: &SetGoalBudgetToolInput) -> Option<GoalBudgetLimits> {
    match input.unit.as_str() {
        "turns" => Some(GoalBudgetLimits {
            turn_budget: Some(f64_to_u64(input.value)),
            ..Default::default()
        }),
        "tokens" => Some(GoalBudgetLimits {
            token_budget: Some(f64_to_u64(input.value)),
            ..Default::default()
        }),
        "milliseconds" | "seconds" | "minutes" | "hours" => {
            let wall_clock_budget_ms = to_milliseconds(input.value, &input.unit).round();
            if !wall_clock_budget_ms.is_finite()
                || wall_clock_budget_ms < MIN_REASONABLE_TIME_BUDGET_MS
            {
                return None;
            }
            Some(GoalBudgetLimits {
                wall_clock_budget_ms: Some(wall_clock_budget_ms as i64),
                ..Default::default()
            })
        }
        _ => None,
    }
}

fn to_milliseconds(value: f64, unit: &str) -> f64 {
    match unit {
        "seconds" => value * 1000.0,
        "minutes" => value * 60.0 * 1000.0,
        "hours" => value * 60.0 * 60.0 * 1000.0,
        _ => value,
    }
}

fn would_exceed_budget(goal: &GoalSnapshot, new_limits: &GoalBudgetLimits) -> bool {
    let current = &goal.budget;
    let turn_budget = new_limits.turn_budget.or(current.turn_budget);
    let token_budget = new_limits.token_budget.or(current.token_budget);
    let wall_clock_budget_ms = new_limits
        .wall_clock_budget_ms
        .or(current.wall_clock_budget_ms);
    turn_budget.is_some_and(|budget| goal.turns_used >= budget)
        || token_budget.is_some_and(|budget| goal.tokens_used >= budget)
        || wall_clock_budget_ms.is_some_and(|budget| goal.wall_clock_ms >= budget)
}

fn format_budget(value: f64, unit: &str) -> String {
    let singular = unit.strip_suffix('s').unwrap_or(unit);
    let label = if value == 1.0 { singular } else { unit };
    format!("{} {label}", js_number(value))
}

fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

fn f64_to_u64(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value as u64
    } else {
        0
    }
}

fn wire_error(message: String) -> WireAsyncError {
    Box::new(Error2::new(INTERNAL_CODE, message))
}

impl ExecutableTool for SetGoalBudgetTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        SetGoalBudgetTool::name(self)
    }

    fn description(&self) -> &str {
        SetGoalBudgetTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        SetGoalBudgetTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
