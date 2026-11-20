use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::agent::context_memory::context_memory::I_AGENT_CONTEXT_MEMORY_SERVICE;
use crate::agent::r#loop::r#loop::I_AGENT_LOOP_SERVICE;
use crate::app::event::event_bus::I_EVENT_BUS;
use crate::session::session_usage::session_usage::I_SESSION_USAGE_SERVICE;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use futures_util::FutureExt;
use kimi_core::primitives::errors::domains::agent_lifecycle::{
    AGENT_MAX_TOKENS_EXCEEDED, AGENT_NO_FINAL_MESSAGE,
};
use kimi_core::primitives::errors::domains::agent_loop::LOOP_MAX_STEPS_EXCEEDED;
use kimi_core::primitives::errors::domains::core::INTERNAL;
use kimi_core::primitives::errors::domains::protocol::PROVIDER_RATE_LIMIT;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::IAgentScopeHandle;
use crate::_base::utils::abort::{
    AbortController, AbortSignal, link_abort_signal, user_cancellation_reason,
};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::r#loop::r#loop::{
    CompletedTurnResult, IAgentLoopService, Turn, TurnResult, is_max_steps_exceeded_error,
};
use crate::agent::r#loop::turn_events::{AssistantDelta, TurnStepRetrying, TurnStepStarted};
use crate::agent::scope_context::scope_context::agent_context_of;
use crate::app::event::event_bus::IEventBus;
use crate::_base::errors::serialize::to_kimi_error_payload;
type KimiErrorPayload = crate::_base::errors::serialize::ErrorPayload;
use crate::human::agent::origin::PromptOrigin;
use crate::human::agent::turn::{UserMeta, create_user_entry};
use crate::human::llm::message::{ContentPart, UserMessage};
use crate::agent::r#loop::r#loop::TextPart;
use crate::llm_adapter::contract::errors::{
    APIProviderRateLimitError, is_provider_rate_limit_error,
};
use crate::session::session_usage::session_usage::ISessionUsageService;
use crate::wire::record::WireAsyncError;

use super::subagent::{
    AgentRunCompletion, AgentRunHandle, AgentRunRequest, SharedAgentRunError,
};

pub const PARTIAL_OUTPUT_MAX_CHARS: usize = 32 * 1024;
pub const PARTIAL_OUTPUT_TRUNCATED_MARKER: &str = "[earlier text truncated]";
pub const SUBAGENT_MAX_TOKENS_ERROR: &str =
    "Subagent turn failed before completing its final summary: reason=max_tokens";

pub static AGENT_RUN_PROMPT_ORIGIN: LazyLock<PromptOrigin> = LazyLock::new(|| PromptOrigin {
    kind: "system_trigger".to_string(),
    extra: Map::from_iter([(
        "name".to_string(),
        Value::String("subagent".to_string()),
    )]),
    ..Default::default()
});
pub static PROMPT_OPTIMIZER_AGENT_ORIGIN: LazyLock<PromptOrigin> = LazyLock::new(|| PromptOrigin {
    kind: "system_trigger".to_string(),
    extra: Map::from_iter([
        (
            "name".to_string(),
            Value::String("subagent".to_string()),
        ),
        (
            "backgroundHelper".to_string(),
            Value::String("prompt_optimizer".to_string()),
        ),
    ]),
    ..Default::default()
});

fn origin_for_request(request: &AgentRunRequest) -> PromptOrigin {
    match request {
        AgentRunRequest::Retry { .. } => PromptOrigin {
            kind: "retry".to_string(),
            ..Default::default()
        },
        AgentRunRequest::Prompt { helper: None, .. } => AGENT_RUN_PROMPT_ORIGIN.clone(),
        AgentRunRequest::Prompt { helper: Some(_), .. } => PROMPT_OPTIMIZER_AGENT_ORIGIN.clone(),
    }
}

