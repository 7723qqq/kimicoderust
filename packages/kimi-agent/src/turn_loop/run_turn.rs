//! Event-driven, stateless turn loop with resource-conflict-aware tool
//! scheduling.
//!
//! Flow per step:
//!   1. Call the LLM with the current message history + tool definitions.
//!   2. If the response has tool calls, schedule them via
//!      [`tool_scheduler::schedule_tool_calls`]: calls whose declared
//!      resource accesses conflict run in separate batches (serial),
//!      non-conflicting calls run concurrently within a batch.
//!   3. Append tool results to the message history.
//!   4. Continue to the next step, or finish when the LLM stops.
//!
//! Tooling is delegated to the JS host through [`HostCallbacks`]; this
//! module only drives control flow and applies conflict scheduling.

use std::borrow::Cow;
use std::sync::Arc;

use super::retry::RetryConfig;
use super::tool_result_render;
use super::tool_scheduler::{self, ScheduledToolCall};
use super::turn_step::execute_loop_step_with_retry;
use super::types::*;
use super::wall_time;
use crate::callbacks::HostCallbacks;
use crate::llm::files_upload::UploadError;
use crate::llm::media_budget::{MEDIA_BUDGET_EXCEEDED_CODE, MediaBudget};
use crate::llm::media_resolver::{MediaResolver, ResolvedRequest, has_media_refs, inline_entries};
use crate::rpc::types::{BoxFuture, TokenUsage, ToolExecuteRequest};

/// Goal/plan state snapshot backed by the host callbacks.
///
/// The injection providers render synchronously, while the state authority is
/// behind the async `host/state_read` channel — so the turn loop refreshes
/// this snapshot at each step head (see the injection pass) and the providers
/// read the cached values. A failed read keeps the previous value, so a host
/// without the state bridge degrades to "no goal/plan reminder" instead of
/// erroring the turn.
#[derive(Default)]
struct CallbackStateSnapshot {
    goal: std::sync::Mutex<Option<serde_json::Value>>,
    plan: std::sync::Mutex<Option<serde_json::Value>>,
}

impl CallbackStateSnapshot {
    async fn refresh(&self, callbacks: &dyn HostCallbacks) {
        // The two domains are independent host round-trips; issue them
        // concurrently so the step pays one latency instead of two.
        let read = |domain: &str| {
            callbacks.state_read(crate::rpc::types::StateReadRequest {
                domain: domain.to_string(),
                key: domain.to_string(),
                turn_id: String::new(),
                tool_call_id: String::new(),
            })
        };
        let (goal, plan) = futures_util::future::join(read("goal"), read("plan")).await;
        *self.goal.lock().unwrap_or_else(|e| e.into_inner()) = goal.ok().map(|r| r.value);
        *self.plan.lock().unwrap_or_else(|e| e.into_inner()) = plan.ok().map(|r| r.value);
    }
}

impl crate::injection::DomainValueSource for CallbackStateSnapshot {
    fn read_domain(&self, domain: &str) -> Option<serde_json::Value> {
        let slot = match domain {
            "goal" => &self.goal,
            "plan" => &self.plan,
            _ => return None,
        };
        slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Incremental estimate of the current history's token cost.
///
/// The per-step compaction check needs the history's token count; rescanning
/// (and re-serializing every tool call in) the whole history each step is
/// O(n²) over a turn. Between compactions the injection-free history only
/// grows, so this caches the running total and adds only the newly appended
/// tail. Any in-place prefix rewrite (goal steering, compaction) invalidates
/// the cache so the next `sync` recomputes from scratch.
#[derive(Default)]
struct ContextTokens {
    tokens: u32,
    len: usize,
    stale: bool,
}

impl ContextTokens {
    /// Re-sync against the current injection-free history.
    fn sync(&mut self, messages: &[LLMMessage]) {
        if self.stale || messages.len() < self.len {
            self.tokens = crate::compaction::estimate_messages_tokens(messages);
            self.len = messages.len();
            self.stale = false;
        } else if messages.len() > self.len {
            self.tokens += crate::compaction::estimate_messages_tokens(&messages[self.len..]);
            self.len = messages.len();
        }
    }

    fn tokens(&self) -> u32 {
        self.tokens
    }

    /// The cached prefix was rewritten in place; force a full recompute.
    fn invalidate(&mut self) {
        self.stale = true;
    }
}

/// Build a [`TurnResult`] with the turn's telemetry counters.
///
/// `events_emitted`, `llm_transport` and `native_tool_calls` are left empty
/// here and filled by the composition root (see
/// [`crate::callbacks::CountingCallbacks`] and
/// [`crate::callbacks::NativeToolCallbacks`]); the LLM retry count is
/// accumulated in this loop from per-step attempt figures.
fn turn_result(
    stop_reason: LoopTurnStopReason,
    steps: u32,
    usage: TokenUsage,
    events_emitted: u32,
    llm_retries: u32,
    messages: Vec<LLMMessage>,
) -> TurnResult {
    TurnResult {
        stop_reason,
        steps,
        usage,
        events_emitted,
        llm_retries,
        llm_transport: String::new(),
        native_tool_calls: 0,
        messages,
        stop_hook_continuation: None,
    }
}

/// v2 `turn.step.started` (loopService.ts:1393-1408): the step boundary belongs
/// to the turn loop, which is the only layer that can name the turn and the
/// step. Call it once per step, after the budget/cancel/compaction guards and
/// outside the overflow-recovery loop, so every `started` is paired with exactly
/// one [`emit_step_end_event`] — a summarizer or retry request that shares this
/// transport never announces a step of its own.
fn emit_step_begin_event(callbacks: &dyn HostCallbacks, agent_id: &str, turn_id: &str, step: u32) {
    callbacks.emit_event(serde_json::json!({
        "type": "llm.step.begin",
        "agent_id": agent_id,
        "turn_id": turn_id,
        "step": step,
    }));
}

/// The turn loop's addressed copy of v2 `turn.step.completed` (upstream #3938
/// puts the request timing on it): the transcript projector folds this into the
/// step — completion, usage, and the timing breakdown. The transport's own sink
/// emission (llm/http.rs) serves the host's message fold and carries no
/// turn/step id, so it cannot address a step; this one can — and `finish_reason`
/// rides along with it so the step's terminal signal (`max_tokens`, `filtered`)
/// is still readable from the only `llm.step.end` that names a step.
fn emit_step_end_event(
    callbacks: &dyn HostCallbacks,
    agent_id: &str,
    turn_id: &str,
    step: u32,
    result: &StepResult,
) {
    callbacks.emit_event(serde_json::json!({
        "type": "llm.step.end",
        "agent_id": agent_id,
        "turn_id": turn_id,
        "step": step,
        "finish_reason": result.finish_reason.as_deref(),
        "usage": {
            "input_tokens": result.usage.input_tokens,
            "output_tokens": result.usage.output_tokens,
            "total_tokens": result.usage.total_tokens,
            "input_cache_read": result.usage.input_cache_read,
            "input_cache_creation": result.usage.input_cache_creation,
        },
        "timing": serde_json::to_value(&result.timing).unwrap_or(serde_json::Value::Null),
    }));
}

/// Map a provider finish reason onto a turn-level stop reason.
///
/// `length` (OpenAI) / `max_tokens` (Anthropic) mean the response was cut off
/// by the token limit → `MaxTokens`. The filtered family → `Filtered`, and
/// `filtered` is one of its values: that is the one value v2 carries
/// (`human/llm/empty-response.ts` and `retry.ts:50-51` both compare against the
/// literal `'filtered'`, because kosong folds the provider-specific spellings
/// into it), and the host-proxy transport delivers whatever the host produced
/// verbatim. The wider set stays accepted so the native transports' raw
/// provider vocabulary also lands on `Filtered`: `content_filter` (OpenAI),
/// `refusal` (Anthropic), and Google's safety words.
///
/// The filter vocabulary is [`turn_step::is_content_filtered`]'s, and this
/// function delegates to it rather than repeating the list: a second copy is
/// how a filtered turn ends up silently re-requested as an empty one and then
/// completes as a success. The Stop-hook guard below reads the same predicate,
/// so the finish-reason vocabulary is written down exactly once.
///
/// Everything else ends the turn normally.
fn turn_stop_reason_from_finish(finish_reason: Option<&str>) -> LoopTurnStopReason {
    match finish_reason {
        Some("length") | Some("max_tokens") => LoopTurnStopReason::MaxTokens,
        _ if crate::turn_loop::turn_step::is_content_filtered(finish_reason) => {
            LoopTurnStopReason::Filtered
        }
        _ => LoopTurnStopReason::EndTurn,
    }
}

/// Run a single turn with engine-side telemetry emission (M1c).
///
/// Wraps [`run_turn`] and emits `turn_started` / `turn_ended` /
/// `turn_interrupted` through the `host/telemetry` seam, merging the
/// host-injected [`TelemetryContext`] (mode / provider_type / protocol /
/// thinking_effort) with the engine-observed outcome (reason, duration_ms,
/// steps, at_step, interrupt_reason). The host forwards one track2 per event
/// and suppresses its own turn-lifecycle telemetry for engine-driven turns —
/// this is the ownership hand-over for telemetry.
///
/// `trace_id` is a known gap: the engine does not yet capture the provider
/// request id from native LLM responses, and in host-proxy mode the host
/// never sees the value either.
/// Run one turn with the turn-lifecycle telemetry only (v2's loopService
/// owns the same events): `turn_started` before the turn, `turn_ended` and
/// — for anything but a clean completion — `turn_interrupted` after. The
/// goal-domain events (`goal.continuation` / `goal.blocked` / `goal.paused`)
/// stay with [`run_turn_with_telemetry`], whose caller owns the goal
/// follow-up; the session pump drives its own follow-ups and emits only the
/// lifecycle.
pub fn run_turn_with_lifecycle_telemetry<'a>(
    input: RunTurnInput<'a>,
    telemetry: &TelemetryContext,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    // Cloned into the future: the caller's borrow ends with the call, the
    // turn outlives it.
    let telemetry = telemetry.clone();
    let turn_id = input.turn_id.clone();
    callbacks.telemetry(telemetry_payload(
        "turn_started",
        &telemetry,
        &turn_id,
        None,
    ));
    Box::pin(async move {
        let started = std::time::Instant::now();
        // Stop-hook vetoes are consumed transparently inside the turn (see
        // `run_turn_continued`): by the time the result surfaces here the
        // continuation has already run.
        let result = run_turn_continued(input, callbacks).await;
        // v2 #3907: the survey payload carries a per-turn trace id so a rating
        // can be tied to the exact turn. The engine cannot see a provider
        // request id (the transport never surfaces one), so the trace id is the
        // engine's own turn-scoped id — stable, unique per turn, and the same
        // value every `turn_ended` consumer already keys off.
        let trace_id = format!("turn-{turn_id}");
        match &result {
            Ok(result) => {
                let reason = telemetry_reason(&result.stop_reason);
                callbacks.telemetry(telemetry_payload(
                    "turn_ended",
                    &telemetry,
                    &turn_id,
                    Some(serde_json::json!({
                        "reason": reason,
                        "duration_ms": started.elapsed().as_millis() as u64,
                        "steps": result.steps,
                        "trace_id": trace_id,
                    })),
                ));
                if reason != "completed" {
                    callbacks.telemetry(telemetry_payload(
                        "turn_interrupted",
                        &telemetry,
                        &turn_id,
                        Some(serde_json::json!({
                            "at_step": result.steps,
                            "interrupt_reason": telemetry_interrupt_reason(&result.stop_reason),
                        })),
                    ));
                }
            }
            Err(_) => {
                callbacks.telemetry(telemetry_payload(
                    "turn_ended",
                    &telemetry,
                    &turn_id,
                    Some(serde_json::json!({
                        "reason": "failed",
                        "duration_ms": started.elapsed().as_millis() as u64,
                    })),
                ));
                callbacks.telemetry(telemetry_payload(
                    "turn_interrupted",
                    &telemetry,
                    &turn_id,
                    Some(serde_json::json!({ "interrupt_reason": "error" })),
                ));
            }
        }
        result
    })
}

pub fn run_turn_with_telemetry<'a>(
    input: RunTurnInput<'a>,
    telemetry: TelemetryContext,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    // Capture the goal before the move into the turn so the continuation
    // emission below can read it after the turn.
    let goal_for_continuation = input.goal.clone();
    let turn_id = input.turn_id.clone();
    let future = run_turn_with_lifecycle_telemetry(input, &telemetry, callbacks);
    Box::pin(async move {
        let result = future.await;
        if let Ok(result) = &result {
            // Goal-mode continuation (v2 `launchContinuationTurn`):
            // a successful goal turn enqueues another turn so the goal
            // progresses past a single model response. v2 enqueued a
            // `ContinuationStepRequest` on the loop; the engine surfaces
            // the decision as `goal.continuation` telemetry, and the
            // print settle (session/mod.rs) turns it into the follow-up
            // turn itself — no native host re-prompts on the event.
            let reason = telemetry_reason(&result.stop_reason);
            if reason == "completed"
                && let Some(goal) = goal_for_continuation.as_ref()
                && matches!(goal.status, crate::turn_loop::types::GoalStatus::Active)
            {
                let continuation_prompt = crate::native::goal::steering::render_continuation(
                    &goal.objective,
                    goal.tokens_used,
                    goal.token_budget,
                );
                callbacks.telemetry(telemetry_payload(
                    "goal.continuation",
                    &telemetry,
                    &turn_id,
                    Some(serde_json::json!({
                        "goal_id": goal.goal_id,
                        "tokens_used": goal.tokens_used,
                        "token_budget": goal.token_budget,
                        "prompt": continuation_prompt,
                    })),
                ));
            }
            // Goal-mode terminal transitions (v2 `blockIfBudgetReached` +
            // `settleAbnormalTurn`): the engine tells the host to leave
            // `active`. `goal.blocked` covers budget stops so the host
            // transitions the goal to `blocked` with a budget reason;
            // `goal.paused` covers failed / aborted / filtered turns so
            // the host pauses with the classified reason. v2 emits
            // `goal.status_changed` for both (goalOps.ts:1083,
            // `goalFailurePauseReason`).
            if let Some(goal) = goal_for_continuation.as_ref()
                && matches!(goal.status, crate::turn_loop::types::GoalStatus::Active)
            {
                match &result.stop_reason {
                    LoopTurnStopReason::BudgetLimited => {
                        callbacks.telemetry(telemetry_payload(
                            "goal.blocked",
                            &telemetry,
                            &turn_id,
                            Some(serde_json::json!({
                                "goal_id": goal.goal_id,
                                "reason": "budget",
                            })),
                        ));
                    }
                    LoopTurnStopReason::Filtered
                    | LoopTurnStopReason::Aborted
                    | LoopTurnStopReason::MaxSteps
                    | LoopTurnStopReason::Unknown => {
                        callbacks.telemetry(telemetry_payload(
                            "goal.paused",
                            &telemetry,
                            &turn_id,
                            Some(serde_json::json!({
                                "goal_id": goal.goal_id,
                                "reason": telemetry_interrupt_reason(&result.stop_reason),
                            })),
                        ));
                    }
                    _ => {}
                }
            }
        }
        result
    })
}

