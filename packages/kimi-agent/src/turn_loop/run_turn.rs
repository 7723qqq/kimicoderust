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
use super::turn_step::{execute_loop_step_with_projection, execute_loop_step_with_retry};
use super::types::*;
use super::wall_time;
use super::watch::TurnPhase;
use crate::callbacks::HostCallbacks;
use crate::llm::files_upload::UploadError;
use crate::llm::media_budget::{MEDIA_BUDGET_EXCEEDED_CODE, MediaBudget};
use crate::llm::media_resolver::{MediaResolver, ResolvedRequest, has_media_refs, inline_entries};
use crate::rpc::types::{BoxFuture, TokenUsage, ToolExecuteRequest};

/// Goal/plan/todo state snapshot backed by the host callbacks.
///
/// The injection providers render synchronously, while the state authority is
/// behind the async `host/state_read` channel — so the turn loop refreshes
/// this snapshot at each step head (see the injection pass) and the providers
/// read the cached values. A failed read is recorded as `Unreadable` with its
/// cause, so a host without the state bridge produces a reported skip instead
/// of silently looking like "this domain has no state".
#[derive(Default)]
struct CallbackStateSnapshot {
    goal: std::sync::Mutex<crate::primitives::read::ReadOutcome<serde_json::Value>>,
    plan: std::sync::Mutex<crate::primitives::read::ReadOutcome<serde_json::Value>>,
    todo: std::sync::Mutex<crate::primitives::read::ReadOutcome<serde_json::Value>>,
}

impl CallbackStateSnapshot {
    async fn refresh(&self, callbacks: &dyn HostCallbacks, include_todo: bool) {
        // The domains are independent host round-trips; issue them
        // concurrently so the step pays one latency instead of two (or three).
        let read = |domain: &str| {
            callbacks.state_read(crate::rpc::types::StateReadRequest {
                domain: domain.to_string(),
                key: domain.to_string(),
                turn_id: String::new(),
                tool_call_id: String::new(),
            })
        };
        if include_todo {
            let (goal, plan, todo) =
                futures_util::future::join3(read("goal"), read("plan"), read("todo")).await;
            self.store("goal", goal);
            self.store("plan", plan);
            self.store("todo", todo);
        } else {
            let (goal, plan) = futures_util::future::join(read("goal"), read("plan")).await;
            self.store("goal", goal);
            self.store("plan", plan);
        }
    }

    /// Record one host read. A failure keeps its cause rather than collapsing
    /// into `Absent`: "the bridge is down" and "there is no goal" render as
    /// different things, and only the first one is worth a warning.
    fn store(&self, domain: &str, outcome: Result<crate::rpc::types::StateReadResponse, String>) {
        let slot = match domain {
            "goal" => &self.goal,
            "plan" => &self.plan,
            _ => &self.todo,
        };
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = match outcome {
            Ok(response) => crate::primitives::read::ReadOutcome::Found(response.value),
            Err(error) => crate::primitives::read::ReadOutcome::Unreadable(
                crate::primitives::read::UnreadableCause::new(
                    format!("host/state_read {domain}"),
                    error,
                ),
            ),
        };
    }
}

impl crate::injection::DomainValueSource for CallbackStateSnapshot {
    fn read_domain_state(
        &self,
        domain: &str,
    ) -> crate::primitives::read::ReadOutcome<serde_json::Value> {
        let slot = match domain {
            "goal" => &self.goal,
            "plan" => &self.plan,
            "todo" => &self.todo,
            // This snapshot only caches those three domains; any other name is
            // genuinely not held here, which is not a read failure.
            _ => return crate::primitives::read::ReadOutcome::Absent,
        };
        slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// v2 `nonHistoryStateFingerprint` (`compactionApply.ts:88-93`): the todo list
/// and the full task list as one stable string. Compaction awaits an LLM round;
/// a todo/task write that lands while it runs would otherwise be overwritten by
/// the summary's stale rendering, so the round is cancelled when the
/// fingerprint moved (`:53-54`).
async fn non_history_state_fingerprint(callbacks: &dyn crate::callbacks::HostCallbacks) -> String {
    let read = |domain: &'static str| async move {
        callbacks
            .state_read(crate::rpc::types::StateReadRequest {
                domain: domain.to_string(),
                key: domain.to_string(),
                turn_id: String::new(),
                tool_call_id: String::new(),
            })
            .await
            .ok()
            .map(|response| response.value)
    };
    let (todos, tasks) = futures_util::future::join(read("todo"), read("task")).await;
    serde_json::json!({ "todos": todos, "tasks": tasks }).to_string()
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
    /// Estimated tokens of the message list alone.
    tokens: u32,
    len: usize,
    stale: bool,
    /// Request overhead the message list does not describe: the tool schema
    /// table the provider also receives (v2 `countRequestTokens` adds the tool
    /// definitions to the estimate). Kept separate so an anchored measurement,
    /// which already includes it, is not double counted.
    overhead: u32,
}

impl ContextTokens {
    fn new(overhead: u32) -> Self {
        Self {
            overhead,
            ..Default::default()
        }
    }

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
        self.tokens.saturating_add(self.overhead)
    }

    /// Anchor the estimate on a provider measurement (v2
    /// `tokenCountingService.tokenCountWithPending`): the last response's
    /// `input_tokens` is the real size of the request the model saw, so the
    /// heuristic only has to cover the messages appended after it. Without
    /// this the estimate drifts low and auto compaction fires late.
    fn record_measured(&mut self, input_tokens: u32, messages: &[LLMMessage]) {
        self.tokens = input_tokens.saturating_sub(self.overhead);
        self.len = messages.len();
        self.stale = false;
    }

    /// The cached prefix was rewritten in place; force a full recompute.
    fn invalidate(&mut self) {
        self.stale = true;
    }

    /// Re-seed after a compaction (v2 `applyCompaction`'s `tokensAfter`): the
    /// summary message costs what the summarization request actually produced,
    /// not what the estimator guesses for its text. The next compaction
    /// threshold therefore sees the measured figure instead of a guess.
    fn rebase_with_summary(&mut self, messages: &[LLMMessage], summary_output_tokens: Option<u32>) {
        self.tokens = crate::compaction::rebased_tokens(messages, summary_output_tokens);
        self.len = messages.len();
        self.stale = false;
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
        last_request_usage: None,
        stop_hook_continuation: None,
        gate_blocked: false,
        // Overwritten by `with_accounting` on every exit path inside the loop;
        // the bare default keeps this helper a total constructor.
        compaction_usage: None,
        tool_schema_tokens: 0,
        cache_observations: Vec::new(),
        cache_probe_requests: Vec::new(),
        cache_observation_resets: Vec::new(),
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
        "step_id": step_event_id(turn_id, step),
    }));
}

/// The `stepId` v2 stamps on `turn.step.started` / `turn.step.completed`
/// (`loopService.ts:1393-1424` draws a fresh uuid per step). The engine derives
/// it from the turn and step number instead: the begin, interrupted, and end
/// boundaries then agree without threading a value through every failure path,
/// and the id is still unique within a turn.
fn step_event_id(turn_id: &str, step: u32) -> String {
    format!("{turn_id}-step-{step}")
}

/// v2 `isCannedUnknownToolResult` (`loopService.ts:1818-1834`): the executor's
/// built-in answer for a name it could not resolve — an error whose whole
/// content is exactly `unknown tool: <name>`. A tool that ran and returned the
/// same text cannot be told apart from it at this layer, and re-running once is
/// what v2 accepts as the cost of the deferral.
fn is_unknown_tool_refusal(content: &str, is_error: bool, tool_name: &str) -> bool {
    is_error && content == format!("unknown tool: {tool_name}")
}

/// The turn loop's step-interrupted event (v2 `TurnStepInterrupted`,
/// `loopService.ts:2301-2313`): a step that began but never completed is closed
/// with the reason it ended (`aborted` / `max_steps` / `error`) and, except for
/// a user cancellation, the message that ended it. v2 emits it immediately
/// before `turn.ended`, and only when a step was actually in flight.
fn emit_step_interrupted_event(
    callbacks: &dyn HostCallbacks,
    agent_id: &str,
    turn_id: &str,
    step: u32,
    reason: &str,
    message: Option<&str>,
) {
    callbacks.emit_event(serde_json::json!({
        "type": "llm.step.interrupted",
        "agent_id": agent_id,
        "turn_id": turn_id,
        "step": step,
        "step_id": step_event_id(turn_id, step),
        "reason": reason,
        "message": message,
    }));
}

/// The turn loop's addressed copy of v2 `turn.step.completed` (upstream #3938
/// puts the request timing on it): the transcript projector folds this into the
/// step — completion, usage, and the timing breakdown. The transport's own sink
/// emission (llm/http.rs) serves the host's message fold and carries no
/// turn/step id, so it cannot address a step; this one can — and `finish_reason`
/// rides along with it so the step's terminal signal (`max_tokens`, `filtered`)
/// is still readable from the only `llm.step.end` that names a step.
/// Reduce one finished call to v2's `ToolReceipt`: the command a shell tool
/// ran and the paths its declared accesses touched. v2 reads these off the
/// tool's `accesses`; the engine derives the same list with
/// [`tool_scheduler::infer_tool_accesses`], so the two agree about what a call
/// wrote.
fn progress_receipt(
    tool_name: &str,
    arguments: &serde_json::Value,
    success: bool,
) -> crate::turn_loop::progress_track::ToolReceipt {
    use crate::turn_loop::types::{FileOperation, ToolResourceAccess};
    let mut receipt = crate::turn_loop::progress_track::ToolReceipt {
        command: arguments
            .get("command")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        success,
        ..Default::default()
    };
    for access in tool_scheduler::infer_tool_accesses(tool_name, arguments) {
        let ToolResourceAccess::File(file) = access else {
            continue;
        };
        if matches!(
            file.operation,
            FileOperation::Write | FileOperation::ReadWrite
        ) {
            receipt.write_paths.push(file.path);
        }
    }
    receipt
}

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
        "step_id": step_event_id(turn_id, step),
        "finish_reason": result.finish_reason.as_deref(),
        "provider_finish_reason": provider_finish_reason(result.finish_reason.as_deref()),
        "raw_finish_reason": result.finish_reason.as_deref(),
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

/// Normalize a provider finish reason the way v2's `ProviderFinishReason`
/// does (`human/llm/finish-reason.ts:1-6`): one value out of
/// `completed | tool_calls | truncated | filtered | paused | other`, next to the
/// provider's own spelling (`raw_finish_reason`). Both are reported on the step
/// so a gateway's new stop token is diagnosable instead of being flattened
/// away. The filtered vocabulary is delegated to
/// [`turn_step::is_content_filtered`], the same list the turn-level mapping
/// uses, so the two cannot drift.
pub(crate) fn provider_finish_reason(finish_reason: Option<&str>) -> Option<&'static str> {
    let raw = finish_reason?;
    // Google sends upper-case reasons (`SAFETY`, `STOP`), the other providers
    // lower-case ones; the vocabulary is case-insensitive.
    let normalized = raw.to_ascii_lowercase();
    if crate::turn_loop::turn_step::is_content_filtered(Some(normalized.as_str())) {
        return Some("filtered");
    }
    Some(match normalized.as_str() {
        "stop" | "end_turn" | "stop_sequence" | "completed" => "completed",
        "tool_calls" | "function_call" | "tool_use" => "tool_calls",
        "length" | "max_tokens" => "truncated",
        "pause_turn" | "paused" => "paused",
        _ => "other",
    })
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
    let agent_id = input.agent_id.clone();
    // v2 `KnowledgeLearner` runs against the session workspace the user
    // opened, not the engine process's cwd. Resolved before `input` moves
    // into the turn.
    let workspace_root = turn_workspace_root(&input);
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
                // v2 `KnowledgeLearner` (`knowledgeLearner.ts:31-53`), main
                // agent only: turn end is when a user correction becomes a
                // stored standard.
                if agent_id == crate::callbacks::MAIN_AGENT_ID
                    && let Some(workspace_root) = &workspace_root
                {
                    crate::injection::knowledge::learn_from_turn(&result.messages, workspace_root);
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

/// The turn with its lifecycle telemetry; goal continuation and abnormal-turn
/// settlement are engine-owned and run after the turn in
/// [`crate::session::goal_turn::settle_goal_after_turn`].
pub fn run_turn_with_telemetry<'a>(
    input: RunTurnInput<'a>,
    telemetry: TelemetryContext,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    run_turn_with_lifecycle_telemetry(input, &telemetry, callbacks)
}

