use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::errors::errors::{SharedError, unwrap_error_cause};
use crate::_base::utils::abort::{
    AbortController, AbortSignal, abort_error, is_abort_error, link_abort_signal,
};
use crate::agent::llm_requester::llm_requester::{
    AgentLLMRequestFinish, AgentLLMRequestOverrides, AgentLLMRequestSource,
    IAgentLLMRequesterService,
};
use crate::human::llm::errors::{LlmErrorKind, LlmRemoteErrorMessage, to_llm_error_message};
use crate::human::llm::finish_reason::FinishInfo;
use crate::human::llm::requester::requester::{
    LlmRequestConfig, LlmRequestContent, LlmRequestControl, LlmRequestEvent, LlmRequester,
};
use crate::human::llm::usage::TokenUsagePatch;
use crate::llm_adapter::contract::errors::llm_message_from_error;
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub enum MachineRequesterGateDecision {
    Proceed {
        signal: Option<AbortSignal>,
        step: Option<u64>,
    },
    Fail,
}

pub type MachineRequesterGate = Arc<
    dyn Fn(AbortSignal) -> BoxFuture<'static, Result<MachineRequesterGateDecision, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct CreateMachineRequesterOptions {
    pub source: Option<Arc<dyn Fn() -> Option<AgentLLMRequestSource> + Send + Sync>>,
    pub gate: Option<MachineRequesterGate>,
    pub on_trace: Option<Arc<dyn Fn(LLMRequestTrace) + Send + Sync>>,
}

pub type MachineRequesterOptions = CreateMachineRequesterOptions;

#[derive(Default)]
struct RequesterState {
    last_finish: Option<AgentLLMRequestFinish>,
    last_error: Option<Value>,
}

pub struct MachineRequester {
    pub requester: Arc<dyn LlmRequester>,
    state: Arc<Mutex<RequesterState>>,
}

impl MachineRequester {
    pub fn last_finish(&self) -> Option<AgentLLMRequestFinish> {
        self.state.lock().last_finish.clone()
    }

    pub fn last_error(&self) -> Option<Value> {
        self.state.lock().last_error.clone()
    }
}

fn gate_failure() -> LlmRemoteErrorMessage {
    LlmRemoteErrorMessage::new(
        LlmErrorKind::Abort,
        "The agent loop gate stopped the step.".to_string(),
        false,
    )
}

fn to_remote_error_message(error: WireAsyncError, signal: &AbortSignal) -> LlmRemoteErrorMessage {
    let original: SharedError = error.into();
    let raw = unwrap_error_cause(original.clone());
    if let Some(known) = llm_message_from_error(raw.as_ref()) {
        return known;
    }
    if !Arc::ptr_eq(&raw, &original)
        && let Some(known) = llm_message_from_error(original.as_ref())
    {
        return known;
    }
    if signal.aborted() {
        return LlmRemoteErrorMessage::new(
            LlmErrorKind::Abort,
            "The operation was aborted.".to_string(),
            false,
        );
    }
    if is_abort_error(raw.as_ref()) {
        return LlmRemoteErrorMessage::new(LlmErrorKind::Abort, raw.to_string(), false);
    }
    let fallback: WireAsyncError = raw.to_string().into();
    to_llm_error_message(&fallback)
}

struct MachineRequesterImpl {
    service: Arc<dyn IAgentLLMRequesterService>,
    options: CreateMachineRequesterOptions,
    state: Arc<Mutex<RequesterState>>,
}

impl MachineRequesterImpl {
    fn emit_failure(control: &LlmRequestControl) {
        (control.on_event)(LlmRequestEvent::FailedRemote {
            error: gate_failure(),
            raw_error: None,
        });
    }
}

