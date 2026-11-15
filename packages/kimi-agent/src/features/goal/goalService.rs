use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::agent::actor_service::agent_actor_runtime::AgentActorHost;
use crate::agent::actor_service::agent_actor_service::{
    AgentActorContext, AgentActorDurable, AgentActorOptions, AgentActorService, StateFold,
};
use crate::agent::actor_service::services::{
    AgentLifecycleService, AgentLoopService, ConfigService, EventBusService, FlagService,
    SessionUsageService, TelemetryService, ToolApprovalRequest, ToolApprovalService,
    ToolExecutorService,
};
use crate::agent::agent_context::AgentContext;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::tool_executor::tool_executor::IAgentToolRegistryService;
use crate::agent::tool_policy::tool_policy::IAgentToolPolicyService;
use crate::agent::permission_mode::permission_mode_service::AgentPermissionModeService;
use crate::callbacks::MAIN_AGENT_ID;
use crate::features::goal::goalDeadlineScheduler::GoalDeadlineScheduler;
use crate::features::goal::goalOps::{
    GoalClear, GoalCreate, GoalUpdate, GoalUpdated, goal_create_event, goal_updated_event,
};
use crate::features::goal::injection::{
    GOAL_WAIT_FOR_GUIDANCE, GoalInjection, GoalInjectionOptions,
};
use crate::features::goal::types::{
    CreateGoalInput, EditGoalObjectiveInput, GoalActor, GoalBudgetLimits, GoalBudgetReport,
    GoalChange, GoalChangeKind, GoalChangeStats, GoalEffectState, GoalReasonInput,
    GoalRuntimeState, GoalSnapshot, GoalState, GoalStatus, GoalToolResult, ResumeGoalInput,
};
use crate::features::reminder::types::ReminderNotification;
use crate::human::xstate2::{
    Action, ActorLogic, Context, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent,
    from_callback, send_to,
};
use crate::permission::PermissionMode;

pub const GOAL_CONTINUATION_ORIGIN: &str = "goal_continuation";

const MAX_GOAL_OBJECTIVE_LENGTH: usize = 4000;
const MAX_GOAL_COMPLETION_CRITERION_LENGTH: usize = MAX_GOAL_OBJECTIVE_LENGTH;

const GOAL_CANCELLED_REMINDER: &str = "The user cancelled the current goal. Ignore earlier active-goal reminders for that goal. Handle the next user request normally unless the user starts or resumes a goal.";
const GOAL_FORK_CLEARED_REMINDER: &str = "This fork does not have a current goal. Ignore earlier active-goal reminders from the source session. Handle requests normally unless the user starts a new goal.";
const GOAL_FORK_CLEARED_REMINDER_NAME: &str = "goal_fork_cleared";
const GOAL_BUDGET_BLOCK_PREFIX: &str = "Blocked after goal budget reached";
const GOAL_BUDGET_STOP_REMINDER: &str = "The goal's hard budget was reached and the goal is now blocked; the user can resume it with /goal resume. Stop immediately. Do not call any more tools: they will be rejected. Write a brief final status message summarizing the progress so far.";
const GOAL_BUDGET_STOP_REMINDER_NAME: &str = "goal_budget_stop";
const GOAL_BUDGET_TOOLS_REJECTED_MESSAGE: &str =
    "Goal budget exhausted; tool calls are rejected. Write your final message.";
const GOAL_STALE_TOOL_RESULT: &str =
    "Goal changed since this turn started; ignored stale goal tool call.";
pub const GOAL_OBJECTIVE_EDITED_TOOL_RESULT: &str =
    "The goal objective was edited since this turn started, so this status update was ignored. End this turn normally; the next turn receives the new objective.";
const LLM_NOT_SET_MESSAGE: &str = "LLM not set, send \"/login\" to login";
const GOAL_CONTINUATION_PROMPT: &str = "Continue working toward the active goal. Keep the self-audit brief. Do not explore unrelated interpretations once the goal can be decided. If the objective is simple, already answered, impossible, unsafe, or contradictory, do not run another goal turn. Explain briefly if useful, then call UpdateGoal with `complete` or `blocked` in the same turn. Otherwise, weigh the objective and any completion criteria against the work done so far, choose one bounded, useful slice of work, and use the existing conversation context and your tools. Do not try to finish a broad goal in one turn unless the whole goal is genuinely small. Most goal turns should not call UpdateGoal: after completing a useful slice, if material work remains, end the turn normally without calling UpdateGoal so the runtime can continue the goal in the next turn. Call UpdateGoal with `complete` only when all required work is done, any stated validation has passed, and there is no useful next action. Completion audit: before calling `complete`, verify the current state against the actual objective and every explicit requirement. Treat weak or indirect evidence as not complete. Do not mark complete after only producing a plan, summary, first pass, or partial result. Do not mark complete merely because a budget is nearly exhausted or you want to stop. Blocked audit: do not call UpdateGoal with `blocked` the first time you hit a blocker. Use `blocked` only for a genuine impasse: an external condition, required user input, missing credentials or permissions, or a persistent technical failure. For those non-terminal blockers, the same blocking condition must repeat for at least 3 consecutive goal turns before you call `blocked`, counting the original/user-triggered turn and automatic continuations. If a previously blocked goal is resumed, treat the resumed run as a fresh blocked audit. Exception: if the objective itself is impossible, unsafe, or contradictory, call UpdateGoal with `blocked` in the same turn; do not run more goal turns just to satisfy the audit. Do not use `blocked` because the work is large, hard, slow, uncertain, incomplete, still needs validation, would benefit from clarification, or needs more goal turns. Once the 3-turn threshold is met and you cannot make meaningful progress without user input or an external-state change, call UpdateGoal with `blocked`; do not keep reporting the blocker while leaving the goal active. Do not ask the user for input unless a real blocker prevents progress.";
const GOAL_STEP_CAP_CONTINUATION_PROMPT: &str = "The previous goal turn reached the per-turn step limit before finishing its work, so a new turn was started for you. Pick up where that turn stopped and keep each slice of work small enough to fit the limit. Continue working toward the active goal. Keep the self-audit brief. Do not explore unrelated interpretations once the goal can be decided. If the objective is simple, already answered, impossible, unsafe, or contradictory, do not run another goal turn. Explain briefly if useful, then call UpdateGoal with `complete` or `blocked` in the same turn. Otherwise, weigh the objective and any completion criteria against the work done so far, choose one bounded, useful slice of work, and use the existing conversation context and your tools. Do not try to finish a broad goal in one turn unless the whole goal is genuinely small. Most goal turns should not call UpdateGoal: after completing a useful slice, if material work remains, end the turn normally without calling UpdateGoal so the runtime can continue the goal in the next turn. Call UpdateGoal with `complete` only when all required work is done, any stated validation has passed, and there is no useful next action. Completion audit: before calling `complete`, verify the current state against the actual objective and every explicit requirement. Treat weak or indirect evidence as not complete. Do not mark complete after only producing a plan, summary, first pass, or partial result. Do not mark complete merely because a budget is nearly exhausted or you want to stop. Blocked audit: do not call UpdateGoal with `blocked` the first time you hit a blocker. Use `blocked` only for a genuine impasse: an external condition, required user input, missing credentials or permissions, or a persistent technical failure. For those non-terminal blockers, the same blocking condition must repeat for at least 3 consecutive goal turns before you call `blocked`, counting the original/user-triggered turn and automatic continuations. If a previously blocked goal is resumed, treat the resumed run as a fresh blocked audit. Exception: if the objective itself is impossible, unsafe, or contradictory, call UpdateGoal with `blocked` in the same turn; do not run more goal turns just to satisfy the audit. Do not use `blocked` because the work is large, hard, slow, uncertain, incomplete, still needs validation, would benefit from clarification, or needs more goal turns. Once the 3-turn threshold is met and you cannot make meaningful progress without user input or an external-state change, call UpdateGoal with `blocked`; do not keep reporting the blocker while leaving the goal active. Do not ask the user for input unless a real blocker prevents progress.";

const GOAL_RATE_LIMIT_PAUSE_REASON: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::RATE_LIMIT;
const GOAL_PROVIDER_CONNECTION_PAUSE_PREFIX: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::PROVIDER_CONNECTION;
const GOAL_PROVIDER_AUTH_PAUSE_PREFIX: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::PROVIDER_AUTH;
const GOAL_PROVIDER_API_PAUSE_PREFIX: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::PROVIDER_API;
const GOAL_PROVIDER_FILTERED_PAUSE_REASON: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::PROVIDER_FILTERED;
const GOAL_MODEL_CONFIG_PAUSE_PREFIX: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::MODEL_CONFIG;
const GOAL_RUNTIME_PAUSE_PREFIX: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::RUNTIME;
const GOAL_CONTINUATION_FAILURE_PAUSE_PREFIX: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::CONTINUATION_FAILURE;
const GOAL_PAUSE_REASON_INTERRUPTION: &str =
    crate::agent::interaction::engineMessages::GoalPauseReason::INTERRUPTION;

fn is_goal_fork_cleared_reminder(message: &Value) -> bool {
    let Some(origin) = message.get("origin") else {
        return false;
    };
    if origin.get("kind").and_then(Value::as_str) == Some("injection") {
        return origin.get("variant").and_then(Value::as_str)
            == Some(GOAL_FORK_CLEARED_REMINDER_NAME);
    }
    origin.get("kind").and_then(Value::as_str) == Some("system_trigger")
        && origin.get("name").and_then(Value::as_str) == Some(GOAL_FORK_CLEARED_REMINDER_NAME)
}

fn goal_continuation_origin() -> Value {
    json!({"kind": "system_trigger", "name": GOAL_CONTINUATION_ORIGIN})
}

fn is_goal_continuation_origin(origin: &Value) -> bool {
    origin.get("kind").and_then(Value::as_str) == Some("system_trigger")
        && origin.get("name").and_then(Value::as_str) == Some(GOAL_CONTINUATION_ORIGIN)
}

#[derive(Clone)]
pub struct GoalOperationContext {
    pub runtime: AgentActorContext<GoalRuntimeState>,
    pub effects: Arc<Mutex<GoalEffectState>>,
}

pub fn goal_operation_context(
    runtime: &AgentActorContext<GoalRuntimeState>,
) -> GoalOperationContext {
    GoalOperationContext {
        runtime: runtime.clone(),
        effects: (runtime.get_logic_state)()
            .downcast::<Mutex<GoalActorContext>>()
            .unwrap()
            .lock()
            .effects
            .clone(),
    }
}

#[derive(Clone)]
pub struct GoalActorContext {
    pub durable: GoalRuntimeState,
    pub effects: Arc<Mutex<GoalEffectState>>,
    pub runtime: Context,
}

fn reminder_of(
    runtime: &AgentActorContext<GoalRuntimeState>,
) -> Arc<dyn crate::features::reminder::reminder_service::ReminderService> {
    runtime.service("reminderService")
}

fn deadline_scheduler_of(
    runtime: &AgentActorContext<GoalRuntimeState>,
) -> Arc<dyn GoalDeadlineScheduler> {
    runtime.service("goalDeadlineScheduler")
}