/// The turn's submitted prompt for `UserPromptSubmit` hooks (v2
/// `agentExternalHooksService.notifyUserPromptSubmit`): the latest
/// user-role message, mirroring what the model will see this turn. Empty
/// when the turn carries no user message (the hook side treats that as
/// observe-only).
fn latest_user_text(user_messages: &[LLMMessage]) -> String {
    user_messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Drop the rebuilt system message (index 0) before a Stop-hook
/// continuation pass; `run_turn` rebuilds it.
fn strip_rebuilt_system_message(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    debug_assert!(
        messages.first().is_none_or(|m| m.role == "system"),
        "turn head must be the rebuilt system message"
    );
    messages.into_iter().skip(1).collect()
}

/// The messages one request carries, after media resolution and the media
/// budget have had their say, with the omission warning surfaced as a
/// `WarningEvent` (protocol `events.ts`) — the channel the host already
/// renders turn warnings on.
///
/// `Err` carries a rejected upload credential and nothing else: v2 throws it
/// out of the resolver (`isMediaUploadAuthError`), so the turn fails with the
/// upload's own error instead of sending a degraded request.
async fn budgeted_request<'a>(
    budget: &mut MediaBudget,
    resolver: Option<&MediaResolver>,
    llm: &dyn LLM,
    messages: &'a [LLMMessage],
    callbacks: &dyn HostCallbacks,
) -> Result<Cow<'a, [LLMMessage]>, UploadError> {
    let target = llm.media_target();
    let mut request = match resolver {
        Some(resolver) => {
            // The credential is only worth a host round-trip when this
            // request actually carries a reference the provider would take
            // by value.
            let credential = match (target.as_ref(), has_media_refs(messages)) {
                (Some(target), true) if target.uploads_media => {
                    match llm.media_upload_credential() {
                        Some(future) => future.await.ok(),
                        None => None,
                    }
                }
                _ => None,
            };
            resolver
                .resolve(messages, target.as_ref(), credential.as_deref())
                .await?
        }
        None => ResolvedRequest {
            messages: Cow::Borrowed(messages),
            entries: inline_entries(messages),
        },
    };
    if let Some(message) = budget.apply(&mut request.messages, &request.entries) {
        callbacks.emit_event(serde_json::json!({
            "type": "warning",
            "code": MEDIA_BUDGET_EXCEEDED_CODE,
            "message": message,
        }));
    }
    Ok(request.messages)
}

/// Run a turn to completion, transparently consuming at most one Stop-hook
/// continuation inside it (v2 `runStopHooks` + `stopHookContinuationUsed`).
///
/// When a text-finished iteration trips a matching `Stop` hook, the veto
/// text is appended as a user message and the loop runs again under the
/// same turn id; the guard is disarmed after the first iteration, so a
/// turn continues at most once no matter what the hook keeps saying —
/// exactly v2's once-per-turn bound. Stale `<system-reminder>` injections
/// are stripped between iterations (the next pass re-derives them); the
/// system prompt is rebuilt by `run_turn` itself.
///
/// Built-in call sites (session pump, standalone server, one-shot entries)
/// all go through here, so a veto never leaks out as an un-actioned result.
/// The consumed continuation is invisible in `TurnResult` (its
/// `stop_hook_continuation` is always `None` on return); direct `run_turn`
/// users who drive the loop themselves read that field and re-prompt.
pub fn run_turn_continued<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    let RunTurnInput {
        agent_id,
        turn_id,
        llm,
        messages,
        tools,
        tool_defs,
        max_steps,
        max_attempts,
        max_context_tokens,
        compaction_max_attempts,
        goal,
        cancellation,
        permission_mode,
        hook_guard,
        media,
        media_dropped,
        toolset,
        previous_turn_aborted,
    } = input;
    Box::pin(async move {
        let mut messages = messages;
        // v2 arms the Stop dispatch once per turn: only the first iteration
        // carries the guard.
        let mut hook_guard = hook_guard;
        // A continuation is a further step of the same turn (v2 enqueues a
        // `stop_hook` ContinuationStepRequest on the loop), so steps, token
        // usage and retry counts accumulate across iterations instead of
        // resetting — otherwise token budgets would under-count.
        let mut steps = 0u32;
        let mut usage = crate::rpc::types::TokenUsage::default();
        let mut llm_retries = 0u32;
        // The interruption reminder is a fresh-turn-head concern, and the
        // first iteration of this loop *is* that head: it carries the caller's
        // flag. A Stop-hook continuation is the same turn, so later iterations
        // clear it instead of re-announcing the reminder.
        let mut carry_previous_turn_aborted = previous_turn_aborted;
        loop {
            let iter_input = RunTurnInput {
                agent_id: agent_id.clone(),
                turn_id: turn_id.clone(),
                llm,
                messages,
                tools,
                tool_defs: tool_defs.clone(),
                max_steps,
                max_attempts,
                max_context_tokens,
                compaction_max_attempts,
                goal: goal.clone(),
                cancellation: cancellation.clone(),
                permission_mode,
                hook_guard: hook_guard.take(),
                media,
                media_dropped: media_dropped.clone(),
                toolset: toolset.clone(),
                // The continuation is the same turn, not a new one: the
                // interruption reminder only fires at a fresh turn's head.
                previous_turn_aborted: carry_previous_turn_aborted,
            };
            let mut result = match run_turn(iter_input, callbacks).await {
                Ok(result) => result,
                Err(error) => {
                    // v2 closes the mode from a `TurnEnded` subscription, so it
                    // fires however the turn ended. The normal end below is the
                    // only other exit, and this is the one that would otherwise
                    // leave a tool-opened swarm latched.
                    crate::swarm::mode::exit_tool_swarm_at_turn_end(callbacks.as_ref(), &agent_id);
                    return Err(error);
                }
            };
            carry_previous_turn_aborted = false;
            steps += result.steps;
            usage.accumulate(&result.usage);
            llm_retries += result.llm_retries;
            // `take` (not `clone`): the consumed continuation never leaks
            // back out — see the `None` arm below.
            match result.stop_hook_continuation.take() {
                Some(text) => {
                    // The next pass re-derives the system message; each
                    // continuation iteration gets a fresh step window under
                    // the same turn id (v2 re-arms the loop the same way).
                    // Reminders stay where they were appended — dropping them
                    // here would shift the indices the caller folds the turn
                    // result by, and the step-head pass re-derives whatever
                    // the next iteration needs.
                    messages = strip_rebuilt_system_message(result.messages);
                    messages.push(LLMMessage::user(&text));
                }
                None => {
                    result.steps = steps;
                    result.usage = usage;
                    result.llm_retries = llm_retries;
                    result.stop_hook_continuation = None;
                    // v2 `AgentSwarmService`'s `TurnEnded` subscription: a
                    // swarm the tool (or a task) opened closes itself here. A
                    // manually opened swarm stays until the user leaves it.
                    // Closing it emits `swarm_mode.exit` plus the status update
                    // the host folds, so the indicator clears and a `task`
                    // swarm renders its "ended" marker.
                    crate::swarm::mode::exit_tool_swarm_at_turn_end(callbacks.as_ref(), &agent_id);
                    return Ok(result);
                }
            }
        }
    })
}

