use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_channel::mpsc;
use futures_util::future::{BoxFuture, Either, FutureExt, select};
use futures_util::stream::{BoxStream, FuturesUnordered, StreamExt};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::lifecycle::{DisposableRef, to_disposable};
use crate::_base::errors::errors::SharedError;
use crate::_base::event::{AsyncEmitter, Event};
use crate::_base::log::log::ILogService;
use crate::_base::state::state_registry::StateKey;
use crate::_base::utils::abort::{
    AbortController, AbortSignal, abortable, is_abort_error, is_user_cancellation,
    link_abort_signal,
};
use crate::agent::state::agent_state::IAgentStateService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::message::{ContentPart, ToolCall};
use crate::state::event_dispatcher::IEventDispatcher;
use crate::tool::args_validator;
use crate::tool::path_access::PathSecurityError;
use crate::tool::tool_args_parse::parse_tool_arguments;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::tool_result_truncation::{ToolResultTruncator, TruncationRequest};
use crate::turn_loop::types::{ToolAccesses, all_access};
use crate::wire::record::WireAsyncError;

use super::before_tool_execute_event::{BeforeToolExecuteEmitter, BeforeToolExecuteEventImpl};
use super::tool_executor::*;
use super::tool_executor_events::{
    ToolCallStarted, ToolProgress, ToolResultEvent, ToolResultEventPayload, ToolUpdate,
};
use super::tool_hooks::*;
use super::tool_scheduler::{ToolCallTask, ToolCallTaskStart, ToolScheduler};

const ABORT_GRACE_MS: u64 = 2_000;
const TOOL_OUTPUT_EMPTY: &str = "Tool output is empty.";
const TOOL_OUTPUT_NON_TEXT: &str = "Tool returned non-text content.";

pub fn tool_executor_tool_call_dup_types_key() -> StateKey<Value> {
    StateKey::new("toolExecutor.toolCallDupTypes", || json!({}))
}

pub fn tool_executor_dup_type_turn_id_key() -> StateKey<Value> {
    StateKey::new("toolExecutor.dupTypeTurnId", || Value::Null)
}

#[derive(Clone)]
pub struct ToolExecutionTask {
    pub accesses: ToolAccesses,
    pub execute: Arc<
        dyn Fn(AbortSignal) -> BoxFuture<'static, Result<ToolExecutionRunResult, WireAsyncError>>
            + Send
            + Sync,
    >,
}

#[derive(Debug)]
pub struct ToolExecutionRunResult {
    pub result: ToolResult,
    pub outcome: ToolExecutionOutcome,
}

#[derive(Debug)]
struct TimedToolResult {
    index: usize,
    result: ToolResult,
    outcome: ToolExecutionOutcome,
    duration_ms: u64,
}

type TimedResultFuture = BoxFuture<'static, Result<TimedToolResult, WireAsyncError>>;
type FinalizedResultFuture = BoxFuture<'static, Result<ToolExecutionResult, WireAsyncError>>;

async fn drain_pending_results(
    timed_results: &mut FuturesUnordered<TimedResultFuture>,
    finalizations: &mut FuturesUnordered<FinalizedResultFuture>,
) {
    while timed_results.next().await.is_some() {}
    while finalizations.next().await.is_some() {}
}

#[derive(Clone)]
enum PreflightedToolCall {
    Runnable {
        tool_call: ToolCall,
        tool: Arc<dyn ExecutableTool>,
        args: Value,
    },
    Rejected {
        tool_call: ToolCall,
        args: Value,
        output: String,
    },
}

impl PreflightedToolCall {
    fn tool_call(&self) -> &ToolCall {
        match self {
            Self::Runnable { tool_call, .. } | Self::Rejected { tool_call, .. } => tool_call,
        }
    }

    fn tool_name(&self) -> &str {
        &self.tool_call().name
    }

    fn args(&self) -> &Value {
        match self {
            Self::Runnable { args, .. } | Self::Rejected { args, .. } => args,
        }
    }

    fn tool(&self) -> Option<Arc<dyn ExecutableTool>> {
        match self {
            Self::Runnable { tool, .. } => Some(tool.clone()),
            Self::Rejected { .. } => None,
        }
    }
}

#[derive(Clone)]
struct PreparedTask {
    call: PreflightedToolCall,
    task: ToolExecutionTask,
    resolved_accesses: Option<ToolAccesses>,
    stop_batch_after_this: Option<bool>,
}

#[derive(Clone, Default)]
struct ToolCallDisplayFields {
    description: Option<String>,
    display: Option<ToolInputDisplay>,
}

pub struct AgentToolExecutorService {
    agent_id: String,
    tool_registry: Arc<dyn IAgentToolRegistryService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    telemetry: Arc<dyn ITelemetryService>,
    result_truncation: Arc<dyn IAgentToolResultTruncationService>,
    states: Arc<dyn IAgentStateService>,
    log: Option<Arc<dyn ILogService>>,
    before_execute_emitter: BeforeToolExecuteEmitter,
    will_execute_emitter: AsyncEmitter<WillExecuteToolEvent>,
    hooks: ToolExecutorHooks,
    missing_tool_describer: Arc<Mutex<Option<MissingToolDescriber>>>,
    unavailable_tool_describer: Arc<Mutex<Option<UnavailableToolDescriber>>>,
    tool_call_guard: Arc<Mutex<Option<ToolCallGuard>>>,
    _registrations: Vec<DisposableRef>,
}