fn assert_supported_agent(context: &GoalOperationContext) -> Result<(), String> {
    if context.runtime.agent.agent_id == MAIN_AGENT_ID {
        return Ok(());
    }
    Err(format!(
        "Goals are only supported by the main agent (agentId: {})",
        context.runtime.agent.agent_id
    ))
}

pub fn get_goal(context: &GoalOperationContext) -> Result<GoalToolResult, String> {
    assert_supported_agent(context)?;
    let state = (context.runtime.get_state)().goal;
    Ok(GoalToolResult {
        goal: state.map(|state| to_snapshot(context, &state)),
    })
}

pub fn is_goal_tool_target(
    context: &GoalOperationContext,
    turn_id: u64,
    goal_id: &str,
) -> Result<bool, String> {
    assert_supported_agent(context)?;
    Ok(context
        .effects
        .lock()
        .goal_turn_targets
        .get(&turn_id)
        .map(|id| id == goal_id)
        .unwrap_or(false))
}

pub fn is_objective_edited_since_turn_start(
    context: &GoalOperationContext,
    turn_id: u64,
) -> Result<bool, String> {
    assert_supported_agent(context)?;
    let (revision, current) = {
        let effects = context.effects.lock();
        (
            effects.turn_objective_revisions.get(&turn_id).copied(),
            effects.objective_revision,
        )
    };
    Ok(revision.is_some() && revision != Some(current))
}

pub async fn create_goal(
    context: &GoalOperationContext,
    input: CreateGoalInput,
    actor: GoalActor,
) -> Result<GoalSnapshot, String> {
    assert_supported_agent(context)?;
    let objective = validate_objective(context, &input.objective)?;
    prepare_for_goal_creation(context, input.replace == Some(true))?;
    let wall_clock_resumed_at = now_ms();
    let goal_id = uuid::Uuid::new_v4().to_string();
    let completion_criterion = normalize_completion_criterion(input.completion_criterion.as_deref());
    (context.runtime.dispatch)(goal_create_event(
        &context.runtime.agent.agent_id,
        &goal_id,
        &objective,
        completion_criterion.as_deref(),
        Some(wall_clock_resumed_at),
        Some(wall_clock_resumed_at),
        Some(wall_clock_resumed_at),
        None,
        None,
        None,
    ))
    .await?;
    context.effects.lock().live_wall_clock_started_at =
        Some(deadline_scheduler_of(&context.runtime).now());
    adopt_starter_turn(context, actor);
    let state = require_state(context)?;
    refresh_wall_clock_deadline(context, &state);
    emit_goal_updated(context, Some(to_snapshot(context, &state)), None).await?;
    context
        .runtime
        .service::<dyn TelemetryService>("telemetryService")
        .track2(
            "goal_created",
            json!({"actor": actor, "replace": input.replace == Some(true)}),
        );
    Ok(to_snapshot(context, &state))
}

fn validate_objective(_context: &GoalOperationContext, value: &str) -> Result<String, String> {
    let objective = trim_js(value);
    if objective.is_empty() {
        return Err("Goal objective cannot be empty".into());
    }
    if objective.encode_utf16().count() > MAX_GOAL_OBJECTIVE_LENGTH {
        return Err(format!(
            "Goal objective cannot exceed {} characters. Put long content in a file and reference the file path.",
            MAX_GOAL_OBJECTIVE_LENGTH
        ));
    }
    Ok(objective.to_string())
}

pub async fn edit_goal_objective(
    context: &GoalOperationContext,
    input: EditGoalObjectiveInput,
    actor: GoalActor,
) -> Result<GoalSnapshot, String> {
    assert_supported_agent(context)?;
    let state = require_state(context)?;
    if state.status == GoalStatus::Complete {
        return Err(format!("Cannot edit a goal in status \"{}\"", state.status));
    }
    let objective = validate_objective(context, &input.objective)?;
    if objective == state.objective {
        return Ok(to_snapshot(context, &state));
    }
    (context.runtime.dispatch)(json!({
        "type": "goal.update",
        "agentId": context.runtime.agent.agent_id,
        "objective": objective,
        "actor": actor,
    }))
    .await?;
    context.effects.lock().objective_revision += 1;
    let snapshot = to_snapshot(context, &require_state(context)?);
    emit_goal_updated(context, Some(snapshot.clone()), None).await?;
    Ok(snapshot)
}

fn prepare_for_goal_creation(context: &GoalOperationContext, replace: bool) -> Result<(), String> {
    if (context.runtime.get_state)().goal.is_none() {
        return Ok(());
    }
    if !replace {
        return Err("A goal already exists; use replace to start a new one".into());
    }
    clear_internal(context, "system", Default::default());
    Ok(())
}

pub async fn pause_goal(
    context: &GoalOperationContext,
    input: GoalReasonInput,
    actor: GoalActor,
) -> Result<GoalSnapshot, String> {
    assert_supported_agent(context)?;
    let state = require_state(context)?;
    if state.status == GoalStatus::Paused {
        return Ok(to_snapshot(context, &state));
    }
    if state.status != GoalStatus::Active {
        return Err(format!(
            "Cannot pause a goal in status \"{}\"",
            state.status
        ));
    }
    apply_lifecycle(
        context,
        &state,
        GoalStatus::Paused,
        input.reason,
        actor,
        Default::default(),
    )
}

pub async fn pause_active_goal(
    context: &GoalOperationContext,
    input: GoalReasonInput,
    actor: GoalActor,
) -> Result<Option<GoalSnapshot>, String> {
    assert_supported_agent(context)?;
    let Some(state) = (context.runtime.get_state)().goal else {
        return Ok(None);
    };
    if state.status != GoalStatus::Active {
        return Ok(None);
    }
    apply_lifecycle(
        context,
        &state,
        GoalStatus::Paused,
        input.reason,
        actor,
        Default::default(),
    )
    .map(Some)
}

pub async fn resume_goal(
    context: &GoalOperationContext,
    input: ResumeGoalInput,
    actor: GoalActor,
) -> Result<GoalSnapshot, String> {
    assert_supported_agent(context)?;
    let state = require_state(context)?;
    if state.status == GoalStatus::Active {
        return Ok(to_snapshot(context, &state));
    }
    if state.status != GoalStatus::Paused && state.status != GoalStatus::Blocked {
        return Err(format!(
            "Cannot resume a goal in status \"{}\"",
            state.status
        ));
    }
    let continue_paused = actor == GoalActor::User
        && state.status == GoalStatus::Paused
        && input.continue_if_paused == Some(true);
    let should_continue = continue_paused
        || (actor == GoalActor::User
            && state.status == GoalStatus::Blocked
            && input.continue_if_blocked == Some(true));
    let snapshot = apply_lifecycle(
        context,
        &state,
        GoalStatus::Active,
        input.reason,
        actor,
        Default::default(),
    )?;
    if !should_continue {
        return Ok(snapshot);
    }
    if let Some(blocked) = block_if_budget_reached(context, &require_state(context)?)? {
        return Ok(blocked);
    }
    if can_launch_continuation(context) {
        launch_continuation_turn(context, &state.goal_id, false);
    } else if continue_paused {
        if let Some(turn_id) = context.effects.lock().live_turn_id {
            context.effects.lock().resume_continuation =
                Some(crate::features::goal::types::ResumeContinuation {
                    turn_id: Some(turn_id),
                    goal_id: state.goal_id.clone(),
                });
        }
    }
    Ok(snapshot)
}

pub async fn set_budget_limits(
    context: &GoalOperationContext,
    input: GoalBudgetLimits,
    actor: GoalActor,
) -> Result<GoalSnapshot, String> {
    assert_supported_agent(context)?;
    let state = require_state(context)?;
    let budget_limits = GoalBudgetLimits {
        token_budget: input.token_budget.or(state.budget_limits.token_budget),
        turn_budget: input.turn_budget.or(state.budget_limits.turn_budget),
        wall_clock_budget_ms: input
            .wall_clock_budget_ms
            .or(state.budget_limits.wall_clock_budget_ms),
    };
    (context.runtime.dispatch)(json!({
        "type": "goal.update",
        "agentId": context.runtime.agent.agent_id,
        "budgetLimits": budget_limits,
    }))
    .await?;
    let next = require_state(context)?;
    emit_goal_updated(context, Some(to_snapshot(context, &next)), None).await?;
    context.runtime.service::<dyn TelemetryService>("telemetryService").track2(
        "goal_budget_set",
        json!({"actor": actor, "has_token_budget": input.token_budget.is_some(), "has_turn_budget": input.turn_budget.is_some(), "has_wall_clock_budget": input.wall_clock_budget_ms.is_some()}),
    );
    if let Some(blocked) = block_if_budget_reached(context, &next)? {
        return Ok(blocked);
    }
    refresh_wall_clock_deadline(context, &next);
    Ok(to_snapshot(context, &next))
}

pub async fn cancel_goal(
    context: &GoalOperationContext,
    _input: GoalReasonInput,
    actor: GoalActor,
) -> Result<GoalSnapshot, String> {
    assert_supported_agent(context)?;
    let state = require_state(context)?;
    let snapshot = to_snapshot(context, &state);
    if state.status == GoalStatus::Active {
        if let Some(turn_id) = context.effects.lock().live_turn_id {
            context
                .runtime
                .service::<dyn AgentLoopService>("loopService")
                .cancel(json!({"turnId": turn_id}), json!("Goal cancelled"));
        }
    }
    clear_internal(context, actor.as_str(), Default::default());
    if actor == GoalActor::User {
        reminder_of(&context.runtime).notify(
            GOAL_CANCELLED_REMINDER,
            &ReminderNotification {
                variant: "goal_cancelled".to_string(),
                owner_prompt_id: None,
            },
        );
    }
    Ok(snapshot)
}

pub async fn mark_blocked(
    context: &GoalOperationContext,
    input: GoalReasonInput,
    actor: GoalActor,
) -> Result<Option<GoalSnapshot>, String> {
    assert_supported_agent(context)?;
    let Some(state) = (context.runtime.get_state)().goal else {
        return Ok(None);
    };
    if state.status != GoalStatus::Active {
        return Ok(None);
    }
    apply_lifecycle(
        context,
        &state,
        GoalStatus::Blocked,
        input.reason,
        actor,
        crate::features::goal::types::LifecycleOptions {
            preserve_live_continuation: true,
            ..Default::default()
        },
    )
    .map(Some)
}

pub async fn mark_complete(
    context: &GoalOperationContext,
    input: GoalReasonInput,
    actor: GoalActor,
) -> Result<Option<GoalSnapshot>, String> {
    assert_supported_agent(context)?;
    let Some(state) = (context.runtime.get_state)().goal else {
        return Ok(None);
    };
    if state.status != GoalStatus::Active {
        return Ok(None);
    }
    dispatch_completion(context, &state, input.reason.clone(), actor).await?;
    let completed = require_state(context)?;
    let snapshot = to_snapshot(context, &completed);
    emit_completion(context, &completed, &snapshot, input.reason, actor).await?;
    track_status_changed(context, &completed, actor);
    clear_internal(
        context,
        actor.as_str(),
        crate::features::goal::types::ClearOptions {
            preserve_live_continuation: true,
            ..Default::default()
        },
    );
    Ok(Some(snapshot))
}

