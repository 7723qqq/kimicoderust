use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use kimi_core::primitives::errors::Error2;
use parking_lot::Mutex;

use crate::_base::errors::errors::SharedError;
use crate::_base::utils::abort::{
    AbortController, AbortSignal, is_abort_error, link_abort_signal,
};
use crate::_base::utils::error_message::error_message;
use crate::agent::task::types::{
    AgentTask, AgentTaskInfo, AgentTaskInfoBase, AgentTaskKind, AgentTaskSettlement,
    AgentTaskSettlementStatus, AgentTaskSink, SubagentTaskInfo,
};
use crate::agent::tool_dedupe::tool_dedupe::REPEAT_BREAKER_STOP_REASON;
use crate::human::llm::usage::TokenUsage;
use crate::session::agent_lifecycle::errors::AGENT_NO_FINAL_MESSAGE;
use crate::session::subagent::config_section::SubagentModelSource;
use crate::wire::record::WireAsyncError;

const REPEAT_BREAKER_SETTLE_REASON: &str = "stopped by the repeat breaker after issuing the same tool call repeatedly; its output is a handoff, not a finished result";

pub const PARTIAL_OUTPUT_NOTICE: &str = "[partial output] The subagent did not finish this run. Below is the text it wrote before it ended; it is incomplete and unverified.";

pub const NO_PARTIAL_OUTPUT: &str =
    "[partial output] none — the subagent wrote no text in this run before it ended.";

#[derive(Debug, Clone)]
pub struct SubagentCompletion {
    pub result: String,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SharedSubagentError(SharedError);

impl SharedSubagentError {
    pub fn new(error: WireAsyncError) -> Self {
        Self(Arc::from(error))
    }

    pub fn from_shared(error: SharedError) -> Self {
        Self(error)
    }

    pub fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.0.as_ref()
    }

    pub fn shared(&self) -> SharedError {
        self.0.clone()
    }