impl AgentToolExecutorService {
    pub fn new(
        agent_id: String,
        tool_registry: Arc<dyn IAgentToolRegistryService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        telemetry: Arc<dyn ITelemetryService>,
        result_truncation: Arc<dyn IAgentToolResultTruncationService>,
        states: Arc<dyn IAgentStateService>,
        log: Option<Arc<dyn ILogService>>,
    ) -> Result<Self, crate::agent::state::errors::StateError> {
        let registrations = vec![
            states.contribute_state(tool_executor_tool_call_dup_types_key())?,
            states.contribute_state(tool_executor_dup_type_turn_id_key())?,
        ];
        Ok(Self {
            agent_id,
            tool_registry,
            dispatcher,
            telemetry,
            result_truncation,
            states,
            log,
            before_execute_emitter: BeforeToolExecuteEmitter::new(),
            will_execute_emitter: AsyncEmitter::new(),
            hooks: ToolExecutorHooks::default(),
            missing_tool_describer: Arc::new(Mutex::new(None)),
            unavailable_tool_describer: Arc::new(Mutex::new(None)),
            tool_call_guard: Arc::new(Mutex::new(None)),
            _registrations: registrations,
        })
    }

    fn tool_call_dup_types(&self) -> HashMap<String, ToolCallDupType> {
        serde_json::from_value(self.states.get(&tool_executor_tool_call_dup_types_key()))
            .unwrap_or_default()
    }

    fn set_tool_call_dup_types(&self, value: HashMap<String, ToolCallDupType>) {
        if let Ok(value) = serde_json::to_value(value) {
            self.states
                .set(&tool_executor_tool_call_dup_types_key(), value);
        }
    }

    fn dup_type_turn_id(&self) -> Option<u64> {
        self.states
            .get(&tool_executor_dup_type_turn_id_key())
            .as_u64()
    }

    fn set_dup_type_turn_id(&self, value: Option<u64>) {
        self.states.set(
            &tool_executor_dup_type_turn_id_key(),
            value.map_or(Value::Null, Value::from),
        );
    }