/// Map the stop reason onto v2's `TurnResult.type` telemetry vocabulary
/// (`completed` / `cancelled` / `failed`). `MaxTokens` is a finish reason on
/// a response the model did produce, so the turn completed; `Filtered` fails
/// the turn in v2 (loopService.ts:877-882 throws `PROVIDER_FILTERED`), as
/// does the step-budget exhaustion (`max_steps`, loopService.ts:956-962);
/// `Paused` / `BudgetLimited` surface as normal turn ends with a blocked
/// marker.
fn telemetry_reason(stop: &LoopTurnStopReason) -> &'static str {
    match stop {
        LoopTurnStopReason::EndTurn
        | LoopTurnStopReason::MaxTokens
        | LoopTurnStopReason::Paused
        | LoopTurnStopReason::RepeatBreaker
        | LoopTurnStopReason::BudgetLimited => "completed",
        LoopTurnStopReason::Aborted => "cancelled",
        LoopTurnStopReason::Filtered
        | LoopTurnStopReason::MaxSteps
        | LoopTurnStopReason::Unknown => "failed",
    }
}

/// The engine sees only the cancellation flag, not the host's abort reason —
/// `user_cancelled` vs `aborted` cannot be distinguished here, so every
/// engine-side cancellation reports `aborted` until the reason travels with
/// the cancel request. `max_steps` / `filtered` mirror v2's
/// `interruptReasonFor` mapping (loopService.ts:2548-2559).
fn telemetry_interrupt_reason(stop: &LoopTurnStopReason) -> &'static str {
    match stop {
        LoopTurnStopReason::Aborted => "aborted",
        LoopTurnStopReason::MaxSteps => "max_steps",
        LoopTurnStopReason::Filtered => "filtered",
        _ => "error",
    }
}

fn telemetry_payload(
    event: &str,
    ctx: &TelemetryContext,
    turn_id: &str,
    extra: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "event": event,
        "turn_id": turn_id,
        "mode": ctx.mode,
        "provider_type": ctx.provider_type,
        "protocol": ctx.protocol,
    });
    if let Some(effort) = &ctx.thinking_effort {
        value["thinking_effort"] = serde_json::Value::String(effort.clone());
    }
    if let Some(plugins) = &ctx.enabled_plugins {
        value["enabled_plugins"] = serde_json::Value::String(plugins.clone());
    }
    if let (Some(extra_obj), Some(obj)) = (
        extra.as_ref().and_then(|extra| extra.as_object()),
        value.as_object_mut(),
    ) {
        for (key, val) in extra_obj {
            obj.insert(key.clone(), val.clone());
        }
    }
    value
}

/// Bridges the host's `AtomicBool` cancellation flag onto a
/// [`tokio_util::sync::CancellationToken`] the LLM transport can select on
/// while a request is in flight — the flag alone is only checked at step
/// boundaries, so a cancel arriving mid-stream would otherwise wait for
/// the whole provider response (v2 aborts through the same window via
/// `AbortSignal`, generate.ts:154-202). The polling watcher exits when the
/// flag fires or when this guard drops at any turn exit; the 25ms poll is
/// the mid-stream cancellation latency ceiling.
pub(crate) struct TurnCancellation {
    token: tokio_util::sync::CancellationToken,
    _shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TurnCancellation {
    pub(crate) fn from_flag(flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) -> Self {
        let token = tokio_util::sync::CancellationToken::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        if let Some(flag) = flag {
            let watcher = token.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = rx => {}
                    _ = async {
                        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                        }
                    } => watcher.cancel(),
                }
            });
        }
        Self {
            token,
            _shutdown: Some(tx),
        }
    }

    pub(crate) fn token(&self) -> &tokio_util::sync::CancellationToken {
        &self.token
    }
}
/// Run a single turn.
#[tracing::instrument(name = "run_turn", skip_all, fields(turn_id = %input.turn_id, max_steps = input.max_steps, has_goal = input.goal.is_some()))]

