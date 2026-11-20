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
use crate::features::goal::tools::update_goal::update_goal_tool::evaluate_goal_completion;
use crate::llm::files_upload::UploadError;
use crate::llm::media_budget::{MEDIA_BUDGET_EXCEEDED_CODE, MediaBudget};
use crate::llm::media_resolver::{MediaResolver, ResolvedRequest, has_media_refs, inline_entries};
use crate::rpc::types::{BoxFuture, ToolExecuteRequest};
use kimi_core::types::TokenUsage;

#[derive(Default)]
struct CallbackStateSnapshot {
    goal: std::sync::Mutex<crate::primitives::read::ReadOutcome<serde_json::Value>>,
    plan: std::sync::Mutex<crate::primitives::read::ReadOutcome<serde_json::Value>>,
    todo: std::sync::Mutex<crate::primitives::read::ReadOutcome<serde_json::Value>>,
}

impl CallbackStateSnapshot {
    async fn refresh(&self, callbacks: &dyn HostCallbacks, include_todo: bool) {
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

            _ => return crate::primitives::read::ReadOutcome::Absent,
        };
        slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

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

fn token_count_u32(value: u64) -> Result<u32, String> {
    u32::try_from(value).map_err(|error| error.to_string())
}

fn optional_token_count_u32(value: Option<u64>) -> Result<Option<u32>, String> {
    value.map(token_count_u32).transpose()
}

#[derive(Default)]
struct ContextTokens {
    tokens: u32,
    len: usize,
    stale: bool,

    overhead: u32,
}

impl ContextTokens {
    fn new(overhead: u32) -> Self {
        Self {
            overhead,
            ..Default::default()
        }
    }

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

    fn record_measured(&mut self, input_tokens: u32, messages: &[LLMMessage]) {
        self.tokens = input_tokens.saturating_sub(self.overhead);
        self.len = messages.len();
        self.stale = false;
    }

    fn invalidate(&mut self) {
        self.stale = true;
    }

    fn rebase_with_summary(&mut self, messages: &[LLMMessage], summary_output_tokens: Option<u32>) {
        self.tokens = crate::compaction::rebased_tokens(messages, summary_output_tokens);
        self.len = messages.len();
        self.stale = false;
    }
}

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

        compaction_usage: None,
        tool_schema_tokens: 0,
        cache_observations: Vec::new(),
        cache_probe_requests: Vec::new(),
        cache_observation_resets: Vec::new(),
    }
}

fn emit_step_begin_event(callbacks: &dyn HostCallbacks, agent_id: &str, turn_id: &str, step: u32) {
    callbacks.emit_event(serde_json::json!({
        "type": "llm.step.begin",
        "agent_id": agent_id,
        "turn_id": turn_id,
        "step": step,
        "step_id": step_event_id(turn_id, step),
    }));
}

fn step_event_id(turn_id: &str, step: u32) -> String {
    format!("{turn_id}-step-{step}")
}

fn is_unknown_tool_refusal(content: &str, is_error: bool, tool_name: &str) -> bool {
    is_error && content == format!("unknown tool: {tool_name}")
}

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
            "input_tokens": result.usage.input_other,
            "output_tokens": result.usage.output,
            "total_tokens": result.usage.measured_context_tokens(),
            "input_cache_read": result.usage.input_cache_read,
            "input_cache_creation": result.usage.input_cache_creation,
        },
        "timing": serde_json::to_value(&result.timing).unwrap_or(serde_json::Value::Null),
    }));
}

fn turn_stop_reason_from_finish(finish_reason: Option<&str>) -> LoopTurnStopReason {
    match finish_reason {
        Some("length") | Some("max_tokens") => LoopTurnStopReason::MaxTokens,
        _ if crate::turn_loop::turn_step::is_content_filtered(finish_reason) => {
            LoopTurnStopReason::Filtered
        }
        _ => LoopTurnStopReason::EndTurn,
    }
}

pub(crate) fn provider_finish_reason(finish_reason: Option<&str>) -> Option<&'static str> {
    let raw = finish_reason?;

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

pub fn run_turn_with_lifecycle_telemetry<'a>(
    input: RunTurnInput<'a>,
    telemetry: &TelemetryContext,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    let telemetry = telemetry.clone();
    let turn_id = input.turn_id.clone();
    let agent_id = input.agent_id.clone();

    let workspace_root = turn_workspace_root(&input);
    callbacks.telemetry(telemetry_payload(
        "turn_started",
        &telemetry,
        &turn_id,
        None,
    ));
    Box::pin(async move {
        let started = std::time::Instant::now();

        let result = run_turn_continued(input, callbacks).await;

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

pub fn run_turn_with_telemetry<'a>(
    input: RunTurnInput<'a>,
    telemetry: TelemetryContext,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    run_turn_with_lifecycle_telemetry(input, &telemetry, callbacks)
}

fn latest_user_text(user_messages: &[LLMMessage]) -> String {
    user_messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

fn strip_rebuilt_system_message(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    debug_assert!(
        messages.first().is_none_or(|m| m.role == "system"),
        "turn head must be the rebuilt system message"
    );
    messages.into_iter().skip(1).collect()
}

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

pub fn run_turn_continued<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    run_turn_continued_with_observations(input, callbacks, None)
}

pub fn run_turn_continued_with_observations<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
    sink: Option<TurnObservationsSink>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    let RunTurnInput {
        llm_request_byte_budget,
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

        let mut hook_guard = hook_guard;

        let mut steps = 0u32;
        let mut usage = kimi_core::types::TokenUsage::default();
        let mut llm_retries = 0u32;

        let mut carry_previous_turn_aborted = previous_turn_aborted;
        loop {
            let iter_input = RunTurnInput {
                llm_request_byte_budget,
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

                previous_turn_aborted: carry_previous_turn_aborted,
            };
            let mut result = match run_turn_inner(iter_input, callbacks, sink.clone()).await {
                Ok(result) => result,
                Err(error) => {
                    crate::swarm::mode::exit_tool_swarm_at_turn_end(callbacks.as_ref(), &agent_id);
                    return Err(error);
                }
            };
            carry_previous_turn_aborted = false;
            steps += result.steps;
            usage.accumulate(&result.usage);
            llm_retries += result.llm_retries;

            match result.stop_hook_continuation.take() {
                Some(text) => {
                    messages = strip_rebuilt_system_message(result.messages);
                    messages.push(LLMMessage::user(&text));
                }
                None => {
                    result.steps = steps;
                    result.usage = usage;
                    result.llm_retries = llm_retries;
                    result.stop_hook_continuation = None;

                    crate::swarm::mode::exit_tool_swarm_at_turn_end(callbacks.as_ref(), &agent_id);
                    return Ok(result);
                }
            }
        }
    })
}

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