fn dispatch_completion(
    context: &GoalOperationContext,
    state: &GoalState,
    reason: Option<String>,
    actor: GoalActor,
) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
    let wall_clock_ms = settle_wall_clock(context, state);
    let agent_id = context.runtime.agent.agent_id.clone();
    let reason = reason.clone();
    let dispatch = context.runtime.dispatch.clone();
    Box::pin(async move {
        dispatch(json!({
            "type": "goal.update",
            "agentId": agent_id,
            "status": "complete",
            "reason": reason,
            "wallClockMs": wall_clock_ms,
            "actor": actor,
        }))
        .await
    })
}

fn emit_completion(
    context: &GoalOperationContext,
    state: &GoalState,
    snapshot: &GoalSnapshot,
    reason: Option<String>,
    actor: GoalActor,
) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
    let change = GoalChange {
        kind: GoalChangeKind::Completion,
        status: Some("complete".to_string()),
        reason,
        stats: Some(stats_of(context, state)),
        actor: Some(actor),
    };
    emit_goal_updated(context, Some(snapshot.clone()), Some(change))
}

pub async fn pause_on_interrupt(
    context: &GoalOperationContext,
    input: GoalReasonInput,
) -> Result<Option<GoalSnapshot>, String> {
    assert_supported_agent(context)?;
    pause_active_goal(context, input, GoalActor::User).await
}

pub async fn record_token_usage(
    context: &GoalOperationContext,
    token_delta: i64,
) -> Result<Option<GoalSnapshot>, String> {
    assert_supported_agent(context)?;
    Ok(account_token_usage(context, token_delta, None))
}

pub async fn increment_turn(
    context: &GoalOperationContext,
) -> Result<Option<GoalSnapshot>, String> {
    assert_supported_agent(context)?;
    Ok(increment_goal_turn(context, None))
}

fn account_token_usage(
    context: &GoalOperationContext,
    token_delta: i64,
    goal_id: Option<&str>,
) -> Option<GoalSnapshot> {
    let state = (context.runtime.get_state)().goal?;
    if state.status != GoalStatus::Active || !matches_goal(&state, goal_id) {
        return None;
    }
    let tokens_used = state.tokens_used + token_delta.max(0) as u64;
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    tokio::spawn(async move {
        let _ = dispatch(
            json!({"type": "goal.update", "agentId": agent_id, "tokensUsed": tokens_used}),
        )
        .await;
    });
    let next = require_state(context).ok()?;
    block_if_budget_reached(context, &next)
        .ok()
        .flatten()
        .or_else(|| Some(to_snapshot(context, &next)))
}

fn increment_goal_turn(
    context: &GoalOperationContext,
    goal_id: Option<&str>,
) -> Option<GoalSnapshot> {
    let state = (context.runtime.get_state)().goal?;
    if state.status != GoalStatus::Active || !matches_goal(&state, goal_id) {
        return None;
    }
    let turns_used = state.turns_used + 1;
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    tokio::spawn(async move {
        let _ =
            dispatch(json!({"type": "goal.update", "agentId": agent_id, "turnsUsed": turns_used}))
                .await;
    });
    let next = require_state(context).ok()?;
    let snapshot = to_snapshot(context, &next);
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    let snapshot_for_event = snapshot.clone();
    tokio::spawn(async move {
        let _ = dispatch(
            json!({"type": "goal.updated", "agentId": agent_id, "snapshot": snapshot_for_event}),
        )
        .await;
    });
    context
        .runtime
        .service::<dyn TelemetryService>("telemetryService")
        .track2("goal_continued", json!({"turns_used": next.turns_used}));
    Some(snapshot)
}

fn handle_turn_launched(context: &GoalOperationContext, turn_id: u64, origin: &Value) {
    context.effects.lock().live_turn_id = Some(turn_id);
    context.effects.lock().goal_turn_targets.remove(&turn_id);
    context
        .effects
        .lock()
        .exhausted_turn_budget_goals
        .remove(&turn_id);
    if let Some(pending) = context.effects.lock().pending_continuation.clone() {
        if pending.turn_id.is_none() && is_goal_continuation_origin(origin) {
            let mut pending = pending;
            pending.turn_id = Some(turn_id);
            context
                .effects
                .lock()
                .pending_continuation_goals
                .insert(turn_id, pending.goal_id.clone());
            context.effects.lock().pending_continuation = Some(pending);
        }
    }
    if !context
        .effects
        .lock()
        .goal_driven_turns
        .contains_key(&turn_id)
    {
        let state = (context.runtime.get_state)().goal;
        let continuation_goal_id = if is_goal_continuation_origin(origin) {
            context
                .effects
                .lock()
                .pending_continuation_goals
                .get(&turn_id)
                .cloned()
        } else {
            None
        };
        if let Some(continuation_goal_id) = continuation_goal_id {
            if state.as_ref().map(|state| &state.goal_id) != Some(&continuation_goal_id) {
                context
                    .effects
                    .lock()
                    .goal_driven_turns
                    .insert(turn_id, continuation_goal_id);
            }
        } else if let Some(state) = state {
            if state.status == GoalStatus::Active
                && block_if_budget_reached(context, &state)
                    .ok()
                    .flatten()
                    .is_none()
            {
                context
                    .effects
                    .lock()
                    .goal_driven_turns
                    .insert(turn_id, state.goal_id.clone());
            }
        }
    }
    context
        .effects
        .lock()
        .pending_continuation_goals
        .remove(&turn_id);
    context
        .effects
        .lock()
        .goal_outcome_tool_result_turns
        .remove(&turn_id);
    context
        .effects
        .lock()
        .goal_outcome_continuation_turns
        .remove(&turn_id);
    let objective_revision = context.effects.lock().objective_revision;
    context
        .effects
        .lock()
        .turn_objective_revisions
        .insert(turn_id, objective_revision);
}

fn adopt_starter_turn(context: &GoalOperationContext, actor: GoalActor) {
    let Some(turn_id) = context.effects.lock().live_turn_id else {
        return;
    };
    let Some(state) = (context.runtime.get_state)().goal else {
        return;
    };
    if state.status != GoalStatus::Active {
        return;
    }
    let objective_revision = context.effects.lock().objective_revision;
    context
        .effects
        .lock()
        .turn_objective_revisions
        .entry(turn_id)
        .or_insert(objective_revision);
    let goal_id = context
        .effects
        .lock()
        .goal_driven_turns
        .get(&turn_id)
        .cloned();
    if actor == GoalActor::Model {
        context
            .effects
            .lock()
            .goal_turn_targets
            .insert(turn_id, state.goal_id.clone());
    }
    if to_snapshot(context, &state).budget.turn_budget_reached {
        context
            .effects
            .lock()
            .exhausted_turn_budget_goals
            .insert(turn_id, state.goal_id.clone());
    } else {
        context
            .effects
            .lock()
            .exhausted_turn_budget_goals
            .remove(&turn_id);
    }
    if goal_id.is_some() {
        return;
    }
    context
        .effects
        .lock()
        .goal_driven_turns
        .insert(turn_id, state.goal_id.clone());
    context.effects.lock().counted_goal_turns.insert(turn_id);
    context.effects.lock().goal_starter_turns.insert(turn_id);
}

async fn handle_before_step(context: &GoalOperationContext, turn_id: u64) {
    let Some(goal_id) = context
        .effects
        .lock()
        .goal_driven_turns
        .get(&turn_id)
        .cloned()
    else {
        return;
    };
    if context.effects.lock().counted_goal_turns.contains(&turn_id) {
        return;
    }
    context.effects.lock().counted_goal_turns.insert(turn_id);
    increment_goal_turn(context, Some(&goal_id));
}

fn handle_usage_recorded(context: &GoalOperationContext, usage: &Value) {
    let Some(source) = usage.get("source") else {
        return;
    };
    if source.get("type").and_then(Value::as_str) != Some("turn") {
        return;
    }
    let Some(turn_id) = source.get("turnId").and_then(Value::as_u64) else {
        return;
    };
    let Some(goal_id) = context
        .effects
        .lock()
        .goal_driven_turns
        .get(&turn_id)
        .cloned()
    else {
        return;
    };
    let Some(recorded) = usage.get("usage") else {
        return;
    };
    account_token_usage(context, chargeable_tokens(recorded), Some(&goal_id));
}

fn handle_after_step(context: &GoalOperationContext, mut step: Value) {
    if stop_after_budget_reached(context, &mut step) {
        return;
    }
    enqueue_goal_outcome_continuation(context, &step);
}

fn stop_after_budget_reached(context: &GoalOperationContext, step: &mut Value) -> bool {
    let turn_id = step.get("turnId").and_then(Value::as_u64).unwrap_or(0);
    let goal_id = goal_turn_target(context, turn_id);
    let state = (context.runtime.get_state)().goal;
    let budget = state
        .as_ref()
        .map(|state| to_snapshot(context, state).budget);
    let turn_budget_blocks_current_turn = budget
        .as_ref()
        .map(|budget| budget.turn_budget_reached)
        .unwrap_or(false)
        && (context
            .effects
            .lock()
            .exhausted_turn_budget_goals
            .get(&turn_id)
            == goal_id.as_ref()
            || (state
                .as_ref()
                .map(|state| state.status == GoalStatus::Blocked)
                .unwrap_or(false)
                && state
                    .as_ref()
                    .and_then(|state| state.terminal_reason.as_deref())
                    .map(|reason| reason.starts_with(GOAL_BUDGET_BLOCK_PREFIX))
                    .unwrap_or(false)));
    if goal_id.is_none()
        || state.is_none()
        || state.as_ref().map(|state| &state.goal_id) != goal_id.as_ref()
        || budget.is_none()
        || (!budget
            .as_ref()
            .map(|budget| budget.token_budget_reached)
            .unwrap_or(false)
            && !budget
                .as_ref()
                .map(|budget| budget.wall_clock_budget_reached)
                .unwrap_or(false)
            && !turn_budget_blocks_current_turn)
    {
        return false;
    }
    let max_steps = context
        .runtime
        .service::<dyn ConfigService>("configService")
        .get("loopControl")
        .and_then(|config| config.get("maxStepsPerTurn").and_then(Value::as_u64));
    if step.get("finishReason").and_then(Value::as_str) == Some("tool_calls")
        && !context.effects.lock().budget_grace_turns.contains(&turn_id)
        && has_step_budget_remaining(
            max_steps,
            step.get("step").and_then(Value::as_u64).unwrap_or(0),
        )
    {
        context.effects.lock().budget_grace_turns.insert(turn_id);
        reminder_of(&context.runtime).notify(
            GOAL_BUDGET_STOP_REMINDER,
            &ReminderNotification {
                variant: GOAL_BUDGET_STOP_REMINDER_NAME.to_string(),
                owner_prompt_id: None,
            },
        );
        return true;
    }
    step["stopTurn"] = json!(true);
    true
}