/// The turn's submitted prompt for `UserPromptSubmit` hooks (v2
/// `agentExternalHooksService.notifyUserPromptSubmit`): the latest
/// user-role message, mirroring what the model will see this turn. Empty
/// when the turn carries no user message; the dispatch below skips that case.
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
/// upload's own error instead of sending a degraded request. `Ok(None)` is a
/// cancellation: v2 #4183 aborts an in-flight upload on interruption, and the
/// select below drops the resolve future — which drops the upload's HTTP
/// request with it.
async fn budgeted_request<'a>(
    budget: &mut MediaBudget,
    resolver: Option<&MediaResolver>,
    llm: &dyn LLM,
    messages: &'a [LLMMessage],
    callbacks: &dyn HostCallbacks,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Option<Cow<'a, [LLMMessage]>>, UploadError> {
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
            let resolve = resolver.resolve(messages, target.as_ref(), credential.as_deref());
            match cancel {
                // `biased` puts the token first, so an already-cancelled turn
                // never starts an upload (v2's `throwIfAborted` at entry).
                Some(token) => {
                    tokio::select! {
                        biased;
                        () = token.cancelled() => return Ok(None),
                        resolved = resolve => resolved?,
                    }
                }
                None => resolve.await?,
            }
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
    Ok(Some(request.messages))
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
    run_turn_continued_with_observations(input, callbacks, None)
}