    pub fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedSubagentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl Error for SharedSubagentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

pub type SubagentCompletionFuture =
    Shared<BoxFuture<'static, Result<SubagentCompletion, SharedSubagentError>>>;

#[derive(Clone)]
pub struct SubagentHandle {
    pub agent_id: String,
    pub profile_name: String,
    pub parent_tool_call_id: Option<String>,
    pub model: Option<String>,
    pub model_source: Option<SubagentModelSource>,
    pub thinking_effort: Option<String>,
    pub completion: SubagentCompletionFuture,
    pub partial_output: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

pub type SubagentOutput = Arc<dyn Fn(String) + Send + Sync>;
pub type SubagentExecutor = Arc<
    dyn Fn(AbortSignal, SubagentOutput) -> SubagentCompletionFuture + Send + Sync,
>;

pub fn create_subagent_executor(
    handle: SubagentHandle,
    abort_controller: Arc<AbortController>,
) -> SubagentExecutor {
    Arc::new(move |signal, output| {
        let handle = handle.clone();
        let abort_controller = abort_controller.clone();
        let output = output.clone();
        async move {
            let unlink = link_abort_signal(&signal, abort_controller);
            let result = async {
                let outcome = handle.completion.clone().await?;
                output(outcome.result.clone());
                Ok(outcome)
            }
            .await;
            unlink();
            result
        }
        .boxed()
        .shared()
    })
}

fn stop_code_of(error: &(dyn Error + 'static)) -> Option<String> {
    let error = error.downcast_ref::<Error2>()?;
    let code = error.code().as_str();
    if code == AGENT_NO_FINAL_MESSAGE {
        if let Some(stop_reason) = error
            .details()
            .and_then(|details| details.get("stopReason"))
            .and_then(serde_json::Value::as_str)
        {
            return Some(stop_reason.to_string());
        }
    }
    Some(code.to_string())
}

fn completed_settle_reason(stop_reason: Option<&str>) -> Option<String> {
    (stop_reason == Some(REPEAT_BREAKER_STOP_REASON))
        .then(|| REPEAT_BREAKER_SETTLE_REASON.to_string())
}

fn same_reason(error: &SharedSubagentError, signal: &AbortSignal) -> bool {
    signal
        .reason()
        .is_some_and(|reason| Arc::ptr_eq(&error.0, &reason))
}

fn is_cancellation(error: &SharedSubagentError, signals: &[AbortSignal]) -> bool {
    signals.iter().any(|signal| {
        signal.aborted()
            && (is_abort_error(error.as_error()) || same_reason(error, signal))
    })
}

#[derive(Default)]
struct ParentAbortState {
    signal: Option<AbortSignal>,
    unlink: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for ParentAbortState {
    fn drop(&mut self) {
        if let Some(unlink) = self.unlink.take() {
            unlink();
        }
    }
}

pub struct SubagentTask {
    handle: SubagentHandle,
    description: String,
    abort_controller: Arc<AbortController>,
    parent: Mutex<ParentAbortState>,
    agent_id: String,
    subagent_type: String,
    parent_tool_call_id: Option<String>,
    model: Option<String>,
    thinking_effort: Option<String>,
    stop_code: Mutex<Option<String>>,
    sink: Mutex<Option<AgentTaskSink>>,
    output_written: AtomicBool,
}

impl SubagentTask {
    pub fn new(
        handle: SubagentHandle,
        description: impl Into<String>,
        abort_controller: Arc<AbortController>,
    ) -> Self {
        Self {
            agent_id: handle.agent_id.clone(),
            subagent_type: handle.profile_name.clone(),
            parent_tool_call_id: handle.parent_tool_call_id.clone(),
            model: handle.model.clone(),
            thinking_effort: handle.thinking_effort.clone(),
            handle,
            description: description.into(),
            abort_controller,
            parent: Mutex::new(ParentAbortState::default()),
            stop_code: Mutex::new(None),
            sink: Mutex::new(None),
            output_written: AtomicBool::new(false),
        }
    }

    pub fn with_parent_signal(mut self, signal: AbortSignal) -> Self {
        let parent = self.parent.get_mut();
        parent.signal = Some(signal.clone());
        parent.unlink = Some(link_abort_signal(&signal, self.abort_controller.clone()));
        self
    }

    fn write_partial_output(&self) {
        let text = self
            .handle
            .partial_output
            .as_ref()
            .map(|partial| partial().trim().to_string())
            .unwrap_or_default();
        self.write_output(if text.is_empty() {
            String::new()
        } else {
            format!("{PARTIAL_OUTPUT_NOTICE}\n\n{text}")
        });
    }

    fn write_output(&self, text: String) {
        if self.output_written.swap(true, Ordering::SeqCst) {
            return;
        }
        if !text.is_empty()
            && let Some(sink) = self.sink.lock().as_ref()
        {
            sink.append_output(text);
        }
    }
}

#[async_trait]
impl AgentTask for SubagentTask {
    fn id_prefix(&self) -> &str {
        "agent"
    }

    fn kind(&self) -> AgentTaskKind {
        AgentTaskKind::Agent
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn start(&self, sink: AgentTaskSink) -> Result<(), WireAsyncError> {
        *self.sink.lock() = Some(sink.clone());
        let unlink_sink = link_abort_signal(&sink.signal, self.abort_controller.clone());
        match self.handle.completion.clone().await {
            Ok(outcome) => {
                *self.stop_code.lock() = outcome.stop_reason.clone();
                self.write_output(outcome.result.clone());
                sink.settle(AgentTaskSettlement {
                    status: AgentTaskSettlementStatus::Completed,
                    stop_reason: completed_settle_reason(outcome.stop_reason.as_deref()),
                })
                .await;
            }
            Err(error) => {
                let parent_signal = self.parent.lock().signal.clone();
                let mut cancellation_signals = vec![sink.signal.clone()];
                cancellation_signals.extend(parent_signal.clone());
                if is_cancellation(&error, &cancellation_signals) {
                    self.write_partial_output();
                    sink.settle(AgentTaskSettlement {
                        status: AgentTaskSettlementStatus::Killed,
                        stop_reason: None,
                    })
                    .await;
                } else {
                    *self.stop_code.lock() = stop_code_of(error.as_error());
                    self.write_partial_output();
                    sink.settle(AgentTaskSettlement {
                        status: AgentTaskSettlementStatus::Failed,
                        stop_reason: Some(error_message(&error)),
                    })
                    .await;
                }
            }
        }
        if let Some(unlink) = self.parent.lock().unlink.take() {
            unlink();
        }
        unlink_sink();
        Ok(())
    }

    fn on_detach(&self) {
        let unlink = {
            let mut parent = self.parent.lock();
            parent.signal = None;
            parent.unlink.take()
        };
        if let Some(unlink) = unlink {
            unlink();
        }
    }

    async fn force_stop(&self) -> Result<(), WireAsyncError> {
        self.write_partial_output();
        Ok(())
    }

    fn to_info(&self, base: AgentTaskInfoBase) -> AgentTaskInfo {
        AgentTaskInfo::Agent(SubagentTaskInfo {
            base,
            agent_id: Some(self.agent_id.clone()),
            subagent_type: Some(self.subagent_type.clone()),
            parent_tool_call_id: self.parent_tool_call_id.clone(),
            model: self.model.clone(),
            thinking_effort: self.thinking_effort.clone(),
            stop_code: self.stop_code.lock().clone(),
            swarm_index: None,
            state_reason: None,
            result_summary: None,
        })
    }
}