    async fn execute_inner(
        self: Arc<Self>,
        calls: Vec<ToolCall>,
        options: ToolExecutorExecuteOptions,
        sender: mpsc::UnboundedSender<Result<ToolExecutionResult, WireAsyncError>>,
    ) {
        if calls.is_empty() {
            return;
        }
        if self.dup_type_turn_id() != Some(options.turn_id) {
            self.set_dup_type_turn_id(Some(options.turn_id));
            self.set_tool_call_dup_types(HashMap::new());
        }

        let preflighted = calls
            .iter()
            .map(|call| {
                self.preflight_tool_call(
                    call.clone(),
                    self.tool_call_guard.lock().clone(),
                    self.unavailable_tool_describer.lock().clone(),
                    self.missing_tool_describer.lock().clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut prepared_tasks = Vec::new();
        let mut stop_batch = false;
        for call in preflighted {
            if stop_batch {
                prepared_tasks.push(self.prepare_skipped_tool_call(call, &options));
                continue;
            }
            match self.prepare_tool_call(call.clone(), &calls, &options).await {
                Ok(prepared) => {
                    stop_batch = prepared.stop_batch_after_this == Some(true);
                    prepared_tasks.push(prepared);
                }
                Err(error) => {
                    let _ = sender.unbounded_send(Err(error));
                    return;
                }
            }
        }

        let scheduler = ToolScheduler::new();
        let mut timed_results = FuturesUnordered::new();
        for (index, prepared) in prepared_tasks.iter().enumerate() {
            let task = prepared.task.clone();
            let accesses = task.accesses.clone();
            let signal = options.signal.clone();
            let start = Arc::new(move || {
                let task = task.clone();
                let signal = signal.clone();
                async move {
                    let started_at = chrono::Utc::now().timestamp_millis();
                    let execute = (task.execute)(signal);
                    Ok(ToolCallTaskStart {
                        result: async move {
                            execute.await.map(|run| TimedToolResult {
                                index,
                                result: run.result,
                                outcome: run.outcome,
                                duration_ms: (chrono::Utc::now().timestamp_millis() - started_at)
                                    .max(0) as u64,
                            })
                        }
                        .boxed(),
                    })
                }
                .boxed()
            });
            timed_results.push(scheduler.add(ToolCallTask {
                accesses,
                start,
            }));
        }

        let mut finalizations = FuturesUnordered::new();
        let mut timed_done = timed_results.is_empty();
        loop {
            if timed_done && finalizations.is_empty() {
                break;
            }
            if timed_done {
                match finalizations.next().await {
                    Some(Ok(result)) => {
                        if sender.unbounded_send(Ok(result)).is_err() {
                            drain_pending_results(&mut timed_results, &mut finalizations).await;
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = sender.unbounded_send(Err(error));
                        drain_pending_results(&mut timed_results, &mut finalizations).await;
                        return;
                    }
                    None => break,
                }
                continue;
            }
            if finalizations.is_empty() {
                match timed_results.next().await {
                    Some(Ok(timed)) => {
                        finalizations.push(self.finalize_timed_result(
                            prepared_tasks[timed.index].clone(),
                            timed,
                            options.clone(),
                        ));
                    }
                    Some(Err(error)) => {
                        let _ = sender.unbounded_send(Err(error));
                        drain_pending_results(&mut timed_results, &mut finalizations).await;
                        return;
                    }
                    None => timed_done = true,
                }
                continue;
            }
            match select(timed_results.next().boxed(), finalizations.next().boxed()).await {
                Either::Left((timed, _)) => match timed {
                    Some(Ok(timed)) => {
                        finalizations.push(self.finalize_timed_result(
                            prepared_tasks[timed.index].clone(),
                            timed,
                            options.clone(),
                        ));
                    }
                    Some(Err(error)) => {
                        let _ = sender.unbounded_send(Err(error));
                        drain_pending_results(&mut timed_results, &mut finalizations).await;
                        return;
                    }
                    None => timed_done = true,
                },
                Either::Right((finalized, _)) => match finalized {
                    Some(Ok(result)) => {
                        if sender.unbounded_send(Ok(result)).is_err() {
                            drain_pending_results(&mut timed_results, &mut finalizations).await;
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = sender.unbounded_send(Err(error));
                        drain_pending_results(&mut timed_results, &mut finalizations).await;
                        return;
                    }
                    None => break,
                },
            }
        }
    }

    fn finalize_timed_result(
        self: &Arc<Self>,
        prepared: PreparedTask,
        timed: TimedToolResult,
        options: ToolExecutorExecuteOptions,
    ) -> BoxFuture<'static, Result<ToolExecutionResult, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move {
            let call = prepared.call;
            let finalized = this
                .finalize_tool_result(
                    call.clone(),
                    timed.result,
                    &options,
                    timed.outcome,
                    prepared.resolved_accesses,
                )
                .await?;
            this.dispatch_tool_result(&call, &finalized, &options);
            this.track_tool_call(&call, &finalized, timed.duration_ms, &options);
            Ok(ToolExecutionResult {
                tool_call_id: call.tool_call().id.clone(),
                tool_name: call.tool_name().to_string(),
                result: finalized,
                duration_ms: timed.duration_ms,
            })
        })
    }

    fn track_tool_call(
        &self,
        call: &PreflightedToolCall,
        result: &ToolResult,
        duration_ms: u64,
        options: &ToolExecutorExecuteOptions,
    ) {
        let outcome = tool_telemetry_outcome(result);
        let tool_call_id = &call.tool_call().id;
        let mut dup_types = self.tool_call_dup_types();
        let dup_type = dup_types
            .remove(tool_call_id)
            .map(|value| value.as_str().to_string())
            .unwrap_or_else(|| "normal".to_string());
        self.set_tool_call_dup_types(dup_types);
        let mut properties = Map::new();
        properties.insert("turn_id".to_string(), json!(options.turn_id));
        properties.insert("tool_call_id".to_string(), json!(tool_call_id));
        properties.insert("tool_name".to_string(), json!(call.tool_name()));
        properties.insert("outcome".to_string(), json!(outcome));
        properties.insert("duration_ms".to_string(), json!(duration_ms));
        properties.insert("dup_type".to_string(), json!(dup_type));
        if let Some(trace_id) = options.trace.as_ref().and_then(|trace| trace.trace_id.as_ref()) {
            properties.insert("trace_id".to_string(), json!(trace_id));
        }
        if result.is_error == Some(true) {
            properties.insert(
                "error_type".to_string(),
                json!(if outcome == "cancelled" { "cancelled" } else { "error" }),
            );
        }
        self.telemetry.track2("tool_call", Some(properties));
    }

    async fn prepare_tool_call(
        self: &Arc<Self>,
        call: PreflightedToolCall,
        all_calls: &[ToolCall],
        options: &ToolExecutorExecuteOptions,
    ) -> Result<PreparedTask, WireAsyncError> {
        if let PreflightedToolCall::Rejected { output, .. } = &call {
            self.dispatch_tool_call(&call, call.args().clone(), options, None);
            return Ok(PreparedTask {
                call: call.clone(),
                task: make_resolved_task(
                    make_error_tool_result(&call, output.clone()),
                    ToolExecutionOutcome::PreflightRejected,
                ),
                resolved_accesses: None,
                stop_batch_after_this: None,
            });
        }

        let tool = call.tool().expect("runnable call has a tool");
        let execution = match tool.resolve_execution(call.args().clone()).await {
            Ok(execution) => execution,
            Err(error) => {
                let output = if let Some(path_error) = error.downcast_ref::<PathSecurityError>() {
                    path_error.message.clone()
                } else {
                    format!(
                        "Tool \"{}\" failed to resolve execution: {}",
                        call.tool_name(),
                        error
                    )
                };
                self.dispatch_tool_call(&call, call.args().clone(), options, None);
                return Ok(PreparedTask {
                    call: call.clone(),
                    task: make_resolved_task(
                        make_error_tool_result(&call, output),
                        ToolExecutionOutcome::ResolutionFailed,
                    ),
                    resolved_accesses: None,
                    stop_batch_after_this: None,
                });
            }
        };
        if options.signal.aborted() {
            let display_fields = match &execution {
                ToolExecution::Runnable(execution) => {
                    Some(tool_call_display_fields_from_execution(execution))
                }
                ToolExecution::Error(_) => None,
            };
            self.dispatch_tool_call(&call, call.args().clone(), options, display_fields);
            let output = aborted_tool_output(call.tool_name(), options.signal.reason());
            return Ok(PreparedTask {
                call: call.clone(),
                task: make_resolved_task(
                    make_error_tool_result(&call, output),
                    ToolExecutionOutcome::Aborted,
                ),
                resolved_accesses: None,
                stop_batch_after_this: None,
            });
        }

        let execution = match execution {
            ToolExecution::Runnable(execution) => execution,
            ToolExecution::Error(result) => {
                let normalized = self.normalize_and_merge_result(result, call.tool_name(), None);
                let stop_batch = normalized.stop_batch_after_this.or(normalized.stop_turn);
                self.dispatch_tool_call(&call, call.args().clone(), options, None);
                return Ok(PreparedTask {
                    call: call.clone(),
                    task: make_resolved_task(normalized, ToolExecutionOutcome::Synthetic),
                    resolved_accesses: None,
                    stop_batch_after_this: stop_batch,
                });
            }
        };

        let display_fields = tool_call_display_fields_from_execution(&execution);

        let before_context = ResolvedToolExecutionHookContext {
            base: ToolExecutionHookContext {
                turn_id: options.turn_id,
                signal: options.signal.clone(),
                trace: options.trace.clone(),
                tool_call: call.tool_call().clone(),
                tool_calls: all_calls.to_vec(),
                tool: Some(tool),
                args: call.args().clone(),
            },
            execution: execution.clone(),
        };
        let decision = self
            .before_execute_emitter
            .fire_before_execute(before_context)
            .await?;
        if let Some(veto) = decision.as_ref().and_then(|decision| decision.veto.clone()) {
            let normalized = self.normalize_and_merge_result(veto, call.tool_name(), None);
            let stop_batch = normalized.stop_batch_after_this.or(normalized.stop_turn);
            self.dispatch_tool_call(&call, call.args().clone(), options, Some(display_fields));
            return Ok(PreparedTask {
                call: call.clone(),
                task: make_resolved_task(normalized, ToolExecutionOutcome::Vetoed),
                resolved_accesses: None,
                stop_batch_after_this: stop_batch,
            });
        }
        let metadata = decision.and_then(|decision| decision.execution_metadata);

        self.will_execute_emitter
            .fire_async(
                super::tool_hooks::WillExecuteToolEventData {
                    turn_id: options.turn_id,
                    tool_call: call.tool_call().clone(),
                    execution: execution.clone(),
                    args: call.args().clone(),
                },
                options.signal.clone(),
            )
            .await;
        self.dispatch_tool_call(&call, call.args().clone(), options, Some(display_fields));

        let resolved_accesses = execution.accesses.clone();
        let stop_batch_after_this = execution.stop_batch_after_this;
        let this = self.clone();
        let task_call = call.clone();
        let task_execution = execution.clone();
        let task_options = options.clone();
        Ok(PreparedTask {
            call,
            task: ToolExecutionTask {
                accesses: execution.accesses.unwrap_or_else(|| vec![all_access()]),
                execute: Arc::new(move |signal| {
                    let this = this.clone();
                    let call = task_call.clone();
                    let execution = task_execution.clone();
                    let options = task_options.clone();
                    let metadata = metadata.clone();
                    async move {
                        this.run_single_execution(call, execution, metadata, options, signal)
                            .await
                    }
                    .boxed()
                }),
            },
            resolved_accesses,
            stop_batch_after_this,
        })
    }

    fn prepare_skipped_tool_call(
        &self,
        call: PreflightedToolCall,
        options: &ToolExecutorExecuteOptions,
    ) -> PreparedTask {
        self.dispatch_tool_call(&call, call.args().clone(), options, None);
        PreparedTask {
            call: call.clone(),
            task: make_resolved_task(
                make_error_tool_result(
                    &call,
                    "Tool skipped because a previous tool call stopped the turn.".to_string(),
                ),
                ToolExecutionOutcome::Skipped,
            ),
            resolved_accesses: None,
            stop_batch_after_this: None,
        }
    }

    async fn run_single_execution(
        self: Arc<Self>,
        call: PreflightedToolCall,
        execution: RunnableToolExecution,
        metadata: Option<Value>,
        options: ToolExecutorExecuteOptions,
        signal: AbortSignal,
    ) -> Result<ToolExecutionRunResult, WireAsyncError> {
        let executed = Arc::new(Mutex::new(false));
        let completed = Arc::new(Mutex::new(None));
        let context = ResolvedToolExecutionHookContext {
            base: ToolExecutionHookContext {
                turn_id: options.turn_id,
                signal: options.signal.clone(),
                trace: options.trace.clone(),
                tool_call: call.tool_call().clone(),
                tool_calls: vec![call.tool_call().clone()],
                tool: call.tool(),
                args: call.args().clone(),
            },
            execution: execution.clone(),
        };
        let terminal_executed = executed.clone();
        let terminal_completed = completed.clone();
        let terminal_call = call.clone();
        let terminal_options = options.clone();
        let terminal_signal = signal.clone();
        let terminal_metadata = metadata.clone();
        let this = self.clone();
        let hook = self.hooks.on_execute_tool.run_with_terminal(
            context,
            move |context| {
                *terminal_executed.lock() = true;
                let completed = terminal_completed.clone();
                let call = terminal_call.clone();
                let options = terminal_options.clone();
                let signal = terminal_signal.clone();
                let metadata = terminal_metadata.clone();
                let this = this.clone();
                Box::pin(async move {
                    let result = this
                        .run_single_execution_inner(
                            call,
                            context.execution,
                            metadata,
                            options,
                            signal,
                        )
                        .await;
                    *completed.lock() = Some(result);
                })
            },
        );
        let hook_result = AssertUnwindSafe(hook).catch_unwind().await;
        if let Err(error) = hook_result {
            let aborted = signal.aborted();
            let output = if aborted {
                format!("Tool \"{}\" aborted during onExecuteTool hook.", call.tool_name())
            } else {
                format!(
                    "onExecuteTool hook failed for \"{}\": {}",
                    call.tool_name(),
                    panic_message(error)
                )
            };
            return Ok(ToolExecutionRunResult {
                result: ToolResult::error(output),
                outcome: if *executed.lock() {
                    ToolExecutionOutcome::Executed
                } else {
                    ToolExecutionOutcome::HookFailed
                },
            });
        }
        let Some(completed) = completed.lock().take() else {
            return Ok(ToolExecutionRunResult {
                result: ToolResult::error(format!(
                    "onExecuteTool hook failed for \"{}\": the execution chain did not run the tool",
                    call.tool_name()
                )),
                outcome: ToolExecutionOutcome::HookFailed,
            });
        };
        completed
    }

    async fn run_single_execution_inner(
        &self,
        call: PreflightedToolCall,
        execution: RunnableToolExecution,
        metadata: Option<Value>,
        options: ToolExecutorExecuteOptions,
        signal: AbortSignal,
    ) -> Result<ToolExecutionRunResult, WireAsyncError> {
        if signal.aborted() {
            return Ok(ToolExecutionRunResult {
                result: make_error_tool_result(
                    &call,
                    aborted_tool_output(call.tool_name(), signal.reason()),
                ),
                outcome: ToolExecutionOutcome::Aborted,
            });
        }
        let this = self.clone();
        let progress_call = call.clone();
        let progress_options = options.clone();
        let progress_signal = signal.clone();
        let on_update = Arc::new(move |update: ToolUpdate| {
            if !progress_signal.aborted() {
                this.dispatch_tool_progress(&progress_call, update, &progress_options);
            }
        });
        let execute = execution.execute.clone();
        let raw_result = execute(ExecutableToolContext {
            turn_id: options.turn_id,
            tool_call_id: call.tool_call().id.clone(),
            trace: options.trace.clone(),
            metadata,
            signal: signal.clone(),
            steer_signal: options.steer_signal.clone(),
            on_update: Some(on_update),
            on_foreground_task_start: None,
        });
        let raw_result = match race_with_abort_grace(raw_result, signal.clone(), call.tool_name()).await {
            Ok(result) => result,
            Err(error) => {
                let aborted = is_abort_error(error.as_ref()) || signal.aborted();
                let output = if aborted {
                    aborted_tool_output(call.tool_name(), signal.reason())
                } else {
                    format!("Tool \"{}\" failed: {error}", call.tool_name())
                };
                return Ok(ToolExecutionRunResult {
                    result: make_error_tool_result(&call, output),
                    outcome: ToolExecutionOutcome::Executed,
                });
            }
        };
        Ok(ToolExecutionRunResult {
            result: self.normalize_and_merge_result(raw_result, call.tool_name(), Some(execution)),
            outcome: ToolExecutionOutcome::Executed,
        })
    }

    fn normalize_and_merge_result(
        &self,
        raw_result: ExecutableToolResult,
        _tool_name: &str,
        execution: Option<RunnableToolExecution>,
    ) -> ToolResult {
        let normalized = normalize_tool_result(raw_result);
        ToolResult {
            description: execution
                .as_ref()
                .and_then(|execution| execution.description.clone())
                .or_else(|| normalized.description.clone()),
            display: execution
                .as_ref()
                .and_then(|execution| execution.display.clone())
                .or_else(|| normalized.display.clone()),
            approval_rule: execution
                .as_ref()
                .map(|execution| execution.approval_rule.clone()),
            stop_batch_after_this: normalized
                .stop_batch_after_this
                .or_else(|| execution.as_ref().and_then(|execution| execution.stop_batch_after_this)),
            ..normalized
        }
    }

    fn dispatch_tool_call(
        &self,
        call: &PreflightedToolCall,
        args: Value,
        options: &ToolExecutorExecuteOptions,
        display_fields: Option<ToolCallDisplayFields>,
    ) {
        let fields = display_fields.unwrap_or_default();
        let event = ToolCallStarted::new(super::tool_executor_events::ToolCallStartedPayload {
            agent_id: self.agent_id.clone(),
            turn_id: options.turn_id,
            tool_call_id: call.tool_call().id.clone(),
            name: call.tool_name().to_string(),
            args: args.clone(),
            description: fields.description,
            display: fields.display.clone(),
        });
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
        if let Some(on_tool_call) = &options.on_tool_call {
            on_tool_call(ToolCallStartedPayload {
                tool_call_id: call.tool_call().id.clone(),
                name: call.tool_name().to_string(),
                args,
                display: fields.display,
            });
        }
    }

    fn dispatch_tool_result(
        &self,
        call: &PreflightedToolCall,
        result: &ToolResult,
        options: &ToolExecutorExecuteOptions,
    ) {
        let event = ToolResultEvent::new(ToolResultEventPayload {
            agent_id: self.agent_id.clone(),
            turn_id: options.turn_id,
            tool_call_id: call.tool_call().id.clone(),
            output: serde_json::to_value(&result.output).unwrap_or(Value::Null),
            is_error: result.is_error,
            synthetic: None,
        });
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
    }

    fn dispatch_tool_progress(
        &self,
        call: &PreflightedToolCall,
        update: ToolUpdate,
        options: &ToolExecutorExecuteOptions,
    ) {
        let event = ToolProgress::new(super::tool_executor_events::ToolProgressPayload {
            agent_id: self.agent_id.clone(),
            turn_id: options.turn_id,
            tool_call_id: call.tool_call().id.clone(),
            update,
        });
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
    }

    async fn finalize_tool_result(
        &self,
        call: PreflightedToolCall,
        result: ToolResult,
        options: &ToolExecutorExecuteOptions,
        outcome: ToolExecutionOutcome,
        resolved_accesses: Option<ToolAccesses>,
    ) -> Result<ToolResult, WireAsyncError> {
        let context = ToolDidExecuteContext {
            base: ToolExecutionHookContext {
                turn_id: options.turn_id,
                signal: options.signal.clone(),
                trace: options.trace.clone(),
                tool_call: call.tool_call().clone(),
                tool_calls: vec![call.tool_call().clone()],
                tool: call.tool(),
                args: call.args().clone(),
            },
            outcome,
            accesses: resolved_accesses.clone(),
            result: result.clone().into_executable(),
            stop_turn: None,
        };
        let final_context = Arc::new(Mutex::new(None));
        let terminal_context = final_context.clone();
        let hook = self.hooks.on_did_execute_tool.run_with_terminal(
            context,
            move |context| {
                *terminal_context.lock() = Some(context);
                Box::pin(async {})
            },
        );
        if let Err(error) = AssertUnwindSafe(hook).catch_unwind().await {
            let aborted = options.signal.aborted();
            let output = if aborted {
                format!("Tool \"{}\" aborted during onDidExecuteTool hook.", call.tool_name())
            } else {
                format!(
                    "onDidExecuteTool hook failed for \"{}\": {}",
                    call.tool_name(),
                    panic_message(error)
                )
            };
            return Ok(ToolResult {
                output: output.into(),
                is_error: Some(true),
                description: result.description,
                display: result.display,
                approval_rule: result.approval_rule,
                ..Default::default()
            });
        }
        let context = final_context.lock().take().unwrap_or_else(|| ToolDidExecuteContext {
            base: ToolExecutionHookContext {
                turn_id: options.turn_id,
                signal: options.signal.clone(),
                trace: options.trace.clone(),
                tool_call: call.tool_call().clone(),
                tool_calls: vec![call.tool_call().clone()],
                tool: call.tool(),
                args: call.args().clone(),
            },
            outcome,
            accesses: resolved_accesses,
            result: result.clone().into_executable(),
            stop_turn: None,
        });
        let mut effective = normalize_tool_result(context.result);
        effective.description = result.description;
        effective.display = result.display;
        effective.approval_rule = result.approval_rule;
        effective.stop_turn = Some(
            result.stop_turn == Some(true)
                || context.stop_turn == Some(true)
                || effective.stop_turn == Some(true),
        );
        effective.stop_batch_after_this = result.stop_batch_after_this;
        self.result_truncation
            .truncate_for_model(ToolTruncationInput {
                tool_name: call.tool_name().to_string(),
                tool_call_id: call.tool_call().id.clone(),
                result: effective,
            })
            .await
    }

    fn preflight_tool_call(
        &self,
        tool_call: ToolCall,
        guard: Option<ToolCallGuard>,
        describe_unavailable: Option<UnavailableToolDescriber>,
        describe_missing: Option<MissingToolDescriber>,
    ) -> PreflightedToolCall {
        let tool_name = tool_call.name.clone();
        let (args, parse_error) = parse_tool_arguments(tool_call.arguments.as_deref());
        if let Some(error) = parse_error
            && let Some(log) = &self.log
        {
            log.debug(
                "tool args JSON parse failed",
                Some(json!({
                    "toolName": tool_name,
                    "toolCallId": tool_call.id,
                    "rawLength": tool_call.arguments.as_ref().map_or(0, |raw| raw.encode_utf16().count()),
                    "error": error.error,
                })),
            );
        }
        let Some(tool) = self.tool_registry.resolve(&tool_name) else {
            let output = describe_missing
                .and_then(|describe| describe(&tool_name))
                .unwrap_or_else(|| format!("Tool \"{tool_name}\" not found"));
            return PreflightedToolCall::Rejected {
                tool_call,
                args,
                output,
            };
        };
        let source = self
            .tool_registry
            .list()
            .into_iter()
            .find(|entry| entry.name == tool_name)
            .map(|entry| entry.source)
            .unwrap_or(ToolSource::Builtin);
        if let Some(denied) = guard.and_then(|guard| {
            guard(ToolCallGuardInput {
                name: tool_name.clone(),
                source,
            })
        }) {
            return PreflightedToolCall::Rejected {
                tool_call,
                args,
                output: denied,
            };
        }
        if let Some(unavailable) = describe_unavailable.and_then(|describe| describe(&tool_name)) {
            return PreflightedToolCall::Rejected {
                tool_call,
                args,
                output: unavailable,
            };
        }
        if let Some(error) = validate_executable_tool_args(tool.as_ref(), &args) {
            return PreflightedToolCall::Rejected {
                tool_call,
                args,
                output: format!("Invalid args for tool \"{tool_name}\": {error}"),
            };
        }
        PreflightedToolCall::Runnable {
            tool_call,
            tool,
            args,
        }
    }
}

impl IAgentToolExecutorService for AgentToolExecutorService {
    fn execute(
        self: &Arc<Self>,
        calls: Vec<ToolCall>,
        options: ToolExecutorExecuteOptions,
    ) -> BoxStream<'static, Result<ToolExecutionResult, WireAsyncError>> {
        let this = self.clone();
        Box::pin(futures_util::stream::once(async move {
            let (sender, receiver) = mpsc::unbounded();
            tokio::spawn(async move {
                this.execute_inner(calls, options, sender).await;
            });
            receiver
        }).flatten())
    }

    fn on_before_execute_tool(&self) -> Event<Arc<BeforeToolExecuteEventImpl>> {
        self.before_execute_emitter.event()
    }

    fn on_will_execute_tool(&self) -> Event<WillExecuteToolEvent> {
        self.will_execute_emitter.event()
    }

    fn hooks(&self) -> ToolExecutorHooks {
        self.hooks.clone()
    }

    fn record_dup_type(&self, tool_call_id: &str, dup_type: ToolCallDupType) {
        let mut values = self.tool_call_dup_types();
        values.insert(tool_call_id.to_string(), dup_type);
        self.set_tool_call_dup_types(values);
    }

    fn register_tool_call_guard(&self, guard: ToolCallGuard) -> DisposableRef {
        let slot = self.tool_call_guard.clone();
        *slot.lock() = Some(guard.clone());
        to_disposable(move || {
            let mut current = slot.lock();
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &guard))
            {
                *current = None;
            }
        })
    }

    fn register_unavailable_tool_describer(
        &self,
        describer: UnavailableToolDescriber,
    ) -> DisposableRef {
        let slot = self.unavailable_tool_describer.clone();
        *slot.lock() = Some(describer.clone());
        to_disposable(move || {
            let mut current = slot.lock();
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &describer))
            {
                *current = None;
            }
        })
    }

    fn register_missing_tool_describer(&self, describer: MissingToolDescriber) -> DisposableRef {
        let slot = self.missing_tool_describer.clone();
        *slot.lock() = Some(describer.clone());
        to_disposable(move || {
            let mut current = slot.lock();
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &describer))
            {
                *current = None;
            }
        })
    }
}