/// [`run_turn_continued`] with an out-of-band observation sink (D14(a)).
///
/// Every pass of the turn writes its per-step cache records to `sink` as they
/// are produced, so a pass that fails hard leaves the caller with the steps
/// that already completed. On a clean finish the returned `TurnResult` carries
/// the same records; a caller that supplies a sink only needs to read it on
/// the `Err` path.
pub fn run_turn_continued_with_observations<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
    sink: Option<TurnObservationsSink>,
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
        media_recovery,
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
                media_recovery: media_recovery.clone(),
                toolset: toolset.clone(),
                // The continuation is the same turn, not a new one: the
                // interruption reminder only fires at a fresh turn's head.
                previous_turn_aborted: carry_previous_turn_aborted,
            };
            let mut result = match run_turn_inner(iter_input, callbacks, sink.clone()).await {
                Ok(result) => result,
                Err(error) => {
                    // v2 closes the mode from a `TurnEnded` subscription, so it
                    // fires however the turn ended. The normal end below is the
                    // only other exit, and this is the one that would otherwise
                    // leave a task-opened (or legacy tool-opened) swarm latched.
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
                    // task-opened (or legacy tool-opened) swarm closes itself
                    // here. A manually opened swarm stays until the user leaves it.
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
/// Push one completed step's observation into a caller-supplied sink.
///
/// A poisoned or absent sink is skipped on purpose: the sink is a diagnostic
/// side channel for the failure path, never a reason to fail a turn.
fn sink_push_observation(
    sink: &Option<TurnObservationsSink>,
    observation: &crate::session::cache_observation::CacheObservation,
) {
    if let Some(sink) = sink
        && let Ok(mut guard) = sink.lock()
    {
        guard.cache_observations.push(observation.clone());
    }
}

fn sink_push_probe_request(
    sink: &Option<TurnObservationsSink>,
    request: &crate::session::cache_observation::CacheProbeRequest,
) {
    if let Some(sink) = sink
        && let Ok(mut guard) = sink.lock()
    {
        guard.cache_probe_requests.push(request.clone());
    }
}

fn sink_push_reset(sink: &Option<TurnObservationsSink>, index: usize) {
    if let Some(sink) = sink
        && let Ok(mut guard) = sink.lock()
    {
        guard.cache_observation_resets.push(index);
    }
}

/// Where this pass's first observation lands in the sink. A Stop-hook
/// continuation reuses one sink across passes, so reset indices are rebased
/// onto the running total instead of restarting at zero.
fn sink_observation_base(sink: &Option<TurnObservationsSink>) -> usize {
    sink.as_ref()
        .and_then(|sink| sink.lock().ok().map(|guard| guard.cache_observations.len()))
        .unwrap_or(0)
}

/// The session workspace root the turn's tools resolve against (v2
/// `workspaceDir`). Falls back to the process cwd only for a toolset-less turn,
/// where this process owns no workspace to name — host-proxy turns with native
/// tools on still carry the toolset, so the product paths use the session root.
fn turn_workspace_root(input: &RunTurnInput<'_>) -> Option<std::path::PathBuf> {
    input
        .toolset
        .as_ref()
        .map(|toolset| toolset.workspace_root().to_path_buf())
        .or_else(|| std::env::current_dir().ok())
}

/// Run a single turn.
#[tracing::instrument(name = "run_turn", skip_all, fields(turn_id = %input.turn_id, max_steps = input.max_steps, has_goal = input.goal.is_some()))]
pub fn run_turn<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    run_turn_inner(input, callbacks, None)
}

/// The turn driver behind [`run_turn`], with an optional out-of-band
/// observation sink.
///
/// `sink` receives every per-step prompt-cache record as it is produced, so a
/// turn that ends in `Err` — with no `TurnResult` to carry them — still hands
/// the steps that completed before the failure to the caller. The `Ok` path is
/// unchanged: the same records are also folded into the returned `TurnResult`.
fn run_turn_inner<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
    sink: Option<TurnObservationsSink>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    let turn_id = input.turn_id.clone();
    // v2 `loopService.ts:1047-1050`: the cap applies only when it is a
    // positive number — an unset or `0` `max_steps_per_turn` means "no cap",
    // not "one step". Clamping to 1 turned a 0 into a turn that failed after
    // its first tool call.
    let max_steps = if input.max_steps == 0 {
        u32::MAX
    } else {
        input.max_steps
    };
    let user_messages = input.messages.clone();
    // Seed the date-change baseline from the conversation history (v2's
    // history-scanned `lastDisclosure`) before `user_messages` moves into the
    // turn body: a prior disclosure suppresses the per-turn baseline
    // re-injection until the date changes.
    let date_baseline = crate::injection::scan_date_baseline(&user_messages);
    // Same for the AGENTS.md reminder: the paths an earlier reminder already
    // named are not suggested again (v2 reads the same set off the last
    // injection's `origin.disclosure`), and the paths this session's system
    // prompt already loaded are seeded as known (v2 `seedInjected`,
    // `agentsMdReminderService.ts:98-124`) — otherwise the reminder names files
    // the model was already given and tells it to read them again.
    let mut agents_md_baseline = crate::injection::scan_agents_md_baseline(&user_messages);
    for path in
        crate::prompt::agents_md::extract_agents_md_paths_from_prompt(input.llm.system_prompt())
    {
        if !agents_md_baseline.iter().any(|seen| seen == &path) {
            agents_md_baseline.push(path);
        }
    }
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
        // D14(a): the sink's current length is this pass's rebase offset for
        // cache-break reset indices (a Stop-hook continuation reuses the sink).
        let sink_base = sink_observation_base(&sink);
        // Per-turn watchdog (v2 `AgentLoopService.armTurnWatch`, upstream
        // #4280 diagnostics half): warns when the turn produces no observable
        // progress — steps, streamed deltas, tool activity, phase — for
        // `TURN_WATCH_WARN_MS`. Stack-owned like the deadline guard below:
        // every turn exit path drops it, which aborts the sampler
        // (`disarmTurnWatch`). The transport is handed the state so its
        // streamed fragments count as progress too.
        let turn_watch_state = Arc::new(crate::turn_loop::watch::TurnWatchState::default());
        let _turn_watchdog = crate::turn_loop::watch::TurnWatchdog::arm(
            &input.agent_id,
            &turn_id,
            Arc::clone(&turn_watch_state),
        );
        input
            .llm
            .set_turn_watch(Some(Arc::clone(&turn_watch_state)));
        let mut total_usage = crate::rpc::types::TokenUsage::default();
        // The most recent step's own usage plus the input scope it served: the
        // measured context anchor a caller puts on the conversation (v2
        // `TokenCountingAgentModel.measured`).
        let mut last_request_usage: Option<crate::turn_loop::types::RequestMeasurement> = None;
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

        // Compaction requests are real LLM calls. Keep their usage apart from
        // the turn's own so session accounting can bill them without inflating
        // per-turn request totals. `did_compact` separates "no compaction" from
        // "a compaction the provider reported no usage for".
        let mut compaction_usage = crate::rpc::types::TokenUsage::default();
        let mut did_compact = false;
        // Prompt-cache observations, one per LLM response (v2
        // `llmRequesterLastTurnRequest.observation`, consumed by
        // `AgentCacheBreakService.observe`). Collected here because only the
        // step loop knows both the request that was sent and its usage; the
        // engine folds them into its per-session break tracker.
        let mut cache_observations: Vec<crate::session::cache_observation::CacheObservation> =
            Vec::new();
        // The tool schema table is a constant part of every request. The
        // compaction trigger (`ContextTokens`) and the persisted context gauge
        // both size against it, so they cannot disagree about how full the
        // context is.
        let tool_schema_tokens = serde_json::to_string(&tool_defs)
            .map(|json| crate::compaction::estimate_tokens_for_json(&json))
            .unwrap_or(0);
        // `prompt_cache_probe` compares the request's shape against the parent
        // agent's last request (v2 `llmRequesterLastTurnRequest`). The system
        // prompt is constant for the turn; the tool table can change per step,
        // so it is hashed with the step's definitions below.
        let system_prompt_hash =
            crate::session::cache_observation::fingerprint(input.llm.system_prompt());
        // Every request of the turn, in step order: the first one is what a
        // fork's first record compares against its parent, the last one is the
        // agent's new `prompt_cache_probe` baseline.
        let mut cache_probe_requests: Vec<crate::session::cache_observation::CacheProbeRequest> =
            Vec::new();
        // Observation indices where a compaction replaced the history: the
        // engine's fold drops the break baseline at each one (v2
        // `AgentCacheBreakService` resets on `context.compactionCompleted`).
        let mut cache_observation_resets: Vec<usize> = Vec::new();
        // Every exit path carries the accounting, so an early return (hook
        // block, budget limit, abort) cannot drop it.
        macro_rules! finish {
            ($($arg:expr),* $(,)?) => {{
                let mut result = turn_result($($arg),*).with_accounting(
                    if did_compact {
                        Some(compaction_usage.clone())
                    } else {
                        None
                    },
                    tool_schema_tokens,
                );
                result.cache_observations = std::mem::take(&mut cache_observations);
                result.cache_probe_requests = std::mem::take(&mut cache_probe_requests);
                result.cache_observation_resets = std::mem::take(&mut cache_observation_resets);
                result
            }};
        }

        // Build system prompt, optionally enriched with goal steering text.
        // A goal that is paused / blocked is not driven by this turn: it gets
        // the goal reminder injection instead of per-step steering, and the
        // turn runs as ordinary chat (v2 only binds a turn to an *active*
        // goal).
        let system_prompt = if let Some(ref goal) = goal
            && goal.status.is_active()
        {
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

        // v2 `shapeHistory` (`toolSelectService.ts:125-130`): a history
        // recorded under a different disclosure mode is reshaped before any
        // hook or provider sees it. With dynamic loading on, only the newest
        // `<tools_relevant>` block survives; with it off, every discovery
        // announcement is dropped so this model is never told to call
        // `select_tools`.
        let supports_dynamic_loading = input
            .toolset
            .as_ref()
            .is_some_and(|toolset| toolset.supports_dynamic_tool_loading());
        messages =
            crate::tools::tool_select_state::shape_history(messages, supports_dynamic_loading);

        if supports_dynamic_loading && let Some(toolset) = &input.toolset {
            toolset.seed_loaded_tools_from_history(&messages);
        }

        // v2 `onBeforeSubmitPrompt`: matching hooks settle before the model
        // call. A block appends the rendered assistant result and skips the
        // step loop; appends fold into content position 0 of the same prompt.
        if let Some(ref guard) = hook_guard
            && let Some(prompt_index) = messages.iter().rposition(|m| m.role == "user")
        {
            let outcome = guard
                .run_user_prompt_submit(&input.agent_id, &turn_id, &submitted_prompt)
                .await;
            if let Some(block) = outcome.block {
                messages.push(LLMMessage {
                    role: "assistant".into(),
                    content: block,
                    origin: Some(serde_json::json!({
                        "kind": "hook_result",
                        "event": "UserPromptSubmit",
                        "blocked": true,
                    })),
                    ..Default::default()
                });
                return Ok(finish!(
                    LoopTurnStopReason::EndTurn,
                    0,
                    crate::rpc::types::TokenUsage::default(),
                    0,
                    0,
                    messages,
                )
                .with_gate_blocked());
            }
            if !outcome.append.is_empty() {
                let prompt = &mut messages[prompt_index];
                // Provider projections prefer blocks over plain content. A
                // text-only prompt therefore needs its original text
                // materialized behind the new hook parts, not left only in
                // `content` where the blocks would shadow it.
                let original_parts = if prompt.blocks.is_empty() {
                    vec![ContentBlock::Text {
                        text: prompt.content.clone(),
                        meta: None,
                    }]
                } else {
                    std::mem::take(&mut prompt.blocks)
                };
                prompt.blocks = outcome.append.into_iter().chain(original_parts).collect();
            }
        }

        // Running token estimate for the injection-free history (see
        // [`ContextTokens`]). The tool-schema baseline was computed above.
        let mut context_tokens = ContextTokens::new(tool_schema_tokens);

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
        let _deadline_guard = DeadlineGuard(
            if let Some(ref g) = goal
                && g.status.is_active()
            {
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
            },
        );

        // Context compaction knobs. The window comes from the host's model
        // resolution; without it the budget falls back to the fixed default.
        // The attempt cap rides the same host-resolved context (v2 #3750).
        // v2 `getEffectiveMaxContextTokens`: an overflow observed earlier in
        // this process lowers the window for this model, so the trigger and
        // the recovery gates use the conservative value.
        let effective_window = crate::compaction::effective_max_tokens_for(
            &input.agent_id,
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
        //
        // v2 #4413 (`llmRequester.mediaRecovery`): a recovery an earlier turn
        // paid for is agent-level state keyed by model and history revision,
        // not a per-turn local — the next turn starts from the recovered
        // projection instead of re-sending the media the provider already
        // rejected. Applied to the turn's working messages (the fork's
        // in-place spelling of v2's per-request rewrite); history on disk
        // keeps the original references.
        let (mut media_degraded, mut media_stripped) = input
            .media_recovery
            .as_ref()
            .and_then(|cell| {
                let state = cell.lock().unwrap_or_else(|e| e.into_inner());
                state.applicable(input.llm.model_name()).map(|recovery| {
                    let entries = input
                        .media
                        .map(|resolver| resolver.media_entries(&messages))
                        .unwrap_or_default();
                    crate::llm::media_budget::apply_media_recovery(
                        &mut messages,
                        &entries,
                        recovery,
                    )
                })
            })
            .unwrap_or((false, false));
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
        // The session workspace root every injection roots itself in: the
        // AGENTS.md reminder probes it, and the knowledge base stores under
        // it. Not the engine process's cwd — the host can run the engine from
        // anywhere, and a session opens one workspace.
        let workspace_root = turn_workspace_root(&input);
        let mut injection_registry = crate::injection::InjectionRegistry::with_defaults(
            workspace_root.clone(),
            date_baseline,
            agents_md_baseline,
        );
        // NotifyUser nudge (v2 `notifyUserNudgeService`): round after round of
        // tool calls without an update is progress the user never sees. The
        // availability gate is the advertised table — when NotifyUser is not
        // there, the model cannot answer the nudge, so the provider stays
        // unregistered (v2 gates on the same resolved availability).
        crate::injection::notify_user_nudge::register_notify_user_nudge(
            &mut injection_registry,
            input
                .tool_defs
                .iter()
                .any(|tool| tool.name == crate::tools::core_tool_defs::NOTIFY_USER_TOOL_NAME),
        );
        // v2 `ProgressTrackerService`: once the turn has made
        // `EBM_BLIND_MUTATION_THRESHOLD` changes nobody verified, the next
        // request asks for a verification run. The reminder is gated on
        // `ebmInjectedThisTurn` (v2 `progressTrackerService.ts:71-75`), which is
        // set once and reset only by `turn.started`, so a turn that keeps
        // writing unverified files after the threshold is crossed still sees it
        // exactly once. Arming alone is not that gate: `blind` stays at or above
        // the threshold, so every later write would re-arm it and the reminder
        // would land on every following request.
        let ebm_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ebm_injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // v2 keeps the tracker on the Agent-scoped `ProgressTrackerService`;
        // the toolset carries the same session-scoped state here. Callers
        // without a toolset (host-proxy tests) fall back to a per-turn tracker.
        let mut local_progress_tracker =
            crate::turn_loop::progress_track::ProgressTracker::default();
        {
            let pending = ebm_pending.clone();
            let injected = ebm_injected.clone();
            injection_registry.register(
                "progress_reminder",
                Box::new(move |_ctx| {
                    if !pending.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        return None;
                    }
                    (!injected.swap(true, std::sync::atomic::Ordering::SeqCst))
                        .then(|| crate::turn_loop::progress_track::EBM_REMINDER.to_string())
                }),
            );
        }
        // v2 `AgentTestScopeGuardService` (`agent/testScopeGuard`): a whole
        // suite run from the repository root arms exactly one reminder for the
        // turn, cleared here once the injection has been emitted.
        let test_scope_guard =
            Arc::new(crate::turn_loop::test_scope_guard::TestScopeGuard::from_env());
        {
            let guard = test_scope_guard.clone();
            injection_registry.register(
                crate::turn_loop::test_scope_guard::TEST_SCOPE_REMINDER_VARIANT,
                Box::new(move |_ctx| guard.take_pending()),
            );
        }
        crate::injection::permission_mode::register_permission_mode_injection(
            &mut injection_registry,
            // v2's permission mode is never unset: a snapshot without a mode
            // defaults to Manual (`permissionMode.ts`), and the reminder says
            // so on the first turn. Treating `None` as "no reminder" left a
            // turn with no policy snapshot running under an unannounced mode.
            Some(
                input
                    .permission_mode
                    .unwrap_or(crate::permission::PermissionMode::Manual),
            ),
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
        // The knowledge-base reminder (v2 `KnowledgeInjection`,
        // `knowledgeService.ts:46-49`): main agent only, first step of the
        // turn, and silent unless a knowledge database already exists.
        if input.agent_id == crate::callbacks::MAIN_AGENT_ID {
            crate::injection::knowledge::register_knowledge_injection(
                &mut injection_registry,
                workspace_root.clone(),
            );
        }
        // Plugin session-start skills (v2 `AgentPluginService`,
        // `agent/plugin`): main agent only. The roots are the ones this
        // turn's toolset scans, so the injected skill is exactly the one the
        // prompt advertises; the reminder freezes after its first injection
        // and only a host-driven refresh supersedes it.
        if input.agent_id == crate::callbacks::MAIN_AGENT_ID
            && let Some(toolset) = &input.toolset
        {
            crate::injection::plugin::register_plugin_session_start(
                &mut injection_registry,
                toolset.plugin_skill_dirs(),
                toolset.session_id().unwrap_or_default(),
                &messages,
                false,
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
        // `todoReminder`): main agent only, only while `TodoList` is active.
        // v2 used to arm the reminder on the todo service's first use; the
        // upstream rewrite (`da5918c0c8`) dropped that gate — the provider
        // stays silent on its own while the list has no open items, so a
        // session that never used `TodoList` is never reminded. The state
        // read also doubles as the capability probe: a host without the
        // state bridge cannot persist todo state, so reminding the model to
        // maintain a list that cannot be saved would be worse than silence.
        let mut todo_reminder_registered = false;
        if input.agent_id == crate::callbacks::MAIN_AGENT_ID
            && input.llm.transport() != "host-proxy"
            && tool_defs
                .iter()
                .any(|def| def.name == crate::injection::todo_reminder::TODO_LIST_TOOL_NAME)
        {
            let todo_read = callbacks
                .state_read(crate::rpc::types::StateReadRequest {
                    domain: "todo".into(),
                    key: "todo".into(),
                    turn_id: String::new(),
                    tool_call_id: String::new(),
                })
                .await;
            // The read is still the capability probe this comment describes:
            // no answer means no todo persistence, so the reminder stays off.
            // What changed is that the failure is now reported instead of the
            // reminder disappearing without a trace.
            let bridged = match &todo_read {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "the todo state bridge did not answer; the stale-todo reminder is not armed"
                    );
                    false
                }
            };
            goal_plan_state.store("todo", todo_read);
            if bridged {
                crate::injection::todo_reminder::register_todo_reminder(
                    &mut injection_registry,
                    goal_plan_state.clone(),
                );
                todo_reminder_registered = true;
            }
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
        // model has not seen yet, removed names whose servers went away, or a
        // ranked `<tools_relevant>` slice for a large catalogue. The provider
        // is turn-gated (`is_new_turn`), matching v2's variant gate, and reads
        // the conversation because v2 folds the announcements already in it.
        if let Some(toolset) = &input.toolset {
            let disclosure_toolset = toolset.clone();
            injection_registry.register(
                crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
                Box::new(move |ctx| disclosure_toolset.take_disclosure_announcement(ctx.history)),
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

        // v2 `lastCompactedTokenCount` (`fullCompactionService.ts:197-201`):
        // once a turn has compacted, another auto compaction is pointless until
        // the history grows past what that round produced — otherwise a
        // summary+tail that still exceeds the window compacts again every step
        // and shrinks the context toward nothing. v2 resets it per turn
        // (`resetForTurn`, :439-443); this is a turn-local, so the same.
        let mut last_compacted_token_count: Option<u32> = None;

        // v2 `compactionRearmPending`: a compaction that spliced the history
        // during the previous step re-arms the turn-scoped reminders for the
        // next injection pass, because it can drop the injection messages the
        // splice crossed. The flag is consumed by exactly one pass and never
        // re-fires per step — a rearm is an event, not the turn head.
        let mut rearm_pending = false;

        for step_num in 0..max_steps {
            steps = step_num + 1;
            turn_watch_state.note_steps(steps as u64);
            let turn_wall_clock_ms = elapsed_wall_clock_ms(turn_started);
            // Whether this step's injection pass runs in rearm mode. A
            // compaction below (before the pass) sets it for this step; the
            // emergency overflow path, which compacts after the pass, sets
            // `rearm_pending` for the next one.
            let mut rearm_this_step = std::mem::take(&mut rearm_pending);

            // ── Goal budget check ──────────────────────────────────────────
            // Only an *active* goal drives this turn. v2 binds a turn to the
            // goal that was active when it started; a goal paused or blocked
            // before the turn is ordinary chat (the host cancels a live turn
            // itself when it pauses the goal mid-flight).
            if let Some(ref goal) = goal
                && goal.status.is_active()
            {
                // v2 `stopAfterBudgetReached` compares the goal's own
                // counters: `tokensUsed` accumulates output tokens, and the
                // turn budget counts whole goal turns (the current turn is
                // folded in at turn end, not before it starts).
                let turn_tokens = total_usage.output_tokens as i64;
                let turns_this_turn = 0;
                if goal.would_exceed_budget(turn_tokens, turns_this_turn, turn_wall_clock_ms) {
                    callbacks.emit_event(serde_json::json!({
                        "type": "goal.budget.limit_reached",
                        "turn_id": turn_id,
                        "goal_id": goal.goal_id,
                    }));
                    return Ok(finish!(
                        LoopTurnStopReason::BudgetLimited,
                        step_num,
                        total_usage,
                        0,
                        llm_retries,
                        messages.clone(),
                    )
                    .with_last_request_usage(last_request_usage.clone()));
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
                return Ok(finish!(
                    LoopTurnStopReason::Aborted,
                    step_num,
                    total_usage,
                    0,
                    llm_retries,
                    messages.clone(),
                )
                .with_last_request_usage(last_request_usage.clone()));
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
            if last_compacted_token_count.is_none_or(|last| context_tokens.tokens() > last)
                && crate::compaction::should_compact_auto(
                    &messages,
                    context_tokens.tokens(),
                    &compaction_config,
                )
            {
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
                    guard
                        .notify_pre_compact(&turn_id, messages.len(), context_tokens.tokens())
                        .await;
                }
                // v2 `postProcessSummary` reads the todo state at compaction
                // time; a host without the bridge or without todos degrades
                // to a plain summary.
                let todos = crate::compaction::read_todos_for_summary(callbacks.as_ref()).await;
                // v2 `nonHistoryStateFingerprint`: the todo/task state this
                // round started from. A write during the summarizer call means
                // the summary's rendering is already stale.
                let non_history_state = non_history_state_fingerprint(callbacks.as_ref()).await;
                turn_watch_state.set_phase(TurnPhase::Compacting);
                match crate::compaction::compact_messages_with_summary_at_report(
                    &messages,
                    context_tokens.tokens(),
                    &compaction_config,
                    input.llm,
                    None,
                    Some(turn_cancel.token()),
                    effective_window,
                    todos,
                    &input.agent_id,
                    // The turn loop is inside the running turn: its
                    // post-last-user messages are v2's `inFlightTurn`.
                    true,
                    // v2 `llmRequesterService` captures the agent's last turn
                    // request as the compaction probe's baseline before the
                    // summarizer request runs.
                    cache_probe_requests.last(),
                    Some(turn_id.as_str()),
                )
                .await
                {
                    Ok(Some((compacted, report))) => {
                        // The summarizer request's `prompt_cache_probe` (v2
                        // `source: 'compaction'`) is about the request, so it is
                        // reported even when the round is cancelled below.
                        if let Some(probe) = report.cache_probe.as_ref() {
                            callbacks.telemetry(probe.telemetry_payload());
                        }
                        // The summarizer request was paid for even when the
                        // round is cancelled below, so bill it either way.
                        if let Some(usage) = &report.usage {
                            compaction_usage.accumulate(usage);
                            did_compact = true;
                        }
                        if non_history_state_fingerprint(callbacks.as_ref()).await
                            != non_history_state
                        {
                            // v2 `applyCompactionRound` (`:53-54`): state moved
                            // while the summary was generated — cancel instead
                            // of applying a summary that would silently
                            // overwrite it. `last_compacted_token_count` stays
                            // unset, so the next step retries against the
                            // updated state.
                            callbacks.emit_event(serde_json::json!({
                                "type": "compaction.cancelled",
                            }));
                        } else {
                            let result = {
                                let mut result = serde_json::json!({
                                    "summary": report.summary,
                                    "compactedCount": report.compacted_count,
                                    "tokensBefore": report.tokens_before,
                                    "tokensAfter": report.tokens_after,
                                });
                                if report.dropped_count > 0 {
                                    result["droppedCount"] =
                                        serde_json::json!(report.dropped_count);
                                }
                                result
                            };
                            callbacks.emit_event(serde_json::json!({
                                "type": "compaction.completed",
                                "result": result,
                                // v2 persists the spliced shape with the compaction
                                // (`contextMemoryService.ts:115-134`,
                                // `context.compaction` carries `messages`): a replay
                                // that only kept the summary lost the kept user
                                // messages and the continuation note. The native
                                // event store folds this array at its boundary.
                                "messages": &compacted,
                            }));
                            // v2 `compaction_finished`: the success telemetry for
                            // the round that just landed, usage included.
                            callbacks.telemetry(report.finished_payload(
                                "auto",
                                1,
                                input.llm.thinking_effort().unwrap_or("none"),
                                crate::turn_loop::turn_index(&turn_id),
                            ));
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
                                    .notify_post_compact(
                                        &turn_id,
                                        "auto",
                                        report.tokens_after as u64,
                                    )
                                    .await;
                            }
                            // The compacted shape already closes its block with
                            // the continuation note (v2 `buildContextCompactionShape`
                            // splices exactly `result.messages`), so no second
                            // continuation is appended here. What the splice
                            // does require is v2's `compactionRearmPending`:
                            // the injection pass below re-runs the turn-scoped
                            // providers with `isNewTurn = true` for this step,
                            // which is how a reminder the compaction dropped
                            // comes back.
                            messages = compacted;
                            rearm_this_step = true;
                            // The context was replaced: a fork captured before
                            // this point must no longer compare against the old
                            // parent request (v2's `prefixRevision` check).
                            if let Some(manager) = input
                                .toolset
                                .as_ref()
                                .and_then(|toolset| toolset.subagent_manager())
                            {
                                manager.bump_probe_generation(&input.agent_id);
                            }
                            // The engine's end-of-turn fold resets the break
                            // baseline here, so the post-compaction steps start
                            // a new trajectory.
                            cache_observation_resets.push(cache_observations.len());
                            sink_push_reset(&sink, sink_base + cache_observations.len());
                            // The request that produced the measurement above
                            // served the pre-splice prefix; v2's identity check
                            // (`matchesContext`) would reject it once the
                            // context is replaced.
                            if let Some(measurement) = last_request_usage.as_mut() {
                                measurement.spliced_after = true;
                            }
                            context_tokens.rebase_with_summary(
                                &messages,
                                report.usage.as_ref().map(|usage| usage.output_tokens),
                            );
                            // v2 `compactionWorker` (:551-558): remember what the
                            // round actually produced so the next `checkAutoCompaction`
                            // does not fire while the history still sits at or below it.
                            last_compacted_token_count = Some(context_tokens.tokens());
                        }
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
                // v2 #4364: a new turn's tool announcement must wait for the
                // MCP servers' first discovery — announcing before it told the
                // model a partial (often empty) loadable set. The provider
                // itself is sync, so the await sits here at the injection pass
                // the announcement is computed in.
                if step_num == 0
                    && let Some(toolset) = &input.toolset
                {
                    toolset.wait_for_mcp_discovery_before_disclosure().await;
                }
                // Refresh the state snapshots through the host callbacks so
                // the injections render the state the tools just wrote — the
                // same channel, so a mid-turn plan exit, goal pause, or todo
                // update shows up at this step head. A failed read is stored
                // as `Unreadable`, so the provider reports the skip instead of
                // rendering stale state as if it were current. The turn-start
                // todo read already covers step 0.
                goal_plan_state
                    .refresh(callbacks.as_ref(), todo_reminder_registered && step_num > 0)
                    .await;
                // `step_num` is zero-based: the turn's first step is 0, not 1.
                // Passing `step_num == 1` here made every turn-scoped provider
                // (the goal reminder is the only one) inject a step late — so
                // on a normal one-step turn it never fired at all.
                //
                // A compaction splice earlier in this step re-arms the
                // turn-scoped providers for this single pass (v2
                // `compactionRearmPending`; see `rearm_this_step`). The
                // providers that derive their state from the conversation
                // re-read it, so a reminder the splice dropped is injected
                // again.
                //
                // The AGENTS.md reminder additionally needs the directories the
                // previous step's tool calls touched; without them it saw an
                // empty world and only ever probed the workspace root.
                let is_new_turn = step_num == 0 || rearm_this_step;
                if rearm_this_step {
                    // v2's injected positions are read off the conversation,
                    // which the splice just replaced; the registry's per-turn
                    // list has to forget the passes the splice undid.
                    injection_registry.clear_injected();
                }
                for (variant, text) in injection_registry.build_injections_rearmed(
                    is_new_turn,
                    rearm_this_step,
                    &step_access,
                    &messages,
                ) {
                    messages.push(crate::injection::injection_message_named(text, &variant));
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
            let telemetry = |event: serde_json::Value| {
                if event.get("event").and_then(|value| value.as_str()) == Some("TurnStepRetrying") {
                    let retry_step = event
                        .get("step")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0) as u32;
                    callbacks.emit_event(serde_json::json!({
                        "type": "llm.step.retrying",
                        "agent_id": input.agent_id,
                        "turn_id": turn_id,
                        "step": retry_step,
                        "step_id": step_event_id(&turn_id, retry_step),
                        "failed_attempt": event["failed_attempt"],
                        "next_attempt": event["next_attempt"],
                        "max_attempts": event["max_attempts"],
                        "delay_ms": event["delay_ms"],
                        "error_name": event["error_name"],
                        "error_message": event["error_message"],
                        "status_code": event["status_code"],
                    }));
                }
                callbacks.telemetry(event);
            };
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
            // Wall-clock start of the request, for `prompt_cache_probe`'s
            // `parent_request_gap_ms` (v2 records `startedAt` per request).
            let step_started_at = chrono::Utc::now().timestamp_millis();
            // v2 `failMachineStep` (loopService.ts:2160-2171): a terminal step
            // failure closes the step it began before the turn ends, carrying
            // the error text the user will see. Every step-block failure
            // return goes through this funnel so none can forget it.
            macro_rules! fail_current_step {
                ($err:expr) => {{
                    // Bind once: the error expression may move a value, and
                    // naming it twice would move it twice.
                    let failure = $err;
                    let error_text = failure.to_string();
                    emit_step_interrupted_event(
                        callbacks.as_ref(),
                        &input.agent_id,
                        &turn_id,
                        steps,
                        "error",
                        Some(&error_text),
                    );
                    return Err(Box::new(failure) as Box<dyn std::error::Error + 'a>);
                }};
            }
            let step_result = 'overflow_recovery: loop {
                // Set inside the loop: an overflow round leaves the phase at
                // `Compacting`, and the retried request is an LLM wait again.
                turn_watch_state.set_phase(TurnPhase::Llm);
                let request_messages = match budgeted_request(
                    &mut media_budget,
                    input.media,
                    input.llm,
                    &messages,
                    callbacks.as_ref(),
                    Some(turn_cancel.token()),
                )
                .await
                {
                    Ok(Some(messages)) => messages,
                    // v2 #4183: interrupting the turn cancels an in-flight
                    // media upload; the turn aborts cleanly (or reports the
                    // goal deadline) instead of failing or waiting it out.
                    Ok(None) => {
                        let is_goal_deadline = goal.as_ref().is_some_and(|g| {
                            g.wall_clock_budget_ms
                                .is_some_and(|budget| elapsed_wall_clock_ms(turn_started) >= budget)
                        });
                        let stop_reason = if is_goal_deadline {
                            LoopTurnStopReason::BudgetLimited
                        } else {
                            LoopTurnStopReason::Aborted
                        };
                        emit_step_interrupted_event(
                            callbacks.as_ref(),
                            &input.agent_id,
                            &turn_id,
                            steps,
                            "aborted",
                            None,
                        );
                        return Ok(finish!(
                            stop_reason,
                            steps,
                            total_usage,
                            0,
                            llm_retries,
                            messages.clone(),
                        )
                        .with_last_request_usage(last_request_usage.clone()));
                    }
                    // v2 `isMediaUploadAuthError` → throw: a rejected upload
                    // credential fails the turn with the upload's own error
                    // rather than degrading the media to a path tag.
                    Err(error) => {
                        fail_current_step!(error);
                    }
                };
                let call = execute_loop_step_with_projection(
                    &turn_id,
                    step_num,
                    input.llm,
                    &request_messages,
                    input.tools,
                    &step_tool_defs,
                    &retry_config,
                    Some(turn_cancel.token()),
                    Some(&telemetry),
                    Some(crate::compaction::estimate_messages_tokens(
                        &request_messages,
                    )),
                    input.max_context_tokens,
                    Some(total_usage.input_tokens),
                    structure_strict,
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
                        emit_step_interrupted_event(
                            callbacks.as_ref(),
                            &input.agent_id,
                            &turn_id,
                            steps,
                            "aborted",
                            None,
                        );
                        return Ok(finish!(
                            stop_reason,
                            steps,
                            total_usage,
                            0,
                            llm_retries,
                            messages.clone(),
                        )
                        .with_last_request_usage(last_request_usage.clone()));
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
                        let entries = input
                            .media
                            .map(|resolver| resolver.media_entries(&messages));
                        let visible_media = entries.as_deref().map_or(0, <[_]>::len);
                        // v2 #4456 `canDegradeMedia`: under an inherited
                        // degrade a fresh 413 re-runs the degrade only when
                        // more than KEEP_RECENT media are still visible —
                        // otherwise the ladder goes straight to the strip.
                        let can_degrade = !media_degraded
                            || visible_media > crate::llm::media_budget::MEDIA_DEGRADE_KEEP_RECENT;
                        let projection = if too_large && can_degrade {
                            media_degraded = true;
                            let degraded = entries.as_deref().map_or(0, |entries| {
                                crate::llm::media_budget::degrade_older_media(
                                    &mut messages,
                                    entries,
                                    crate::llm::media_budget::MEDIA_DEGRADE_KEEP_RECENT,
                                )
                            });
                            if degraded == 0 {
                                fail_current_step!(std::io::Error::other(err_str));
                            }
                            // v2 #4413 `markMediaRecovery`: the recovery the
                            // provider just forced is agent-level state, so the
                            // next turn starts from it instead of re-sending
                            // the rejected media. v2 #4456 pins the degrade
                            // count at capture time: recomputing it per request
                            // would let a media added later push an older kept
                            // one out and break the prompt-cache prefix.
                            if let Some(cell) = &input.media_recovery {
                                cell.lock().unwrap_or_else(|e| e.into_inner()).mark(
                                    input.llm.model_name().to_string(),
                                    crate::llm::media_budget::MediaRecoveryAction::Degraded(
                                        degraded,
                                    ),
                                );
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
                                fail_current_step!(std::io::Error::other(err_str));
                            }
                            // v2 #4413: the strip snapshot is the captured key
                            // set, so media added by a later turn stay visible
                            // until a rejection of their own.
                            if let Some(cell) = &input.media_recovery {
                                let mut keys: Vec<String> = entries
                                    .as_deref()
                                    .unwrap_or_default()
                                    .iter()
                                    .map(|entry| entry.key.clone())
                                    .collect();
                                // v2 re-captures from the pre-projection
                                // history, where the media an inherited strip
                                // already replaced still exist. The fork's
                                // strip runs in place, so this turn's entries
                                // name only the media that were still visible —
                                // merge the inherited keys back or the next
                                // turn would resend the older media.
                                let mut state = cell.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(existing) = &state.recovery
                                    && existing.model == input.llm.model_name()
                                    && existing.context_revision == state.revision
                                    && let crate::llm::media_budget::MediaRecoveryAction::Stripped(
                                        old_keys,
                                    ) = &existing.action
                                {
                                    for key in old_keys {
                                        if !keys.contains(key) {
                                            keys.push(key.clone());
                                        }
                                    }
                                }
                                state.mark(
                                    input.llm.model_name().to_string(),
                                    crate::llm::media_budget::MediaRecoveryAction::Stripped(keys),
                                );
                            }
                            crate::llm::media_budget::MEDIA_STRIPPED_CODE
                        } else {
                            fail_current_step!(std::io::Error::other(err_str));
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
                        callbacks.emit_event(serde_json::json!({
                            "type": "warning",
                            "code": crate::llm::request_structure::STRUCTURE_STRICT_CODE,
                            "message": "Provider rejected the structure of the request; the request was retried with a strict projection.",
                        }));
                        continue 'overflow_recovery;
                    }
                    let estimated_request_tokens =
                        crate::compaction::estimate_messages_tokens(&messages);
                    if !crate::compaction::should_recover_from_context_overflow(
                        &err_str,
                        estimated_request_tokens,
                        effective_window,
                    ) {
                        fail_current_step!(std::io::Error::other(err_str));
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
                        fail_current_step!(std::io::Error::other(message));
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
                        guard
                            .notify_pre_compact(&turn_id, messages.len(), estimated_request_tokens)
                            .await;
                    }
                    // Same todo suffix as the threshold path (v2
                    // `postProcessSummary`), read at compaction time.
                    let todos = crate::compaction::read_todos_for_summary(callbacks.as_ref()).await;
                    // Same fingerprint as the threshold path: the overflow
                    // recovery round must not overwrite a todo/task write that
                    // landed while the summary was generated.
                    let non_history_state = non_history_state_fingerprint(callbacks.as_ref()).await;
                    // v2's `compaction_failed` carries `duration_ms`: the time
                    // until the failure, measured around the failing call.
                    let compaction_started = std::time::Instant::now();
                    turn_watch_state.set_phase(TurnPhase::Compacting);
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
                            &input.agent_id,
                            true,
                            cache_probe_requests.last(),
                            Some(turn_id.as_str()),
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
                            // `thinking_effort` is required by v2's schema and
                            // is read off the transport actually serving this
                            // turn; `none` is the same spelling the wire uses
                            // when no effort is in effect, so it states the
                            // truth rather than omitting a required property.
                            // v2's `CompactionFailedEvent.retry_count` is the
                            // backoff retries this round performed, carried by
                            // the failure itself; a cancellation or an empty
                            // summary spent none.
                            let summarizer_error = match &error {
                                crate::compaction::CompactionError::Provider {
                                    error: inner,
                                    ..
                                } => Some(inner.as_ref()),
                                _ => None,
                            };
                            let failure_retry_count = match &error {
                                crate::compaction::CompactionError::Provider {
                                    retry_count,
                                    ..
                                } => *retry_count,
                                _ => 0,
                            };
                            let mut payload = serde_json::json!({
                                "event": "compaction_failed",
                                "source": "auto",
                                "tokens_before": crate::compaction::estimate_messages_tokens(&messages),
                                "duration_ms": compaction_started.elapsed().as_millis() as u64,
                                "round": consecutive_overflow_compactions,
                                "retry_count": failure_retry_count,

                                "error_type": format!("{error:?}"),
                                "thinking_effort": input.llm.thinking_effort().unwrap_or("none"),
                            });
                            if let Some(index) = crate::turn_loop::turn_index(&turn_id) {
                                payload["turn_id"] = serde_json::json!(index);
                            }
                            // The summarizer request's trace id, when the
                            // failure carried one (v2 `request_id`).
                            if let Some(id) = summarizer_error
                                .and_then(|inner| inner.downcast_ref::<kimi_core::error::LlmError>())
                                .and_then(kimi_core::error::LlmError::trace_id)
                            {
                                payload["request_id"] = serde_json::json!(id);
                            }
                            callbacks.telemetry(payload);
                            fail_current_step!(error);
                        }
                    };
                    // The recovery summarizer request's `prompt_cache_probe`
                    // (v2 `source: 'compaction'`) describes the request, so it
                    // is reported regardless of whether the round is applied.
                    if let Some(probe) = report.cache_probe.as_ref() {
                        callbacks.telemetry(probe.telemetry_payload());
                    }
                    // The recovery summarizer call is billable whether or not
                    // the round below is applied.
                    if let Some(usage) = &report.usage {
                        compaction_usage.accumulate(usage);
                        did_compact = true;
                    }
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
                        fail_current_step!(std::io::Error::other(err_str));
                    }
                    // v2 `applyCompactionRound` (`:53-54`): a state write
                    // during summarization cancels the round. The original
                    // overflow stands, so the step fails the same way an
                    // unrecovered overflow does.
                    if non_history_state_fingerprint(callbacks.as_ref()).await != non_history_state
                    {
                        callbacks.emit_event(serde_json::json!({
                            "type": "compaction.cancelled",
                        }));
                        fail_current_step!(std::io::Error::other(err_str));
                    }
                    let result = {
                        let mut result = serde_json::json!({
                            "summary": report.summary,
                            "compactedCount": report.compacted_count,
                            "tokensBefore": report.tokens_before,
                            "tokensAfter": report.tokens_after,
                        });
                        if report.dropped_count > 0 {
                            result["droppedCount"] = serde_json::json!(report.dropped_count);
                        }
                        result
                    };
                    callbacks.emit_event(serde_json::json!({
                        "type": "compaction.completed",
                        "result": result,
                        // Same as the auto path: replay needs the spliced shape,
                        // not just the summary.
                        "messages": &force_compacted,
                    }));
                    // v2 `compaction_finished` for the recovery round that
                    // just landed (the round index is the overflow attempt).
                    callbacks.telemetry(report.finished_payload(
                        "auto",
                        consecutive_overflow_compactions,
                        input.llm.thinking_effort().unwrap_or("none"),
                        crate::turn_loop::turn_index(&turn_id),
                    ));
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
                    // The injection pass already ran for this step, so v2's
                    // rearm lands on the next pass; the retried request inside
                    // this step sees the compacted prefix without re-running
                    // the providers, exactly like a post-compaction step head.
                    rearm_pending = true;
                    // Same context replacement as the threshold path: forks
                    // captured before this round stop comparing against the
                    // old parent request.
                    if let Some(manager) = input
                        .toolset
                        .as_ref()
                        .and_then(|toolset| toolset.subagent_manager())
                    {
                        manager.bump_probe_generation(&input.agent_id);
                    }
                    // The end-of-turn fold resets the break baseline at this
                    // boundary, so the retry's cache read is not compared
                    // with the pre-compaction one.
                    cache_observation_resets.push(cache_observations.len());
                    sink_push_reset(&sink, sink_base + cache_observations.len());
                    // The overflow retry replaces the prefix the previous
                    // request measured: the anchor no longer describes the
                    // context v2's `matchesContext` would compare it with.
                    if let Some(measurement) = last_request_usage.as_mut() {
                        measurement.spliced_after = true;
                    }
                    context_tokens.rebase_with_summary(
                        &messages,
                        report.usage.as_ref().map(|usage| usage.output_tokens),
                    );
                }
            };
            // The step produced a result, so the window fits: v2 `afterStep`
            // clears the consecutive-overflow counter here.
            consecutive_overflow_compactions = 0;

            total_usage.accumulate(&step_result.usage);
            // Anchor the measurement to the input this request served
            // (`messages` at this point is the request, system message
            // included) instead of the turn's final transcript, which still
            // grows by the reply and any tool exchanges below. v2
            // `TokenCountingAgentModel.measured`: length = `context.length`.
            last_request_usage = Some(crate::turn_loop::types::RequestMeasurement {
                usage: step_result.usage.clone(),
                input_messages: messages.len(),
                spliced_after: false,
            });
            // Re-anchor the compaction estimate on what the provider actually
            // charged for this step's input (system prompt + tool schema + the
            // history as projected). The messages appended below are then
            // estimated on top of the measurement.
            // A transport that reports no usage leaves the heuristic in place.
            if step_result.usage.input_tokens > 0 {
                context_tokens.record_measured(step_result.usage.input_tokens, &messages);
            }
            llm_retries += step_result.attempts.saturating_sub(1);
            last_finish_reason = step_result.finish_reason.clone();

            // Cache observation for this request (v2 `buildCacheObservation`):
            // the model/projection the turn ran with, the history the request
            // carried, and the cache buckets the provider charged. `messages`
            // here is still the request; the reply/tool exchanges are appended
            // below.
            {
                let request_json = serde_json::to_string(&messages).unwrap_or_default();
                let history_hash = crate::session::cache_observation::fingerprint(&request_json);
                let observation = crate::session::cache_observation::build_cache_observation(
                    crate::session::cache_observation::CacheObservationInput {
                        model_alias: input
                            .llm
                            .model_alias()
                            .unwrap_or_else(|| input.llm.model_name()),
                        protocol: input.llm.protocol(),
                        provider_type: input.llm.provider_type(),
                        message_count: messages.len(),
                        history_hash: &history_hash,
                        projection: crate::session::cache_observation::projection_label(
                            structure_strict,
                            media_degraded,
                            media_stripped,
                        ),
                        thinking_effort: input.llm.thinking_effort(),
                        usage: Some(&step_result.usage),
                    },
                );
                sink_push_observation(&sink, &observation);
                cache_observations.push(observation);
                // This request is also what `prompt_cache_probe` compares: a
                // fork inherits the latest one as its baseline, and the agent's
                // own last request replaces it for the next comparison (v2
                // `llmRequesterLastTurnRequest`).
                let probe_request = crate::session::cache_observation::CacheProbeRequest {
                    model_alias: input
                        .llm
                        .model_alias()
                        .unwrap_or_else(|| input.llm.model_name())
                        .to_string(),
                    tools_hash: crate::session::cache_observation::fingerprint(
                        &serde_json::to_string(&step_tool_defs).unwrap_or_default(),
                    ),
                    system_prompt_hash: system_prompt_hash.clone(),
                    message_count: messages.len(),
                    history_hash,
                    input_tokens: Some(crate::session::cache_observation::input_total(
                        &step_result.usage,
                    )),
                    started_at: step_started_at,
                    finished_at: chrono::Utc::now().timestamp_millis(),
                };
                // Main-agent turns expose the manager through their toolset;
                // publishing per step is what lets a fork spawned by this
                // step's tool call capture the parent's last request exactly
                // as v2 does at fork-scope creation. Subagent turns carry no
                // toolset, so the manager publishes their last request when
                // the turn returns.
                if let Some(manager) = input
                    .toolset
                    .as_ref()
                    .and_then(|toolset| toolset.subagent_manager())
                {
                    manager.record_probe_request(&input.agent_id, probe_request.clone());
                }
                sink_push_probe_request(&sink, &probe_request);
                cache_probe_requests.push(probe_request);
            }

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
                        guard
                            .notify_stop(&input.agent_id, &turn_id, "", "", "stop")
                            .await
                    } else {
                        None
                    };
                    let mut result = finish!(
                        turn_stop_reason_from_finish(step_result.finish_reason.as_deref()),
                        steps,
                        total_usage,
                        0,
                        llm_retries,
                        messages.clone(),
                    )
                    .with_last_request_usage(last_request_usage.clone());
                    result.stop_hook_continuation = stop_hook_continuation;
                    return Ok(result);
                }
                LoopStepStopReason::ToolCalls(mut tool_calls) => {
                    // v2 `parseToolCallArguments` reads a `null` payload as an
                    // absent one. Normalizing before the message is pushed keeps
                    // what we replay to the provider identical to what the loop
                    // judged.
                    for tc in &mut tool_calls {
                        if tc.arguments.is_null() {
                            tc.arguments = serde_json::json!({});
                        }
                    }

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

                    // v2 `preflightToolCall` (`toolExecutorService.ts`): a call
                    // whose arguments did not decode, or decoded to something
                    // other than an object, is neither executed nor dropped. The
                    // assistant message above keeps its `tool_use` — a strict
                    // provider rejects a `tool_use` with no matching
                    // `tool_result` — and the call is answered with an error the
                    // model can read and correct. Dropping it (what the streaming
                    // transports used to do) gave the model nothing; executing a
                    // fabricated `{}` would run Bash/Write with no inputs.
                    //
                    // The refusal is recorded against the call's position rather
                    // than answered here, because the call keeps its place in the
                    // dedup plan: v2 `registerSkipped` registers a call the
                    // before-hook never saw, so a turn that keeps re-issuing one
                    // undecodable call still earns the repeat ladder and is
                    // force-stopped at 12 (`toolDedupe.test.ts:1131`). Answering
                    // it outside the plan — what this used to do — hid the loop
                    // from the breaker and treated every repeat as the first.
                    //
                    // It is built *after* the gates that remove calls from the
                    // batch: the vector is indexed by position, and the dedup
                    // plan below is built from the surviving calls, so a refusal
                    // recorded before a `retain` would be read against the wrong
                    // call and leave the plan one entry short.

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
                    // while swarm mode is active, a model response may issue
                    // `AgentSwarm` only on its own. While it is off the shape
                    // veto is skipped entirely, so sibling tools still run and
                    // AgentSwarm receives its own mode-off refusal at execution.
                    if crate::swarm::mode::swarm_mode_registry().is_active(&input.agent_id)
                        && let Some(veto) = crate::swarm::mode::veto_swarm_batch(
                            &tool_calls
                                .iter()
                                .map(|tc| tc.name.as_str())
                                .collect::<Vec<_>>(),
                        )
                    {
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

                    // Built here, after every gate that can remove a call from the
                    // batch, so `refusals[i]` names `tool_calls[i]` and the dedup
                    // plan built from those same calls stays index-aligned.
                    let mut refusals: Vec<Option<String>> = tool_calls
                        .iter()
                        .map(|tc| {
                            unusable_tool_arguments(tc, &step_tool_defs, &tool_defs)
                                .map(|reason| reason.message(&tc.name))
                        })
                        .collect();

                    // Tool-call dedup plan (v2 `toolDedupeService`, G-6 #2):
                    // identical calls inside this step never execute twice —
                    // a repeat awaits the original's result instead. Scoped
                    // to natively-executable names: calls forwarded to the
                    // host stay under the host's own dedup service. Cells
                    // carry each call's result by its position in
                    // `tool_calls` (the scheduler preserves call order).
                    let dedupe_plan = tool_dedupe.plan_step_by(&tool_calls, |tc| {
                        crate::tools::is_native_tool_name(&tc.name)
                            .then(|| crate::tools::tool_dedupe::dedupe_key(tc))
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

                    // v2 `AgentGoalJudgeService` through `UpdateGoal`
                    // (`updateGoalTool.ts:68-83`): marking a goal `complete`
                    // is verified against the transcript by an independent
                    // judge request before the write. A rejection never
                    // reaches the goal service; an `impossible` verdict turns
                    // the write into `blocked`. The judge fails open (v2:
                    // network/timeout allows completion).
                    let mut synthetic_results: Vec<Option<ExecutableToolResult>> =
                        tool_calls.iter().map(|_| None).collect();
                    if input.agent_id == crate::callbacks::MAIN_AGENT_ID {
                        for index in 0..tool_calls.len() {
                            let (name, arguments, call_id) = {
                                let tc = &tool_calls[index];
                                (tc.name.clone(), tc.arguments.clone(), tc.id.clone())
                            };
                            if refusals[index].is_some()
                                || dedupe_plan
                                    .original_of
                                    .get(index)
                                    .is_some_and(|original| *original != index)
                                || !name.eq_ignore_ascii_case("UpdateGoal")
                                || arguments.get("status").and_then(|value| value.as_str())
                                    != Some("complete")
                            {
                                continue;
                            }
                            // The judge checks the goal as it exists right now
                            // (v2 `goalAtExecution`); an unreadable goal is
                            // left to the write's own error.
                            let goal = callbacks
                                .state_read(crate::rpc::types::StateReadRequest {
                                    domain: "goal".into(),
                                    key: "goal".into(),
                                    turn_id: turn_id.clone(),
                                    tool_call_id: call_id.clone(),
                                })
                                .await
                                .ok()
                                .and_then(|response| {
                                    crate::goal::judge::goal_snapshot_from_domain(&response.value)
                                });
                            let Some(goal) = goal else {
                                continue;
                            };
                            let system_offset = usize::from(
                                messages
                                    .first()
                                    .is_some_and(|message| message.role == "system"),
                            );
                            let transcript = messages[system_offset..].to_vec();
                            let verdict = crate::goal::judge::evaluate_transcript(
                                input.llm,
                                &transcript,
                                &goal,
                                Some(turn_cancel.token().clone()),
                            )
                            .await;
                            if verdict.ok {
                                continue;
                            }
                            if verdict.impossible {
                                // v2 blocks with the judge's reason; the state
                                // bridge's update action carries no reason
                                // field, so only the status is recorded here.
                                tool_calls[index].arguments =
                                    serde_json::json!({ "status": "blocked" });
                                continue;
                            }
                            // The host still sees the call start/complete so
                            // its transcript card is not lost; the result
                            // below answers the call without touching state.
                            callbacks.emit_event(serde_json::json!({
                                "type": "tool.call.started",
                                "agent_id": input.agent_id,
                                "turn_id": turn_id,
                                "tool_call_id": call_id,
                                "tool_name": name,
                                "args": arguments,
                            }));
                            synthetic_results[index] = Some(ExecutableToolResult {
                                spill: None,
                                parts: Vec::new(),
                                delivery: None,
                                stop_turn: false,
                                content: format!(
                                    "Goal completion rejected by judge: {}\nContinue working \
                                     toward the goal objective.",
                                    verdict.reason
                                ),
                                is_error: false,
                                note: None,
                                display: None,
                            });
                        }
                    }
                    let synthetic_indices: std::collections::HashSet<usize> = synthetic_results
                        .iter()
                        .enumerate()
                        .filter_map(|(index, result)| result.is_some().then_some(index))
                        .collect();

                    let tool_descriptions: std::collections::HashMap<String, String> =
                        step_tool_defs
                            .iter()
                            .map(|def| (def.name.clone(), def.description.clone()))
                            .collect();
                    let workspace_dir = input
                        .toolset
                        .as_ref()
                        .map(|toolset| toolset.workspace_root().to_string_lossy().into_owned());

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
                        let tool_descriptions = tool_descriptions.clone();
                        let workspace_dir = workspace_dir.clone();
                        let toolset = input.toolset.clone();
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
                            let toolset = toolset.clone();
                            let tool_descriptions = tool_descriptions.clone();
                            let workspace_dir = workspace_dir.clone();
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
                                            ExecutableToolResult { spill: None, delivery: None,
                                                parts: Vec::new(),
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
                                let resolution = crate::tools::resolve::resolve_tool_execution(
                                    &tc.name,
                                    &tc.arguments,
                                    workspace_dir.as_ref().map(String::as_str),
                                );
                                let call_description = resolution
                                    .description
                                    .or_else(|| {
                                        if tc.name.starts_with("mcp__") {
                                            tool_descriptions.get(&tc.name).cloned()
                                        } else {
                                            None
                                        }
                                    })
                                    .filter(|description| !description.is_empty());
                                let call_display = resolution.display;
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
                                    if let Some(display) = call_display
                                        && let Ok(display) = serde_json::to_value(display)
                                        && let Some(map) = started.as_object_mut()
                                    {
                                        map.insert("display".into(), display);
                                    }
                                    if let Some(map) = started.as_object_mut()
                                        && let Some(description) = call_description
                                    {
                                        map.insert("description".into(), description.into());
                                    }
                                    callbacks.emit_event(started.clone());
                                    let mut outcome = callbacks.execute_tool(req.clone()).await;
                                    // v2 B15 (loopService.ts:1798-1845): a call
                                    // answered with the canned
                                    // `unknown tool: <name>` result is
                                    // re-executed once the tool-execution
                                    // chain drains, because a late MCP
                                    // registration makes the name callable
                                    // after the first attempt. Re-announce the
                                    // start so the retry reads exactly like
                                    // v2's re-execution.
                                    if let Ok(first) = &outcome
                                        && is_unknown_tool_refusal(
                                            &first.content,
                                            first.is_error,
                                            &tc.name,
                                        )
                                        && let Some(mcp) = toolset
                                            .as_ref()
                                            .and_then(|toolset| toolset.mcp_manager())
                                    {
                                        mcp.wait_for_initial_load().await;
                                        callbacks.emit_event(started);
                                        outcome = callbacks.execute_tool(req).await;
                                    }
                                    match outcome {
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
                                                spill: None,
                                                parts: Vec::new(),
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
                                                spill: None,
                                                parts: Vec::new(),
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
                    // Tower protocol guard: destructive shell commands are
                    // refused before scheduling. Unowned workspaces skip the
                    // parser entirely; an owned but unparseable command is
                    // refused conservatively by the pure tower analyzer.
                    let mut schedulable = Vec::with_capacity(tool_calls.len());
                    for (index, tc) in tool_calls.iter().enumerate() {
                        // Answered by the preflight refusal above; the tower
                        // guard reads `command`, which an undecodable call does
                        // not have. A judge-rejected goal write is answered
                        // with its synthetic result instead of executing.
                        if refusals[index].is_some() || synthetic_results[index].is_some() {
                            continue;
                        }
                        if tc.name.eq_ignore_ascii_case("bash")
                            && let Some(toolset) = input.toolset.as_ref()
                            && let Some(command) =
                                tc.arguments.get("command").and_then(|value| value.as_str())
                            && crate::tools::tower::bash_guard::command_needs_tower_guard(command)
                        {
                            let main_checkout =
                                crate::tools::strip_verbatim_prefix(std::path::PathBuf::from(
                                    crate::tools::tower::paths::resolve_tower_repo_root(
                                        &toolset.tower_root().to_string_lossy(),
                                    ),
                                ));
                            let state_file =
                                main_checkout.join(crate::tools::tower::paths::STATE_FILE);
                            let owned = tokio::fs::read_to_string(&state_file)
                                .await
                                .map(|raw| {
                                    crate::tools::tower::bash_guard::tower_workspace_owned_json(
                                        &raw,
                                    )
                                })
                                .unwrap_or(false);
                            if owned {
                                // Relative `cwd` arguments resolve against
                                // what the toolset actually executes in (the
                                // caller's bound directory, else the session
                                // workspace root), never the engine process's
                                // own cwd.
                                let execution_cwd = input
                                    .toolset
                                    .as_ref()
                                    .map(|toolset| toolset.execution_root())
                                    .or_else(|| std::env::current_dir().ok())
                                    .and_then(|cwd| std::fs::canonicalize(&cwd).ok())
                                    .map(crate::tools::strip_verbatim_prefix)
                                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                                let cwd = tc
                                    .arguments
                                    .get("cwd")
                                    .and_then(|value| value.as_str())
                                    .filter(|cwd| !cwd.is_empty())
                                    .map_or(execution_cwd.clone(), |cwd| {
                                        let path = std::path::Path::new(cwd);
                                        crate::tools::strip_verbatim_prefix(if path.is_absolute() {
                                            path.to_path_buf()
                                        } else {
                                            execution_cwd.join(path)
                                        })
                                    });
                                let veto =
                                    crate::tools::tower::bash_guard::analyze_tower_bash_command(
                                        &crate::tools::tower::bash_guard::TowerBashGuardTarget {
                                            command: command.to_string(),
                                            cwd: cwd.clone(),
                                            main_checkout: main_checkout.clone(),
                                        },
                                        crate::tools::tower::bash_guard::parse_shell_script,
                                    );
                                if let Some(veto) = veto {
                                    refusals[index] = Some(veto);
                                    continue;
                                }
                            }
                        }
                        schedulable.push(tc.clone());
                    }
                    // v2 `stopBatchAfterThis` (`updateGoalTool.ts:44-48`,
                    // `setGoalBudgetTool.ts:35-44`) is resolved before the
                    // batch starts: the prediction decides which calls may run
                    // at all, independent of whether the predicting tool later
                    // succeeds. The goal read comes from the host, which owns
                    // the snapshot v2 reads in memory.
                    let mut predicted_stop_index: Option<usize> = None;
                    for (index, tc) in tool_calls.iter().enumerate() {
                        if refusals[index].is_some() {
                            continue;
                        }
                        if crate::tools::goal_tools::predicted_stop_batch(
                            callbacks.as_ref(),
                            &input.agent_id,
                            &tc.name,
                            &tc.arguments,
                        )
                        .await
                        {
                            predicted_stop_index = Some(index);
                            break;
                        }
                    }
                    let scheduled: Vec<ScheduledToolCall> = schedulable
                        .iter()
                        .map(|tc| ScheduledToolCall {
                            tool_call: tc.clone(),
                            accesses: tool_scheduler::infer_tool_accesses(&tc.name, &tc.arguments),
                        })
                        .collect();
                    let run_count = predicted_stop_index.map(|stop| {
                        // Count the calls the scheduler will actually receive up
                        // to and including the predicted stop. A judge-rejected
                        // (synthetic) stop still stops the batch in v2 —
                        // `stopBatchAfterThis` is resolved before execution —
                        // so it must not fall out of the count just because it
                        // is not scheduled.
                        (0..=stop)
                            .filter(|index| {
                                refusals[*index].is_none() && synthetic_results[*index].is_none()
                            })
                            .count()
                    });

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
                            let arguments = &call.tool_call.arguments;
                            let cwd_arg = arguments.get("cwd").and_then(|value| value.as_str());
                            if let Some(cwd) = cwd_arg {
                                step_access.declared_cwds.push(cwd.to_string());
                            }
                            // v2's other half (`extractBashTargetDirs`): the
                            // directories a Bash call's listing operands
                            // resolve to. The engine parses bash itself
                            // (`tools::bash_target_dirs`); a host that supplies
                            // its own parser overrides it through
                            // `host/parse_bash`. Either way a command that
                            // fails to parse contributes nothing beyond the
                            // declared cwd — v2's own fallback shape
                            // (`!parsed.ok || parsed.hasError` → cwd only),
                            // never an error into the turn.
                            if let Some(command) =
                                arguments.get("command").and_then(|value| value.as_str())
                            {
                                // Same base the call will execute in: a
                                // relative `cwd` is resolved against the
                                // session's execution root, not the engine
                                // process's working directory.
                                let execution_cwd = input
                                    .toolset
                                    .as_ref()
                                    .map(|toolset| toolset.execution_root())
                                    .or_else(|| std::env::current_dir().ok())
                                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                                let effective_cwd = match cwd_arg {
                                    Some(cwd) if !cwd.is_empty() => {
                                        let candidate = std::path::Path::new(cwd);
                                        if candidate.is_absolute() {
                                            candidate.to_path_buf()
                                        } else {
                                            execution_cwd.join(candidate)
                                        }
                                    }
                                    _ => execution_cwd,
                                };
                                let home = std::env::var_os("USERPROFILE")
                                    .or_else(|| std::env::var_os("HOME"))
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_else(|| effective_cwd.clone());
                                let operands = if callbacks.supports_parse_bash() {
                                    callbacks
                                        .parse_bash(crate::rpc::types::ParseBashRequest {
                                            command: command.to_string(),
                                            cwd: effective_cwd.to_string_lossy().into_owned(),
                                            home: home.to_string_lossy().into_owned(),
                                        })
                                        .await
                                        .ok()
                                        .flatten()
                                } else {
                                    crate::tools::bash_target_dirs::bash_target_dirs(
                                        command,
                                        &effective_cwd.to_string_lossy(),
                                        &home.to_string_lossy(),
                                    )
                                };
                                if let Some(dirs) = operands {
                                    step_access
                                        .dirs
                                        .extend(dirs.into_iter().map(std::path::PathBuf::from));
                                }
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
                            } else {
                                step_access.dirs.push(accessed);
                            }
                        }
                    }
                    turn_watch_state.set_phase(TurnPhase::Tools);
                    // The scheduler answers exactly the calls it was handed, so
                    // its outcome is indexed by *scheduled* call, not by call
                    // position. Re-aligning here is what lets a refused call
                    // (preflight or tower guard) keep its position in the plan
                    // and in the history: `finalize_step` and every consumer
                    // below index by position in `tool_calls`.
                    let (executed, executed_durations, _batch_cancelled) = if scheduled.is_empty() {
                        (Vec::new(), Vec::new(), false)
                    } else {
                        match tool_scheduler::execute_scheduled(
                            input.cancellation.as_ref(),
                            scheduled,
                            exec_fn,
                            run_count,
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
                                    return Ok(finish!(
                                        LoopTurnStopReason::Aborted,
                                        steps,
                                        total_usage,
                                        0,
                                        llm_retries,
                                        messages.clone(),
                                    )
                                    .with_last_request_usage(last_request_usage.clone()));
                                }
                                return Err(err);
                            }
                        }
                    };
                    turn_watch_state.note_tool_activity();
                    let mut executed = executed.into_iter();
                    let mut executed_durations = executed_durations.into_iter();
                    let mut results = Vec::with_capacity(tool_calls.len());
                    let mut durations_ms = Vec::with_capacity(tool_calls.len());
                    for (index, refusal) in refusals.iter().enumerate() {
                        // v2 `prepareSkippedToolCall` runs for every call after
                        // the predicted stop, including one the preflight would
                        // have rejected — the batch stopped before it could be
                        // prepared. The scheduler only ran the calls up to
                        // `run_count`, so no executed result is consumed here.
                        if run_count.is_some()
                            && predicted_stop_index.is_some_and(|stop| index > stop)
                        {
                            results.push(ExecutableToolResult {
                                spill: None,
                                parts: Vec::new(),
                                delivery: None,
                                stop_turn: false,
                                content: tool_scheduler::SKIPPED_TOOL_OUTPUT.to_string(),
                                is_error: true,
                                note: None,
                                display: None,
                            });
                            durations_ms.push(None);
                            continue;
                        }
                        // A judge-rejected goal write never reaches the
                        // scheduler; its answer is already decided.
                        if let Some(synthetic) = synthetic_results[index].take() {
                            results.push(synthetic);
                            durations_ms.push(None);
                            continue;
                        }
                        match refusal {
                            Some(content) => {
                                results.push(ExecutableToolResult {
                                    spill: None,
                                    parts: Vec::new(),
                                    delivery: None,
                                    stop_turn: false,
                                    content: content.clone(),
                                    is_error: true,
                                    note: None,
                                    display: None,
                                });
                                durations_ms.push(None);
                            }
                            None => {
                                results.push(
                                    executed
                                        .next()
                                        .expect("the scheduler answers every scheduled call"),
                                );
                                durations_ms.push(executed_durations.next().flatten());
                            }
                        }
                    }

                    // Dedup finalize (v2 `finalizeResult` + `endStep`):
                    // streak reminders are appended to the originals'
                    // results, same-step repeats receive the original's
                    // final result, and the cross-step streak advances.
                    let dedupe_force_stop = tool_dedupe.finalize_step(&dedupe_plan, &mut results);

                    // v2 `agentsMdReminderService.trackChanges`: a read only
                    // marks the file known when the call actually succeeded
                    // (`onDidExecuteTool` fires on a completed call). Recording
                    // it at plan time suppressed the reminder for reads that
                    // were then refused or failed, so the model never learned
                    // the file exists.
                    for (index, result) in results.iter().enumerate() {
                        if result.is_error {
                            continue;
                        }
                        let Some(call) = tool_calls.get(index) else {
                            continue;
                        };
                        if !matches!(
                            call.name.to_ascii_lowercase().as_str(),
                            "read" | "edit" | "write"
                        ) {
                            continue;
                        }
                        for access in
                            tool_scheduler::infer_tool_accesses(&call.name, &call.arguments)
                        {
                            let crate::turn_loop::types::ToolResourceAccess::File(access) = access
                            else {
                                continue;
                            };
                            let accessed = std::path::PathBuf::from(&access.path);
                            if crate::injection::is_agents_md_path(&accessed) {
                                step_access.self_read.push(accessed);
                            }
                        }
                    }

                    // v2 `AgentTestScopeGuardService.observe` through
                    // `toolExecutor.hooks.onDidExecuteTool`: a completed `Bash`
                    // call that ran the whole suite from the repository root
                    // arms one reminder for the next request of this turn.
                    for (index, result) in results.iter().enumerate() {
                        if result.is_error {
                            continue;
                        }
                        let Some(call) = tool_calls.get(index) else {
                            continue;
                        };
                        if !call.name.eq_ignore_ascii_case("Bash") {
                            continue;
                        }
                        let Some(command) = call
                            .arguments
                            .get("command")
                            .and_then(|value| value.as_str())
                        else {
                            continue;
                        };
                        if test_scope_guard.observe(command).is_some() {
                            break;
                        }
                    }

                    // v2 `ProgressTrackerService.onDidExecuteTool`: score each
                    // executed call; three unverified mutations arm the
                    // verification reminder for the next request.
                    for (index, result) in results.iter().enumerate() {
                        // A same-step repeat shared the original's execution;
                        // scoring it again would double-count the mutation.
                        if dedupe_plan
                            .original_of
                            .get(index)
                            .is_some_and(|original| *original != index)
                        {
                            continue;
                        }
                        let Some(call) = tool_calls.get(index) else {
                            continue;
                        };
                        let receipt =
                            progress_receipt(&call.name, &call.arguments, !result.is_error);
                        let blind = match input.toolset.as_ref() {
                            Some(toolset) => toolset.observe_progress(&receipt),
                            None => local_progress_tracker.observe(&receipt),
                        };
                        if blind >= crate::turn_loop::progress_track::EBM_BLIND_MUTATION_THRESHOLD {
                            ebm_pending.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }

                    // Insert tool results, each linked back to its call
                    // via `tool_call_id` (same call order as `tool_calls`).
                    for (i, tr) in results.iter().enumerate() {
                        // A same-step repeat shared the original's execution
                        // and never reached the native gate, so no tool.native
                        // event surfaced it — emit one here so the transcript
                        // still shows the call with its shared result (v2
                        // keeps the vetoed repeat visible). A judge-rejected
                        // goal write likewise never ran a tool, so its card is
                        // closed here.
                        if (dedupe_plan.original_of.get(i).is_some_and(|o| *o != i)
                            || synthetic_indices.contains(&i))
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
                        let rendered_parts = if tr.parts.is_empty() {
                            Vec::new()
                        } else {
                            let mut parts =
                                tool_result_render::render_parts(&tr.parts, tr.is_error);
                            if let Some(duration_ms) = durations_ms.get(i).copied().flatten()
                                && wall_time::should_render_wall_time(tool_name)
                            {
                                tool_result_render::prepend_wall_time_parts(
                                    &mut parts,
                                    duration_ms,
                                );
                            }
                            if let Some(note) = tr.note.as_deref().filter(|note| !note.is_empty()) {
                                tool_result_render::append_note_parts(&mut parts, note);
                            }
                            parts
                        };
                        let content = if rendered_parts.is_empty() {
                            let rendered =
                                tool_result_render::render_status(&tr.content, tr.is_error);
                            let content = match durations_ms.get(i).copied().flatten() {
                                Some(duration_ms)
                                    if wall_time::should_render_wall_time(tool_name) =>
                                {
                                    wall_time::prepend_wall_time(&rendered, duration_ms)
                                }
                                _ => rendered,
                            };
                            let content = match tr.note.as_deref().filter(|note| !note.is_empty()) {
                                Some(note) => format!("{content}\n{note}"),
                                None => content,
                            };
                            content
                        } else {
                            match rendered_parts.as_slice() {
                                [ContentBlock::Text { text, .. }] => text.clone(),
                                _ => String::new(),
                            }
                        };
                        messages.push(LLMMessage {
                            role: "tool".into(),
                            content,
                            blocks: rendered_parts,
                            tool_calls: Vec::new(),
                            tool_call_id: tool_calls.get(i).map(|tc| tc.id.clone()),
                            prompt_id: None,
                            origin: None,
                        });
                        // v2 `deliverToolResult` (`loopService.ts:678-690`):
                        // `delivery` is stripped from the result and, for
                        // `kind: 'steer'`, submitted as its own user message.
                        // No media rides this channel — the result's own parts
                        // carry it. Skill is the only producer
                        // (`skillTool.ts:132-135`). A same-step repeat shares
                        // the original's delivery, so only the original emits
                        // it.
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
                        return Ok(finish!(
                            LoopTurnStopReason::EndTurn,
                            steps,
                            total_usage,
                            0,
                            llm_retries,
                            messages.clone(),
                        )
                        .with_last_request_usage(last_request_usage.clone()));
                    }

                    if dedupe_force_stop {
                        // v2 `settleHandoff`: one extra step with the normal
                        // tool table. The model is expected to answer in text
                        // because the last tool result carries the
                        // final-response reminder; a call it makes anyway is
                        // refused (`HANDOFF_VETO_TEXT`) and answered, never
                        // executed. Dropping it silently left the refusal
                        // invisible to the model and the transcript.
                        steps += 1;
                        turn_watch_state.note_steps(steps as u64);
                        turn_watch_state.set_phase(TurnPhase::Llm);
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
                            &step_tool_defs,
                            &retry_config,
                            Some(turn_cancel.token()),
                            Some(&telemetry),
                            Some(crate::compaction::estimate_messages_tokens(&messages)),
                            input.max_context_tokens,
                            Some(total_usage.input_tokens),
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

                        if let Ok(step_res) = handoff_res {
                            let vetoed = match step_res.stop_reason {
                                crate::turn_loop::types::LoopStepStopReason::ToolCalls(calls) => {
                                    calls
                                }
                                _ => Vec::new(),
                            };
                            if vetoed.is_empty() {
                                if !step_res.content.is_empty() {
                                    messages.push(LLMMessage {
                                        role: "assistant".into(),
                                        content: step_res.content,
                                        blocks: step_res.thinking,
                                        tool_calls: Vec::new(),
                                        tool_call_id: None,

                                        prompt_id: None,
                                        origin: None,
                                    });
                                }
                            } else {
                                messages.push(LLMMessage {
                                    role: "assistant".into(),
                                    content: step_res.content,
                                    blocks: step_res.thinking,
                                    tool_calls: vetoed.clone(),
                                    tool_call_id: None,

                                    prompt_id: None,
                                    origin: None,
                                });
                                let veto = crate::tools::tool_dedupe::handoff_veto_text();
                                let rendered = tool_result_render::render_status(&veto, true);
                                for call in &vetoed {
                                    callbacks.emit_event(serde_json::json!({
                                        "type": "tool.native",
                                        "agent_id": input.agent_id,
                                        "turn_id": turn_id,
                                        "tool_call_id": call.id,
                                        "tool_name": call.name,
                                        "arguments": call.arguments,
                                        "content": rendered,
                                        "is_error": true,
                                        "note": null,
                                    }));
                                    messages.push(LLMMessage {
                                        role: "tool".into(),
                                        content: rendered.clone(),
                                        tool_call_id: Some(call.id.clone()),
                                        ..Default::default()
                                    });
                                }
                            }
                        }

                        return Ok(finish!(
                            LoopTurnStopReason::RepeatBreaker,
                            steps,
                            total_usage,
                            0,
                            llm_retries,
                            messages.clone(),
                        )
                        .with_last_request_usage(last_request_usage.clone()));
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
        // The last step completed, but the budget ran out with the model still
        // asking for tools: v2 closes that step with `max_steps`
        // (loopService.ts:2201 emitStepInterrupted via failMachineStep).
        if reason == LoopTurnStopReason::MaxSteps {
            emit_step_interrupted_event(
                callbacks.as_ref(),
                &input.agent_id,
                &turn_id,
                steps,
                "max_steps",
                None,
            );
        }
        Ok(
            finish!(reason, steps, total_usage, 0, llm_retries, messages.clone())
                .with_last_request_usage(last_request_usage),
        )
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

/// Why a tool call's arguments cannot be executed (v2 `preflightToolCall`).
enum UnusableArguments {
    /// The provider's payload was not valid JSON; the reason is what the decoder
    /// reported.
    NotJson(String),
    /// The payload decoded, but tool arguments are always a JSON object.
    NotObject,
    /// The arguments decoded and are an object, but they do not satisfy the
    /// tool's declared schema (v2 `validateExecutableToolArgs`).
    Schema(String),
}

impl UnusableArguments {
    /// The model-facing sentence. It follows the `Invalid <tool> arguments: …`
    /// family the per-tool validators already use, so one failure reads like the
    /// next. `detail` stays as the decoder (or the schema validator) reported
    /// it — v2 likewise hands the underlying error text through rather than
    /// rewriting it.
    fn message(&self, tool_name: &str) -> String {
        match self {
            Self::NotJson(detail) => crate::i18n::LocalizedText::with_params(
                "engine.tools.argsNotJson",
                crate::i18n::i18n_params!["tool" => tool_name, "detail" => detail],
            ),
            Self::NotObject => crate::i18n::LocalizedText::with_params(
                "engine.tools.argsNotObject",
                crate::i18n::i18n_params!["tool" => tool_name],
            ),
            Self::Schema(detail) => crate::i18n::LocalizedText::with_params(
                "engine.tools.argsInvalidSchema",
                crate::i18n::i18n_params!["tool" => tool_name, "detail" => detail],
            ),
        }
        .render()
    }
}

/// v2 `preflightToolCall`: why this call cannot run, or `None` when it can.
///
/// The schema gate is v2's last check before execution (`toolExecutorService.ts`
/// `validateExecutableToolArgs`) and applies only to a tool the tables declare:
/// a name with no definition is left to the missing-tool path, and an absent or
/// empty schema accepts anything, the same way v2 compiles Ajv over the empty
/// object it substitutes.
///
/// `step_defs` is what the model was shown for this step; `turn_defs` is the
/// snapshot the turn started from. The step table is consulted first because it
/// is the one the model saw, and the snapshot is the fallback for a host whose
/// `list_tools` answers empty — without it, a host that never implements that
/// seam would silently disable validation.
fn unusable_tool_arguments(
    call: &ToolCall,
    step_defs: &[crate::rpc::types::ToolInfo],
    turn_defs: &[crate::rpc::types::ToolInfo],
) -> Option<UnusableArguments> {
    if let Some(failure) = &call.args_parse_error {
        return Some(UnusableArguments::NotJson(failure.error.clone()));
    }
    if !call.arguments.is_object() {
        return Some(UnusableArguments::NotObject);
    }
    let definition = find_tool_definition(step_defs, &call.name)
        .or_else(|| find_tool_definition(turn_defs, &call.name))?;
    crate::tools::args_validate::validate(&definition.input_schema, &call.arguments)
        .err()
        .map(UnusableArguments::Schema)
}

/// Exact name first, then the case-insensitive spelling the dispatcher accepts.
fn find_tool_definition<'a>(
    defs: &'a [crate::rpc::types::ToolInfo],
    name: &str,
) -> Option<&'a crate::rpc::types::ToolInfo> {
    defs.iter()
        .find(|definition| definition.name == name)
        .or_else(|| {
            defs.iter()
                .find(|definition| definition.name.eq_ignore_ascii_case(name))
        })
}

#[cfg(test)]
mod tests;