fn enqueue_goal_outcome_continuation(context: &GoalOperationContext, step: &Value) {
    let turn_id = step.get("turnId").and_then(Value::as_u64).unwrap_or(0);
    if context
        .effects
        .lock()
        .goal_outcome_continuation_turns
        .contains(&turn_id)
    {
        return;
    }
    let goal_id = goal_turn_target(context, turn_id);
    let outcome_goal_id = context
        .effects
        .lock()
        .goal_outcome_tool_result_turns
        .get(&turn_id)
        .cloned();
    context
        .effects
        .lock()
        .goal_outcome_tool_result_turns
        .remove(&turn_id);
    if goal_id.is_none() || outcome_goal_id != goal_id {
        return;
    }
    let state = (context.runtime.get_state)().goal;
    if state.as_ref().map(|state| &state.goal_id) != goal_id.as_ref() {
        return;
    }
    context
        .effects
        .lock()
        .goal_outcome_continuation_turns
        .insert(turn_id);
    let max_steps = context
        .runtime
        .service::<dyn ConfigService>("configService")
        .get("loopControl")
        .and_then(|config| config.get("maxStepsPerTurn").and_then(Value::as_u64));
    if !has_step_budget_remaining(
        max_steps,
        step.get("step").and_then(Value::as_u64).unwrap_or(0),
    ) {
        return;
    }
    context
        .runtime
        .service::<dyn AgentLoopService>("loopService")
        .notify();
}

async fn handle_turn_ended(context: &GoalOperationContext, turn_id: u64, result: &Value) {
    let (goal_id, lifecycle_goal_id, starter_turn) = clear_turn_tracking(context, turn_id);
    let resume_continuation = context.effects.lock().resume_continuation.clone();
    if resume_continuation
        .as_ref()
        .and_then(|resume| resume.turn_id)
        == Some(turn_id)
    {
        context.effects.lock().resume_continuation = None;
    }
    if resume_continuation
        .as_ref()
        .and_then(|resume| resume.turn_id)
        == Some(turn_id)
        && result.get("reason").and_then(Value::as_str) == Some("cancelled")
    {
        let state = (context.runtime.get_state)().goal;
        let resume_goal_id = resume_continuation.as_ref().map(|resume| &resume.goal_id);
        if state
            .as_ref()
            .map(|state| {
                state.status == GoalStatus::Active && Some(&state.goal_id) == resume_goal_id
            })
            .unwrap_or(false)
        {
            if block_if_budget_reached(context, &state.unwrap())
                .ok()
                .flatten()
                .is_none()
            {
                launch_continuation_turn(context, resume_goal_id.unwrap(), false);
            }
        }
        return;
    }
    if goal_id.is_none() || lifecycle_goal_id.is_none() {
        return;
    }
    let step_capped = is_max_steps_turn_failure(result);
    if !step_capped
        && matches!(
            result.get("reason").and_then(Value::as_str),
            Some("blocked") | Some("cancelled") | Some("failed")
        )
    {
        settle_abnormal_turn(context, result, lifecycle_goal_id.as_ref().unwrap()).await;
        return;
    }
    if starter_turn {
        increment_goal_turn(context, goal_id.as_deref());
    }
    let state = (context.runtime.get_state)().goal;
    if state
        .as_ref()
        .map(|state| {
            state.status == GoalStatus::Active && Some(&state.goal_id) == lifecycle_goal_id.as_ref()
        })
        .unwrap_or(false)
    {
        if block_if_budget_reached(context, state.as_ref().unwrap())
            .ok()
            .flatten()
            .is_none()
        {
            launch_continuation_turn(context, lifecycle_goal_id.as_ref().unwrap(), step_capped);
        }
    }
}

fn clear_turn_tracking(
    context: &GoalOperationContext,
    turn_id: u64,
) -> (Option<String>, Option<String>, bool) {
    if context
        .effects
        .lock()
        .pending_continuation
        .as_ref()
        .and_then(|pending| pending.turn_id)
        == Some(turn_id)
    {
        context.effects.lock().pending_continuation = None;
    }
    if context.effects.lock().live_turn_id == Some(turn_id) {
        context.effects.lock().live_turn_id = None;
    }
    let goal_id = context
        .effects
        .lock()
        .goal_driven_turns
        .get(&turn_id)
        .cloned();
    let lifecycle_goal_id = goal_turn_target(context, turn_id);
    let starter_turn = context.effects.lock().goal_starter_turns.remove(&turn_id);
    context.effects.lock().goal_driven_turns.remove(&turn_id);
    context.effects.lock().counted_goal_turns.remove(&turn_id);
    context
        .effects
        .lock()
        .goal_outcome_tool_result_turns
        .remove(&turn_id);
    context
        .effects
        .lock()
        .goal_outcome_continuation_turns
        .remove(&turn_id);
    context.effects.lock().budget_grace_turns.remove(&turn_id);
    context
        .effects
        .lock()
        .pending_continuation_goals
        .remove(&turn_id);
    context.effects.lock().goal_turn_targets.remove(&turn_id);
    context
        .effects
        .lock()
        .turn_objective_revisions
        .remove(&turn_id);
    context
        .effects
        .lock()
        .exhausted_turn_budget_goals
        .remove(&turn_id);
    (goal_id, lifecycle_goal_id, starter_turn)
}

async fn settle_abnormal_turn(
    context: &GoalOperationContext,
    result: &Value,
    goal_id: &str,
) -> bool {
    if !is_active_goal(context, goal_id) {
        return false;
    }
    match result.get("reason").and_then(Value::as_str) {
        Some("blocked") => {
            let _ = mark_blocked(
                context,
                GoalReasonInput {
                    reason: Some("Blocked by UserPromptSubmit hook".into()),
                },
                GoalActor::Runtime,
            )
            .await;
            true
        }
        Some("cancelled") => {
            let _ = pause_on_interrupt(
                context,
                GoalReasonInput {
                    reason: Some(GOAL_PAUSE_REASON_INTERRUPTION.to_string()),
                },
            )
            .await;
            true
        }
        Some("failed") => {
            let _ = pause_active_goal(
                context,
                GoalReasonInput {
                    reason: Some(goal_failure_pause_reason(
                        result.get("error").unwrap_or(&Value::Null),
                    )),
                },
                GoalActor::Runtime,
            )
            .await;
            true
        }
        _ => false,
    }
}

pub async fn settle_goal_after_continuation_failure(
    context: &GoalOperationContext,
    error: &Value,
    goal_id: Option<&str>,
) {
    let Some(goal_id) = goal_id else {
        return;
    };
    if !is_active_goal(context, goal_id) {
        return;
    }
    let reason = pause_reason_with_message(
        GOAL_CONTINUATION_FAILURE_PAUSE_PREFIX,
        normalize_goal_error_payload(error)
            .get("message")
            .and_then(Value::as_str),
    );
    let _ = pause_active_goal(
        context,
        GoalReasonInput {
            reason: Some(reason),
        },
        GoalActor::System,
    )
    .await;
}

fn is_wait_for_available(context: &GoalOperationContext) -> bool {
    context
        .runtime
        .service::<dyn FlagService>("flagService")
        .enabled("wait_for")
        && context
            .runtime
            .service::<dyn IAgentToolRegistryService>("toolRegistryService")
            .resolve("WaitFor")
            .is_some()
        && context
            .runtime
            .service::<dyn IAgentToolPolicyService>("toolPolicyService")
            .is_tool_active("WaitFor", None)
}

fn launch_continuation_turn(context: &GoalOperationContext, goal_id: &str, step_capped: bool) {
    if !is_active_goal(context, goal_id) {
        return;
    }
    if context.effects.lock().pending_continuation.is_some() {
        return;
    }
    let prompt = if step_capped {
        GOAL_STEP_CAP_CONTINUATION_PROMPT
    } else {
        GOAL_CONTINUATION_PROMPT
    };
    let text = if is_wait_for_available(context) {
        format!("{prompt} {}", GOAL_WAIT_FOR_GUIDANCE)
    } else {
        prompt.to_string()
    };
    let message = json!({
        "role": "user",
        "content": [{"type": "text", "text": text}],
        "toolCalls": [],
        "origin": goal_continuation_origin(),
    });
    let loop_service = context
        .runtime
        .service::<dyn AgentLoopService>("loopService");
    let id = loop_service
        .submit(
            json!({
                "message": {"role": "user", "content": message.get("content")},
                "meta": {"origin": message.get("origin")},
            }),
            None,
        )
        .expect("goal continuation submit failed");
    let pending = crate::features::goal::types::PendingContinuation {
        prompt_id: id.clone(),
        goal_id: goal_id.to_string(),
        turn_id: None,
        turn: None,
    };
    context.effects.lock().pending_continuation = Some(pending.clone());
    if let Some(handle) = loop_service.prompt_handle(&id) {
        let pending_for_launch = pending.clone();
        let effects_for_completion = context.effects.clone();
        tokio::spawn(async move {
            let mut pending = pending_for_launch;
            if let Ok(launched_turn) = handle.launched.await {
                pending.turn_id = launched_turn.get("id").and_then(Value::as_u64);
                pending.turn = Some(launched_turn);
            }
            let _ = handle.completion.await;
            if let Some(turn_id) = pending.turn_id {
                effects_for_completion
                    .lock()
                    .pending_continuation_goals
                    .remove(&turn_id);
            }
            let mut effects = effects_for_completion.lock();
            if effects
                .pending_continuation
                .as_ref()
                .is_some_and(|current| current.prompt_id == pending.prompt_id)
            {
                effects.pending_continuation = None;
            }
        });
    }
}