fn make_resolved_task(result: ToolResult, outcome: ToolExecutionOutcome) -> ToolExecutionTask {
    ToolExecutionTask {
        accesses: Vec::new(),
        execute: Arc::new(move |_| {
            let result = result.clone();
            async move {
                Ok(ToolExecutionRunResult { result, outcome })
            }
            .boxed()
        }),
    }
}

fn make_error_tool_result(call: &PreflightedToolCall, output: String) -> ToolResult {
    let _ = call;
    ToolResult::error(output)
}

fn tool_call_display_fields_from_execution(
    execution: &RunnableToolExecution,
) -> ToolCallDisplayFields {
    ToolCallDisplayFields {
        description: execution
            .description
            .as_ref()
            .filter(|description| !description.is_empty())
            .cloned(),
        display: execution.display.clone(),
    }
}

fn validate_executable_tool_args(tool: &dyn ExecutableTool, args: &Value) -> Option<String> {
    let parameters = tool.parameters();
    if let Err(error) = args_validator::validate(&Value::Object(parameters.clone()), args) {
        let valid_names = parameters
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if !valid_names.is_empty() && error.contains("must NOT have additional property '") {
            return Some(format!("{error}; valid parameters: {}", valid_names.join(", ")));
        }
        return Some(error);
    }
    None
}