pub fn run_turn<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    let turn_id = input.turn_id.clone();
    let max_steps = input.max_steps.max(1);
    let user_messages = input.messages.clone();
    // Seed the date-change baseline from the conversation history (v2's
    // history-scanned `lastDisclosure`) before `user_messages` moves into the
    // turn body: a prior disclosure suppresses the per-turn baseline
    // re-injection until the date changes.
    let date_baseline = crate::injection::scan_date_baseline(&user_messages);
    // Same for the workspace AGENTS.md reminder: the paths an earlier reminder
    // already named are not suggested again (v2 reads the same set off the
    // last injection's `origin.disclosure`).
    let agents_md_baseline = crate::injection::scan_agents_md_baseline(&user_messages);
    // Same for the permission-mode reminders: the mode the last reminder
    // recorded survives across turns, so a resumed session does not
    // re-announce a mode the model was already told about.
    let permission_mode_baseline =
        crate::injection::permission_mode::scan_permission_mode_baseline(&user_messages);
    // Same for the previous-session task reminder: the ids an earlier
    // reminder already named in this conversation are scanned out of the
    // history, so reconcile reports a loss only once even when the persisted
    // `resumeReminded` marker write was lost (v2
    // `hasPreviousSessionReminder` scans the transcript the same way).
    let previous_session_reminder_baseline =
        crate::storage::scan_previous_session_reminders(&user_messages);
    // The interruption reminder (v2 `interruptionReminderService`): when the
    // previous turn aborted, the history scan keeps a resumed session from
    // re-announcing a reminder that is already in the transcript.
    let interruption_baseline =
        crate::injection::interruption_reminder::scan_interruption_baseline(&user_messages);
    let previous_turn_aborted = input.previous_turn_aborted;
    let tool_defs = input.tool_defs.clone();
    let goal = input.goal.clone();
    let submitted_prompt = latest_user_text(&user_messages);
    let hook_guard = input.hook_guard.clone();
    // Bind this turn to the goal that was active when it started (G-6 #8):
    // the native goal gate vetoes mutation calls once the current goal no
    // longer matches. The default no-op leaves unguarded paths unbound.
    callbacks.set_turn_goal(&turn_id, goal.as_ref().map(|g| g.goal_id.as_str()));

    Box::pin(async move {
        // Fire-and-forget once per turn before the step loop (v2 wires this
        // at the same point); hooks observe but never block the prompt.
        if let Some(ref guard) = hook_guard {
            guard
                .notify_user_prompt_submit(&turn_id, &submitted_prompt)
                .await;
        }
        let mut total_usage = crate::rpc::types::TokenUsage::default();
        let mut steps: u32 = 0;
        // LLM retries performed so far (attempts beyond the first per step);
        // surfaced on the turn result as a telemetry figure. Event counting
        // lives at the composition root, hence the zero here.
        let mut llm_retries: u32 = 0;
        // Provider finish reason of the most recent step, consulted when the
        // turn ends to surface provider-side truncation/filtering.
        let mut last_finish_reason: Option<String> = None;

        // Turn wall-clock anchor for the goal's wall-clock budget.
        let turn_started = std::time::Instant::now();
        let elapsed_wall_clock_ms = |started: std::time::Instant| {
            started.elapsed().as_millis().min(i64::MAX as u128) as i64
        };

        // Build system prompt, optionally enriched with goal steering text.
        let system_prompt = if let Some(ref goal) = goal {
            format!(
                "{}\n\n{}",
                input.llm.system_prompt(),
                render_goal_steering(goal, 0, 0, 0)
            )
        } else {
            input.llm.system_prompt().to_string()
        };
        let mut messages = vec![LLMMessage {
            role: "system".into(),
            content: system_prompt,
            ..Default::default()
        }];
        messages.extend(user_messages);

        // Running token estimate for the injection-free history (see
        // [`ContextTokens`]).
        let mut context_tokens = ContextTokens::default();

        // Retry configuration for LLM calls within this turn. The attempt
        // count is host-configurable (v2 `loopControl.maxAttemptsPerStep`);
        // the backoff shape stays engine-owned.
        let retry_config = RetryConfig {
            max_attempts: input
                .max_attempts
                .unwrap_or(crate::turn_loop::retry::DEFAULT_MAX_RETRY_ATTEMPTS),
            ..RetryConfig::default()
        };

        // Bridge the host's AtomicBool cancellation onto a CancellationToken
        // the LLM transport can select on mid-stream (v2 aborts through the same window via `AbortSignal`,
        // kosong generate.ts:107-295). The watcher shuts down when this
        // local drops at any turn exit.
        let turn_cancel = TurnCancellation::from_flag(input.cancellation.clone());

        // Goal deadline active wake-up & timer (v2 IGoalDeadlineScheduler mirror)
        struct DeadlineGuard(Option<tokio::task::JoinHandle<()>>);
        impl Drop for DeadlineGuard {
            fn drop(&mut self) {
                if let Some(h) = self.0.take() {
                    h.abort();
                }
            }
        }
        let _deadline_guard = DeadlineGuard(if let Some(ref g) = goal {
            if let Some(budget_ms) = g.wall_clock_budget_ms {
                let total_used = g.wall_clock_ms;
                let remaining_ms = (budget_ms - total_used).max(0) as u64;
                let cancel_token = turn_cancel.token().clone();
                let goal_id = g.goal_id.clone();
                let tid = turn_id.clone();
                let cbs = callbacks.clone();
                Some(tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(remaining_ms)).await;
                    cbs.emit_event(serde_json::json!({
                        "type": "goal.budget.limit_reached",
                        "turn_id": tid,
                        "goal_id": goal_id,
                        "budget_type": "wall_clock",
                    }));
                    cancel_token.cancel();
                }))
            } else {
                None
            }
        } else {
            None
        });

        // Context compaction knobs. The window comes from the host's model
        // resolution; without it the budget falls back to the fixed default.
        // The attempt cap rides the same host-resolved context (v2 #3750).
        // v2 `getEffectiveMaxContextTokens`: an overflow observed earlier in
        // this process lowers the window for this model, so the trigger and
        // the recovery gates use the conservative value.
        let effective_window = crate::compaction::effective_max_tokens(
            input.llm.model_name(),
            input.max_context_tokens,
        );
        let mut compaction_config = crate::compaction::config_for_window(effective_window);
        compaction_config.max_attempts = input.compaction_max_attempts;
        // v2 `consecutiveOverflowCompactions` (`resetForTurn`,
        // fullCompactionService.ts:462-466): how many times in a row this turn
        // has compacted in response to an overflow without a step completing.
        let mut consecutive_overflow_compactions: u32 = 0;
        // v2's media projection fallbacks, spent at most once each per turn:
        // degrade the older media, then strip every media part.
        let mut media_degraded = false;
        let mut media_stripped = false;
        // v2's third projection fallback, also spent at most once per turn: a
        // provider that rejects the request's *shape* gets one resend with the
        // strict projection (`llmRequesterService.ts:594-601`).
        let mut structure_strict = false;

        // Turn-level injection registry. The built-in date-change and
        // workspace-AGENTS.md reminders are registered by `with_defaults`;
        // goal/plan-mode providers read the state through the host callbacks
        // (the same channel the state-bridge tools write through), so the
        // reminders track the state authority wherever it lives — the host in
        // the product, the local store in the REPL. No local store is built
        // here: in the product the state lives host-side and a workspace- or
        // home-local directory would be a side effect with no consumer.
        let mut injection_registry =
            crate::injection::InjectionRegistry::with_defaults(date_baseline, agents_md_baseline);
        crate::injection::permission_mode::register_permission_mode_injection(
            &mut injection_registry,
            input.permission_mode,
            permission_mode_baseline,
        );
        // The interruption reminder's baseline was scanned at the top of the
        // function (before `user_messages` moved); register from that result.
        if previous_turn_aborted && !interruption_baseline {
            crate::injection::interruption_reminder::register_interruption_reminder(
                &mut injection_registry,
                true,
                &[],
            );
        }
        let goal_plan_state = Arc::new(CallbackStateSnapshot::default());
        if input.llm.transport() != "host-proxy" {
            crate::injection::goal_plan::register_goal_plan_injections(
                &mut injection_registry,
                goal_plan_state.clone(),
            );
        }
        // The stale-todo reminder (v2 `TodoFeature` -> `todoService` ->
        // `todoReminder`): main agent only, only while `TodoList` is active,
        // and only after the todo feature has been used. v2's `used` flag lives
        // in the todo actor; the engine has no such actor, so a `TodoList` call
        // in the history stands in for it. The state read also doubles as the
        // capability probe: a host without the state bridge cannot persist
        // todo state, so reminding the model to maintain a list that cannot be
        // saved would be worse than silence.
        if input.agent_id == crate::callbacks::MAIN_AGENT_ID
            && input.llm.transport() != "host-proxy"
            && tool_defs
                .iter()
                .any(|def| def.name == crate::injection::todo_reminder::TODO_LIST_TOOL_NAME)
            && crate::injection::todo_reminder::todo_list_used(&messages)
            && let Ok(response) = callbacks
                .state_read(crate::rpc::types::StateReadRequest {
                    domain: "todo".into(),
                    key: "todo".into(),
                    turn_id: String::new(),
                    tool_call_id: String::new(),
                })
                .await
        {
            crate::injection::todo_reminder::register_todo_reminder(
                &mut injection_registry,
                response.value,
            );
        }
        // Swarm mode is a mode like plan mode, so it announces itself the same
        // way (v2 `SwarmInjection`): the `swarm_mode` variant injects an
        // enter/exit reminder when the mode changes.
        crate::injection::swarm_mode::register_swarm_mode_injection(
            &mut injection_registry,
            input.agent_id.clone(),
            crate::injection::swarm_mode::scan_swarm_mode_baseline(&messages),
        );
        // Progressive tool disclosure (v2 `toolSelectAnnouncementsService`):
        // every new turn re-announces the deferred tool set — added names the
        // model has not seen yet, removed names whose servers went away. The
        // provider is turn-gated (`is_new_turn`), matching v2's variant gate.
        if let Some(toolset) = &input.toolset {
            let disclosure_toolset = toolset.clone();
            injection_registry.register(
                "tool_select",
                Box::new(move |_ctx| disclosure_toolset.take_disclosure_announcement()),
            );
            // Previous-session background tasks (v2 `taskService.reconcile` →
            // `appendPreviousSessionTasksReminder`): a task the last process
            // left mid-flight lost contact with it, so the model is told
            // rather than left to assume it completed. Two gates keep it
            // one-shot: the runner's persisted `resumeReminded` marker, and
            // the ids an earlier reminder already named in this
            // conversation, so registering per turn is safe.
            let reminder_toolset = toolset.clone();
            let announced = previous_session_reminder_baseline;
            injection_registry.register(
                "task_resume_termination",
                Box::new(move |_ctx| reminder_toolset.take_previous_session_reminder(&announced)),
            );
        }

        // Tool-call dedup guard (v2 `toolDedupeService` mirror, G-6 #2):
        // same-step repeats share the original's result instead of executing,
        // cross-step streaks earn escalating reminders, and a 12-repeat
        // streak stops the turn. State is per-turn (v2 resets the streak
        // when the turn id changes), so the guard lives only in this call.
        let mut tool_dedupe = crate::tools::tool_dedupe::DedupeGuard::new();

        // Request media budget (upstream #3784): a request whose images and
        // videos exceed the budget is degraded, not failed. The record of
        // what an earlier request already omitted is seeded from the caller
        // and lives for the whole turn.
        let mut media_budget = match input.media_dropped.clone() {
            Some(shared) => MediaBudget::shared(shared),
            None => MediaBudget::default(),
        };

        // Tool-call-id ledger for this turn (v2 `ToolCallIdNormalizer`, #3734).
        // Seeded from the history this turn starts with: every id already
        // recorded there — restored from disk or assigned by an earlier
        // continuation pass — is taken, so a provider that replays an id gets
        // it disambiguated instead of corrupting the tool-result linkage.
        // Seeding is one-shot, so ids claimed by the steps below are only
        // released by an attempt rollback.
        let tool_call_ids =
            std::sync::Arc::new(crate::turn_loop::tool_call_id::ToolCallIdNormalizer::new());
        tool_call_ids.seed_from(&messages);
        // The transport opens one attempt per request on this ledger, so the
        // fragments it streams carry the ids its finalized calls will carry,
        // and a request that fails releases its claims before the retry re-sends
        // (v2 #3734: the requester is handed the turn's normalizer).
        input
            .llm
            .set_tool_call_ids(std::sync::Arc::clone(&tool_call_ids));
        // Attribute every streamed fragment of this turn to the agent running
        // it (v2 stamps `agentId` from `scopeContext` at event construction —
        // one loopService per agent). The transport is shared across the main
        // agent and all subagents, so the turn re-installs the owner per turn;
        // without it the host's attribution fallback files a subagent's whole
        // stream under the main transcript.
        input.llm.set_stream_agent_id(&input.agent_id);

        // What the *previous* step's tool calls did, carried across the step
        // boundary for the injection pass below. v2's AGENTS.md reminder hooks
        // `onDidExecuteTool` and probes from the project root down to each of
        // these; the injection pass runs at the same observation point (after
        // those calls, before the next LLM call), so this is the only input it
        // was missing. Empty on the first step, which is why the reminder's own
        // root probe still has to run there.
        let mut step_access = crate::injection::StepAccess::default();

        for step_num in 0..max_steps {
            steps = step_num + 1;
            let turn_wall_clock_ms = elapsed_wall_clock_ms(turn_started);

            // ── Goal budget check ──────────────────────────────────────────
            // Before each step, verify the goal is still active and within
            // budget. If the host paused/blocked it or a budget is exhausted,
            // stop the turn immediately.
            if let Some(ref goal) = goal {
                if !goal.status.is_active() {
                    let reason = match goal.status {
                        GoalStatus::Paused => LoopTurnStopReason::Paused,
                        GoalStatus::Blocked => LoopTurnStopReason::Aborted,
                        _ => LoopTurnStopReason::EndTurn,
                    };
                    return Ok(turn_result(
                        reason,
                        step_num, // this step didn't run
                        total_usage,
                        0,
                        llm_retries,
                        messages.clone(),
                    ));
                }
                // Check budgets with cumulative usage so far.
                let turn_tokens = total_usage.total_tokens as i64;
                let turns_this_turn = step_num as i64;
                if goal.would_exceed_budget(turn_tokens, turns_this_turn, turn_wall_clock_ms) {
                    callbacks.emit_event(serde_json::json!({
                        "type": "goal.budget.limit_reached",
                        "turn_id": turn_id,
                        "goal_id": goal.goal_id,
                    }));
                    return Ok(turn_result(
                        LoopTurnStopReason::BudgetLimited,
                        step_num,
                        total_usage,
                        0,
                        llm_retries,
                        messages.clone(),
                    ));
                }
                // Update steering text in system prompt with current progress.
                let steering =
                    render_goal_steering(goal, turn_tokens, turns_this_turn, turn_wall_clock_ms);
                messages[0].content = format!("{}\n\n{}", input.llm.system_prompt(), steering);
                context_tokens.invalidate();
            }

            // ── Cancellation check ────────────────────────────────────────
            // If the host sent a CANCEL_TURN request, the cancellation flag
            // is set. Abort the turn before calling the LLM.
            if let Some(ref cancel) = input.cancellation
                && cancel.load(std::sync::atomic::Ordering::Relaxed)
            {
                return Ok(turn_result(
                    LoopTurnStopReason::Aborted,
                    step_num,
                    total_usage,
                    0,
                    llm_retries,
                    messages.clone(),
                ));
            }

            // Drain mid-turn steering from host when engine owns the history.
            if input.llm.transport() != "host-proxy"
                && let Ok(steers) = callbacks.drain_steers().await
            {
                messages.extend(steers);
            }

            // ── Context compaction check ─────────────────────────────────
            // Before each LLM call, estimate the message history against
            // the context window. When it crosses the trigger threshold,
            // replace the oldest messages with a summary placeholder so
            // the turn can continue instead of failing on a context
            // overflow. Independent of the goal budget check above: the
            // goal budget stops the turn, compaction keeps it running.
            //
            // Per-turn reminders are appended in place and stay in the
            // history (v2 `reminderService.append`), so `messages` is always
            // `[system] + carried history + what this turn appended` — the
            // shape the callers fold the turn result by. Compaction sees them
            // like any other message; v2 re-injects after a compaction splice
            // (`compactionRearmPending`), which the step-head pass below does
            // by re-deriving them.
            context_tokens.sync(&messages);
            // Announce the auto-compaction so the host can render a transcript
            // card (v2 `compaction.started` / `completed` / `cancelled`); the
            // host adds the session id. Guarded on the same trigger the
            // compaction itself uses, so `started` never fires without a
            // matching terminal event — and on the split search, so a history
            // the split cannot cut (see `should_compact_auto`) stays silent
            // instead of announcing a `completed` that folded nothing.
            if crate::compaction::should_compact_auto(
                &messages,
                context_tokens.tokens(),
                &compaction_config,
            ) {
                callbacks.emit_event(serde_json::json!({
                    "type": "compaction.started",
                    "trigger": "auto",
                }));
                // Fire-and-forget pair around each compaction (v2
                // `agentExternalHooksService.notifyPreCompact` / its
                // completion arm), paired with the host events above:
                // `PreCompact` with the pre-trim message count fires while
                // the original history still exists, `PostCompact` only on
                // the success arm below. Hooks observe the trim but never
                // block it.
                if let Some(ref guard) = hook_guard {
                    guard.notify_pre_compact(&turn_id, messages.len()).await;
                }
                // v2 `postProcessSummary` reads the todo state at compaction
                // time; a host without the bridge or without todos degrades
                // to a plain summary.
                let todos = crate::compaction::read_todos_for_summary(callbacks.as_ref()).await;
                match crate::compaction::compact_messages_with_summary_at_report(
                    &messages,
                    context_tokens.tokens(),
                    &compaction_config,
                    input.llm,
                    None,
                    Some(turn_cancel.token()),
                    effective_window,
                    todos,
                )
                .await
                {
                    Ok(Some((compacted, report))) => {
                        callbacks.emit_event(serde_json::json!({
                            "type": "compaction.completed",
                            "result": {
                                "summary": report.summary,
                                "compactedCount": report.compacted_count,
                                "tokensBefore": report.tokens_before,
                                "tokensAfter": report.tokens_after,
                            },
                        }));
                        tracing::debug!(
                            turn_id = %turn_id,
                            step = step_num,
                            before = messages.len(),
                            after = compacted.len(),
                            "compacted turn context before LLM call"
                        );
                        // The completion arm of the pair opened alongside
                        // `compaction.started` above (v2
                        // `agentExternalHooksService` PostCompact): the
                        // trigger and the post-trim token count describe the
                        // trim that just landed.
                        if let Some(ref guard) = hook_guard {
                            guard
                                .notify_post_compact(&turn_id, "auto", report.tokens_after as u64)
                                .await;
                        }
                        context_tokens.invalidate();
                        // The compacted shape already closes its block with
                        // the continuation note (v2 `buildContextCompactionShape`
                        // splices exactly `result.messages`), so nothing is
                        // rearmed here: v2's `compactionRearmPending`
                        // re-injects reminders, which the injection pass below
                        // owns, not a second continuation.
                        messages = compacted;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        callbacks.emit_event(serde_json::json!({
                            "type": "compaction.cancelled",
                        }));
                        return Err(Box::new(error) as Box<dyn std::error::Error + 'a>);
                    }
                }
            }

            // ── Injection pass ───────────────────────────────────────────
            // Mirror v2's `onWillBeginStep` injection gate: build this
            // step's reminders (date change, workspace AGENTS.md, …) and
            // append them right before the LLM call. In host-proxy mode
            // the host owns the transcript and injects itself, so the
            // pass is skipped there to avoid duplicate reminders.
            if input.llm.transport() != "host-proxy" {
                // Refresh the goal/plan snapshot through the host callbacks so
                // the injections render the state the tools just wrote — the
                // same channel, so a mid-turn plan exit or goal pause shows up
                // at this step head. A failed read keeps the previous value.
                goal_plan_state.refresh(callbacks.as_ref()).await;
                // `step_num` is zero-based: the turn's first step is 0, not 1.
                // Passing `step_num == 1` here made every turn-scoped provider
                // (the goal reminder is the only one) inject a step late — so
                // on a normal one-step turn it never fired at all.
                //
                // The AGENTS.md reminder additionally needs the directories the
                // previous step's tool calls touched; without them it saw an
                // empty world and only ever probed the workspace root.
                for text in injection_registry.build_injections_with_history(
                    step_num == 0,
                    &step_access,
                    &messages,
                ) {
                    messages.push(crate::injection::injection_message(text));
                }
            }

            // M1d: pull the host's current tool table before each LLM call so
            // mid-turn registry changes (feature tools, MCP reconnects) reach
            // the model. Host-proxy mode rebuilds tools host-side per call and
            // never consults the engine's table; a host without the seam (or a
            // failed pull) falls back to the turn-start snapshot.
            let step_tool_defs = if input.llm.transport() != "host-proxy" {
                match callbacks.list_tools().await {
                    Ok(response) => response.tools,
                    Err(_) => tool_defs.clone(),
                }
            } else {
                tool_defs.clone()
            };

            // Delegate LLM call (with retry) to turn_step module.
            // Convert the 'static error to the turn's 'a-bounded error type.
            let telemetry = |event: serde_json::Value| callbacks.telemetry(event);
            // v2 `recoverFromContextOverflow` (fullCompactionService.ts:468-478)
            // does not give up on the first overflow: it compacts and **retries
            // the step**, and `recordOverflowRecovery` allows
            // `maxOverflowCompactionAttempts` (3) consecutive rounds before the
            // turn fails. One attempt — what this used to do — fails a turn that
            // v2 recovers on its second or third try.
            //
            // The counter is per-step-run, and reset below as soon as a step
            // completes: the window evidently fits again, so the next overflow
            // starts a fresh budget (v2 `afterStep`).
            //
            // Announced here rather than at the loop top: the goal/cancel
            // guards above return with `step_num` ("this step didn't run") and
            // the compaction guard can fail the turn, and outside the recovery
            // loop so an overflow round does not announce the same step twice.
            emit_step_begin_event(callbacks.as_ref(), &input.agent_id, &turn_id, steps);
            let step_result = 'overflow_recovery: loop {
                let request_messages = match budgeted_request(
                    &mut media_budget,
                    input.media,
                    input.llm,
                    &messages,
                    callbacks.as_ref(),
                )
                .await
                {
                    Ok(messages) => messages,
                    // v2 `isMediaUploadAuthError` → throw: a rejected upload
                    // credential fails the turn with the upload's own error
                    // rather than degrading the media to a path tag.
                    Err(error) => {
                        return Err(Box::new(error) as Box<dyn std::error::Error + 'a>);
                    }
                };
                let call = execute_loop_step_with_retry(
                    &turn_id,
                    step_num,
                    input.llm,
                    &request_messages,
                    input.tools,
                    &step_tool_defs,
                    &retry_config,
                    Some(turn_cancel.token()),
                    Some(&telemetry),
                )
                .await;
                let e = match call {
                    Ok(res) => break 'overflow_recovery res,
                    Err(e) => e,
                };
                {
                    let err_str = e.to_string();
                    // A cancellation landing mid-step (the LLM transport
                    // aborted the in-flight request) is a clean abort or budget exhaustion, not a
                    // failed turn — mirror the step-top and scheduler paths.
                    if turn_cancel.token().is_cancelled()
                        || input
                            .cancellation
                            .as_ref()
                            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
                    {
                        let is_goal_deadline = goal.as_ref().is_some_and(|g| {
                            g.wall_clock_budget_ms
                                .is_some_and(|budget| elapsed_wall_clock_ms(turn_started) >= budget)
                        });
                        let stop_reason = if is_goal_deadline {
                            LoopTurnStopReason::BudgetLimited
                        } else {
                            LoopTurnStopReason::Aborted
                        };
                        return Ok(turn_result(
                            stop_reason,
                            steps,
                            total_usage,
                            0,
                            llm_retries,
                            messages.clone(),
                        ));
                    }
                    // v2 `nextProjectionPolicyForError`: a provider
                    // "request too large" (`APIRequestTooLargeError`)
                    // degrades the older media and retries; a second
                    // rejection strips every media part to its path tag.
                    // An image-format rejection (`isImageFormatError`)
                    // strips straight away — v2 gives it no degrade round.
                    // The transform runs on the unresolved messages — a
                    // reference still names its file there, which is what
                    // the path tag needs — and the loop re-resolves.
                    let too_large = crate::llm::media_budget::is_request_too_large(&err_str);
                    let image_format =
                        !too_large && crate::llm::media_budget::is_image_format_error(&err_str);
                    if too_large || image_format {
                        let projection = if too_large && !media_degraded {
                            media_degraded = true;
                            let entries = input
                                .media
                                .map(|resolver| resolver.media_entries(&messages));
                            let degraded = entries.as_deref().map_or(0, |entries| {
                                crate::llm::media_budget::degrade_older_media(
                                    &mut messages,
                                    entries,
                                    crate::llm::media_budget::MEDIA_DEGRADE_KEEP_RECENT,
                                )
                            });
                            if degraded == 0 {
                                return Err(Box::new(std::io::Error::other(err_str))
                                    as Box<dyn std::error::Error + 'a>);
                            }
                            crate::llm::media_budget::MEDIA_DEGRADED_CODE
                        } else if !media_stripped {
                            media_stripped = true;
                            let entries = input
                                .media
                                .map(|resolver| resolver.media_entries(&messages));
                            let stripped = entries.as_deref().map_or(0, |entries| {
                                crate::llm::media_budget::strip_all_media(&mut messages, entries)
                            });
                            if stripped == 0 {
                                return Err(Box::new(std::io::Error::other(err_str))
                                    as Box<dyn std::error::Error + 'a>);
                            }
                            crate::llm::media_budget::MEDIA_STRIPPED_CODE
                        } else {
                            return Err(Box::new(std::io::Error::other(err_str))
                                as Box<dyn std::error::Error + 'a>);
                        };
                        callbacks.emit_event(serde_json::json!({
                            "type": "warning",
                            "code": projection,
                            "message": if projection == crate::llm::media_budget::MEDIA_DEGRADED_CODE {
                                "Provider rejected the request as too large; older media were dropped and the request was retried."
                            } else {
                                "Provider rejected the media in the request; all media were omitted and the request was retried."
                            },
                        }));
                        continue 'overflow_recovery;
                    }
                    // v2 `nextProjectionPolicyForError`, third arm
                    // (`llmRequesterService.ts:594-601`): a provider that
                    // rejects the request's *shape* — a duplicated
                    // `tool_call_id`, roles that do not alternate — gets one
                    // resend with the strict projection. Checked after the two
                    // media arms because those share a 400 and their recovery
                    // actually works, and before the overflow arm for the same
                    // reason: compaction cannot repair a malformed exchange.
                    if !structure_strict
                        && crate::llm::request_structure::is_recoverable_request_structure_error(
                            &err_str,
                        )
                    {
                        structure_strict = true;
                        // Only a real repair earns the round trip: resending an
                        // unchanged request would spend the one strict attempt
                        // on a shape problem the dedupe cannot see (an
                        // unrecorded field, a provider quirk).
                        if super::tool_call_id::dedupe_duplicate_tool_calls(&mut messages) {
                            callbacks.emit_event(serde_json::json!({
                                "type": "warning",
                                "code": crate::llm::request_structure::STRUCTURE_STRICT_CODE,
                                "message": "Provider rejected the structure of the request; duplicate tool calls were removed and the request was retried.",
                            }));
                            continue 'overflow_recovery;
                        }
                        structure_strict = false;
                    }
                    let estimated_request_tokens =
                        crate::compaction::estimate_messages_tokens(&messages);
                    if !crate::compaction::should_recover_from_context_overflow(
                        &err_str,
                        estimated_request_tokens,
                        effective_window,
                    ) {
                        return Err(Box::new(std::io::Error::other(err_str))
                            as Box<dyn std::error::Error + 'a>);
                    }
                    // v2 `recordOverflowRecovery`: count the round, then refuse
                    // once the budget is spent. The sentence comes from
                    // `engine.compaction.overflowFailed` in the embedded
                    // catalog and goes out through the engine i18n layer; no
                    // host matches on the text itself (v2 hosts matched on
                    // error codes).
                    consecutive_overflow_compactions += 1;
                    let max_attempts = compaction_config.max_overflow_compaction_attempts;
                    if consecutive_overflow_compactions > max_attempts {
                        let message = crate::i18n::LocalizedText::with_params(
                            "engine.compaction.overflowFailed",
                            crate::i18n::i18n_params!["max_attempts" => max_attempts],
                        )
                        .render();
                        return Err(Box::new(std::io::Error::other(message))
                            as Box<dyn std::error::Error + 'a>);
                    }
                    callbacks.emit_event(serde_json::json!({
                        "type": "compaction.started",
                        "trigger": "auto",
                    }));
                    // Same pair as the threshold path above (v2 fires
                    // PreCompact / PostCompact around every compaction):
                    // `PreCompact` while the original history still exists,
                    // `PostCompact` on the success arm below.
                    if let Some(ref guard) = hook_guard {
                        guard.notify_pre_compact(&turn_id, messages.len()).await;
                    }
                    // Same todo suffix as the threshold path (v2
                    // `postProcessSummary`), read at compaction time.
                    let todos = crate::compaction::read_todos_for_summary(callbacks.as_ref()).await;
                    // v2's `compaction_failed` carries `duration_ms`: the time
                    // until the failure, measured around the failing call.
                    let compaction_started = std::time::Instant::now();
                    let compacted_report =
                        crate::compaction::force_compact_messages_with_summary_report(
                            &messages,
                            &compaction_config,
                            input.llm,
                            None,
                            Some(turn_cancel.token()),
                            effective_window,
                            crate::compaction::estimate_messages_tokens(&messages),
                            todos,
                        )
                        .await;
                    let (force_compacted, report) = match compacted_report {
                        Ok(pair) => pair,
                        Err(error) => {
                            callbacks.emit_event(serde_json::json!({
                                "type": "compaction.cancelled",
                            }));
                            // v2 `compaction_failed`: the summarizer call
                            // failed outright, which is a different event
                            // from a compaction that merely produced nothing
                            // (that one is a no-op, handled below).
                            //
                            // The property names are v2's
                            // (`CompactionFailedEvent`); `reason` was this
                            // engine's invention and v2 has no such field.
                            //
                            // `thinking_effort` is required by v2's schema and is
                            // deliberately absent: it lives on the
                            // `TelemetryContext` the wrappers own, and `run_turn`
                            // does not receive it. An empty string would claim an
                            // effort that was never in effect, which is worse than
                            // a missing property. Threading the context in is the
                            // follow-up.
                            callbacks.telemetry(serde_json::json!({
                                "event": "compaction_failed",
                                "source": "auto",
                                "tokens_before": crate::compaction::estimate_messages_tokens(&messages),
                                "duration_ms": compaction_started.elapsed().as_millis() as u64,
                                "round": consecutive_overflow_compactions,
                                "retry_count": compaction_config.max_overflow_compaction_attempts,
                                "error_type": format!("{error:?}"),
                            }));
                            return Err(Box::new(error) as Box<dyn std::error::Error + 'a>);
                        }
                    };
                    // A compaction that produced no summary cannot change the
                    // next request, so retrying would just burn the remaining
                    // attempts on an identical prompt. The v2 shape keeps the
                    // compacted range's user input verbatim (head + tail), so
                    // the message count alone cannot prove progress; the
                    // consecutive-attempt budget above bounds the loop.
                    //
                    // This branch has to run *before* the terminal event.
                    // `force_compact_messages_with_summary_report` returns
                    // `count == 0` as a bare no-op (no summarizer request, the
                    // messages unchanged, `tokensAfter == tokensBefore`), so
                    // announcing `compaction.completed` for it would report a
                    // success card for zero work and only then fail the turn.
                    // It is the only source of an empty summary here:
                    // `summarize_with_llm_budgeted` answers an empty response
                    // with `Err(EmptySummary)`, never `Ok("")`.
                    // v2 does the same thing from the other direction:
                    // `fullCompactionService` throws on an empty summary
                    // inside `compactionRound`, so `compactionWorker` never
                    // dispatches `CompactionCompleted` for it and the `catch`
                    // lands on `cancelActive` -> `CompactionCancelled`.
                    // `compaction.started` is already out either way, so a
                    // terminal event is owed; `cancelled` is the one that
                    // matches what actually happened.
                    if report.summary.is_empty() && force_compacted.len() >= messages.len() {
                        callbacks.emit_event(serde_json::json!({
                            "type": "compaction.cancelled",
                        }));
                        return Err(Box::new(std::io::Error::other(err_str))
                            as Box<dyn std::error::Error + 'a>);
                    }
                    callbacks.emit_event(serde_json::json!({
                        "type": "compaction.completed",
                        "result": {
                            "summary": report.summary,
                            "compactedCount": report.compacted_count,
                            "tokensBefore": report.tokens_before,
                            "tokensAfter": report.tokens_after,
                        },
                    }));
                    // Completion arm of the pair opened alongside
                    // `compaction.started` above.
                    if let Some(ref guard) = hook_guard {
                        guard
                            .notify_post_compact(&turn_id, "auto", report.tokens_after as u64)
                            .await;
                    }
                    tracing::warn!(
                        turn_id = %turn_id,
                        step = step_num,
                        before = messages.len(),
                        after = force_compacted.len(),
                        attempt = consecutive_overflow_compactions,
                        max_attempts = max_attempts,
                        "recovered from context overflow via emergency compaction"
                    );
                    messages = force_compacted;
                    context_tokens.invalidate();
                }
            };
            // The step produced a result, so the window fits: v2 `afterStep`
            // clears the consecutive-overflow counter here.
            consecutive_overflow_compactions = 0;

            total_usage.accumulate(&step_result.usage);
            llm_retries += step_result.attempts.saturating_sub(1);
            last_finish_reason = step_result.finish_reason.clone();

            // v2 `turn.step.completed` — the addressed copy of this step's
            // boundary, paired with the `emit_step_begin_event` above.
            emit_step_end_event(
                callbacks.as_ref(),
                &input.agent_id,
                &turn_id,
                steps,
                &step_result,
            );

            match step_result.stop_reason {
                LoopStepStopReason::Complete => {
                    // Persist the assistant text into the in-turn history so
                    // the session (and any future cross-turn caller) sees the
                    // model's reply. For ToolCalls the assistant message is
                    // pushed below with the tool_calls payload.
                    if !step_result.content.is_empty() {
                        messages.push(LLMMessage {
                            role: "assistant".into(),
                            content: step_result.content.clone(),
                            // Fold the step's reasoning back in: providers
                            // that attested it (Anthropic signature, Gemini
                            // thought signature) reject follow-ups without it.
                            blocks: step_result.thinking.clone(),
                            tool_calls: Vec::new(),
                            tool_call_id: None,

                            prompt_id: None,
                            origin: None,
                        });
                    }
                    // Stop hooks (v2 `runStopHooks`, onDidFinishStep without
                    // tool calls): a text-finished step vetoes the stop when
                    // a matching hook blocks. A filtered finish skips the
                    // dispatch entirely, so a veto cannot turn a Filtered
                    // turn into a continuation: v2 only compares
                    // `finishReason === 'filtered'`, and latches
                    // `ProviderFilteredError` after `runAfterStep` while
                    // ignoring `hookStopTurn` (`loopService.ts:877-882`).
                    // The same predicate the turn mapper uses decides it, so
                    // the native transports' raw spellings skip the dispatch
                    // too — a single-value guard here only ever caught
                    // OpenAI's `content_filter` and let the canonical
                    // `filtered` through to a continuation.
                    // At a clean stop there is no tool to match
                    // against, so the matcher runs against "" (v2 passes no
                    // matcher value) and only empty-matcher hooks fire.
                    // The engine never self-continues: the veto text rides
                    // `TurnResult.stop_hook_continuation` for the host.
                    let stop_hook_continuation = if crate::turn_loop::turn_step::is_content_filtered(
                        step_result.finish_reason.as_deref(),
                    ) {
                        None
                    } else if let Some(ref guard) = hook_guard {
                        // Clean text stop: no tool to match against, so
                        // the matcher runs against "" (v2 passes no
                        // matcher value) and only empty-matcher hooks
                        // fire.
                        guard.notify_stop("", "", "stop").await
                    } else {
                        None
                    };
                    let mut result = turn_result(
                        turn_stop_reason_from_finish(step_result.finish_reason.as_deref()),
                        steps,
                        total_usage,
                        0,
                        llm_retries,
                        messages.clone(),
                    );
                    result.stop_hook_continuation = stop_hook_continuation;
                    return Ok(result);
                }
                LoopStepStopReason::ToolCalls(mut tool_calls) => {
                    // Append ONE assistant message carrying all tool calls. Wire
                    // formats group an assistant turn's calls into a single
                    // message; keeping them structural (not flattened into
                    // `content`) lets a native provider round-trip them.
                    messages.push(LLMMessage {
                        role: "assistant".into(),
                        content: step_result.content.clone(),
                        blocks: step_result.thinking.clone(),
                        tool_calls: tool_calls.clone(),
                        tool_call_id: None,

                        prompt_id: None,
                        origin: None,
                    });

                    // v2 `AgentSwarmService`'s first gate: in swarm mode the
                    // single-subagent `Agent` tool is refused, so the model uses
                    // `AgentSwarm` for parallel dispatch instead. v2's hook reads
                    // a single `event.toolCall`, so the refusal is per call: the
                    // denial lands as that call's tool result and the rest of the
                    // batch still runs.
                    let mut denied_in_swarm_mode = Vec::new();
                    tool_calls.retain(|tc| {
                        let Some(veto) = crate::swarm::mode::veto_agent_in_swarm_mode(
                            &input.agent_id,
                            tc.name.as_str(),
                        ) else {
                            return true;
                        };
                        denied_in_swarm_mode.push((tc.id.clone(), veto));
                        false
                    });
                    for (tool_call_id, veto) in &denied_in_swarm_mode {
                        messages.push(LLMMessage {
                            role: "tool".into(),
                            content: crate::swarm::mode::veto_message(*veto),
                            blocks: Vec::new(),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(tool_call_id.clone()),
                            prompt_id: None,
                            origin: None,
                        });
                    }
                    if tool_calls.is_empty() {
                        // Nothing left to run this step: the denial is the whole
                        // result, so hand the turn back to the model.
                        continue;
                    }

                    // Swarm gate (v2 `AgentSwarmService.onBeforeExecuteTool`):
                    // a model response may issue `AgentSwarm` only on its own.
                    // Two swarms, or a swarm mixed with other tools, are
                    // refused before anything runs — otherwise the batch and
                    // the sibling calls race inside one turn.
                    if let Some(veto) = crate::swarm::mode::veto_swarm_batch(
                        &tool_calls
                            .iter()
                            .map(|tc| tc.name.as_str())
                            .collect::<Vec<_>>(),
                    ) {
                        let refusal = crate::swarm::mode::veto_message(veto);
                        for tc in &tool_calls {
                            messages.push(LLMMessage {
                                role: "tool".into(),
                                content: refusal.clone(),
                                blocks: Vec::new(),
                                tool_calls: Vec::new(),
                                tool_call_id: Some(tc.id.clone()),
                                prompt_id: None,
                                origin: None,
                            });
                        }
                        // v2 `AgentSwarmService.onBeforeExecuteTool` vetoes the
                        // call and the loop carries on: the refusal lands as the
                        // tool result, so the model reads what it did wrong and
                        // can reissue the batch on its own. Ending the turn
                        // instead (what this used to do) handed the user a dead
                        // end on a recoverable mistake, and the step budget
                        // above is the same bound v2 relies on — a model that
                        // keeps repeating the batch runs out of steps and the
                        // turn ends `max_steps`, which is the honest outcome.
                        continue;
                    }

                    // Tool-call dedup plan (v2 `toolDedupeService`, G-6 #2):
                    // identical calls inside this step never execute twice —
                    // a repeat awaits the original's result instead. Scoped
                    // to natively-executable names: calls forwarded to the
                    // host stay under the host's own dedup service. Cells
                    // carry each call's result by its position in
                    // `tool_calls` (the scheduler preserves call order).
                    let dedupe_plan = tool_dedupe.plan_step_by(&tool_calls, |tc| {
                        crate::tools::is_native_tool_name(&tc.name)
                            .then(|| crate::tools::tool_dedupe::make_key(&tc.name, &tc.arguments))
                    });
                    let call_index: std::collections::HashMap<String, usize> = tool_calls
                        .iter()
                        .enumerate()
                        .map(|(i, tc)| (tc.id.clone(), i))
                        .collect();
                    let dedupe_cells: Vec<
                        std::sync::Arc<tokio::sync::OnceCell<ExecutableToolResult>>,
                    > = tool_calls
                        .iter()
                        .map(|_| std::sync::Arc::default())
                        .collect();

                    // Execute tools with resource-conflict scheduling:
                    // non-conflicting calls run concurrently, conflicting
                    // calls (e.g. two writes to the same file) are
                    // serialized across batches.
                    let exec_fn = {
                        let turn_id = turn_id.clone();
                        let agent_id = input.agent_id.clone();
                        let callbacks = callbacks.clone();
                        let dedupe_cells = dedupe_cells.clone();
                        let original_of = dedupe_plan.original_of.clone();
                        move |tc: ToolCall| {
                            let index = call_index.get(&tc.id).copied();
                            // The step's first same-key occurrence this
                            // repeat shares (`None` for originals).
                            let dup_source = index
                                .zip(index.map(|i| original_of[i]))
                                .filter(|(i, o)| i != o)
                                .map(|(_, o)| o);
                            let turn_id = turn_id.clone();
                            let agent_id = agent_id.clone();
                            let callbacks = callbacks.clone();
                            let dedupe_cells = dedupe_cells.clone();
                            async move {
                                // A same-step repeat never executes: it
                                // shares the original's result, which the
                                // step's finalize pass rewrites with any
                                // repeat reminder before the results reach
                                // the history (v2 deferred resolution). The
                                // fallback only runs if the original's task
                                // vanished before publishing (a cancellation
                                // abort) — never executes the tool, matching
                                // v2's lost-deferred error result.
                                if let Some(original) = dup_source {
                                    let shared = dedupe_cells[original]
                                        .get_or_init(|| async {
                                            ExecutableToolResult { delivery: None,
                                                stop_turn: false,
                                                content: "Tool call deduplicated but original result was lost".into(),
                                                is_error: true,
                                                note: None,
                                                display: None,
                                            }
                                        })
                                        .await;
                                    return Ok(shared.clone());
                                }
                                let req = ToolExecuteRequest {
                                    turn_id: turn_id.clone(),
                                    agent_id: agent_id.clone(),
                                    tool_call_id: tc.id.clone(),
                                    tool_name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                };
                                // Publish the outcome through the cell so a
                                // repeat can share it; a transport error
                                // becomes the same error result the
                                // scheduler would synthesize for it.
                                let execute = async {
                                    // The tool-call lifecycle: `tool.native`
                                    // only reports the outcome, so a consumer
                                    // could never tell that a call had
                                    // started (or how long it ran). A
                                    // deduplicated repeat never reaches here
                                    // — it shares the original's cell.
                                    let mut started = serde_json::json!({
                                        "type": "tool.call.started",
                                        "agent_id": agent_id,
                                        "turn_id": turn_id,
                                        "tool_call_id": tc.id,
                                        "tool_name": tc.name,
                                        "args": tc.arguments,
                                    });
                                    // v2's `RunnableToolExecution.display`: a
                                    // host renders this instead of the raw
                                    // argument string — the approval prompt
                                    // reads "spawn agent swarm (3 subagents)"
                                    // rather than dumping the JSON. Declared
                                    // before execution so it rides this event.
                                    // v2's `RunnableToolExecution.display`: a
                                    // host renders this instead of the raw
                                    // argument string — the approval prompt
                                    // reads "spawn agent swarm (3 subagents)"
                                    // rather than dumping the JSON. Declared
                                    // before execution so it rides this event.
                                    if let Some(display) =
                                        crate::tools::agent_tool::agent_call_display(
                                            &tc.name,
                                            &tc.arguments,
                                        )
                                        && let Some(map) = started.as_object_mut()
                                    {
                                        map.insert("display".into(), display);
                                    }
                                    callbacks.emit_event(started);
                                    match callbacks.execute_tool(req).await {
                                        Ok(response) => {
                                            callbacks.emit_event(serde_json::json!({
                                                "type": if response.is_error {
                                                    "tool.call.failed"
                                                } else {
                                                    "tool.call.completed"
                                                },
                                                "agent_id": agent_id,
                                                "turn_id": turn_id,
                                                "tool_call_id": tc.id,
                                                "tool_name": tc.name,
                                                "is_error": response.is_error,
                                            }));
                                            ExecutableToolResult {
                                                delivery: response.delivery,
                                                stop_turn: response.stop_turn,
                                                content: response.content,
                                                is_error: response.is_error,
                                                note: response.note,
                                                display: None,
                                            }
                                        }
                                        Err(e) => {
                                            callbacks.emit_event(serde_json::json!({
                                                "type": "tool.call.failed",
                                                "agent_id": agent_id,
                                                "turn_id": turn_id,
                                                "tool_call_id": tc.id,
                                                "tool_name": tc.name,
                                                "is_error": true,
                                                "error": e,
                                            }));
                                            ExecutableToolResult {
                                                delivery: None,
                                                stop_turn: false,
                                                content: format!("Tool execution error: {e}"),
                                                is_error: true,
                                                note: None,
                                                display: None,
                                            }
                                        }
                                    }
                                };
                                match index {
                                    Some(i) => {
                                        let result = dedupe_cells[i].get_or_init(|| execute).await;
                                        Ok(result.clone())
                                    }
                                    // Unknown call id (defensive): no cell to
                                    // share through, execute plainly.
                                    None => Ok(execute.await),
                                }
                            }
                        }
                    };
                    let scheduled: Vec<ScheduledToolCall> = tool_calls
                        .iter()
                        .map(|tc| ScheduledToolCall {
                            tool_call: tc.clone(),
                            accesses: tool_scheduler::infer_tool_accesses(&tc.name, &tc.arguments),
                        })
                        .collect();

                    // Derive what the AGENTS.md reminder needs from the accesses
                    // already computed above, exactly as v2's
                    // `targetDirsFromAccesses` does (`agentsMdReminderService.ts:284-306`):
                    // a file tool contributes its *parent directory*, a tree
                    // search contributes the root itself, and an AGENTS.md the
                    // call just read is recorded separately so the reminder does
                    // not tell the model to go read a file it already has. A
                    // Bash call contributes its own `cwd` argument, which v2 adds
                    // whether or not any operand resolves (`:255-262, 274-276`).
                    step_access = crate::injection::StepAccess::default();
                    for call in &scheduled {
                        let name = call.tool_call.name.to_ascii_lowercase();
                        if name == "bash" {
                            if let Some(cwd) = call
                                .tool_call
                                .arguments
                                .get("cwd")
                                .and_then(|value| value.as_str())
                            {
                                step_access.declared_cwds.push(cwd.to_string());
                            }
                            continue;
                        }
                        let targets_file = matches!(name.as_str(), "read" | "edit" | "write");
                        for access in &call.accesses {
                            let crate::turn_loop::types::ToolResourceAccess::File(access) = access
                            else {
                                continue;
                            };
                            let accessed = std::path::PathBuf::from(&access.path);
                            if targets_file {
                                if let Some(parent) = accessed.parent() {
                                    step_access.dirs.push(parent.to_path_buf());
                                }
                                if crate::injection::is_agents_md_path(&accessed) {
                                    step_access.self_read.push(accessed);
                                }
                            } else {
                                step_access.dirs.push(accessed);
                            }
                        }
                    }
                    let (mut results, durations_ms, _batch_cancelled) =
                        match tool_scheduler::execute_scheduled(
                            input.cancellation.as_ref(),
                            scheduled,
                            exec_fn,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                (outcome.results, outcome.durations_ms, outcome.cancelled)
                            }
                            Err(err) => {
                                // The scheduler stops mid-batch when the host
                                // cancels. That is a clean abort, not a failed
                                // turn — mirror the step-top and step-result paths.
                                let cancelled = input.cancellation.as_ref().is_some_and(|flag| {
                                    flag.load(std::sync::atomic::Ordering::Relaxed)
                                });
                                if cancelled {
                                    return Ok(turn_result(
                                        LoopTurnStopReason::Aborted,
                                        steps,
                                        total_usage,
                                        0,
                                        llm_retries,
                                        messages.clone(),
                                    ));
                                }
                                return Err(err);
                            }
                        };

                    // Dedup finalize (v2 `finalizeResult` + `endStep`):
                    // streak reminders are appended to the originals'
                    // results, same-step repeats receive the original's
                    // final result, and the cross-step streak advances.
                    let dedupe_force_stop = tool_dedupe.finalize_step(&dedupe_plan, &mut results);

                    // Insert tool results, each linked back to its call
                    // via `tool_call_id` (same call order as `tool_calls`).
                    for (i, tr) in results.iter().enumerate() {
                        // A same-step repeat shared the original's execution
                        // and never reached the native gate, so no tool.native
                        // event surfaced it — emit one here so the transcript
                        // still shows the call with its shared result (v2
                        // keeps the vetoed repeat visible).
                        if dedupe_plan.original_of.get(i).is_some_and(|o| *o != i)
                            && let Some(tc) = tool_calls.get(i)
                        {
                            callbacks.emit_event(serde_json::json!({
                                "type": "tool.native",
                                "agent_id": input.agent_id,
                                "turn_id": turn_id,
                                "tool_call_id": tc.id,
                                "tool_name": tc.name,
                                "arguments": tc.arguments,
                                "content": tr.content,
                                "is_error": tr.is_error,
                                "note": tr.note,
                            }));
                        }
                        // Upstream #3966: the persisted (and model-facing)
                        // tool result carries a `Wall time: X.XXX seconds`
                        // header for the wall-time tool set and every MCP
                        // tool; the scheduler measured the duration around
                        // the call. Tools outside the set keep the bare
                        // content, and a result with no measured duration
                        // (a scheduler-synthesized error) never gets one.
                        let tool_name = tool_calls.get(i).map(|tc| tc.name.as_str()).unwrap_or("");
                        // v2 renders the status wrapper first and prepends the
                        // wall-time header on top of it
                        // (`renderToolResultForModel`), so a failed call reads
                        // `Wall time: …\n<system>ERROR: …`.
                        let rendered = tool_result_render::render_status(&tr.content, tr.is_error);
                        let content = match durations_ms.get(i).copied().flatten() {
                            Some(duration_ms) if wall_time::should_render_wall_time(tool_name) => {
                                wall_time::prepend_wall_time(&rendered, duration_ms)
                            }
                            _ => rendered,
                        };
                        // v2 appends the note last (`renderToolResultForModel`,
                        // `contextMemory/toolResultRender.ts:43-48`): status
                        // wrapper, then the wall-time header, then the note.
                        //
                        // The note is not decoration. Read fills it with the
                        // guidance the model needs to act on what it just got —
                        // "use Bash to read the elided content", "Edit and Write
                        // expect UTF-8 — convert the file's encoding first" — so
                        // leaving it out hides the instructions and leaves the
                        // model with a truncated file and no idea why.
                        let content = match tr.note.as_deref().filter(|note| !note.is_empty()) {
                            Some(note) => format!("{content}\n{note}"),
                            None => content,
                        };
                        messages.push(LLMMessage {
                            role: "tool".into(),
                            content,
                            blocks: Vec::new(),
                            tool_calls: Vec::new(),
                            tool_call_id: tool_calls.get(i).map(|tc| tc.id.clone()),
                            prompt_id: None,
                            origin: None,
                        });
                        // Rich content the tool delivers (an image the model
                        // asked to read): a follow-up user message right after
                        // the tool result, the same shape a pasted image takes.
                        // A same-step repeat shares the original's result —
                        // and its delivery — so only the original emits it.
                        let is_repeat = dedupe_plan.original_of.get(i).is_some_and(|o| *o != i);
                        if !is_repeat
                            && let Some(delivery) = tr.delivery.as_ref()
                            && !delivery.blocks.is_empty()
                        {
                            messages.push(LLMMessage {
                                role: "user".into(),
                                content: String::new(),
                                blocks: delivery.blocks.clone(),
                                tool_calls: Vec::new(),
                                tool_call_id: None,

                                prompt_id: None,
                                origin: delivery.origin.clone(),
                            });
                        }
                    }

                    // v2 `executeStepTools` stopTurn (loopService.ts:2117-2119):
                    // any tool result carrying `stopTurn: true` ends the turn
                    // as `completed` — no continuation step is enqueued
                    // (loopContinuationService.ts:14-20). Used by update-goal
                    // (complete/blocked), set-goal-budget (over budget) and
                    // the plan-review reject paths.
                    if results.iter().any(|tr| tr.stop_turn) {
                        return Ok(turn_result(
                            LoopTurnStopReason::EndTurn,
                            steps,
                            total_usage,
                            0,
                            llm_retries,
                            messages.clone(),
                        ));
                    }

                    if dedupe_force_stop {
                        // Upstream #3459: Handoff step after repeat breaker forced stop.
                        // Append a text-only reminder and give the model one step to
                        // write its final handoff reply without tool calls.
                        messages.push(LLMMessage {
                            role: "user".into(),
                            content: crate::injection::wrap_system_reminder(
                                "The repeat breaker has stopped the turn due to repeated tool calls. \
Deliver your final response as text now. Further tool calls are refused.",
                            ),
                            ..Default::default()
                        });

                        steps += 1;
                        // The handoff step sits outside the `for` loop, so it
                        // needs its own boundary pair — without it the final
                        // reply would stream into the card of the step that
                        // already completed.
                        emit_step_begin_event(callbacks.as_ref(), &input.agent_id, &turn_id, steps);
                        let handoff_res = execute_loop_step_with_retry(
                            &turn_id,
                            steps,
                            input.llm,
                            &messages,
                            input.tools,
                            &[],
                            &retry_config,
                            Some(turn_cancel.token()),
                            Some(&telemetry),
                        )
                        .await;

                        if let Ok(step_res) = &handoff_res {
                            emit_step_end_event(
                                callbacks.as_ref(),
                                &input.agent_id,
                                &turn_id,
                                steps,
                                step_res,
                            );
                        }

                        if let Ok(step_res) = handoff_res
                            && !step_res.content.is_empty()
                        {
                            messages.push(LLMMessage {
                                role: "assistant".into(),
                                content: step_res.content,
                                blocks: Vec::new(),
                                tool_calls: Vec::new(),
                                tool_call_id: None,

                                prompt_id: None,
                                origin: None,
                            });
                        }

                        return Ok(turn_result(
                            LoopTurnStopReason::RepeatBreaker,
                            steps,
                            total_usage,
                            0,
                            llm_retries,
                            messages.clone(),
                        ));
                    }
                }
            }
        }

        // The loop only falls through when the step budget was consumed while
        // the model was still requesting tools — v2 raises
        // `MaxStepsExceededError` there (loopService.ts:841-843) and the turn
        // fails with interrupt_reason `max_steps` (loopService.ts:956-962).
        // A provider-side `length` / `content_filter` finish on the final
        // step still surfaces its own cause instead of the generic one.
        let reason = match turn_stop_reason_from_finish(last_finish_reason.as_deref()) {
            LoopTurnStopReason::EndTurn => LoopTurnStopReason::MaxSteps,
            other => other,
        };
        Ok(turn_result(
            reason,
            steps,
            total_usage,
            0,
            llm_retries,
            messages.clone(),
        ))
    })
}

// ── Goal steering ───────────────────────────────────────────────────────────

/// Render goal steering text injected into the system prompt.
///
/// Mirrors the TS `buildGoalReminder` format: objective, progress, budgets,
/// and convergence guidance when nearing a budget.
fn render_goal_steering(
    goal: &GoalContext,
    turn_tokens: i64,
    turns_this_turn: i64,
    turn_wall_clock_ms: i64,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "## Goal\n{}",
        crate::goal::escape_untrusted_text(&goal.objective)
    ));

    // Progress line
    let total_tokens = goal.tokens_used + turn_tokens;
    let total_turns = goal.turns_used + turns_this_turn;
    let total_wall_clock_ms = goal.wall_clock_ms + turn_wall_clock_ms;
    lines.push(format!(
        "Progress: {} continuation turns, {} tokens consumed, {} elapsed.",
        total_turns,
        total_tokens,
        format_elapsed(total_wall_clock_ms)
    ));

    // Budgets line
    let mut budget_parts = Vec::new();
    if let Some(budget) = goal.token_budget {
        let remaining = (budget - total_tokens).max(0);
        budget_parts.push(format!(
            "tokens {}/{} (remaining {})",
            total_tokens, budget, remaining
        ));
    }
    if let Some(budget) = goal.turn_budget {
        let remaining = (budget - total_turns).max(0);
        budget_parts.push(format!(
            "turns {}/{} (remaining {})",
            total_turns, budget, remaining
        ));
    }
    if let Some(budget) = goal.wall_clock_budget_ms {
        let remaining = (budget - total_wall_clock_ms).max(0);
        budget_parts.push(format!(
            "time {}/{} (remaining {})",
            format_elapsed(total_wall_clock_ms),
            format_elapsed(budget),
            format_elapsed(remaining)
        ));
    }
    if !budget_parts.is_empty() {
        lines.push(format!("Budgets: {}.", budget_parts.join("; ")));
    }

    // Budget guidance
    let fraction = goal.budget_fraction(turn_tokens, turns_this_turn, turn_wall_clock_ms);
    if fraction >= 0.75 {
        lines.push(
            "Budget guidance: you are nearing a budget. \
             Converge on the objective and avoid starting new discretionary work."
                .to_string(),
        );
    } else {
        lines.push(
            "Budget guidance: you are within budget. \
             Make steady, focused progress toward the objective."
                .to_string(),
        );
    }

    lines.join("\n")
}

/// Format a millisecond duration compactly (`1m05s`, `2h00m`, …).
fn format_elapsed(ms: i64) -> String {
    let total_seconds = ms / 1000;
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = total_seconds / 60;
    if minutes < 60 {
        let seconds = total_seconds % 60;
        return format!("{minutes}m{seconds:02}s");
    }
    let hours = minutes / 60;
    format!("{hours}h{}m", minutes % 60)
}

#[cfg(test)]
mod tests;