fn can_launch_continuation(context: &GoalOperationContext) -> bool {
    if context.effects.lock().live_turn_id.is_some()
        || context.effects.lock().pending_continuation.is_some()
    {
        return false;
    }
    let status = context
        .runtime
        .service::<dyn AgentLoopService>("loopService")
        .snapshot();
    status.get("state").and_then(Value::as_str) == Some("idle")
        && !status
            .get("hasPendingRequests")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn is_active_goal(context: &GoalOperationContext, goal_id: &str) -> bool {
    (context.runtime.get_state)()
        .goal
        .map(|state| state.status == GoalStatus::Active && state.goal_id == goal_id)
        .unwrap_or(false)
}

fn is_stale_goal_tool_call(context: &GoalOperationContext, event: &Value) -> bool {
    let tool_name = event
        .get("toolCall")
        .and_then(|call| call.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !is_goal_mutation_tool(tool_name) {
        return false;
    }
    let turn_id = event.get("turnId").and_then(Value::as_u64).unwrap_or(0);
    let Some(goal_id) = goal_turn_target(context, turn_id) else {
        return false;
    };
    (context.runtime.get_state)()
        .goal
        .map(|state| state.goal_id != goal_id)
        .unwrap_or(false)
}

fn goal_turn_target(context: &GoalOperationContext, turn_id: u64) -> Option<String> {
    let effects = context.effects.lock();
    effects
        .goal_turn_targets
        .get(&turn_id)
        .cloned()
        .or_else(|| effects.goal_driven_turns.get(&turn_id).cloned())
}

fn cancel_pending_continuation(
    context: &GoalOperationContext,
    preserve_live_continuation: bool,
    reason: Option<Value>,
) {
    let pending = context.effects.lock().pending_continuation.clone();
    if preserve_live_continuation
        && pending.as_ref().and_then(|pending| pending.turn_id)
            == context.effects.lock().live_turn_id
    {
        return;
    }
    context.effects.lock().pending_continuation = None;
    let cancellation = reason.unwrap_or_else(|| json!("Goal continuation cancelled"));
    let cancelled = pending
        .as_ref()
        .and_then(|pending| pending.turn.as_ref())
        .map(|turn| {
            let _ = turn;
            false
        })
        .unwrap_or(false);
    if pending.is_some() && !cancelled {
        let pending = pending.unwrap();
        let selector = if pending.turn_id.is_some() {
            json!({"turnId": pending.turn_id})
        } else {
            json!({"promptId": pending.prompt_id})
        };
        context
            .runtime
            .service::<dyn AgentLoopService>("loopService")
            .cancel(selector, cancellation);
    }
}

fn normalize_after_replay(context: &GoalOperationContext) {
    append_fork_cleared_reminder(context);
    (context.runtime.send)(XEvent::new("goal.deadline.clear", Value::Null));
    context.effects.lock().live_wall_clock_started_at = None;
    let Some(state) = (context.runtime.get_state)().goal else {
        return;
    };
    if state.status == GoalStatus::Complete {
        clear_internal(
            context,
            "runtime",
            crate::features::goal::types::ClearOptions {
                emit: false,
                track: false,
                ..Default::default()
            },
        );
        return;
    }
    if state.status != GoalStatus::Active {
        return;
    }
    let wall_clock_ms = settle_wall_clock(context, &state);
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    tokio::spawn(async move {
        let _ = dispatch(json!({
            "type": "goal.update",
            "agentId": agent_id,
            "status": "paused",
            "reason": "Paused after agent resume",
            "wallClockMs": wall_clock_ms,
            "actor": "runtime",
        }))
        .await;
    });
    track_status_changed(
        context,
        &require_state(context).unwrap(),
        GoalActor::Runtime,
    );
}

fn append_fork_cleared_reminder(context: &GoalOperationContext) {
    if !(context.runtime.get_state)().fork_notice.reminder_pending {
        return;
    }
    reminder_of(&context.runtime).notify(
        GOAL_FORK_CLEARED_REMINDER,
        &ReminderNotification {
            variant: GOAL_FORK_CLEARED_REMINDER_NAME.to_string(),
            owner_prompt_id: None,
        },
    );
}

fn clear_internal(
    context: &GoalOperationContext,
    actor: &str,
    options: crate::features::goal::types::ClearOptions,
) {
    if (context.runtime.get_state)().goal.is_none() {
        return;
    }
    context.effects.lock().resume_continuation = None;
    cancel_pending_continuation(context, options.preserve_live_continuation, None);
    (context.runtime.send)(XEvent::new("goal.deadline.clear", Value::Null));
    context.effects.lock().live_wall_clock_started_at = None;
    context.effects.lock().turn_objective_revisions.clear();
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    let emit = options.emit;
    tokio::spawn(async move {
        let _ = dispatch(json!({"type": "goal.clear", "agentId": agent_id})).await;
        if emit {
            let _ = dispatch(
                json!({"type": "goal.updated", "agentId": agent_id, "snapshot": Value::Null}),
            )
            .await;
        }
    });
    if options.track {
        context
            .runtime
            .service::<dyn TelemetryService>("telemetryService")
            .track2("goal_cleared", json!({"actor": actor}));
    }
}

fn apply_lifecycle(
    context: &GoalOperationContext,
    state: &GoalState,
    status: GoalStatus,
    reason: Option<String>,
    actor: GoalActor,
    options: crate::features::goal::types::LifecycleOptions,
) -> Result<GoalSnapshot, String> {
    let wall_clock_ms = settle_wall_clock(context, state);
    let wall_clock_resumed_at = if status == GoalStatus::Active {
        Some(now_ms())
    } else {
        None
    };
    if status == GoalStatus::Active {
        context.effects.lock().live_wall_clock_started_at =
            Some(deadline_scheduler_of(&context.runtime).now());
    } else if state.status == GoalStatus::Active {
        context.effects.lock().resume_continuation = None;
        cancel_pending_continuation(
            context,
            options.preserve_live_continuation,
            options.cancellation_reason,
        );
        (context.runtime.send)(XEvent::new("goal.deadline.clear", Value::Null));
        context.effects.lock().live_wall_clock_started_at = None;
    }
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    let status_str = status.as_str().to_string();
    let reason_for_event = reason.clone();
    let actor_for_event = actor;
    tokio::spawn(async move {
        let _ = dispatch(json!({
            "type": "goal.update",
            "agentId": agent_id,
            "status": status_str,
            "reason": reason_for_event,
            "wallClockMs": wall_clock_ms,
            "wallClockResumedAt": wall_clock_resumed_at,
            "actor": actor_for_event,
        }))
        .await;
    });
    let next = require_state(context)?;
    if status == GoalStatus::Active {
        adopt_starter_turn(context, actor);
        refresh_wall_clock_deadline(context, &next);
    }
    let snapshot = to_snapshot(context, &next);
    let change = GoalChange {
        kind: GoalChangeKind::Lifecycle,
        status: Some(status.as_str().to_string()),
        reason,
        stats: None,
        actor: Some(actor),
    };
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    let snapshot_for_event = snapshot.clone();
    tokio::spawn(async move {
        let _ = dispatch(json!({"type": "goal.updated", "agentId": agent_id, "snapshot": snapshot_for_event, "change": change})).await;
    });
    track_status_changed(context, &next, actor);
    Ok(snapshot)
}

fn track_status_changed(context: &GoalOperationContext, state: &GoalState, actor: GoalActor) {
    context
        .runtime
        .service::<dyn TelemetryService>("telemetryService")
        .track2(
            "goal_status_changed",
            json!({
                "actor": actor,
                "status": state.status,
                "turns_used": state.turns_used,
                "tokens_used": state.tokens_used,
                "wall_clock_ms": live_wall_clock_ms(context, state),
                "has_token_budget": state.budget_limits.token_budget.is_some(),
                "has_turn_budget": state.budget_limits.turn_budget.is_some(),
                "has_wall_clock_budget": state.budget_limits.wall_clock_budget_ms.is_some(),
            }),
        );
}

fn require_state(context: &GoalOperationContext) -> Result<GoalState, String> {
    (context.runtime.get_state)()
        .goal
        .ok_or_else(|| "No current goal".to_string())
}

fn emit_goal_updated(
    context: &GoalOperationContext,
    snapshot: Option<GoalSnapshot>,
    change: Option<GoalChange>,
) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
    let dispatch = context.runtime.dispatch.clone();
    let agent_id = context.runtime.agent.agent_id.clone();
    Box::pin(async move {
        dispatch(goal_updated_event(&agent_id, snapshot, change)).await
    })
}

fn settle_wall_clock(context: &GoalOperationContext, state: &GoalState) -> i64 {
    if state.status == GoalStatus::Active {
        if let Some(started_at) = context.effects.lock().live_wall_clock_started_at {
            return state.wall_clock_ms
                + (deadline_scheduler_of(&context.runtime).now() - started_at).max(0);
        }
    }
    state.wall_clock_ms
}

fn live_wall_clock_ms(context: &GoalOperationContext, state: &GoalState) -> i64 {
    settle_wall_clock(context, state)
}

fn stats_of(context: &GoalOperationContext, state: &GoalState) -> GoalChangeStats {
    GoalChangeStats {
        turns_used: state.turns_used,
        tokens_used: state.tokens_used,
        wall_clock_ms: live_wall_clock_ms(context, state),
    }
}

fn to_snapshot(context: &GoalOperationContext, state: &GoalState) -> GoalSnapshot {
    let wall_clock_ms = live_wall_clock_ms(context, state);
    GoalSnapshot {
        goal_id: state.goal_id.clone(),
        objective: state.objective.clone(),
        completion_criterion: state.completion_criterion.clone(),
        status: state.status,
        turns_used: state.turns_used,
        tokens_used: state.tokens_used,
        input_tokens_used: state.input_tokens_used,
        output_tokens_used: state.output_tokens_used,
        wall_clock_ms,
        budget: compute_budget_report(state, wall_clock_ms),
        terminal_reason: state.terminal_reason.clone(),
        blocked_streak: None,
        created_at: state.created_at,
        updated_at: state.updated_at,
    }
}

fn block_if_budget_reached(
    context: &GoalOperationContext,
    state: &GoalState,
) -> Result<Option<GoalSnapshot>, String> {
    if state.status != GoalStatus::Active {
        return Ok(None);
    }
    let Some(reason) = goal_budget_block_reason(&to_snapshot(context, state).budget) else {
        return Ok(None);
    };
    apply_lifecycle(
        context,
        state,
        GoalStatus::Blocked,
        Some(reason),
        GoalActor::Runtime,
        crate::features::goal::types::LifecycleOptions {
            preserve_live_continuation: true,
            ..Default::default()
        },
    )
    .map(Some)
}

fn refresh_wall_clock_deadline(context: &GoalOperationContext, _state: &GoalState) {
    (context.runtime.send)(XEvent::new("goal.deadline.refresh", Value::Null));
}

fn wall_clock_deadline_delay(context: &GoalOperationContext) -> Option<i64> {
    let state = (context.runtime.get_state)().goal?;
    let budget_ms = state.budget_limits.wall_clock_budget_ms?;
    if state.status != GoalStatus::Active
        || context.effects.lock().live_wall_clock_started_at.is_none()
    {
        return None;
    }
    Some((budget_ms - live_wall_clock_ms(context, &state)).clamp(0, 2_147_483_647))
}

fn handle_wall_clock_deadline(context: &GoalOperationContext) {
    (context.runtime.send)(XEvent::new("goal.deadline.clear", Value::Null));
    let Some(state) = (context.runtime.get_state)().goal else {
        return;
    };
    if state.status != GoalStatus::Active {
        return;
    }
    let Some(budget_ms) = state.budget_limits.wall_clock_budget_ms else {
        return;
    };
    if live_wall_clock_ms(context, &state) < budget_ms {
        refresh_wall_clock_deadline(context, &state);
        return;
    }
    let Some(reason) = goal_budget_block_reason(&to_snapshot(context, &state).budget) else {
        return;
    };
    let cancellation = json!(reason);
    let live_turn_id = context.effects.lock().live_turn_id;
    let pending_turn_id = context
        .effects
        .lock()
        .pending_continuation
        .as_ref()
        .and_then(|pending| pending.turn_id);
    let _ = apply_lifecycle(
        context,
        &state,
        GoalStatus::Blocked,
        Some(reason.clone()),
        GoalActor::Runtime,
        crate::features::goal::types::LifecycleOptions {
            cancellation_reason: Some(cancellation.clone()),
            ..Default::default()
        },
    );
    if let Some(turn_id) = live_turn_id {
        if Some(turn_id) != pending_turn_id {
            context
                .runtime
                .service::<dyn AgentLoopService>("loopService")
                .cancel(json!({"turnId": turn_id}), cancellation);
        }
    }
}

fn compute_budget_report(state: &GoalState, wall_clock_ms: i64) -> GoalBudgetReport {
    let token_budget = state.budget_limits.token_budget;
    let turn_budget = state.budget_limits.turn_budget;
    let wall_clock_budget_ms = state.budget_limits.wall_clock_budget_ms;
    let token_budget_reached = token_budget
        .map(|budget| state.tokens_used >= budget)
        .unwrap_or(false);
    let turn_budget_reached = turn_budget
        .map(|budget| state.turns_used >= budget)
        .unwrap_or(false);
    let wall_clock_budget_reached = wall_clock_budget_ms
        .map(|budget| wall_clock_ms >= budget)
        .unwrap_or(false);
    GoalBudgetReport {
        token_budget,
        turn_budget,
        wall_clock_budget_ms,
        remaining_tokens: token_budget.map(|budget| budget.saturating_sub(state.tokens_used)),
        remaining_turns: turn_budget.map(|budget| budget.saturating_sub(state.turns_used)),
        remaining_wall_clock_ms: wall_clock_budget_ms
            .map(|budget| budget.saturating_sub(wall_clock_ms)),
        token_budget_reached,
        turn_budget_reached,
        wall_clock_budget_reached,
        over_budget: token_budget_reached || turn_budget_reached || wall_clock_budget_reached,
        input_tokens_used: state.input_tokens_used,
        output_tokens_used: state.output_tokens_used,
    }
}

fn matches_goal(state: &GoalState, goal_id: Option<&str>) -> bool {
    goal_id
        .map(|goal_id| state.goal_id == goal_id)
        .unwrap_or(true)
}

fn is_goal_mutation_tool(tool_name: &str) -> bool {
    matches!(tool_name, "CreateGoal" | "UpdateGoal" | "SetGoalBudget")
}

fn goal_budget_block_reason(budget: &GoalBudgetReport) -> Option<String> {
    let mut reached = Vec::new();
    if budget.turn_budget_reached {
        reached.push(
            format!(
                "turn budget {}",
                budget
                    .turn_budget
                    .map(|budget| budget.to_string())
                    .unwrap_or_default()
            )
            .trim()
            .to_string(),
        );
    }
    if budget.token_budget_reached {
        reached.push(
            format!(
                "token budget {}",
                budget
                    .token_budget
                    .map(|budget| budget.to_string())
                    .unwrap_or_default()
            )
            .trim()
            .to_string(),
        );
    }
    if budget.wall_clock_budget_reached {
        reached.push(
            format!(
                "wall-clock budget {}ms",
                budget
                    .wall_clock_budget_ms
                    .map(|budget| budget.to_string())
                    .unwrap_or_default()
            )
            .trim()
            .to_string(),
        );
    }
    if reached.is_empty() {
        None
    } else {
        Some(format!(
            "{}: {}",
            GOAL_BUDGET_BLOCK_PREFIX,
            reached.join(", ")
        ))
    }
}

fn normalize_completion_criterion(value: Option<&str>) -> Option<String> {
    let trimmed = trim_js(value?);
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.encode_utf16().count() > MAX_GOAL_COMPLETION_CRITERION_LENGTH {
        return Some(String::from_utf16_lossy(
            &trimmed
                .encode_utf16()
                .take(MAX_GOAL_COMPLETION_CRITERION_LENGTH)
                .collect::<Vec<_>>(),
        ));
    }
    Some(trimmed.to_string())
}

fn has_step_budget_remaining(max_steps: Option<u64>, current_step: u64) -> bool {
    max_steps
        .map(|max_steps| max_steps == 0 || current_step < max_steps)
        .unwrap_or(true)
}

fn is_terminal_update_goal_result(tool_name: &str, args: &Value, result: &Value) -> bool {
    if tool_name != "UpdateGoal"
        || result.get("isError").and_then(Value::as_bool) == Some(true)
        || result.get("stopTurn").and_then(Value::as_bool) != Some(true)
    {
        return false;
    }
    if !args.is_object() {
        return false;
    }
    matches!(
        args.get("status").and_then(Value::as_str),
        Some("complete") | Some("blocked")
    )
}

fn is_max_steps_turn_failure(result: &Value) -> bool {
    result.get("reason").and_then(Value::as_str) == Some("failed")
        && normalize_goal_error_payload(result.get("error").unwrap_or(&Value::Null))
            .get("code")
            .and_then(Value::as_str)
            == Some(crate::agent::r#loop::errors::LoopErrors::LOOP_MAX_STEPS_EXCEEDED)
}

fn goal_failure_pause_reason(error: &Value) -> String {
    let payload = normalize_goal_error_payload(error);
    match payload.get("code").and_then(Value::as_str) {
        Some(kimi_core::primitives::errors::domains::protocol::PROVIDER_RATE_LIMIT) => {
            GOAL_RATE_LIMIT_PAUSE_REASON.to_string()
        }
        Some(kimi_core::primitives::errors::domains::protocol::PROVIDER_CONNECTION_ERROR) => {
            pause_reason_with_message(
                GOAL_PROVIDER_CONNECTION_PAUSE_PREFIX,
                payload.get("message").and_then(Value::as_str),
            )
        }
        Some(kimi_core::primitives::errors::domains::protocol::PROVIDER_AUTH_ERROR) => {
            pause_reason_with_message(
                GOAL_PROVIDER_AUTH_PAUSE_PREFIX,
                payload.get("message").and_then(Value::as_str),
            )
        }
        Some(kimi_core::primitives::errors::domains::protocol::PROVIDER_FILTERED) => {
            GOAL_PROVIDER_FILTERED_PAUSE_REASON.to_string()
        }
        Some(kimi_core::primitives::errors::domains::protocol::PROVIDER_API_ERROR) => {
            pause_reason_with_message(
                GOAL_PROVIDER_API_PAUSE_PREFIX,
                payload.get("message").and_then(Value::as_str),
            )
        }
        Some(kimi_core::primitives::errors::domains::profile::MODEL_NOT_CONFIGURED) => {
            pause_reason_with_message(GOAL_MODEL_CONFIG_PAUSE_PREFIX, Some(LLM_NOT_SET_MESSAGE))
        }
        Some(kimi_core::primitives::errors::domains::profile::MODEL_CONFIG_INVALID) => {
            pause_reason_with_message(
                GOAL_MODEL_CONFIG_PAUSE_PREFIX,
                payload.get("message").and_then(Value::as_str),
            )
        }
        _ => pause_reason_with_message(
            GOAL_RUNTIME_PAUSE_PREFIX,
            payload.get("message").and_then(Value::as_str),
        ),
    }
}

fn normalize_goal_error_payload(error: &Value) -> Value {
    let mut payload = goal_error_payload_at_depth(error, 0);
    if payload.get("code").and_then(Value::as_str)
        == Some(kimi_core::primitives::errors::domains::profile::MODEL_NOT_CONFIGURED)
    {
        payload["message"] = json!(LLM_NOT_SET_MESSAGE);
    }
    payload
}

fn goal_error_payload_at_depth(error: &Value, depth: usize) -> Value {
    let code = error.get("code").and_then(Value::as_str);
    let mut payload = match code.filter(|code| kimi_core::primitives::errors::is_error_code(code)) {
        Some(code) => {
            let retryable = error
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| kimi_core::primitives::errors::error_info(code).retryable);
            json!({
                "code": code,
                "message": error.get("message").and_then(Value::as_str).unwrap_or_default(),
                "name": error.get("name"),
                "details": error.get("details"),
                "retryable": retryable,
            })
        }
        None => {
            let message = match error {
                Value::String(message) => message.clone(),
                Value::Null => "null".to_string(),
                Value::Object(_) => "[object Object]".to_string(),
                _ => error.to_string(),
            };
            json!({
                "code": kimi_core::primitives::errors::INTERNAL_CODE,
                "message": message,
                "retryable": kimi_core::primitives::errors::error_info(
                    kimi_core::primitives::errors::INTERNAL_CODE,
                )
                .retryable,
            })
        }
    };
    if depth < kimi_core::primitives::errors::MAX_CAUSE_DEPTH
        && let Some(cause) = error.get("cause").filter(|cause| !cause.is_null())
    {
        payload["cause"] = goal_error_payload_at_depth(cause, depth + 1);
    }
    payload
}

fn pause_reason_with_message(prefix: &str, message: Option<&str>) -> String {
    let trimmed = message
        .map(trim_js)
        .filter(|message| !message.is_empty());
    match trimmed {
        Some(message) => format!("{prefix}: {message}"),
        None => prefix.to_string(),
    }
}

fn chargeable_tokens(usage: &Value) -> i64 {
    crate::llm::usage::chargeable_tokens(usage)
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn to_goal_start_review_permission_mode(label: Option<&str>) -> Option<PermissionMode> {
    match label {
        Some("auto") => Some(PermissionMode::Auto),
        Some("yolo") => Some(PermissionMode::Yolo),
        Some("manual") => Some(PermissionMode::Manual),
        _ => None,
    }
}

fn create_goal_effect_handlers(
    runtime: &AgentActorContext<GoalRuntimeState>,
) -> GoalEffectHandlers {
    let context = goal_operation_context(runtime);
    GoalEffectHandlers {
        deadline_delay: {
            let context = context.clone();
            Arc::new(move || wall_clock_deadline_delay(&context))
        },
        deadline_fired: {
            let context = context.clone();
            Arc::new(move || handle_wall_clock_deadline(&context))
        },
        injection: {
            let goal_context = context.clone();
            let wait_context = context.clone();
            GoalInjectionOptions {
                get_goal: Arc::new(move || {
                    get_goal(&goal_context).ok().and_then(|result| result.goal)
                }),
                is_wait_for_enabled: Some(Arc::new(move || is_wait_for_available(&wait_context))),
            }
        },
        normalize: {
            let context = context.clone();
            Arc::new(move || normalize_after_replay(&context))
        },
        closing: {
            let context = context.clone();
            Arc::new(move |agent: AgentContext| {
                if agent != context.runtime.agent {
                    return;
                }
                let Some(state) = (context.runtime.get_state)().goal else {
                    return;
                };
                if state.status != GoalStatus::Active {
                    return;
                }
                let _ = apply_lifecycle(
                    &context,
                    &state,
                    GoalStatus::Paused,
                    Some("Paused after agent closed".into()),
                    GoalActor::Runtime,
                    Default::default(),
                );
            })
        },
        turn_started: {
            let context = context.clone();
            Arc::new(move |event: Value| {
                let turn_id = event.get("turnId").and_then(Value::as_u64).unwrap_or(0);
                let origin = event.get("origin").cloned().unwrap_or(Value::Null);
                handle_turn_launched(&context, turn_id, &origin);
            })
        },
        usage_recorded: {
            let context = context.clone();
            Arc::new(move |usage: Value| {
                handle_usage_recorded(&context, &usage);
            })
        },
        before_step: {
            let context = context.clone();
            Arc::new(move |step: Value| {
                let turn_id = step.get("turnId").and_then(Value::as_u64).unwrap_or(0);
                let context = context.clone();
                Box::pin(async move {
                    handle_before_step(&context, turn_id).await;
                }) as crate::rpc::types::BoxFuture<'static, ()>
            })
        },
        after_step: {
            let context = context.clone();
            Arc::new(move |step: Value| {
                handle_after_step(&context, step);
            })
        },
        approval: {
            let context = context.clone();
            Arc::new(move |event| {
                let context = context.clone();
                Box::pin(async move {
                    let permission_mode = context
                        .runtime
                        .service::<AgentPermissionModeService>("permissionModeService");
                    let should_request = {
                        let event = event.lock();
                        let tool_name = event
                            .get("toolCall")
                            .and_then(|call| call.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        tool_name == "CreateGoal"
                            && permission_mode.mode() != PermissionMode::Auto
                            && event
                                .get("execution")
                                .and_then(|execution| execution.get("display"))
                                .and_then(|display| display.get("kind"))
                                .and_then(Value::as_str)
                                == Some("goal_start")
                    };
                    if !should_request {
                        return Ok(());
                    }
                    let resolve_approval = Arc::new(move |approval: Value| {
                        let permission_mode = permission_mode.clone();
                        Box::pin(async move {
                            if approval.get("decision").and_then(Value::as_str) == Some("approved")
                                && let Some(mode) = to_goal_start_review_permission_mode(
                                    approval.get("selectedLabel").and_then(Value::as_str),
                                )
                                && mode != permission_mode.mode()
                            {
                                permission_mode.set_mode(mode);
                            }
                            None
                        })
                            as crate::rpc::types::BoxFuture<'static, Option<Value>>
                    });
                    let approval_service = context
                        .runtime
                        .service::<dyn ToolApprovalService>("toolApprovalService");
                    let decision = approval_service
                        .request_tool_approval(
                            event.clone(),
                            ToolApprovalRequest {
                                result: json!({"kind": "ask"}),
                                resolve_approval: Some(resolve_approval),
                            },
                            "goal-start-review-ask",
                        )
                        .await?;
                    if let Some(fields) = decision.as_ref().and_then(Value::as_object) {
                        let mut event = event.lock();
                        for (key, value) in fields {
                            event[key] = value.clone();
                        }
                    }
                    Ok(())
                }) as crate::rpc::types::BoxFuture<'static, Result<(), String>>
            })
        },
        veto: {
            let context = context.clone();
            Arc::new(move |event| {
                let context = context.clone();
                Box::pin(async move {
                    let mut event = event.lock();
                    if is_stale_goal_tool_call(&context, &event) {
                        event["veto"] = json!({"output": GOAL_STALE_TOOL_RESULT});
                        return Ok(());
                    }
                    let turn_id = event.get("turnId").and_then(Value::as_u64).unwrap_or(0);
                    if context.effects.lock().budget_grace_turns.contains(&turn_id) {
                        event["veto"] = json!({"output": GOAL_BUDGET_TOOLS_REJECTED_MESSAGE});
                    }
                    Ok(())
                }) as crate::rpc::types::BoxFuture<'static, Result<(), String>>
            })
        },
        tool_completed: {
            let context = context.clone();
            Arc::new(move |tool: Value| {
                let turn_id = tool.get("turnId").and_then(Value::as_u64).unwrap_or(0);
                let goal_id = goal_turn_target(&context, turn_id);
                let tool_name = tool
                    .get("toolCall")
                    .and_then(|call| call.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = tool.get("args").cloned().unwrap_or(Value::Null);
                let result = tool.get("result").cloned().unwrap_or(Value::Null);
                if goal_id.is_some() && is_terminal_update_goal_result(tool_name, &args, &result) {
                    context
                        .effects
                        .lock()
                        .goal_outcome_tool_result_turns
                        .insert(turn_id, goal_id.unwrap());
                }
            })
        },
        turn_ended: {
            let context = context.clone();
            Arc::new(move |event: Value| {
                let turn_id = event.get("turnId").and_then(Value::as_u64).unwrap_or(0);
                let goal_id = goal_turn_target(&context, turn_id);
                let context = context.clone();
                tokio::spawn(async move {
                    handle_turn_ended(&context, turn_id, &event).await;
                    let _ = goal_id;
                });
            })
        },
    }
}

#[derive(Clone)]
pub struct GoalEffectHandlers {
    pub deadline_delay: Arc<dyn Fn() -> Option<i64> + Send + Sync>,
    pub deadline_fired: Arc<dyn Fn() + Send + Sync>,
    pub injection: GoalInjectionOptions,
    pub normalize: Arc<dyn Fn() + Send + Sync>,
    pub closing: Arc<dyn Fn(AgentContext) + Send + Sync>,
    pub turn_started: Arc<dyn Fn(Value) + Send + Sync>,
    pub usage_recorded: Arc<dyn Fn(Value) + Send + Sync>,
    pub before_step: Arc<dyn Fn(Value) -> crate::rpc::types::BoxFuture<'static, ()> + Send + Sync>,
    pub after_step: Arc<dyn Fn(Value) + Send + Sync>,
    pub approval: crate::agent::actor_service::services::BeforeToolExecuteHandler,
    pub veto: crate::agent::actor_service::services::BeforeToolExecuteHandler,
    pub tool_completed: Arc<dyn Fn(Value) + Send + Sync>,
    pub turn_ended: Arc<dyn Fn(Value) + Send + Sync>,
}

fn goal_effects() -> ActorLogic {
    from_callback(|input| {
        let runtime = input
            .runtime
            .downcast_ref::<AgentActorContext<GoalRuntimeState>>()
            .unwrap()
            .clone();
        let handlers = create_goal_effect_handlers(&runtime);
        let deadline = Arc::new(Mutex::new(
            None::<crate::features::goal::goalDeadlineScheduler::DeadlineHandle>,
        ));
        let receive = {
            let runtime = runtime.clone();
            let handlers = handlers.clone();
            let deadline = deadline.clone();
            Arc::new(move |event: XEvent| {
                deadline.lock().take();
                if event.type_ == "goal.deadline.refresh" {
                    if let Some(delay) = (handlers.deadline_delay)() {
                        *deadline.lock() = Some(
                            deadline_scheduler_of(&runtime)
                                .schedule(delay, handlers.deadline_fired.clone()),
                        );
                    }
                }
            })
        };
        (input.receive)(receive);
        let mut disposables: Vec<Box<dyn FnOnce() + Send>> = vec![Box::new(move || {
            deadline.lock().take();
        })];
        if runtime.agent.agent_id == MAIN_AGENT_ID {
            disposables.push(
                runtime
                    .service::<dyn AgentLifecycleService>("agentLifecycleService")
                    .on_will_close(handlers.closing.clone()),
            );
            let injection = GoalInjection::new(
                handlers.injection.clone(),
                reminder_of(&runtime),
                runtime.service::<dyn IAgentContextMemoryService>("contextMemoryService"),
            );
            disposables.push(Box::new(move || injection.dispose()));
            disposables.push(
                runtime
                    .service::<dyn EventBusService>("eventBus")
                    .subscribe("turn.started", handlers.turn_started.clone()),
            );
            disposables.push(
                runtime
                    .service::<dyn SessionUsageService>("sessionUsageService")
                    .on_did_record(handlers.usage_recorded.clone()),
            );
            let loop_service = runtime.service::<dyn AgentLoopService>("loopService");
            let loop_hooks = loop_service.hooks();
            disposables.push(loop_hooks.on_will_begin_step.register(
                "goal-count-turn",
                Arc::new(move |step, next| {
                    let before = handlers.before_step.clone();
                    Box::pin(async move {
                        before(step.value).await;
                        next().await;
                    })
                }),
            ));
            disposables.push(loop_hooks.on_did_finish_step.register(
                "goal-outcome-continuation",
                Arc::new(move |step, next| {
                    let after = handlers.after_step.clone();
                    Box::pin(async move {
                        after(step.value);
                        next().await;
                    })
                }),
            ));
            let tools = runtime.service::<dyn ToolExecutorService>("toolExecutorService");
            disposables.push(tools.on_before_execute_tool(handlers.approval.clone()));
            disposables.push(tools.on_before_execute_tool(handlers.veto.clone()));
            disposables.push(tools.hooks().on_did_execute_tool.register(
                "goal-outcome-tool-result",
                Arc::new(move |tool, next| {
                    let completed = handlers.tool_completed.clone();
                    Box::pin(async move {
                        completed(tool.value);
                        next().await;
                    })
                }),
            ));
            disposables.push(
                runtime
                    .service::<dyn EventBusService>("eventBus")
                    .subscribe("turn.ended", handlers.turn_ended.clone()),
            );
            (handlers.normalize)();
        }
        if let Some(restore) = input.restore {
            restore.wait_until(Box::pin(async {}));
        }
        Box::new(move || {
            for disposable in disposables.into_iter().rev() {
                disposable();
            }
        })
    })
}

pub fn goal_actor_logic() -> Machine {
    Machine {
        context: Arc::new(|input| {
            Arc::new(Mutex::new(GoalActorContext {
                durable: GoalRuntimeState::default(),
                effects: Arc::new(Mutex::new(GoalEffectState::default())),
                runtime: input,
            }))
        }),
        initial: "beforeRestore".to_string(),
        states: std::collections::HashMap::from([
            (
                "beforeRestore".to_string(),
                StateNode {
                    final_: false,
                    on: std::collections::HashMap::from([(
                        "runtime.restore".to_string(),
                        vec![Transition {
                            target: Some("active".to_string()),
                            actions: Vec::new(),
                            ..Default::default()
                        }],
                    )]),
                    invoke: None,
                },
            ),
            (
                "active".to_string(),
                StateNode {
                    final_: false,
                    on: std::collections::HashMap::from([
                        (
                            "goal.deadline.refresh".to_string(),
                            vec![Transition {
                                target: None,
                                actions: vec![send_to("goalEffects", |args| args.event.clone())],
                                ..Default::default()
                            }],
                        ),
                        (
                            "goal.deadline.clear".to_string(),
                            vec![Transition {
                                target: None,
                                actions: vec![send_to("goalEffects", |args| args.event.clone())],
                                ..Default::default()
                            }],
                        ),
                    ]),
                    invoke: Some(InvokeDef {
                        id: "goalEffects".to_string(),
                        src: "goalEffects".to_string(),
                        input: Arc::new(|args| {
                            let context = args
                                .context
                                .downcast_ref::<Mutex<GoalActorContext>>()
                                .unwrap()
                                .lock();
                            InvokeInput {
                                runtime: context.runtime.clone(),
                                restore: args.event.restore.clone(),
                            }
                        }),
                        on_done: Vec::new(),
                        on_error: Vec::new(),
                    }),
                },
            ),
        ]),
        on: std::collections::HashMap::from([(
            "goal.commit".to_string(),
            vec![Transition {
                target: None,
                actions: vec![Action::Custom(Arc::new(|context, args| {
                    let context = context.downcast_ref::<Mutex<GoalActorContext>>().unwrap();
                    context.lock().durable =
                        serde_json::from_value(args.event.payload.clone()).unwrap_or_default();
                }))],
                ..Default::default()
            }],
        )]),
        actors: std::collections::HashMap::from([("goalEffects".to_string(), goal_effects())]),
        ..Default::default()
    }
}

pub struct AgentGoalService {
    base: AgentActorService,
    actor: AgentActorContext<GoalRuntimeState>,
}

impl AgentGoalService {
    pub fn new(
        dispatcher: Arc<dyn AgentActorHost>,
        agent: AgentContext,
        get: Arc<dyn Fn(&str) -> Arc<dyn std::any::Any + Send + Sync> + Send + Sync>,
        dispatch: Arc<
            dyn Fn(Value) -> crate::rpc::types::BoxFuture<'static, Result<(), String>>
                + Send
                + Sync,
        >,
    ) -> Self {
        let base = AgentActorService::new(dispatcher, agent, get, dispatch);
        let actor = base.attach_actor(
            goal_actor_logic(),
            AgentActorOptions {
                id: "goal".to_string(),
                input: None,
                durable: Some(AgentActorDurable {
                    events: vec![
                        "goal.create".to_string(),
                        "goal.update".to_string(),
                        "goal.clear".to_string(),
                        "session.forked".to_string(),
                        "context.append_message".to_string(),
                    ],
                    undoable: false,
                    transition: goal_transition(),
                    read: Arc::new(|context| {
                        context
                            .downcast_ref::<Mutex<GoalActorContext>>()
                            .unwrap()
                            .lock()
                            .durable
                            .clone()
                    }),
                    commit: Arc::new(|actor, durable, _| {
                        actor.send(XEvent::new(
                            "goal.commit",
                            serde_json::to_value(&durable).unwrap_or_default(),
                        ));
                    }),
                }),
            },
        );
        Self { base, actor }
    }

    pub fn get_goal(&self) -> Result<GoalToolResult, String> {
        get_goal(&goal_operation_context(&self.actor))
    }

    pub fn is_goal_tool_target(&self, turn_id: u64, goal_id: &str) -> Result<bool, String> {
        is_goal_tool_target(&goal_operation_context(&self.actor), turn_id, goal_id)
    }

    pub fn is_objective_edited_since_turn_start(&self, turn_id: u64) -> Result<bool, String> {
        is_objective_edited_since_turn_start(&goal_operation_context(&self.actor), turn_id)
    }

    pub async fn create_goal(
        &self,
        input: CreateGoalInput,
        actor: GoalActor,
    ) -> Result<GoalSnapshot, String> {
        create_goal(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn edit_goal_objective(
        &self,
        input: EditGoalObjectiveInput,
        actor: GoalActor,
    ) -> Result<GoalSnapshot, String> {
        edit_goal_objective(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn pause_goal(
        &self,
        input: GoalReasonInput,
        actor: GoalActor,
    ) -> Result<GoalSnapshot, String> {
        pause_goal(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn resume_goal(
        &self,
        input: ResumeGoalInput,
        actor: GoalActor,
    ) -> Result<GoalSnapshot, String> {
        resume_goal(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn set_budget_limits(
        &self,
        input: GoalBudgetLimits,
        actor: GoalActor,
    ) -> Result<GoalSnapshot, String> {
        set_budget_limits(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn cancel_goal(
        &self,
        input: GoalReasonInput,
        actor: GoalActor,
    ) -> Result<GoalSnapshot, String> {
        cancel_goal(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn mark_blocked(
        &self,
        input: GoalReasonInput,
        actor: GoalActor,
    ) -> Result<Option<GoalSnapshot>, String> {
        mark_blocked(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn mark_complete(
        &self,
        input: GoalReasonInput,
        actor: GoalActor,
    ) -> Result<Option<GoalSnapshot>, String> {
        mark_complete(&goal_operation_context(&self.actor), input, actor).await
    }

    pub async fn pause_on_interrupt(
        &self,
        input: GoalReasonInput,
    ) -> Result<Option<GoalSnapshot>, String> {
        pause_on_interrupt(&goal_operation_context(&self.actor), input).await
    }

    pub async fn record_token_usage(
        &self,
        token_delta: i64,
    ) -> Result<Option<GoalSnapshot>, String> {
        record_token_usage(&goal_operation_context(&self.actor), token_delta).await
    }

    pub async fn increment_turn(&self) -> Result<Option<GoalSnapshot>, String> {
        increment_turn(&goal_operation_context(&self.actor)).await
    }
}

impl crate::agent::mode_mutex::mode_mutex_service_impl::GoalControl for AgentGoalService {
    fn is_active(&self) -> bool {
        (self.actor.get_state)()
            .goal
            .is_some_and(|goal| goal.status == GoalStatus::Active)
    }

    fn pause_goal<'a>(
        &'a self,
        reason: &'a str,
        source: &'a str,
    ) -> crate::rpc::types::BoxFuture<'a, Result<(), String>> {
        let actor = match source {
            "user" => GoalActor::User,
            "model" => GoalActor::Model,
            "runtime" => GoalActor::Runtime,
            _ => GoalActor::System,
        };
        Box::pin(async move {
            AgentGoalService::pause_goal(
                self,
                GoalReasonInput {
                    reason: Some(reason.to_string()),
                },
                actor,
            )
            .await
            .map(|_| ())
        })
    }
}

fn goal_transition() -> StateFold<GoalRuntimeState> {
    Arc::new(|mut state, event| {
        match event.type_.as_str() {
            "goal.create" => {
                state.goal = Some(GoalState {
                    goal_id: event
                        .payload
                        .get("goalId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    objective: event
                        .payload
                        .get("objective")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    completion_criterion: event
                        .payload
                        .get("completionCriterion")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    status: GoalStatus::Active,
                    turns_used: 0,
                    tokens_used: 0,
                    input_tokens_used: 0,
                    output_tokens_used: 0,
                    wall_clock_ms: 0,
                    wall_clock_resumed_at: event
                        .payload
                        .get("wallClockResumedAt")
                        .and_then(Value::as_i64),
                    budget_limits: GoalBudgetLimits::default(),
                    terminal_reason: None,
                    created_at: event
                        .payload
                        .get("createdAt")
                        .and_then(Value::as_i64)
                        .or_else(|| {
                            event
                                .payload
                                .get("wallClockResumedAt")
                                .and_then(Value::as_i64)
                        })
                        .unwrap_or(0),
                    updated_at: event
                        .payload
                        .get("updatedAt")
                        .and_then(Value::as_i64)
                        .or_else(|| event.payload.get("createdAt").and_then(Value::as_i64))
                        .unwrap_or(0),
                });
                state.fork_notice.goal_present = true;
            }
            "goal.update" => {
                if let Some(goal) = state.goal.as_mut() {
                    let mut changed = false;
                    if let Some(objective) = event.payload.get("objective").and_then(Value::as_str)
                    {
                        if objective != goal.objective {
                            goal.objective = objective.to_string();
                            changed = true;
                        }
                    }
                    if let Some(status) = event.payload.get("status").and_then(Value::as_str) {
                        let status = GoalStatus::from(status);
                        if status != goal.status {
                            goal.status = status;
                            goal.terminal_reason = if status == GoalStatus::Active {
                                None
                            } else {
                                event
                                    .payload
                                    .get("reason")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            };
                            goal.wall_clock_resumed_at = if status == GoalStatus::Active {
                                event
                                    .payload
                                    .get("wallClockResumedAt")
                                    .and_then(Value::as_i64)
                            } else {
                                None
                            };
                            changed = true;
                        }
                    }
                    for (key, target) in [
                        ("turnsUsed", &mut goal.turns_used),
                        ("tokensUsed", &mut goal.tokens_used),
                        ("inputTokensUsed", &mut goal.input_tokens_used),
                        ("outputTokensUsed", &mut goal.output_tokens_used),
                    ] {
                        if let Some(value) = event.payload.get(key).and_then(Value::as_u64) {
                            if *target != value {
                                *target = value;
                                changed = true;
                            }
                        }
                    }
                    if let Some(value) = event.payload.get("wallClockMs").and_then(Value::as_i64) {
                        if goal.wall_clock_ms != value {
                            goal.wall_clock_ms = value;
                            changed = true;
                        }
                    }
                    if let Some(value) = event
                        .payload
                        .get("wallClockResumedAt")
                        .and_then(Value::as_i64)
                    {
                        if (event
                            .payload
                            .get("status")
                            .and_then(Value::as_str)
                            .map(GoalStatus::from)
                            .unwrap_or(goal.status))
                            == GoalStatus::Active
                            && goal.wall_clock_resumed_at != Some(value)
                        {
                            goal.wall_clock_resumed_at = Some(value);
                            changed = true;
                        }
                    }
                    if let Some(value) = event.payload.get("budgetLimits") {
                        let limits: GoalBudgetLimits =
                            serde_json::from_value(value.clone()).unwrap_or_default();
                        if goal.budget_limits != limits {
                            goal.budget_limits = limits;
                            changed = true;
                        }
                    }
                    if changed {
                        goal.updated_at = event
                            .payload
                            .get("updatedAt")
                            .and_then(Value::as_i64)
                            .unwrap_or_else(now_ms);
                    }
                }
            }
            "goal.clear" => {
                state.goal = None;
                state.fork_notice.goal_present = false;
            }
            "session.forked" => {
                state.goal = None;
                state.fork_notice.reminder_pending =
                    state.fork_notice.goal_present || state.fork_notice.reminder_pending;
                state.fork_notice.goal_present = false;
            }
            "context.append_message" => {
                if state.fork_notice.reminder_pending {
                    if let Some(message) = event.payload.get("message") {
                        if is_goal_fork_cleared_reminder(message) {
                            state.fork_notice.reminder_pending = false;
                        }
                    }
                }
            }
            _ => {}
        }
        state
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_criterion_truncates_by_characters_not_bytes() {
        let long = "界".repeat(MAX_GOAL_COMPLETION_CRITERION_LENGTH + 10);
        let trimmed = normalize_completion_criterion(Some(long.as_str())).unwrap();
        assert_eq!(trimmed.chars().count(), MAX_GOAL_COMPLETION_CRITERION_LENGTH);
        assert!(trimmed.chars().all(|character| character == '界'));
    }

    #[test]
    fn completion_criterion_trims_and_drops_empty() {
        assert_eq!(normalize_completion_criterion(Some("   ")), None);
        assert_eq!(normalize_completion_criterion(Some(" ok ")).as_deref(), Some("ok"));
        assert_eq!(normalize_completion_criterion(None), None);
    }
}