fn normalize_tool_result(result: ExecutableToolResult) -> ToolResult {
    let output = match result.output {
        ExecutableToolOutput::Text(text) => {
            if text.is_empty() {
                ExecutableToolOutput::Text(TOOL_OUTPUT_EMPTY.to_string())
            } else {
                ExecutableToolOutput::Text(text)
            }
        }
        ExecutableToolOutput::Parts(parts) => {
            if parts.is_empty() {
                ExecutableToolOutput::Text(TOOL_OUTPUT_EMPTY.to_string())
            } else {
                let has_media = parts.iter().any(is_media_content_part);
                if has_media {
                    let has_text = parts.iter().any(|part| {
                        matches!(part, ContentPart::Text { text, .. } if !text.is_empty())
                    });
                    if has_text {
                        ExecutableToolOutput::Parts(parts)
                    } else {
                        let mut output = vec![ContentPart::Text {
                            text: TOOL_OUTPUT_NON_TEXT.to_string(),
                            meta: None,
                        }];
                        output.extend(parts);
                        ExecutableToolOutput::Parts(output)
                    }
                } else {
                    let text = parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    if text.is_empty() {
                        ExecutableToolOutput::Text(TOOL_OUTPUT_EMPTY.to_string())
                    } else {
                        ExecutableToolOutput::Text(text)
                    }
                }
            }
        }
    };
    ToolResult {
        output,
        is_error: (result.is_error == Some(true)).then_some(true),
        stop_turn: result.stop_turn,
        stop_turn_reason: result.stop_turn_reason,
        truncated: (result.truncated == Some(true)).then_some(true),
        note: result.note.filter(|note| !note.is_empty()),
        delivery: result.delivery,
        spill: result.spill,
        spill_exempt: result.spill_exempt,
        ..Default::default()
    }
}

