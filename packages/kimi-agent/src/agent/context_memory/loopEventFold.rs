use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::{ContentPart, Role, ToolCall};
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::message::create_tool_message;
use crate::tool::tool_input_display::ToolInputDisplay;

use super::tool_result_render::should_render_wall_time;
use super::types::{ContextMessage, ContextMessageTiming};
use super::vacuous_content::is_vacuous_content_part;

const TOOL_INTERRUPTED_ON_RESUME_OUTPUT: &str = "Tool execution was interrupted before its result was recorded. Do not assume the tool completed successfully.";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantSealMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_timing: Option<ContextMessageTiming>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LoopRecordedEvent {
    #[serde(rename = "step.begin")]
    StepBegin {
        uuid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
    },
    #[serde(rename = "step.end")]
    StepEnd {
        uuid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_first_token_latency_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_stream_duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_request_build_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_server_first_token_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_server_decode_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_client_consume_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_client_blocked_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_finish_reason: Option<FinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_finish_reason: Option<String>,
    },
    #[serde(rename = "content.part")]
    ContentPart {
        step_uuid: String,
        part: crate::human::llm::message::ContentPart,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
    },
    #[serde(rename = "tool.call")]
    ToolCall {
        step_uuid: String,
        tool_call_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extras: Option<Map<String, Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ToolInputDisplay>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
    },
    #[serde(rename = "tool.result")]
    ToolResult {
        tool_call_id: String,
        result: LoopToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_uuid: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopToolResult {
    pub output: LoopToolOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LoopToolOutput {
    Text(String),
    Parts(Vec<crate::human::llm::message::ContentPart>),
}

pub trait LoopEventFoldSink: Send {
    fn current(&self) -> Vec<ContextMessage> {
        Vec::new()
    }
    fn open_assistant(&mut self, time: Option<i64>);
    fn append_open_content(&mut self, part: crate::human::llm::message::ContentPart);
    fn append_open_tool_call(&mut self, call: ToolCall, display: Option<ToolInputDisplay>);
    fn drop_open_assistant(&mut self);
    fn seal_open_assistant(&mut self, meta: Option<AssistantSealMeta>);
    fn push_tool_message(&mut self, message: ContextMessage, time: Option<i64>);
    fn push_message(&mut self, message: ContextMessage, time: Option<i64>);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenStep {
    Undefined,
    Recovered,
    Open,
}

pub struct LoopEventFold {
    sink: Box<dyn LoopEventFoldSink>,
    open_step_uuid: Option<String>,
    open_step: OpenStep,
    open_has_tool_calls: bool,
    open_vacuous: bool,
    pending: IndexMap<String, String>,
    deferred: Vec<(ContextMessage, Option<i64>)>,
}

pub fn create_loop_event_fold(sink: Box<dyn LoopEventFoldSink>) -> LoopEventFold {
    LoopEventFold::new(sink)
}

impl LoopEventFold {
    pub fn new(sink: Box<dyn LoopEventFoldSink>) -> Self {
        Self::with_state(sink, None)
    }

    fn with_state(sink: Box<dyn LoopEventFoldSink>, initial: Option<InitialFoldState>) -> Self {
        let has_initial = initial.is_some();
        let (open_has_tool_calls, open_vacuous, pending) = match initial {
            Some(initial) => (
                initial.open_has_tool_calls,
                initial.open_vacuous,
                initial.pending_tool_calls.into_iter().collect(),
            ),
            None => (false, true, IndexMap::new()),
        };
        Self {
            sink,
            open_step_uuid: None,
            open_step: if has_initial {
                OpenStep::Recovered
            } else {
                OpenStep::Undefined
            },
            open_has_tool_calls,
            open_vacuous,
            pending,
            deferred: Vec::new(),
        }
    }

    pub fn append_message(&mut self, message: ContextMessage, time: Option<i64>) {
        if !self.pending.is_empty() {
            self.deferred.push((message, time));
            return;
        }
        self.sink.push_message(message, time);
    }

    pub fn loop_event(&mut self, event: &LoopRecordedEvent, time: Option<i64>) {
        match event {
            LoopRecordedEvent::StepBegin { uuid, .. } => {
                self.settle_open(time, None);
                self.sink.open_assistant(time);
                self.open_step_uuid = Some(uuid.clone());
                self.open_step = OpenStep::Open;
                self.open_has_tool_calls = false;
                self.open_vacuous = true;
            }
            LoopRecordedEvent::StepEnd {
                finish_reason,
                usage,
                llm_first_token_latency_ms,
                llm_stream_duration_ms,
                ..
            } => {
                if matches!(finish_reason.as_deref(), Some("interrupted" | "error")) {
                    return;
                }
                let timing = ContextMessageTiming {
                    llm_first_token_latency_ms: *llm_first_token_latency_ms,
                    llm_stream_duration_ms: *llm_stream_duration_ms,
                };
                let has_timing = timing.llm_first_token_latency_ms.is_some()
                    || timing.llm_stream_duration_ms.is_some();
                let meta = if usage.is_none() && !has_timing {
                    None
                } else {
                    Some(AssistantSealMeta {
                        usage: usage.clone(),
                        llm_timing: has_timing.then_some(timing),
                    })
                };
                self.settle_open(time, meta);
                self.flush_deferred();
            }
            LoopRecordedEvent::ContentPart { step_uuid, part, .. } => {
                if !self.accepts_open_step(step_uuid) {
                    return;
                }
                self.sink.append_open_content(part.clone());
                self.open_vacuous = self.open_vacuous && is_vacuous_content_part(part);
            }
            LoopRecordedEvent::ToolCall {
                step_uuid,
                tool_call_id,
                name,
                args,
                extras,
                raw_id,
                display,
                ..
            } => {
                if !self.accepts_open_step(step_uuid) {
                    return;
                }
                let call = ToolCall {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    arguments: args.as_ref().map(|args| serde_json::to_string(args).unwrap()),
                    extras: extras.clone(),
                    raw_id: raw_id.clone(),
                    _stream_index: None,
                };
                self.sink.append_open_tool_call(call, display.clone());
                self.pending.insert(tool_call_id.clone(), name.clone());
                self.open_has_tool_calls = true;
            }
            LoopRecordedEvent::ToolResult {
                tool_call_id,
                result,
                ..
            } => {
                let Some(tool_name) = self.pending.get(tool_call_id).cloned() else {
                    return;
                };
                self.pending.shift_remove(tool_call_id);
                let mut message = match &result.output {
                    LoopToolOutput::Text(output) => {
                        ContextMessage::from(create_tool_message(tool_call_id, output.clone()))
                    }
                    LoopToolOutput::Parts(parts) => {
                        ContextMessage::from(create_tool_message(tool_call_id, parts.clone()))
                    }
                };
                message.is_error = result.is_error;
                message.note = result.note.clone();
                message.duration_ms = should_render_wall_time(&tool_name)
                    .then_some(result.duration_ms)
                    .flatten();
                self.sink.push_tool_message(message, time);
                self.flush_deferred();
            }
        }
    }

    pub fn settle(&mut self, time: Option<i64>) {
        self.settle_open(time, None);
        self.flush_deferred();
    }

    pub fn reset(&mut self) {
        self.open_step_uuid = None;
        self.open_step = OpenStep::Undefined;
        self.open_has_tool_calls = false;
        self.open_vacuous = true;
        self.pending.clear();
        self.deferred.clear();
    }

    fn accepts_open_step(&mut self, step_uuid: &str) -> bool {
        match self.open_step {
            OpenStep::Undefined => false,
            OpenStep::Recovered => {
                self.open_step_uuid = Some(step_uuid.to_string());
                self.open_step = OpenStep::Open;
                true
            }
            OpenStep::Open => self.open_step_uuid.as_deref() == Some(step_uuid),
        }
    }

    fn settle_open(&mut self, time: Option<i64>, meta: Option<AssistantSealMeta>) {
        if self.open_step == OpenStep::Undefined {
            return;
        }
        self.close_pending(time);
        if !self.open_has_tool_calls && self.open_vacuous {
            self.sink.drop_open_assistant();
        } else {
            self.sink.seal_open_assistant(meta);
        }
        self.open_step = OpenStep::Undefined;
        self.open_step_uuid = None;
    }

    fn close_pending(&mut self, time: Option<i64>) {
        if self.pending.is_empty() {
            return;
        }
        for (tool_call_id, _) in self.pending.drain(..) {
            self.sink.push_tool_message(
                interrupted_tool_message(&tool_call_id),
                time,
            );
        }
        self.flush_deferred();
    }

    fn flush_deferred(&mut self) {
        if !self.pending.is_empty() || self.deferred.is_empty() {
            return;
        }
        for (message, time) in self.deferred.drain(..) {
            self.sink.push_message(message, time);
        }
    }
}

struct InitialFoldState {
    open_has_tool_calls: bool,
    open_vacuous: bool,
    pending_tool_calls: IndexMap<String, String>,
}

pub fn fold_append_message(
    state: &[ContextMessage],
    message: &ContextMessage,
    _mutable: bool,
) -> Vec<ContextMessage> {
    let sink = create_fold_sink(state);
    let initial = recover_fold_state(state);
    let mut fold = LoopEventFold::with_state(Box::new(sink), initial);
    fold.append_message(message.clone(), None);
    fold.sink_current()
}

pub fn fold_loop_event(
    state: &[ContextMessage],
    event: &LoopRecordedEvent,
    _mutable: bool,
) -> Vec<ContextMessage> {
    let sink = create_fold_sink(state);
    let initial = recover_fold_state(state);
    let mut fold = LoopEventFold::with_state(Box::new(sink), initial);
    fold.loop_event(event, None);
    fold.sink_current()
}

impl LoopEventFold {
    fn sink_current(self) -> Vec<ContextMessage> {
        self.sink.current()
    }
}

pub fn reset_fold(state: &[ContextMessage]) -> Vec<ContextMessage> {
    state.to_vec()
}

struct VecFoldSink {
    current: Vec<ContextMessage>,
    open_index: Option<usize>,
}

impl LoopEventFoldSink for VecFoldSink {
    fn current(&self) -> Vec<ContextMessage> {
        self.current.clone()
    }

    fn open_assistant(&mut self, _time: Option<i64>) {
        let mut message = ContextMessage::new(Role::Assistant, Vec::new());
        message.partial = Some(true);
        self.current.push(message);
        self.open_index = Some(self.current.len() - 1);
    }

    fn append_open_content(&mut self, part: crate::human::llm::message::ContentPart) {
        if let Some(open) = self.open_message() {
            open.content.push(part);
        }
    }

    fn append_open_tool_call(&mut self, call: ToolCall, display: Option<ToolInputDisplay>) {
        if let Some(open) = self.open_message() {
            if let Some(display) = display {
                open.tool_call_displays
                    .get_or_insert_with(Default::default)
                    .insert(call.id.clone(), display);
            }
            open.tool_calls.push(call);
        }
    }

    fn drop_open_assistant(&mut self) {
        if let Some(index) = self.open_index.take() {
            self.current.remove(index);
        }
    }

    fn seal_open_assistant(&mut self, meta: Option<AssistantSealMeta>) {
        if let Some(open) = self.open_message() {
            let meta = meta.unwrap_or_default();
            open.usage = meta.usage;
            open.llm_timing = meta.llm_timing;
            open.partial = None;
        }
        self.open_index = None;
    }

    fn push_tool_message(&mut self, message: ContextMessage, _time: Option<i64>) {
        self.current.push(message);
    }

    fn push_message(&mut self, message: ContextMessage, _time: Option<i64>) {
        self.current.push(message);
    }
}

impl VecFoldSink {
    fn open_message(&mut self) -> Option<&mut ContextMessage> {
        self.open_index.and_then(|index| self.current.get_mut(index))
    }
}

fn create_fold_sink(initial: &[ContextMessage]) -> VecFoldSink {
    VecFoldSink {
        current: initial.to_vec(),
        open_index: find_open_assistant_index(initial),
    }
}

fn find_open_assistant_index(state: &[ContextMessage]) -> Option<usize> {
    state.iter().rposition(|message| message.partial == Some(true))
}

fn recover_fold_state(state: &[ContextMessage]) -> Option<InitialFoldState> {
    let open_index = find_open_assistant_index(state)?;
    let open = &state[open_index];
    let resolved = state[open_index + 1..]
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.clone())
        .collect::<std::collections::HashSet<_>>();
    Some(InitialFoldState {
        open_has_tool_calls: !open.tool_calls.is_empty(),
        open_vacuous: open.content.iter().all(is_vacuous_content_part),
        pending_tool_calls: open
            .tool_calls
            .iter()
            .filter(|call| !resolved.contains(&call.id))
            .map(|call| (call.id.clone(), call.name.clone()))
            .collect(),
    })
}

fn interrupted_tool_message(tool_call_id: &str) -> ContextMessage {
    let mut message = ContextMessage::from(create_tool_message(
        tool_call_id,
        TOOL_INTERRUPTED_ON_RESUME_OUTPUT,
    ));
    message.is_error = Some(true);
    message
}
