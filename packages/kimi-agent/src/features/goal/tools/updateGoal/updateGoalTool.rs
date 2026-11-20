use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use serde_json::{Map, Value};

use super::updateGoal::{UpdateGoalToolInput, update_goal_input_schema};
use crate::agent::goal::judge::goal_judge_service::AgentGoalJudgeService;
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolOutput, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::agent::tools::main_agent_only::{GOAL_MAIN_AGENT_ONLY, main_agent_only_execution};
use crate::features::goal::goal_service::{
    AgentGoalService, GOAL_OBJECTIVE_EDITED_TOOL_RESULT,
};
use crate::features::goal::tools::outcomePrompts::{
    build_goal_blocked_reason_prompt, build_goal_completion_summary_prompt,
};
use crate::features::goal::types::{GoalActor, GoalBudgetReport, GoalReasonInput, GoalSnapshot, GoalStatus};
use crate::wire::record::WireAsyncError;

pub const UPDATE_GOAL_DESCRIPTION: &str = include_str!("update-goal.md");

#[derive(Clone)]
pub struct UpdateGoalTool {
    goal: Arc<AgentGoalService>,
    judge: Arc<AgentGoalJudgeService>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl UpdateGoalTool {
    pub fn new(
        goal: Arc<AgentGoalService>,
        judge: Arc<AgentGoalJudgeService>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            goal,
            judge,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        "UpdateGoal"
    }

    pub fn description(&self) -> &'static str {
        UPDATE_GOAL_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        update_goal_input_schema()
            .as_object()
            .cloned()
            .expect("UpdateGoal input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<UpdateGoalToolInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for UpdateGoal: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &UpdateGoalToolInput) -> ToolExecution {
        let agent_id = self.scope_context.agent_id();
        if let Some(denied) = main_agent_only_execution(&agent_id, GOAL_MAIN_AGENT_ONLY) {
            return ToolExecution::Error(ExecutableToolResult::error(denied.content));
        }
        if !is_update_goal_status(&args.status) {
            return ToolExecution::Error(ExecutableToolResult::error(
                "Invalid goal status. Use `active`, `complete`, or `blocked`.",
            ));
        }

        let status = args.status.clone();
        let current_goal = match self.goal.get_goal() {
            Ok(result) => result.goal,
            Err(error) => return ToolExecution::Error(ExecutableToolResult::error(error)),
        };
        let goal_is_active = current_goal
            .as_ref()
            .is_some_and(|snapshot| snapshot.status == GoalStatus::Active);

        let goal = self.goal.clone();
        let judge = self.judge.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(format!("Setting goal status: {status}")),
            stop_batch_after_this: Some(status != "active" && goal_is_active),
            approval_rule: "UpdateGoal".to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let goal = goal.clone();
                let judge = judge.clone();
                let status = status.clone();
                let current_goal = current_goal.clone();
                Box::pin(async move {
                    let goal_at_execution = goal.get_goal().map_err(wire_error)?.goal;
                    let Some(goal_at_execution) = goal_at_execution else {
                        return Ok(output_result(missing_goal_output(&status)));
                    };
                    if current_goal.is_none() && status == "active" {
                        return Ok(output_result(missing_goal_output(&status)));
                    }
                    let changed = current_goal
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.goal_id != goal_at_execution.goal_id);
                    if changed
                        && !goal
                            .is_goal_tool_target(context.turn_id, &goal_at_execution.goal_id)
                            .unwrap_or(false)
                    {
                        return Ok(output_result(changed_goal_output(&status)));
                    }
                    if status == "active" {
                        goal.resume_goal(Default::default(), GoalActor::Model)
                            .await
                            .map_err(wire_error)?;
                        return Ok(output_result("Goal resumed."));
                    }
                    if goal
                        .is_objective_edited_since_turn_start(context.turn_id)
                        .unwrap_or(false)
                    {
                        return Ok(output_result(GOAL_OBJECTIVE_EDITED_TOOL_RESULT));
                    }
                    if status == "complete" {
                        let verdict = judge
                            .evaluate(&judge_snapshot(&goal_at_execution), None)
                            .await;
                        if !verdict.ok {
                            if verdict.impossible {
                                let blocked = goal
                                    .mark_blocked(
                                        GoalReasonInput {
                                            reason: Some(verdict.reason),
                                        },
                                        GoalActor::Model,
                                    )
                                    .await
                                    .map_err(wire_error)?;
                                let Some(blocked) = blocked else {
                                    return Ok(output_result(
                                        "Goal not blocked: no active goal.",
                                    ));
                                };
                                return Ok(ExecutableToolResult {
                                    output: build_goal_blocked_reason_prompt(&blocked).into(),
                                    stop_turn: Some(true),
                                    ..Default::default()
                                });
                            }
                            return Ok(output_result(format!(
                                "Goal completion rejected by judge: {}\nContinue working toward the goal objective.",
                                verdict.reason
                            )));
                        }
                        let completed = goal
                            .mark_complete(Default::default(), GoalActor::Model)
                            .await
                            .map_err(wire_error)?;
                        let Some(completed) = completed else {
                            return Ok(output_result("Goal not completed: no active goal."));
                        };
                        return Ok(ExecutableToolResult {
                            output: build_goal_completion_summary_prompt(&completed).into(),
                            stop_turn: Some(true),
                            ..Default::default()
                        });
                    }
                    if status == "blocked" {
                        let blocked = goal
                            .mark_blocked(Default::default(), GoalActor::Model)
                            .await
                            .map_err(wire_error)?;
                        let Some(blocked) = blocked else {
                            return Ok(output_result("Goal not blocked: no active goal."));
                        };
                        return Ok(ExecutableToolResult {
                            output: build_goal_blocked_reason_prompt(&blocked).into(),
                            stop_turn: Some(true),
                            ..Default::default()
                        });
                    }
                    Ok(ExecutableToolResult::error(
                        "Invalid goal status. Use `active`, `complete`, or `blocked`.",
                    ))
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

fn is_update_goal_status(status: &str) -> bool {
    matches!(status, "active" | "complete" | "blocked")
}

fn missing_goal_output(status: &str) -> &'static str {
    match status {
        "active" => "Goal not resumed: no current goal.",
        "complete" => "Goal not completed: no active goal.",
        _ => "Goal not blocked: no active goal.",
    }
}

fn changed_goal_output(status: &str) -> &'static str {
    match status {
        "active" => "Goal not resumed: the current goal changed.",
        "complete" => "Goal not completed: the current goal changed.",
        _ => "Goal not blocked: the current goal changed.",
    }
}

fn judge_snapshot(goal: &GoalSnapshot) -> crate::goal::GoalSnapshot {
    crate::goal::GoalSnapshot {
        goal_id: goal.goal_id.clone(),
        objective: goal.objective.clone(),
        completion_criterion: goal.completion_criterion.clone(),
        status: judge_status(goal.status),
        turns_used: goal.turns_used,
        tokens_used: goal.tokens_used,
        input_tokens_used: goal.input_tokens_used,
        output_tokens_used: goal.output_tokens_used,
        wall_clock_ms: goal.wall_clock_ms.max(0) as u64,
        budget: judge_budget(&goal.budget),
        terminal_reason: goal.terminal_reason.clone(),
        created_at: goal.created_at.max(0) as u64,
        updated_at: goal.updated_at.max(0) as u64,
    }
}

fn judge_status(status: GoalStatus) -> crate::goal::GoalStatus {
    match status {
        GoalStatus::Active => crate::goal::GoalStatus::Active,
        GoalStatus::Paused => crate::goal::GoalStatus::Paused,
        GoalStatus::Blocked => crate::goal::GoalStatus::Blocked,
        GoalStatus::Complete => crate::goal::GoalStatus::Complete,
        GoalStatus::BudgetLimited => crate::goal::GoalStatus::BudgetLimited,
        GoalStatus::UsageLimited => crate::goal::GoalStatus::UsageLimited,
    }
}

fn judge_budget(budget: &GoalBudgetReport) -> crate::goal::GoalBudgetReport {
    crate::goal::GoalBudgetReport {
        token_budget: budget.token_budget,
        turn_budget: budget.turn_budget,
        wall_clock_budget_ms: budget.wall_clock_budget_ms.map(|value| value.max(0) as u64),
        remaining_tokens: budget.remaining_tokens,
        remaining_turns: budget.remaining_turns,
        remaining_wall_clock_ms: budget
            .remaining_wall_clock_ms
            .map(|value| value.max(0) as u64),
        token_budget_reached: budget.token_budget_reached,
        turn_budget_reached: budget.turn_budget_reached,
        wall_clock_budget_reached: budget.wall_clock_budget_reached,
        over_budget: budget.over_budget,
        input_tokens_used: budget.input_tokens_used,
        output_tokens_used: budget.output_tokens_used,
    }
}

fn wire_error(message: String) -> WireAsyncError {
    Box::new(Error2::new(INTERNAL_CODE, message))
}

impl ExecutableTool for UpdateGoalTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        UpdateGoalTool::name(self)
    }

    fn description(&self) -> &str {
        UpdateGoalTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        UpdateGoalTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