#[derive(Clone)]
pub struct RunAgentTurnOptions {
    pub signal: AbortSignal,
    pub on_ready: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub async fn run_agent_turn(
    target: &Arc<IAgentScopeHandle>,
    request: AgentRunRequest,
    options: RunAgentTurnOptions,
) -> Result<AgentRunHandle, WireAsyncError> {
    options.signal.throw_if_aborted()?;
    let loop_ = target.accessor().get(&I_AGENT_LOOP_SERVICE);
    let entry = match &request {
        AgentRunRequest::Prompt { prompt, .. } => create_user_entry(
            UserMessage {
                role: crate::human::llm::message::Role::User,
                content: vec![ContentPart::Text {
                    text: prompt.clone(),
                    meta: None,
                }],
            },
            UserMeta {
                origin: Some(origin_for_request(&request)),
                tracked: Some(true),
                ..Default::default()
            },
        ),
        AgentRunRequest::Retry { .. } => create_user_entry(
            UserMessage {
                role: crate::human::llm::message::Role::User,
                content: Vec::new(),
            },
            UserMeta {
                origin: Some(origin_for_request(&request)),
                ..Default::default()
            },
        ),
    };
    let id = loop_.submit(entry, None).id;
    let output = collect_run_output(&target.accessor().get(&I_EVENT_BUS));
    let turn = match loop_.prompt_handle(&id) {
        Some(handle) => handle.launched.clone().await,
        None => None,
    };
    let Some(turn) = turn else {
        output.dispose();
        return Err(Box::new(Error2::new(
            INTERNAL,
            "Agent turn could not be started",
        )));
    };
    output.bind(turn.id);
    if let Some(on_ready) = options.on_ready.clone() {
        let ready = turn.ready.clone();
        tokio::spawn(async move {
            let _ = ready.await;
            on_ready();
        });
    }
    let completion_target = target.clone();
    let completion_turn = turn.clone();
    let completion_options = options.clone();
    let completion_output = output.clone();
    let completion = async move {
        let result = await_run(completion_target, completion_turn, completion_options)
            .await
            .map_err(SharedAgentRunError::new);
        completion_output.dispose();
        result
    }
    .boxed()
    .shared();
    let partial_output = output.clone();
    Ok(AgentRunHandle {
        agent_id: target.id().to_string(),
        turn,
        completion,
        partial_output: Arc::new(move || partial_output.text()),
    })
}

async fn await_run(
    target: Arc<IAgentScopeHandle>,
    turn: Arc<Turn>,
    options: RunAgentTurnOptions,
) -> Result<AgentRunCompletion, WireAsyncError> {
    let controller = Arc::new(AbortController::new());
    let unlink = link_abort_signal(&options.signal, controller.clone());
    let cancel_turn: Arc<dyn Fn(Option<crate::_base::errors::errors::SharedError>) + Send + Sync> = {
        let turn = turn.clone();
        Arc::new(move |reason: Option<crate::_base::errors::errors::SharedError>| {
            turn.cancel(reason);
        })
    };
    let result = async {
        let result = classify_turn_result(
            await_turn(&turn, &controller, cancel_turn.clone()).await?,
        )?;
        let summary = latest_assistant_text(
            &target
                .accessor()
                .get(&I_AGENT_CONTEXT_MEMORY_SERVICE)
                .get(),
        );
        let stop_reason = result.stop_reason.clone();
        if summary.trim().is_empty() {
            let details = stop_reason.as_ref().map(|stop_reason| {
                Map::from_iter([(
                    "stopReason".to_string(),
                    Value::String(stop_reason.clone()),
                )])
            });
            return Err(Box::new(Error2::with_options(
                AGENT_NO_FINAL_MESSAGE,
                no_final_message_error(stop_reason.as_deref()),
                Error2Options {
                    details,
                    ..Default::default()
                },
            )) as WireAsyncError);
        }
        let usage = target
            .accessor()
            .get(&I_SESSION_USAGE_SERVICE)
            .status(&agent_context_of(&target))
            .total;
        Ok(AgentRunCompletion {
            summary,
            usage,
            stop_reason,
        })
    }
    .await;
    unlink();
    if controller.signal().aborted() {
        cancel_turn(controller.signal().reason());
    }
    result
}

async fn await_turn(
    turn: &Arc<Turn>,
    controller: &AbortController,
    cancel_turn: Arc<dyn Fn(Option<crate::_base::errors::errors::SharedError>) + Send + Sync>,
) -> Result<TurnResult, WireAsyncError> {
    let signal = controller.signal();
    let reason = signal.clone();
    let cancel = cancel_turn.clone();
    let cancel_on_abort = Arc::new(move || {
        cancel(reason.reason());
    });
    if signal.aborted() {
        cancel_on_abort();
    }
    signal.add_event_listener("abort", cancel_on_abort.clone(), true);
    let result = turn.result.clone().await;
    let aborted = signal.throw_if_aborted();
    signal.remove_event_listener("abort", cancel_on_abort);
    aborted?;
    Ok(result)
}

pub fn classify_turn_result(result: TurnResult) -> Result<CompletedTurnResult, WireAsyncError> {
    match result {
        TurnResult::Completed(result) => {
            if result.truncated {
                return Err(Box::new(Error2::new(
                    AGENT_MAX_TOKENS_EXCEEDED,
                    SUBAGENT_MAX_TOKENS_ERROR,
                )));
            }
            Ok(result)
        }
        TurnResult::Failed { error, .. } => {
            if is_provider_rate_limit_error(error.as_ref()) {
                return Err(crate::_base::errors::errors::shared_to_wire(error));
            }
            let payload = to_kimi_error_payload(&crate::_base::errors::serialize::UnknownError::Error(error.clone()));
            if payload.code.as_str() == PROVIDER_RATE_LIMIT {
                return Err(Box::new(provider_rate_limit_error_from_payload(&payload)));
            }
            if is_max_steps_exceeded_error(error.as_ref()) {
                return Err(Box::new(max_steps_error_from_payload(&payload)));
            }
            Err(to_run_error(Some(
                crate::_base::errors::errors::shared_to_wire(error),
            )))
        }
        TurnResult::Cancelled { reason, .. } => Err(to_run_error(Some(
            crate::_base::errors::errors::shared_to_wire(
                reason.unwrap_or_else(|| Arc::new(user_cancellation_reason())),
            ),
        ))),
    }
}

pub fn no_final_message_error(stop_reason: Option<&str>) -> String {
    let base = "Subagent turn ended without a final message";
    match stop_reason {
        Some(stop_reason) => format!("{base} (stop reason: {stop_reason})."),
        None => format!("{base}."),
    }
}

pub fn max_steps_error_from_payload(payload: &KimiErrorPayload) -> Error2 {
    let max_steps = payload
        .details
        .as_ref()
        .and_then(|details| details.get("maxSteps"))
        .and_then(Value::as_f64);
    let cap = max_steps.map_or_else(String::new, |max_steps| format!(" (maxSteps={max_steps})"));
    Error2::with_options(
        LOOP_MAX_STEPS_EXCEEDED,
        format!("Subagent hit the per-turn step cap{cap} before finishing its handoff."),
        Error2Options {
            details: max_steps.map(|max_steps| {
                Map::from_iter([("maxSteps".to_string(), Value::from(max_steps))])
            }),
            ..Default::default()
        },
    )
}

fn to_run_error(error: Option<WireAsyncError>) -> WireAsyncError {
    error.unwrap_or_else(|| "Agent turn failed".into())
}

fn provider_rate_limit_error_from_payload(payload: &KimiErrorPayload) -> APIProviderRateLimitError {
    let request_id = payload
        .details
        .as_ref()
        .and_then(|details| details.get("requestId"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let status_code = payload
        .details
        .as_ref()
        .and_then(|details| details.get("statusCode"))
        .and_then(Value::as_i64);
    APIProviderRateLimitError::new(
        payload.message.clone(),
        request_id,
        None,
        None,
        status_code,
    )
}

pub fn latest_assistant_text(messages: &[ContextMessage]) -> String {
    for message in messages.iter().rev() {
        if message.role != "assistant" {
            continue;
        }
        return content_text(&message.content);
    }
    String::new()
}

fn content_text(content: &impl Serialize) -> String {
    match serde_json::to_value(content).unwrap_or(Value::Null) {
        Value::String(text) => text,
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

struct RunOutputState {
    turns: HashMap<u64, RunText>,
    bound: bool,
    bound_turn_id: Option<u64>,
}

pub struct RunOutputCollector {
    state: Arc<Mutex<RunOutputState>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl RunOutputCollector {
    pub fn bind(&self, turn_id: Option<u64>) {
        let mut state = self.state.lock();
        state.bound = true;
        state.bound_turn_id = turn_id;
        state.turns.retain(|id, _| Some(*id) == turn_id);
    }

    pub fn text(&self) -> String {
        let state = self.state.lock();
        state
            .bound_turn_id
            .and_then(|turn_id| state.turns.get(&turn_id))
            .map(RunText::render)
            .unwrap_or_default()
    }

    fn text_of(state: &mut RunOutputState, turn_id: u64) -> Option<&mut RunText> {
        if state.bound && Some(turn_id) != state.bound_turn_id {
            return None;
        }
        Some(state.turns.entry(turn_id).or_default())
    }
}

impl IDisposable for RunOutputCollector {
    fn dispose(&self) {
        self.subscriptions.lock().clear();
    }
}

fn collect_run_output(bus: &Arc<dyn IEventBus>) -> Arc<RunOutputCollector> {
    let collector = Arc::new(RunOutputCollector {
        state: Arc::new(Mutex::new(RunOutputState {
            turns: HashMap::new(),
            bound: false,
            bound_turn_id: None,
        })),
        subscriptions: Mutex::new(Vec::new()),
    });
    let weak = Arc::downgrade(&collector);
    collector.subscriptions.lock().push(bus.subscribe_turn_step_started(
        Arc::new(move |event: TurnStepStarted| {
            if let Some(collector) = weak.upgrade()
                && let Some(text) = RunOutputCollector::text_of(&mut collector.state.lock(), event.turn_id)
            {
                text.start_step();
            }
        }),
    ));
    let weak = Arc::downgrade(&collector);
    collector.subscriptions.lock().push(bus.subscribe_assistant_delta(
        Arc::new(move |event: AssistantDelta| {
            if let Some(collector) = weak.upgrade()
                && let Some(text) = RunOutputCollector::text_of(&mut collector.state.lock(), event.turn_id)
            {
                text.append(&event.delta);
            }
        }),
    ));
    let weak = Arc::downgrade(&collector);
    collector.subscriptions.lock().push(bus.subscribe_turn_step_retrying(
        Arc::new(move |event: TurnStepRetrying| {
            if let Some(collector) = weak.upgrade()
                && let Some(text) = RunOutputCollector::text_of(&mut collector.state.lock(), event.turn_id)
            {
                text.discard_step();
            }
        }),
    ));
    collector
}

#[derive(Default)]
struct RunText {
    steps: Vec<String>,
    length: usize,
    truncated: bool,
}

impl RunText {
    fn start_step(&mut self) {
        self.steps.push(String::new());
    }

    fn append(&mut self, delta: &str) {
        if self.steps.is_empty() {
            self.steps.push(String::new());
        }
        self.steps.last_mut().unwrap().push_str(delta);
        self.length += utf16_len(delta);
        if self.length > PARTIAL_OUTPUT_MAX_CHARS * 2 {
            self.drop_head(self.length - PARTIAL_OUTPUT_MAX_CHARS);
        }
    }

    fn discard_step(&mut self) {
        let Some(last) = self.steps.last_mut() else {
            return;
        };
        self.length -= utf16_len(last);
        last.clear();
    }

    fn render(&self) -> String {
        let text = self
            .steps
            .iter()
            .map(|step| step.trim())
            .filter(|step| !step.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let start = utf16_code_point_boundary_start(
            &text,
            utf16_len(&text).saturating_sub(PARTIAL_OUTPUT_MAX_CHARS),
        );
        let kept = slice_utf16_from(&text, start);
        if kept.is_empty() || (!self.truncated && start == 0) {
            return kept;
        }
        format!("{PARTIAL_OUTPUT_TRUNCATED_MARKER}\n{kept}")
    }

    fn drop_head(&mut self, excess: usize) {
        self.truncated = true;
        let mut remaining = excess;
        while self.steps.len() > 1 && utf16_len(&self.steps[0]) <= remaining {
            let dropped = self.steps.remove(0);
            remaining -= utf16_len(&dropped);
            self.length -= utf16_len(&dropped);
        }
        if remaining == 0 || self.steps.is_empty() {
            return;
        }
        let first = &self.steps[0];
        let start = utf16_code_point_boundary_start(first, remaining.min(utf16_len(first)));
        self.steps[0] = slice_utf16_from(first, start);
        self.length -= start;
    }
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn utf16_code_point_boundary_start(text: &str, start: usize) -> usize {
    let mut units = 0;
    for character in text.chars() {
        let width = character.len_utf16();
        if start > units && start < units + width {
            return units + width;
        }
        units += width;
    }
    start.min(units)
}

fn slice_utf16_from(text: &str, start: usize) -> String {
    if start == 0 {
        return text.to_string();
    }
    let mut units = 0;
    for (index, character) in text.char_indices() {
        if units == start {
            return text[index..].to_string();
        }
        units += character.len_utf16();
    }
    String::new()
}