#[async_trait]
impl LlmRequester for MachineRequesterImpl {
    async fn generate(
        &self,
        _config: LlmRequestConfig,
        _content: LlmRequestContent,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        let controller = Arc::new(AbortController::new());
        if control.signal.is_cancelled() {
            controller.abort(Some(Arc::new(abort_error())));
        }
        let cancellation = control.signal.clone();
        let cancellation_controller = controller.clone();
        let cancellation_task = tokio::spawn(async move {
            cancellation.cancelled().await;
            cancellation_controller.abort(Some(Arc::new(abort_error())));
        });
        let gate_signal = controller.signal();
        let decision = match &self.options.gate {
            Some(gate) => match gate(gate_signal.clone()).await {
                Ok(decision) => decision,
                Err(error) => {
                    cancellation_task.abort();
                    return Err(error);
                }
            },
            None => MachineRequesterGateDecision::Proceed {
                signal: None,
                step: None,
            },
        };
        let MachineRequesterGateDecision::Proceed { signal, step } = decision else {
            cancellation_task.abort();
            Self::emit_failure(&control);
            return Ok(());
        };
        if control.signal.is_cancelled() || gate_signal.aborted() {
            cancellation_task.abort();
            Self::emit_failure(&control);
            return Ok(());
        }
        let unlink = signal
            .as_ref()
            .map(|signal| link_abort_signal(signal, controller.clone()));
        let signal = controller.signal();
        self.state.lock().last_error = None;
        (control.on_event)(LlmRequestEvent::Sent);
        let source = match (self.options.source.as_ref().and_then(|source| source()), step) {
            (
                Some(AgentLLMRequestSource::Turn {
                    turn_id,
                    step: original_step,
                    log_fields,
                }),
                decision_step,
            ) => Some(AgentLLMRequestSource::Turn {
                turn_id,
                step: decision_step.or(original_step),
                log_fields,
            }),
            (source, _) => source,
        };
        let retry_event = control.on_event.clone();
        let part_event = control.on_event.clone();
        let task = self.service.start(
            Some(AgentLLMRequestOverrides {
                source,
                on_attempt_retry: Some(Arc::new(move || {
                    retry_event(LlmRequestEvent::RequestRetrying);
                })),
                ..Default::default()
            }),
            Some(Arc::new(move |part| {
                part_event(LlmRequestEvent::StreamingPart { part });
                Box::pin(async { Ok(()) })
            })),
            Some(signal.clone()),
        );
        if let Some(on_trace) = &self.options.on_trace {
            on_trace(task.trace);
        }
        let result = task.result.await;
        cancellation_task.abort();
        if let Some(unlink) = unlink {
            unlink();
        }
        match result {
            Ok(finish) => {
                self.state.lock().last_finish = Some(finish.clone());
                (control.on_event)(LlmRequestEvent::StreamingUsage {
                    usage: TokenUsagePatch {
                        input_other: Some(finish.usage.input_other),
                        output: Some(finish.usage.output),
                        input_cache_read: Some(finish.usage.input_cache_read),
                        input_cache_creation: Some(finish.usage.input_cache_creation),
                        raw: finish.usage.raw.clone(),
                    },
                });
                (control.on_event)(LlmRequestEvent::StreamingFinish {
                    finish: FinishInfo {
                        finish_reason: finish.provider_finish_reason,
                        raw_finish_reason: finish.raw_finish_reason.clone(),
                    },
                });
                if let Some(message_id) = finish.provider_message_id.clone() {
                    (control.on_event)(LlmRequestEvent::StreamingMessageId { message_id });
                }
                (control.on_event)(LlmRequestEvent::Done);
            }
            Err(error) => {
                self.state.lock().last_finish = None;
                self.state.lock().last_error = Some(Value::String(error.to_string()));
                let error = to_remote_error_message(error, &signal);
                (control.on_event)(LlmRequestEvent::FailedRemote {
                    error,
                    raw_error: None,
                });
            }
        }
        Ok(())
    }
}

pub fn create_machine_requester(
    service: Arc<dyn IAgentLLMRequesterService>,
    options: CreateMachineRequesterOptions,
) -> MachineRequester {
    let state = Arc::new(Mutex::new(RequesterState::default()));
    let requester = Arc::new(MachineRequesterImpl {
        service,
        options,
        state: state.clone(),
    });
    MachineRequester { requester, state }
}