fn sink_observation_base(sink: &Option<TurnObservationsSink>) -> usize {
    sink.as_ref()
        .and_then(|sink| sink.lock().ok().map(|guard| guard.cache_observations.len()))
        .unwrap_or(0)
}

fn turn_workspace_root(input: &RunTurnInput<'_>) -> Option<std::path::PathBuf> {
    input
        .toolset
        .as_ref()
        .map(|toolset| toolset.workspace_root().to_path_buf())
        .or_else(|| std::env::current_dir().ok())
}

#[tracing::instrument(name = "run_turn", skip_all, fields(turn_id = %input.turn_id, max_steps = input.max_steps, has_goal = input.goal.is_some()))]
pub fn run_turn<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    run_turn_inner(input, callbacks, None)
}

fn run_turn_inner<'a>(
    input: RunTurnInput<'a>,
    callbacks: &'a Arc<dyn HostCallbacks>,
    sink: Option<TurnObservationsSink>,
) -> BoxFuture<'a, Result<TurnResult, Box<dyn std::error::Error + 'a>>> {
    let turn_id = input.turn_id.clone();

    let max_steps = if input.max_steps == 0 {
        u32::MAX
    } else {
        input.max_steps
    };
    let user_messages = input.messages.clone();

    let date_baseline = crate::injection::scan_date_baseline(&user_messages);

    let mut agents_md_baseline = crate::injection::scan_agents_md_baseline(&user_messages);
    for path in
        crate::prompt::agents_md::extract_agents_md_paths_from_prompt(input.llm.system_prompt())
    {
        if !agents_md_baseline.iter().any(|seen| seen == &path) {
            agents_md_baseline.push(path);
        }
    }

    let permission_mode_baseline =
        crate::injection::permission_mode::scan_permission_mode_baseline(&user_messages);

    let previous_session_reminder_baseline =
        crate::storage::scan_previous_session_reminders(&user_messages);

    let interruption_baseline =
        crate::injection::interruption_reminder::scan_interruption_baseline(&user_messages);
    let previous_turn_aborted = input.previous_turn_aborted;
    let tool_defs = input.tool_defs.clone();
    let goal = input.goal.clone();
    let submitted_prompt = latest_user_text(&user_messages);
    let hook_guard = input.hook_guard.clone();

    callbacks.set_turn_goal(&turn_id, goal.as_ref().map(|g| g.goal_id.as_str()));

    Box::pin(async move {
        let sink_base = sink_observation_base(&sink);

        let turn_watch_state = Arc::new(crate::turn_loop::watch::TurnWatchState::default());
        let _turn_watchdog = crate::turn_loop::watch::TurnWatchdog::arm(
            &input.agent_id,
            &turn_id,
            Arc::clone(&turn_watch_state),
        );
        input
            .llm
            .set_turn_watch(Some(Arc::clone(&turn_watch_state)));
        let mut total_usage = kimi_core::types::TokenUsage::default();

        let mut last_request_usage: Option<crate::turn_loop::types::RequestMeasurement> = None;
        let mut steps: u32 = 0;

        let mut llm_retries: u32 = 0;

        let mut last_finish_reason: Option<String> = None;

        let turn_started = std::time::Instant::now();
        let elapsed_wall_clock_ms = |started: std::time::Instant| {
            started.elapsed().as_millis().min(i64::MAX as u128) as i64
        };

        let mut compaction_usage = kimi_core::types::TokenUsage::default();
        let mut did_compact = false;

        let mut cache_observations: Vec<crate::session::cache_observation::CacheObservation> =
            Vec::new();

        let tool_schema_tokens = serde_json::to_string(&tool_defs)
            .map(|json| crate::compaction::estimate_tokens_for_json(&json))
            .unwrap_or(0);

        let system_prompt_hash =
            crate::session::cache_observation::fingerprint(input.llm.system_prompt());

        let mut cache_probe_requests: Vec<crate::session::cache_observation::CacheProbeRequest> =
            Vec::new();

        let mut cache_observation_resets: Vec<usize> = Vec::new();

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
            role: kimi_core::types::Role::System,
            content: system_prompt,
            ..Default::default()
        }];
        messages.extend(user_messages);

        let supports_dynamic_loading = input
            .toolset
            .as_ref()
            .is_some_and(|toolset| toolset.supports_dynamic_tool_loading());
        let shaped = crate::tools::tool_select_state::shape_history(
            crate::tools::tool_select_state::context_messages(&messages),
            supports_dynamic_loading,
        );
        messages = crate::tools::tool_select_state::llm_messages(shaped);

        if supports_dynamic_loading && let Some(toolset) = &input.toolset {
            toolset.seed_loaded_tools_from_history(&messages);
        }

        if let Some(ref guard) = hook_guard
            && let Some(prompt_index) = messages.iter().rposition(|m| m.role == "user")
        {
            let outcome = guard
                .run_user_prompt_submit(&input.agent_id, &turn_id, &submitted_prompt)
                .await;
            if let Some(block) = outcome.block {
                messages.push(LLMMessage {
                    role: kimi_core::types::Role::Assistant,
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
                    kimi_core::types::TokenUsage::default(),
                    0,
                    0,
                    messages,
                )
                .with_gate_blocked());
            }
            if !outcome.append.is_empty() {
                let prompt = &mut messages[prompt_index];

                let original_parts = if prompt.blocks.is_empty() {
                    vec![ContentPart::Text {
                        text: prompt.content.clone(),
                        meta: None,
                    }]
                } else {
                    std::mem::take(&mut prompt.blocks)
                };
                prompt.blocks = outcome.append.into_iter().chain(original_parts).collect();
            }
        }

        let mut context_tokens = ContextTokens::new(tool_schema_tokens);

        let retry_config = RetryConfig {
            max_attempts: input
                .max_attempts
                .unwrap_or(crate::turn_loop::retry::DEFAULT_MAX_RETRY_ATTEMPTS),
            ..RetryConfig::default()
        };

        let turn_cancel = TurnCancellation::from_flag(input.cancellation.clone());

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

        let effective_window = crate::compaction::effective_max_tokens_for(
            &input.agent_id,
            input.llm.model_name(),
            input.max_context_tokens,
        );
        let mut compaction_config = crate::compaction::config_for_window(effective_window);
        compaction_config.max_attempts = input.compaction_max_attempts;

        let mut consecutive_overflow_compactions: u32 = 0;

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

        let mut structure_strict = false;

        let workspace_root = turn_workspace_root(&input);
        let mut injection_registry = crate::injection::InjectionRegistry::with_defaults(
            workspace_root.clone(),
            date_baseline,
            agents_md_baseline,
        );
        if let Some(service) = input
            .toolset
            .as_ref()
            .and_then(|toolset| toolset.git_memory_service())
        {
            injection_registry.register(
                "git_memory_signal",
                crate::injection::git_memory::git_memory_signal_provider(service),
            );
        }

        crate::injection::notify_user_nudge::register_notify_user_nudge(
            &mut injection_registry,
            input
                .tool_defs
                .iter()
                .any(|tool| tool.name == crate::tools::core_tool_defs::NOTIFY_USER_TOOL_NAME),
        );

        let ebm_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ebm_injected = Arc::new(std::sync::atomic::AtomicBool::new(false));

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

        let test_scope_guard =
            Arc::new(crate::turn_loop::test_scope_guard::AgentTestScopeGuardService::from_env());
        {
            let guard = test_scope_guard.clone();
            injection_registry.register(
                crate::turn_loop::test_scope_guard::TEST_SCOPE_REMINDER_VARIANT,
                Box::new(move |_ctx| guard.take_pending()),
            );
        }
        crate::injection::permission_mode::register_permission_mode_injection(
            &mut injection_registry,
            Some(
                input
                    .permission_mode
                    .unwrap_or(crate::permission::PermissionMode::Manual),
            ),
            permission_mode_baseline,
        );

        if previous_turn_aborted && !interruption_baseline {
            crate::injection::interruption_reminder::register_interruption_reminder(
                &mut injection_registry,
                true,
                &[],
            );
        }

        if input.agent_id == crate::callbacks::MAIN_AGENT_ID {
            crate::injection::knowledge::register_knowledge_injection(
                &mut injection_registry,
                workspace_root.clone(),
            );
        }

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

        crate::injection::swarm_mode::register_swarm_mode_injection(
            &mut injection_registry,
            input.agent_id.clone(),
            crate::injection::swarm_mode::scan_swarm_mode_baseline(&messages),
        );
        crate::injection::hello_world_mode::register_hello_world_mode_injection(
            &mut injection_registry,
            input.agent_id.clone(),
            crate::injection::hello_world_mode::scan_hello_world_baseline(&messages),
        );

        if let Some(toolset) = &input.toolset {
            let disclosure_toolset = toolset.clone();
            injection_registry.register(
                crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
                Box::new(move |ctx| disclosure_toolset.take_disclosure_announcement(ctx.history)),
            );

            let reminder_toolset = toolset.clone();
            let announced = previous_session_reminder_baseline;
            injection_registry.register(
                "task_resume_termination",
                Box::new(move |_ctx| reminder_toolset.take_previous_session_reminder(&announced)),
            );
        }

        let mut tool_dedupe = crate::tools::tool_dedupe::DedupeGuard::new();

        let mut media_budget = match input.media_dropped.clone() {
            Some(shared) => MediaBudget::shared(shared),
            None => MediaBudget::default(),
        };

        let tool_call_ids =
            std::sync::Arc::new(crate::turn_loop::tool_call_id::ToolCallIdNormalizer::new());
        tool_call_ids.seed_from(&messages);

        input
            .llm
            .set_tool_call_ids(std::sync::Arc::clone(&tool_call_ids));

        input.llm.set_stream_agent_id(&input.agent_id);

        let mut step_access = crate::injection::StepAccess::default();

        let mut last_compacted_token_count: Option<u32> = None;

        let mut rearm_pending = false;

        for step_num in 0..max_steps {
            steps = step_num + 1;
            turn_watch_state.note_steps(steps as u64);
            let turn_wall_clock_ms = elapsed_wall_clock_ms(turn_started);

            let mut rearm_this_step = std::mem::take(&mut rearm_pending);

            if let Some(ref goal) = goal
                && goal.status.is_active()
            {
                let turn_tokens = i64::try_from(total_usage.output)
                    .map_err(|error| error.to_string())?;
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

                let steering =
                    render_goal_steering(goal, turn_tokens, turns_this_turn, turn_wall_clock_ms);
                messages[0].content = format!("{}\n\n{}", input.llm.system_prompt(), steering);
                context_tokens.invalidate();
            }

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

            if input.llm.transport() != "host-proxy"
                && let Ok(steers) = callbacks.drain_steers().await
            {
                messages.extend(steers);
            }

            context_tokens.sync(&messages);

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

                if let Some(ref guard) = hook_guard {
                    guard
                        .notify_pre_compact(&turn_id, messages.len(), context_tokens.tokens())
                        .await;
                }

                let todos = crate::compaction::read_todos_for_summary(callbacks.as_ref()).await;

                let non_history_state = non_history_state_fingerprint(callbacks.as_ref()).await;
                turn_watch_state.set_phase(TurnPhase::Compacting);
                let emit_compaction_request =
                    |event: serde_json::Value| callbacks.emit_event(event);
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
                    true,
                    cache_probe_requests.last(),
                    Some(turn_id.as_str()),
                    Some(&emit_compaction_request),
                )
                .await
                {
                    Ok(Some((compacted, report))) => {
                        if let Some(probe) = report.cache_probe.as_ref() {
                            callbacks.telemetry(probe.telemetry_payload());
                        }

                        if let Some(usage) = &report.usage {
                            compaction_usage.accumulate(usage);
                            did_compact = true;
                        }
                        if non_history_state_fingerprint(callbacks.as_ref()).await
                            != non_history_state
                        {
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






                                "messages": &compacted,
                            }));

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

                            if let Some(ref guard) = hook_guard {
                                guard
                                    .notify_post_compact(
                                        &turn_id,
                                        "auto",
                                        report.tokens_after as u64,
                                    )
                                    .await;
                            }

                            messages = compacted;
                            rearm_this_step = true;
                            if let Some(toolset) = &input.toolset {
                                toolset.clear_pending_tool_schemas();
                                toolset.seed_loaded_tools_from_history(&messages);
                            }

                            if let Some(manager) = input
                                .toolset
                                .as_ref()
                                .and_then(|toolset| toolset.subagent_manager())
                            {
                                manager.bump_probe_generation(&input.agent_id);
                            }

                            cache_observation_resets.push(cache_observations.len());
                            sink_push_reset(&sink, sink_base + cache_observations.len());

                            if let Some(measurement) = last_request_usage.as_mut() {
                                measurement.spliced_after = true;
                            }
                            context_tokens.rebase_with_summary(
                                &messages,
                                optional_token_count_u32(
                                    report.usage.as_ref().map(|usage| usage.output),
                                )?,
                            );

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

            let step_tool_defs = if input.llm.transport() != "host-proxy" {
                match callbacks.list_tools().await {
                    Ok(response) => response.tools,
                    Err(_) => tool_defs.clone(),
                }
            } else {
                tool_defs.clone()
            };

            if input.llm.transport() != "host-proxy" {
                if step_num == 0
                    && let Some(toolset) = &input.toolset
                {
                    toolset.wait_for_mcp_discovery_before_disclosure().await;
                }

                goal_plan_state
                    .refresh(callbacks.as_ref(), todo_reminder_registered && step_num > 0)
                    .await;

                let is_new_turn = step_num == 0 || rearm_this_step;
                if rearm_this_step {
                    injection_registry.clear_injected();
                }
                let mut injections = injection_registry.build_injections_rearmed(
                    is_new_turn,
                    rearm_this_step,
                    &step_access,
                    &messages,
                );
                if let Some(toolset) = &input.toolset {
                    let pending = toolset
                        .take_tool_update_pending_reminders()
                        .into_iter()
                        .map(|text| {
                            (
                                "tool_update_pending".to_string(),
                                crate::injection::wrap_system_reminder(&text),
                            )
                        })
                        .collect::<Vec<_>>();
                    let position = injections
                        .iter()
                        .position(|(variant, _)| {
                            variant == crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT
                        })
                        .unwrap_or(injections.len());
                    injections.splice(position..position, pending);
                }
                for (variant, text) in injections {
                    let mut message = crate::injection::injection_message_named(text, &variant);
                    if variant == crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT
                        && let Some(toolset) = &input.toolset
                        && let Some(disclosure) = toolset.take_last_announcement_disclosure()
                        && let Some(origin) = message
                            .origin
                            .as_mut()
                            .and_then(|origin| origin.as_object_mut())
                    {
                        origin.insert("disclosure".to_string(), disclosure);
                    }
                    if let Some(context_message) =
                        crate::session::agent_records::context_message(&message)
                    {
                        callbacks.emit_event(serde_json::json!({
                            "type": "context.append_message",
                            "agentId": input.agent_id,
                            "message": context_message,
                        }));
                    }
                    messages.push(message);
                }
                if let Some(toolset) = &input.toolset
                    && let Some(tools) = toolset.drain_pending_tool_schemas()
                {
                    let message = LLMMessage {
                        role: kimi_core::types::Role::System,
                        origin: Some(serde_json::json!({
                            "kind": "injection",
                            "variant": crate::tools::tool_select_state::DYNAMIC_TOOL_SCHEMA_VARIANT,
                        })),
                        tools,
                        ..Default::default()
                    };
                    if let Some(context_message) =
                        crate::session::agent_records::context_message(&message)
                    {
                        callbacks.emit_event(serde_json::json!({
                            "type": "context.append_message",
                            "agentId": input.agent_id,
                            "message": context_message,
                        }));
                    }
                    messages.push(message);
                }
            }

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

            emit_step_begin_event(callbacks.as_ref(), &input.agent_id, &turn_id, steps);

            let step_started_at = chrono::Utc::now().timestamp_millis();

            macro_rules! fail_current_step {
                ($err:expr) => {{
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

                    Err(error) => {
                        fail_current_step!(error);
                    }
                };
                let emit_llm_request = |event: serde_json::Value| callbacks.emit_event(event);
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
                    Some(token_count_u32(total_usage.input_other)?),
                    structure_strict,
                    &input.agent_id,
                    input
                        .toolset
                        .as_ref()
                        .is_some_and(|toolset| toolset.tool_select_enabled()),
                    Some(&emit_llm_request),
                )
                .await;
                let e = match call {
                    Ok(res) => break 'overflow_recovery res,
                    Err(e) => e,
                };
                {
                    let err_str = e.to_string();

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
                        if let Some(salvaged) = input
                            .llm
                            .take_interrupted_response()
                            .and_then(super::interrupted::salvage_interrupted_response)
                        {
                            messages.push(salvaged);
                        }
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

                    let too_large = crate::llm::media_budget::is_request_too_large(&err_str);
                    let image_format =
                        !too_large && crate::llm::media_budget::is_image_format_error(&err_str);
                    if too_large || image_format {
                        let entries = input
                            .media
                            .map(|resolver| resolver.media_entries(&messages));
                        let visible_media = entries.as_deref().map_or(0, <[_]>::len);

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

                            if let Some(cell) = &input.media_recovery {
                                let mut keys: Vec<String> = entries
                                    .as_deref()
                                    .unwrap_or_default()
                                    .iter()
                                    .map(|entry| entry.key.clone())
                                    .collect();

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

                    if let Some(ref guard) = hook_guard {
                        guard
                            .notify_pre_compact(&turn_id, messages.len(), estimated_request_tokens)
                            .await;
                    }

                    let todos = crate::compaction::read_todos_for_summary(callbacks.as_ref()).await;

                    let non_history_state = non_history_state_fingerprint(callbacks.as_ref()).await;

                    let compaction_started = std::time::Instant::now();
                    turn_watch_state.set_phase(TurnPhase::Compacting);
                    let emit_compaction_request =
                        |event: serde_json::Value| callbacks.emit_event(event);
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
                            Some(&emit_compaction_request),
                        )
                        .await;
                    let (force_compacted, report) = match compacted_report {
                        Ok(pair) => pair,
                        Err(error) => {
                            callbacks.emit_event(serde_json::json!({
                                "type": "compaction.cancelled",
                            }));

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

                            if let Some(id) = summarizer_error
                                .and_then(|inner| {
                                    inner.downcast_ref::<kimi_core::error::LlmError>()
                                })
                                .and_then(kimi_core::error::LlmError::trace_id)
                            {
                                payload["request_id"] = serde_json::json!(id);
                            }
                            callbacks.telemetry(payload);
                            fail_current_step!(error);
                        }
                    };

                    if let Some(probe) = report.cache_probe.as_ref() {
                        callbacks.telemetry(probe.telemetry_payload());
                    }

                    if let Some(usage) = &report.usage {
                        compaction_usage.accumulate(usage);
                        did_compact = true;
                    }

                    if report.summary.is_empty() && force_compacted.len() >= messages.len() {
                        callbacks.emit_event(serde_json::json!({
                            "type": "compaction.cancelled",
                        }));
                        fail_current_step!(std::io::Error::other(err_str));
                    }

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


                        "messages": &force_compacted,
                    }));

                    callbacks.telemetry(report.finished_payload(
                        "auto",
                        consecutive_overflow_compactions,
                        input.llm.thinking_effort().unwrap_or("none"),
                        crate::turn_loop::turn_index(&turn_id),
                    ));

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

                    rearm_pending = true;
                    if let Some(toolset) = &input.toolset {
                        toolset.clear_pending_tool_schemas();
                        toolset.seed_loaded_tools_from_history(&messages);
                    }

                    if let Some(manager) = input
                        .toolset
                        .as_ref()
                        .and_then(|toolset| toolset.subagent_manager())
                    {
                        manager.bump_probe_generation(&input.agent_id);
                    }

                    cache_observation_resets.push(cache_observations.len());
                    sink_push_reset(&sink, sink_base + cache_observations.len());

                    if let Some(measurement) = last_request_usage.as_mut() {
                        measurement.spliced_after = true;
                    }
                    context_tokens.rebase_with_summary(
                        &messages,
                        optional_token_count_u32(
                            report.usage.as_ref().map(|usage| usage.output),
                        )?,
                    );
                }
            };

            consecutive_overflow_compactions = 0;

            total_usage.accumulate(&step_result.usage);

            last_request_usage = Some(crate::turn_loop::types::RequestMeasurement {
                usage: step_result.usage.clone(),
                input_messages: messages.len(),
                spliced_after: false,
            });

            if step_result.usage.input_other > 0 {
                context_tokens.record_measured(
                    token_count_u32(step_result.usage.input_other)?,
                    &messages,
                );
            }
            llm_retries += step_result.attempts.saturating_sub(1);
            last_finish_reason = step_result.finish_reason.clone();

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

            emit_step_end_event(
                callbacks.as_ref(),
                &input.agent_id,
                &turn_id,
                steps,
                &step_result,
            );

            if let Some(toolset) = input.toolset.as_ref() {
                toolset.set_llm_trace_id(step_result.trace_id.clone());
            }

            match step_result.stop_reason {
                LoopStepStopReason::Complete => {
                    if !step_result.content.is_empty() {
                        messages.push(LLMMessage {
                            role: kimi_core::types::Role::Assistant,
                            content: step_result.content.clone(),

                            blocks: step_result.thinking.clone(),
                            tool_calls: Vec::new(),
                            tool_call_id: None,

                            prompt_id: None,
                            origin: None,
                            tools: Vec::new(),
                        });
                    }

                    let stop_hook_continuation = if crate::turn_loop::turn_step::is_content_filtered(
                        step_result.finish_reason.as_deref(),
                    ) {
                        None
                    } else if let Some(ref guard) = hook_guard {
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
                    messages.push(LLMMessage {
                        role: kimi_core::types::Role::Assistant,
                        content: step_result.content.clone(),
                        blocks: step_result.thinking.clone(),
                        tool_calls: tool_calls.clone(),
                        tool_call_id: None,

                        prompt_id: None,
                        origin: None,
                        tools: Vec::new(),
                    });

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
                            role: kimi_core::types::Role::Tool,
                            content: crate::swarm::mode::veto_message(*veto),
                            blocks: Vec::new(),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(tool_call_id.clone()),
                            prompt_id: None,
                            origin: None,
                            tools: Vec::new(),
                        });
                    }
                    if tool_calls.is_empty() {
                        continue;
                    }

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
                                role: kimi_core::types::Role::Tool,
                                content: refusal.clone(),
                                blocks: Vec::new(),
                                tool_calls: Vec::new(),
                                tool_call_id: Some(tc.id.clone()),
                                prompt_id: None,
                                origin: None,
                                tools: Vec::new(),
                            });
                        }

                        continue;
                    }

                    let mut parsed_arguments: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|call| {
                            let parsed = crate::llm::tool_args::parse_tool_arguments(
                                call.arguments.as_deref(),
                            );
                            if parsed.parse_failed {
                                tracing::debug!(
                                    tool_name = %call.name,
                                    tool_call_id = %call.id,
                                    raw_length = call.arguments.as_ref().map_or(0, |raw| raw.encode_utf16().count()),
                                    error = ?parsed.error,
                                    "tool args JSON parse failed"
                                );
                            }
                            parsed.data
                        })
                        .collect();
                    let mut refusals: Vec<Option<String>> = tool_calls
                        .iter()
                        .zip(&parsed_arguments)
                        .map(|(tc, arguments)| {
                            unusable_tool_arguments(tc, arguments, &step_tool_defs, &tool_defs)
                                .map(|reason| reason.message(&tc.name))
                        })
                        .collect();

                    let dedupe_plan = tool_dedupe.plan_step_by(&tool_calls, |tc| {
                        crate::tools::is_native_tool_name(&tc.name)
                            .then(|| crate::tools::tool_dedupe::dedupe_key(tc))
                    });
                    {
                        let trace_id = input
                            .toolset
                            .as_ref()
                            .and_then(|toolset| toolset.llm_trace_id());
                        let step_no = steps.saturating_sub(1);
                        for (index, original) in dedupe_plan.original_of.iter().enumerate() {
                            if *original == index {
                                continue;
                            }
                            let call = &tool_calls[index];
                            callbacks.telemetry(serde_json::json!({
                                "event": "tool_call_dedup_detected",
                                "turn_id": turn_id,
                                "step_no": step_no,
                                "tool_call_id": call.id,
                                "tool_name": call.name,
                                "dup_type": "same_step",
                                "args_hash": crate::tools::tool_dedupe::args_hash(&parsed_arguments[index]),
                                "trace_id": trace_id,
                            }));
                        }
                        let turn_repeat_count = tool_dedupe.turn_repeat_count();
                        for index in &dedupe_plan.cross_step {
                            let call = &tool_calls[*index];
                            let args_hash = crate::tools::tool_dedupe::args_hash(&parsed_arguments[*index]);
                            callbacks.telemetry(serde_json::json!({
                                "event": "tool_call_dedup_detected",
                                "turn_id": turn_id,
                                "step_no": step_no,
                                "tool_call_id": call.id,
                                "tool_name": call.name,
                                "dup_type": "cross_step",
                                "args_hash": args_hash,
                                "trace_id": trace_id,
                            }));
                            callbacks.telemetry(serde_json::json!({
                                "event": "tool_call_turn_repeat",
                                "turn_id": turn_id,
                                "step_no": step_no,
                                "tool_call_id": call.id,
                                "tool_name": call.name,
                                "turn_repeat_count": turn_repeat_count,
                                "args_hash": args_hash,
                                "trace_id": trace_id,
                            }));
                        }
                    }
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

                    let mut synthetic_results: Vec<Option<ExecutableToolResult>> =
                        tool_calls.iter().map(|_| None).collect();
                    for index in 0..tool_calls.len() {
                        if refusals[index].is_some()
                            || dedupe_plan
                                .original_of
                                .get(index)
                                .is_some_and(|original| *original != index)
                        {
                            continue;
                        }

                        synthetic_results[index] = evaluate_goal_completion(
                            callbacks.as_ref(),
                            input.llm,
                            &messages,
                            &turn_id,
                            &input.agent_id,
                            &mut tool_calls[index],
                            &mut parsed_arguments[index],
                            turn_cancel.token().clone(),
                        )
                        .await;
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

                    let execution_arguments = parsed_arguments.clone();
                    let exec_fn = {
                        let execution_arguments = execution_arguments.clone();
                        let call_index = call_index.clone();
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
                            let execution_arguments = execution_arguments.clone();
                            async move {
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
                                let arguments = index
                                    .map(|index| execution_arguments[index].clone())
                                    .expect("scheduled call has parsed arguments");
                                let req = ToolExecuteRequest {
                                    turn_id: turn_id.clone(),
                                    agent_id: agent_id.clone(),
                                    tool_call_id: tc.id.clone(),
                                    tool_name: tc.name.clone(),
                                    arguments: arguments.clone(),
                                };

                                let resolution = crate::tools::resolve::resolve_tool_execution(
                                    &tc.name,
                                    &arguments,
                                    workspace_dir.as_ref().map(String::as_str),
                                );
                                let call_description = match resolution.description {
                                    Some(description) => Some(description),
                                    None if tc.name.starts_with("mcp__") => {
                                        let authenticate = match toolset
                                            .as_ref()
                                            .and_then(|toolset| toolset.mcp_manager())
                                        {
                                            Some(mcp) => {
                                                mcp.authenticate_tool_description(&tc.name).await
                                            }
                                            None => None,
                                        };
                                        authenticate
                                            .or_else(|| tool_descriptions.get(&tc.name).cloned())
                                    }
                                    None => None,
                                }
                                .filter(|description| !description.is_empty());
                                let call_display = resolution.display;
                                let execute = async {
                                    let mut started = serde_json::json!({
                                        "type": "tool.call.started",
                                        "agent_id": agent_id,
                                        "turn_id": turn_id,
                                        "tool_call_id": tc.id,
                                        "tool_name": tc.name,
                                        "args": arguments,
                                    });

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
                                                delivery: response
                                                    .delivery
                                                    .as_ref()
                                                    .map(crate::llm::wire::tool_delivery_from_wire),
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

                                    None => Ok(execute.await),
                                }
                            }
                        }
                    };

                    let mut schedulable = Vec::with_capacity(tool_calls.len());
                    for (index, tc) in tool_calls.iter().enumerate() {
                        if refusals[index].is_some() || synthetic_results[index].is_some() {
                            continue;
                        }
                        if tc.name.eq_ignore_ascii_case("bash")
                            && let Some(toolset) = input.toolset.as_ref()
                            && let Some(command) =
                                parsed_arguments[index].get("command").and_then(|value| value.as_str())
                            && crate::features::tower::bashGuard::command_needs_tower_guard(command)
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
                                    crate::features::tower::bashGuard::tower_workspace_owned_json(
                                        &raw,
                                    )
                                })
                                .unwrap_or(false);
                            if owned {
                                let execution_cwd = input
                                    .toolset
                                    .as_ref()
                                    .map(|toolset| toolset.execution_root())
                                    .or_else(|| std::env::current_dir().ok())
                                    .and_then(|cwd| std::fs::canonicalize(&cwd).ok())
                                    .map(crate::tools::strip_verbatim_prefix)
                                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                                let cwd = parsed_arguments[index]
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
                                    crate::features::tower::bashGuard::analyze_tower_bash_command(
                                        &crate::features::tower::bashGuard::TowerBashGuardTarget {
                                            command: command.to_string(),
                                            cwd: cwd.clone(),
                                            main_checkout: main_checkout.clone(),
                                        },
                                        crate::features::tower::bashGuard::parse_shell_script,
                                    );
                                if let Some(veto) = veto {
                                    refusals[index] = Some(veto);
                                    continue;
                                }
                            }
                        }
                        schedulable.push(tc.clone());
                    }

                    let mut predicted_stop_index: Option<usize> = None;
                    for (index, tc) in tool_calls.iter().enumerate() {
                        if refusals[index].is_some() {
                            continue;
                        }
                        if crate::tools::goal_tools::predicted_stop_batch(
                            callbacks.as_ref(),
                            &input.agent_id,
                            &tc.name,
                            &parsed_arguments[index],
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
                            accesses: tool_scheduler::infer_tool_accesses(
                                &tc.name,
                                &execution_arguments[call_index[&tc.id]],
                            ),
                        })
                        .collect();
                    let run_count = predicted_stop_index.map(|stop| {
                        (0..=stop)
                            .filter(|index| {
                                refusals[*index].is_none() && synthetic_results[*index].is_none()
                            })
                            .count()
                    });

                    turn_watch_state.set_phase(TurnPhase::Tools);

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
                    let mut executed_call_indices = Vec::new();
                    for (index, refusal) in refusals.iter().enumerate() {
                        if run_count.is_some()
                            && predicted_stop_index.is_some_and(|stop| index > stop)
                        {
                            let skipped_call = &tool_calls[index];
                            callbacks.emit_event(serde_json::json!({
                                "type": "tool.call.started",
                                "agent_id": input.agent_id,
                                "turn_id": turn_id,
                                "tool_call_id": skipped_call.id,
                                "tool_name": skipped_call.name,
                                "args": execution_arguments[index],
                            }));
                            callbacks.emit_event(serde_json::json!({
                                "type": "tool.native",
                                "agent_id": input.agent_id,
                                "turn_id": turn_id,
                                "tool_call_id": skipped_call.id,
                                "tool_name": skipped_call.name,
                                "arguments": execution_arguments[index],
                                "content": tool_scheduler::SKIPPED_TOOL_OUTPUT,
                                "is_error": true,
                                "note": null,
                            }));
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
                                let duration = executed_durations.next().flatten();
                                if duration.is_some() {
                                    executed_call_indices.push(index);
                                }
                                durations_ms.push(duration);
                            }
                        }
                    }

                    step_access = crate::injection::StepAccess::default();
                    for index in executed_call_indices.iter().copied() {
                        if dedupe_plan
                            .original_of
                            .get(index)
                            .is_some_and(|original| *original != index)
                        {
                            continue;
                        }
                        let call = &tool_calls[index];
                        let name = call.name.to_ascii_lowercase();
                        if name == "bash" {
                            let arguments = &execution_arguments[index];
                            let cwd_arg = arguments.get("cwd").and_then(|value| value.as_str());
                            if let Some(cwd) = cwd_arg {
                                step_access.declared_cwds.push(cwd.to_string());
                            }
                            if let Some(command) =
                                arguments.get("command").and_then(|value| value.as_str())
                            {
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
                        for access in
                            tool_scheduler::infer_tool_accesses(&call.name, &execution_arguments[index])
                        {
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

                    let dedupe_report =
                        tool_dedupe.finalize_step_report(&dedupe_plan, &mut results);
                    let dedupe_force_stop = dedupe_report.force_stop;
                    {
                        let trace_id = input
                            .toolset
                            .as_ref()
                            .and_then(|toolset| toolset.llm_trace_id());
                        for repeat in &dedupe_report.repeats {
                            let Some(call) = tool_calls.get(repeat.index) else {
                                continue;
                            };
                            callbacks.telemetry(serde_json::json!({
                                "event": "tool_call_repeat",
                                "turn_id": turn_id,
                                "tool_name": call.name,
                                "repeat_count": repeat.repeat_count,
                                "action": repeat.action,
                                "trace_id": trace_id,
                            }));
                        }
                    }

                    for (index, result) in results.iter().enumerate() {
                        if result.is_error || !executed_call_indices.contains(&index) {
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
                            tool_scheduler::infer_tool_accesses(&call.name, &execution_arguments[index])
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
                        let Some(command) = execution_arguments[index]
                            .get("command")
                            .and_then(|value| value.as_str())
                        else {
                            continue;
                        };
                        if test_scope_guard.observe(command).is_some() {
                            break;
                        }
                    }

                    for (index, result) in results.iter().enumerate() {
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
                            progress_receipt(&call.name, &execution_arguments[index], !result.is_error);
                        let blind = match input.toolset.as_ref() {
                            Some(toolset) => toolset.observe_progress(&receipt),
                            None => local_progress_tracker.observe(&receipt),
                        };
                        if blind >= crate::turn_loop::progress_track::EBM_BLIND_MUTATION_THRESHOLD {
                            ebm_pending.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }

                    for (i, tr) in results.iter().enumerate() {
                        if (dedupe_plan.original_of.get(i).is_some_and(|o| *o != i)
                            || synthetic_indices.contains(&i))
                            && let Some(tc) = tool_calls.get(i)
                        {
                            callbacks.emit_event(serde_json::json!({
                                "type": "tool.call.started",
                                "agent_id": input.agent_id,
                                "turn_id": turn_id,
                                "tool_call_id": tc.id,
                                "tool_name": tc.name,
                                "args": execution_arguments[i],
                            }));
                            callbacks.emit_event(serde_json::json!({
                                "type": "tool.native",
                                "agent_id": input.agent_id,
                                "turn_id": turn_id,
                                "tool_call_id": tc.id,
                                "tool_name": tc.name,
                                "arguments": execution_arguments[i],
                                "content": tr.content,
                                "is_error": tr.is_error,
                                "note": tr.note,
                            }));
                        }

                        let tool_name = tool_calls.get(i).map(|tc| tc.name.as_str()).unwrap_or("");

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
                                [ContentPart::Text { text, .. }] => text.clone(),
                                _ => String::new(),
                            }
                        };
                        messages.push(LLMMessage {
                            role: kimi_core::types::Role::Tool,
                            content,
                            blocks: rendered_parts,
                            tool_calls: Vec::new(),
                            tool_call_id: tool_calls.get(i).map(|tc| tc.id.clone()),
                            prompt_id: None,
                            origin: None,
                            tools: Vec::new(),
                        });

                        let is_repeat = dedupe_plan.original_of.get(i).is_some_and(|o| *o != i);
                        if !is_repeat
                            && let Some(delivery) = tr.delivery.as_ref()
                            && !delivery.blocks.is_empty()
                        {
                            messages.push(LLMMessage {
                                role: kimi_core::types::Role::User,
                                content: String::new(),
                                blocks: delivery.blocks.clone(),
                                tool_calls: Vec::new(),
                                tool_call_id: None,

                                prompt_id: None,
                                origin: delivery.origin.clone(),
                                tools: Vec::new(),
                            });
                        }
                    }

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
                        steps += 1;
                        turn_watch_state.note_steps(steps as u64);
                        turn_watch_state.set_phase(TurnPhase::Llm);

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
                            Some(token_count_u32(total_usage.input_other)?),
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
                            callbacks.telemetry(serde_json::json!({
                                "event": "tool_call_repeat_handoff",
                                "turn_id": turn_id,
                                "outcome": if vetoed.is_empty() { "text" } else { "vetoed" },
                            }));
                            if vetoed.is_empty() {
                                if !step_res.content.is_empty() {
                                    messages.push(LLMMessage {
                                        role: kimi_core::types::Role::Assistant,
                                        content: step_res.content,
                                        blocks: step_res.thinking,
                                        tool_calls: Vec::new(),
                                        tool_call_id: None,

                                        prompt_id: None,
                                        origin: None,
                                        tools: Vec::new(),
                                    });
                                }
                            } else {
                                messages.push(LLMMessage {
                                    role: kimi_core::types::Role::Assistant,
                                    content: step_res.content,
                                    blocks: step_res.thinking,
                                    tool_calls: vetoed.clone(),
                                    tool_call_id: None,

                                    prompt_id: None,
                                    origin: None,
                                    tools: Vec::new(),
                                });
                                let veto = crate::tools::tool_dedupe::handoff_veto_text();
                                let rendered = tool_result_render::render_status(&veto, true);
                                for call in &vetoed {
                                    callbacks.emit_event(serde_json::json!({
                                        "type": "tool.call.started",
                                        "agent_id": input.agent_id,
                                        "turn_id": turn_id,
                                        "tool_call_id": call.id,
                                        "tool_name": call.name,
                                        "args": crate::llm::tool_args::parse_tool_arguments(call.arguments.as_deref()).data,
                                    }));
                                    callbacks.emit_event(serde_json::json!({
                                        "type": "tool.native",
                                        "agent_id": input.agent_id,
                                        "turn_id": turn_id,
                                        "tool_call_id": call.id,
                                        "tool_name": call.name,
                                        "arguments": crate::llm::tool_args::parse_tool_arguments(call.arguments.as_deref()).data,
                                        "content": rendered,
                                        "is_error": true,
                                        "note": null,
                                    }));
                                    messages.push(LLMMessage {
                                        role: kimi_core::types::Role::Tool,
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

        let reason = match turn_stop_reason_from_finish(last_finish_reason.as_deref()) {
            LoopTurnStopReason::EndTurn => LoopTurnStopReason::MaxSteps,
            other => other,
        };

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

    let total_tokens = goal.tokens_used + turn_tokens;
    let total_turns = goal.turns_used + turns_this_turn;
    let total_wall_clock_ms = goal.wall_clock_ms + turn_wall_clock_ms;
    lines.push(format!(
        "Progress: {} continuation turns, {} tokens consumed, {} elapsed.",
        total_turns,
        total_tokens,
        format_elapsed(total_wall_clock_ms)
    ));

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

enum UnusableArguments {
    NotObject,

    Schema(String),
}

impl UnusableArguments {
    fn message(&self, tool_name: &str) -> String {
        match self {
            Self::NotObject => crate::i18n::LocalizedText::with_params(
                "engine.tools.argsNotObject",
                crate::i18n::i18n_params!["tool" => tool_name],
            ),
            Self::Schema(detail) => crate::i18n::LocalizedText::with_params(
                "engine.execution.invalidArgs",
                crate::i18n::i18n_params!["toolName" => tool_name, "error" => detail],
            ),
        }
        .render()
    }
}

fn unusable_tool_arguments(
    call: &ToolCall,
    arguments: &serde_json::Value,
    step_defs: &[crate::rpc::types::ToolInfo],
    turn_defs: &[crate::rpc::types::ToolInfo],
) -> Option<UnusableArguments> {
    if !arguments.is_object() {
        return Some(UnusableArguments::NotObject);
    }
    let definition = find_tool_definition(step_defs, &call.name)
        .or_else(|| find_tool_definition(turn_defs, &call.name))?;
    crate::tools::args_validate::validate(&definition.input_schema, arguments)
        .err()
        .map(UnusableArguments::Schema)
}

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
