use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::BoxStream};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::_base::utils::abort::{AbortController, AbortSignal};
use crate::human::llm::message::{ContentPart, Deferred, ToolCall};
use crate::human::tool::executor::{
    OnToolUpdate, ToolExecuteInput, ToolExecutor, ToolResult, ToolUpdate,
};
use crate::human::tool::tool::ToolDefinition;
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::tool::tool_contract::{ToolDefinition as ToolInfo, ToolDisclosure};
use crate::tool_input_display::ToolInputDisplay;
use crate::turn_loop::types::ToolResultSpill;
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct MachineToolCallPayload {
    pub tool_call_id: String,
    pub name: String,
    pub args: Value,
    pub display: Option<ToolInputDisplay>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentToolOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolDelivery {
    pub kind: String,
    pub message: crate::llm_adapter::contract::message::Message,
}

#[derive(Debug, Clone)]
pub struct AgentToolResult {
    pub output: AgentToolOutput,
    pub is_error: Option<bool>,
    pub stop_turn: Option<bool>,
    pub stop_turn_reason: Option<String>,
    pub truncated: Option<bool>,
    pub note: Option<String>,
    pub delivery: Option<AgentToolDelivery>,
    pub spill: Option<ToolResultSpill>,
    pub spill_exempt: Option<bool>,
    pub description: Option<String>,
    pub display: Option<ToolInputDisplay>,
    pub approval_rule: Option<String>,
    pub stop_batch_after_this: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentToolUpdate {
    pub kind: String,
    pub text: Option<String>,
    pub percent: Option<f64>,
    pub custom_kind: Option<String>,
    pub custom_data: Option<Value>,
    pub replace: Option<bool>,
}

#[derive(Clone)]
pub struct AgentToolExecutionContext {
    pub signal: AbortSignal,
    pub steer_signal: Option<AbortSignal>,
    pub turn_id: u64,
    pub trace: Option<LLMRequestTrace>,
    pub on_tool_call: Option<Arc<dyn Fn(MachineToolCallPayload) + Send + Sync>>,
}

#[derive(Clone)]
pub struct ToolExecutionResult {
    pub tool_call_id: String,
    pub result: AgentToolResult,
    pub duration_ms: u64,
}

pub trait IAgentToolExecutorService: Send + Sync {
    fn execute(
        &self,
        calls: Vec<ToolCall>,
        context: AgentToolExecutionContext,
    ) -> BoxStream<'static, Result<ToolExecutionResult, WireAsyncError>>;
}

#[derive(Clone)]
pub struct CreateMachineToolsOptions {
    pub tool_executor: Arc<dyn IAgentToolExecutorService>,
    pub tool_infos: Arc<dyn Fn() -> Vec<ToolInfo> + Send + Sync>,
    pub turn_id: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub steer_signal: Option<Arc<dyn Fn() -> Option<AbortSignal> + Send + Sync>>,
    pub trace: Option<Arc<dyn Fn() -> Option<LLMRequestTrace> + Send + Sync>>,
    pub on_tool_call: Arc<dyn Fn(MachineToolCallPayload) + Send + Sync>,
    pub on_tool_result: Option<Arc<dyn Fn(String, AgentToolResult, u64) + Send + Sync>>,
    pub on_batch_error: Arc<dyn Fn(Value) + Send + Sync>,
}

#[derive(Clone)]
struct PendingEntry {
    input: ToolExecuteInput,
    resolve: Arc<Mutex<Option<oneshot::Sender<Result<ToolResult, WireAsyncError>>>>>,
    abort_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl PendingEntry {
    fn new(input: ToolExecuteInput) -> (Self, oneshot::Receiver<Result<ToolResult, WireAsyncError>>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                input,
                resolve: Arc::new(Mutex::new(Some(sender))),
                abort_task: Arc::new(Mutex::new(None)),
            },
            receiver,
        )
    }
}

#[derive(Default)]
struct QueueState {
    pending: IndexMap<String, PendingEntry>,
    expected_ids: Option<Vec<String>>,
    batch_in_flight: bool,
}

struct MachineToolsInner {
    options: CreateMachineToolsOptions,
    progress_handlers: Mutex<HashMap<String, Option<OnToolUpdate>>>,
    definitions: Mutex<HashMap<String, ToolDefinition>>,
    tools: Mutex<Vec<ToolDefinition>>,
    queue: Mutex<QueueState>,
}

impl MachineToolsInner {
    fn settle_entry(&self, entry: PendingEntry, result: ToolResult) {
        if let Some(task) = entry.abort_task.lock().take() {
            task.abort();
        }
        self.progress_handlers
            .lock()
            .remove(&entry.input.tool_call.id);
        if let Some(resolve) = entry.resolve.lock().take() {
            let _ = resolve.send(Ok(result));
        }
    }

    fn settle_aborted(&self, entry: PendingEntry) {
        let name = entry.input.tool_call.name.clone();
        self.settle_entry(
            entry,
            ToolResult {
                content: vec![ContentPart::Text {
                    text: format!("Tool \"{name}\" aborted before execution."),
                    meta: None,
                }],
                is_error: Some(true),
            },
        );
    }

    fn begin_batch(self: &Arc<Self>, expected_calls: Option<Vec<ToolCall>>) {
        materialize_all(self);
        let stale = {
            let mut queue = self.queue.lock();
            match expected_calls {
                None => {
                    queue.expected_ids = None;
                    queue.pending.drain(..).map(|(_, entry)| entry).collect::<Vec<_>>()
                }
                Some(calls) => {
                    let definitions = self.definitions.lock();
                    let mut seen = HashSet::new();
                    let next_ids = calls
                        .into_iter()
                        .filter(|call| definitions.contains_key(&call.name))
                        .filter_map(|call| seen.insert(call.id.clone()).then_some(call.id))
                        .collect::<Vec<_>>();
                    let next = next_ids.iter().cloned().collect::<HashSet<_>>();
                    let stranded = queue
                        .pending
                        .iter()
                        .filter(|(id, _)| !next.contains(*id))
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    let stale = stranded
                        .into_iter()
                        .filter_map(|id| queue.pending.shift_remove(&id))
                        .collect::<Vec<_>>();
                    queue.expected_ids = Some(next_ids);
                    stale
                }
            }
        };
        for entry in stale {
            self.settle_aborted(entry);
        }
        self.flush_if_ready();
    }

    fn flush_if_ready(self: &Arc<Self>) {
        let entries = {
            let mut queue = self.queue.lock();
            let Some(expected) = queue.expected_ids.clone() else {
                return;
            };
            if queue.batch_in_flight
                || !expected.iter().all(|id| queue.pending.contains_key(id))
            {
                return;
            }
            expected
                .into_iter()
                .filter_map(|id| queue.pending.shift_remove(&id))
                .collect::<Vec<_>>()
        };
        if !entries.is_empty() {
            self.start_batch(entries);
        }
    }

    fn start_batch(self: &Arc<Self>, entries: Vec<PendingEntry>) {
        self.queue.lock().batch_in_flight = true;
        let inner = self.clone();
        tokio::spawn(async move {
            inner.run_batch(entries).await;
        });
    }

    async fn run_batch(self: Arc<Self>, entries: Vec<PendingEntry>) {
        self.queue.lock().batch_in_flight = true;
        let mut in_flight = entries
            .iter()
            .cloned()
            .map(|entry| (entry.input.tool_call.id.clone(), entry))
            .collect::<IndexMap<_, _>>();
        let (signal, cancellation_tasks) = combined_signal(&entries);
        let context = AgentToolExecutionContext {
            signal,
            steer_signal: self.options.steer_signal.as_ref().and_then(|signal| signal()),
            turn_id: (self.options.turn_id)(),
            trace: self.options.trace.as_ref().and_then(|trace| trace()),
            on_tool_call: Some(self.options.on_tool_call.clone()),
        };
        let calls = entries
            .iter()
            .map(|entry| entry.input.tool_call.clone())
            .collect();
        let mut stream = self.options.tool_executor.execute(calls, context);
        let mut batch_error = None;
        while let Some(result) = stream.next().await {
            match result {
                Ok(matched) => {
                    let Some(entry) = in_flight.shift_remove(&matched.tool_call_id) else {
                        continue;
                    };
                    if let Some(on_result) = &self.options.on_tool_result {
                        on_result(
                            matched.tool_call_id.clone(),
                            matched.result.clone(),
                            matched.duration_ms,
                        );
                    }
                    let output = match matched.result.output {
                        AgentToolOutput::Text(text) => vec![ContentPart::Text {
                            text,
                            meta: None,
                        }],
                        AgentToolOutput::Parts(parts) => parts,
                    };
                    self.settle_entry(
                        entry,
                        ToolResult {
                            content: output,
                            is_error: (matched.result.is_error == Some(true)).then_some(true),
                        },
                    );
                }
                Err(error) => {
                    batch_error = Some(error.to_string());
                    break;
                }
            }
        }
        drop(stream);
        for task in cancellation_tasks {
            task.abort();
        }
        if let Some(error) = &batch_error {
            (self.options.on_batch_error)(Value::String(error.clone()));
        }
        for (_, entry) in in_flight.drain(..) {
            let name = entry.input.tool_call.name.clone();
            let text = match &batch_error {
                Some(error) => format!("Tool \"{name}\" failed: {error}"),
                None => format!("Tool \"{name}\" produced no result."),
            };
            self.settle_entry(
                entry,
                ToolResult {
                    content: vec![ContentPart::Text { text, meta: None }],
                    is_error: Some(true),
                },
            );
        }
        self.queue.lock().batch_in_flight = false;
        self.flush_if_ready();
    }

    fn abort_pending(self: &Arc<Self>, id: &str, current: PendingEntry) {
        let stale = {
            let mut queue = self.queue.lock();
            if queue.pending.shift_remove(id).is_none() {
                return;
            }
            queue.pending.drain(..).map(|(_, entry)| entry).collect::<Vec<_>>()
        };
        self.settle_aborted(current);
        for entry in stale {
            self.settle_aborted(entry);
        }
    }

    async fn execute(self: &Arc<Self>, input: ToolExecuteInput) -> Result<ToolResult, WireAsyncError> {
        materialize_all(self);
        if !self.definitions.lock().contains_key(&input.tool_call.name) {
            return Ok(ToolResult {
                content: vec![ContentPart::Text {
                    text: format!("unknown tool: {}", input.tool_call.name),
                    meta: None,
                }],
                is_error: Some(true),
            });
        }
        let direct = {
            let queue = self.queue.lock();
            queue.expected_ids.is_none() || queue.batch_in_flight
        };
        let (entry, receiver) = PendingEntry::new(input.clone());
        if direct {
            self.progress_handlers
                .lock()
                .insert(input.tool_call.id.clone(), input.on_update.clone());
            self.start_batch(vec![entry]);
        } else {
            if let Some(previous) = self.queue.lock().pending.shift_remove(&input.tool_call.id) {
                let name = previous.input.tool_call.name.clone();
                self.settle_entry(
                    previous,
                    ToolResult {
                        content: vec![ContentPart::Text {
                            text: format!(
                                "Tool \"{name}\" superseded by a duplicate tool call id."
                            ),
                            meta: None,
                        }],
                        is_error: Some(true),
                    },
                );
            }
            self.progress_handlers
                .lock()
                .insert(input.tool_call.id.clone(), input.on_update.clone());
            let weak = Arc::downgrade(self);
            let abort_entry = entry.clone();
            let abort_id = input.tool_call.id.clone();
            let signal = input.signal.clone();
            if !signal.is_cancelled() {
                let task = tokio::spawn(async move {
                    signal.cancelled().await;
                    if let Some(inner) = weak.upgrade() {
                        inner.abort_pending(&abort_id, abort_entry);
                    }
                });
                *entry.abort_task.lock() = Some(task);
            }
            self.queue
                .lock()
                .pending
                .insert(input.tool_call.id.clone(), entry);
            self.flush_if_ready();
        }
        receiver
            .await
            .unwrap_or_else(|_| Err("Tool execution ended without a result".into()))
    }

    fn handle_progress(&self, tool_call_id: &str, update: AgentToolUpdate) {
        let Some(handler) = self
            .progress_handlers
            .lock()
            .get(tool_call_id)
            .cloned()
            .flatten()
        else {
            return;
        };
        handler(ToolUpdate {
            kind: update.kind,
            text: update.text,
            percent: update.percent,
            custom_kind: update.custom_kind,
            custom_data: update.custom_data,
            replace: update.replace,
        });
    }
}

fn combined_signal(entries: &[PendingEntry]) -> (AbortSignal, Vec<JoinHandle<()>>) {
    let controller = Arc::new(AbortController::new());
    let mut tasks = Vec::new();
    for entry in entries {
        let signal = entry.input.signal.clone();
        if signal.is_cancelled() {
            controller.abort(None);
        }
        let controller = controller.clone();
        tasks.push(tokio::spawn(async move {
            signal.cancelled().await;
            controller.abort(None);
        }));
    }
    (controller.signal(), tasks)
}

struct MachineExecutor {
    inner: Arc<MachineToolsInner>,
}

#[async_trait]
impl ToolExecutor for MachineExecutor {
    async fn execute(&self, input: ToolExecuteInput) -> Result<ToolResult, WireAsyncError> {
        self.inner.execute(input).await
    }
}

pub struct MachineTools {
    pub tools: Vec<ToolDefinition>,
    pub executor: Arc<dyn ToolExecutor>,
    inner: Arc<MachineToolsInner>,
}

impl MachineTools {
    pub fn sync(&self) {
        materialize_all(&self.inner);
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.inner.tools.lock().clone()
    }

    pub fn begin_batch(&self, expected_calls: Option<Vec<ToolCall>>) {
        self.inner.begin_batch(expected_calls);
    }

    pub fn handle_progress(&self, tool_call_id: &str, update: ToolUpdate) {
        self.inner.handle_progress(
            tool_call_id,
            AgentToolUpdate {
                kind: update.kind,
                text: update.text,
                percent: update.percent,
                custom_kind: update.custom_kind,
                custom_data: update.custom_data,
                replace: update.replace,
            },
        );
    }

    pub fn handle_agent_progress(&self, tool_call_id: &str, update: AgentToolUpdate) {
        self.inner.handle_progress(tool_call_id, update);
    }
}

fn materialize_definition(inner: &Weak<MachineToolsInner>, info: ToolInfo) -> ToolDefinition {
    let weak = Weak::clone(inner);
    ToolDefinition {
        name: info.name.clone(),
        description: info.description,
        parameters: info.input_schema.as_object().cloned().unwrap_or_else(|| {
            json!({ "type": "object", "properties": {} })
                .as_object()
                .cloned()
                .unwrap_or_else(Map::new)
        }),
        deferred: (info.disclosure == ToolDisclosure::Deferred).then_some(Deferred),
        execute: Arc::new(move |input| {
            let weak = weak.clone();
            Box::pin(async move {
                let inner = weak.upgrade().ok_or_else(|| -> WireAsyncError {
                    "Machine tools are no longer available".into()
                })?;
                inner.execute(input).await
            })
        }),
    }
}

fn materialize_all(inner: &Arc<MachineToolsInner>) {
    let mut definitions = inner.definitions.lock();
    let mut tools = inner.tools.lock();
    for info in (inner.options.tool_infos)() {
        if definitions.contains_key(&info.name) {
            continue;
        }
        let definition = materialize_definition(&Arc::downgrade(inner), info.clone());
        definitions.insert(info.name.clone(), definition.clone());
        tools.push(definition);
    }
}

pub fn create_machine_tools(options: CreateMachineToolsOptions) -> MachineTools {
    let inner = Arc::new(MachineToolsInner {
        options,
        progress_handlers: Mutex::new(HashMap::new()),
        definitions: Mutex::new(HashMap::new()),
        tools: Mutex::new(Vec::new()),
        queue: Mutex::new(QueueState::default()),
    });
    MachineTools {
        tools: Vec::new(),
        executor: Arc::new(MachineExecutor { inner: inner.clone() }),
        inner,
    }
}