fn tool_telemetry_outcome(result: &ToolResult) -> &'static str {
    if result.is_error != Some(true) {
        return "success";
    }
    let text = tool_output_text(&result.output).to_lowercase();
    if text.contains("aborted") || text.contains("cancelled") || text.contains("manually interrupted") {
        "cancelled"
    } else {
        "error"
    }
}

fn tool_output_text(output: &ExecutableToolOutput) -> String {
    match output {
        ExecutableToolOutput::Text(text) => text.clone(),
        ExecutableToolOutput::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

fn is_media_content_part(part: &ContentPart) -> bool {
    matches!(
        part,
        ContentPart::ImageUrl { .. } | ContentPart::AudioUrl { .. } | ContentPart::VideoUrl { .. }
    )
}

pub fn aborted_tool_output(tool_name: &str, reason: Option<SharedError>) -> String {
    if reason
        .as_ref()
        .is_some_and(|reason| is_user_cancellation(reason.as_ref()))
    {
        return format!(
            "The user manually interrupted \"{tool_name}\" (and anything else running at the same time). This was a deliberate user action, not a system error, timeout, or capacity limit. Do not retry automatically or guess at the cause — wait for the user's next instruction."
        );
    }
    format!("Tool \"{tool_name}\" was aborted")
}

async fn race_with_abort_grace(
    execute: BoxFuture<'static, Result<ExecutableToolResult, WireAsyncError>>,
    signal: AbortSignal,
    tool_name: &str,
) -> Result<ExecutableToolResult, WireAsyncError> {
    let controller = Arc::new(AbortController::new());
    let unlink = link_abort_signal(&signal, controller.clone());
    let grace_signal = controller.signal();
    let reason_signal = signal.clone();
    let name = tool_name.to_string();
    let grace = async move {
        let _ = abortable(std::future::pending::<()>(), grace_signal).await;
        tokio::time::sleep(Duration::from_millis(ABORT_GRACE_MS)).await;
        Ok(ExecutableToolResult::error(aborted_tool_output(
            &name,
            reason_signal.reason(),
        )))
    };
    let mut execute = execute;
    let mut grace = grace.boxed();
    let result = tokio::select! {
        biased;
        result = &mut execute => result,
        result = &mut grace => result,
    };
    drop(unlink);
    result
}

fn panic_message(error: Box<dyn std::any::Any + Send>) -> String {
    error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
        .unwrap_or_else(|| "unknown hook error".to_string())
}

impl Drop for AgentToolExecutorService {
    fn drop(&mut self) {
        self.before_execute_emitter.dispose();
        self.will_execute_emitter.dispose();
    }
}

impl IAgentToolResultTruncationService for ToolResultTruncator {
    fn truncate_for_model(
        &self,
        input: ToolTruncationInput,
    ) -> BoxFuture<'static, Result<ToolResult, WireAsyncError>> {
        let mut result = input.result;
        let is_error = result.is_error == Some(true);
        let note = result.note.clone();
        let spill = result.spill.clone();
        let spill_exempt = result.spill_exempt == Some(true);
        let has_media = matches!(&result.output, ExecutableToolOutput::Parts(parts) if parts.iter().any(is_media_content_part));
        match &mut result.output {
            ExecutableToolOutput::Text(text) => {
                let finalized = self.truncate_call(TruncationRequest {
                    tool_name: &input.tool_name,
                    tool_call_id: &input.tool_call_id,
                    content: text,
                    is_error,
                    note: note.as_deref(),
                    spill_exempt,
                    has_media,
                    spill: spill.as_ref(),
                });
                *text = finalized.content;
                result.is_error = Some(finalized.is_error);
                result.note = finalized.note;
                result.truncated = Some(finalized.truncated);
                if let Some(path) = finalized.spill_path {
                    result.spill.get_or_insert_with(Default::default).output_path = Some(path);
                }
            }
            ExecutableToolOutput::Parts(parts) => {
                let mut any_truncated = false;
                for part in parts {
                    let ContentPart::Text { text, .. } = part else {
                        continue;
                    };
                    let finalized = self.truncate_call(TruncationRequest {
                        tool_name: &input.tool_name,
                        tool_call_id: &input.tool_call_id,
                        content: text,
                        is_error,
                        note: note.as_deref(),
                        spill_exempt,
                        has_media,
                        spill: spill.as_ref(),
                    });
                    *text = finalized.content;
                    result.is_error = Some(finalized.is_error);
                    result.note = finalized.note;
                    any_truncated |= finalized.truncated;
                    if let Some(path) = finalized.spill_path {
                        result.spill.get_or_insert_with(Default::default).output_path = Some(path);
                    }
                }
                result.truncated = Some(any_truncated);
            }
        }
        async move { Ok(result) }.boxed()
    }
}
