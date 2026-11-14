use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::human::llm::message::ToolCall;
use crate::human::tool::executor::{
    TaskWaitInput, TaskWaitOutcome, ToolExecuteInput, ToolExecutor, ToolResult, ToolUpdate,
    WaitForTasks,
};

#[derive(Clone)]
pub struct ToolInput {
    pub tool_call: ToolCall,
    pub signal: CancellationToken,
    pub wait_for_tasks: Option<WaitForTasks>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum ToolEvent {
    #[serde(rename = "tool.update")]
    Update {
        tool_call_id: String,
        update: ToolUpdate,
    },
    #[serde(rename = "tool.detached")]
    Detached { tool_call_id: String, text: String },
    #[serde(rename = "tool.done")]
    Done {
        tool_call_id: String,
        result: ToolResult,
    },
    #[serde(rename = "tool.failed")]
    Failed {
        tool_call_id: String,
        error: String,
    },
    #[serde(rename = "tool.aborted")]
    Aborted { tool_call_id: String },
    #[serde(rename = "tool.abort")]
    Abort,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ToolOutput {
    #[serde(rename = "succeeded")]
    Succeeded { result: ToolResult },
    #[serde(rename = "failed")]
    Failed { error: String },
    #[serde(rename = "aborted")]
    Aborted,
}

#[derive(Clone, Debug)]
pub struct ToolBeforeInput {
    pub tool_call: ToolCall,
}

#[derive(Clone, Debug)]
pub enum ToolBeforeDecision {
    Proceed { tool_call: Option<ToolCall> },
    Denied { result: ToolResult },
}

#[derive(Clone, Debug)]
pub struct ToolAfterInput {
    pub tool_call: ToolCall,
    pub result: ToolResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    Aborted,
}

#[derive(Clone)]
pub struct ToolMachineContext {
    pub input: ToolInput,
    pub tool_call: ToolCall,
    pub outcome: Option<ToolOutcome>,
    pub result: Option<ToolResult>,
    pub error: Option<String>,
}

pub struct ToolMachineLogic {
    executor: Arc<dyn ToolExecutor>,
}

impl ToolMachineLogic {
    pub fn start(
        &self,
        input: ToolInput,
        parent: Arc<dyn Fn(ToolEvent) + Send + Sync>,
    ) -> Arc<ToolActor> {
        let tool_call = input.tool_call.clone();
        let actor = Arc::new(ToolActor {
            executor: self.executor.clone(),
            context: Mutex::new(ToolMachineContext {
                input: input.clone(),
                tool_call,
                outcome: None,
                result: None,
                error: None,
            }),
            state: Mutex::new("preparing".to_string()),
            parent,
            listeners: Mutex::new(Vec::new()),
            detached: AtomicBool::new(false),
        });
        let starting = actor.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            starting.start();
        });
        actor
    }
}

pub struct ToolActor {
    executor: Arc<dyn ToolExecutor>,
    context: Mutex<ToolMachineContext>,
    state: Mutex<String>,
    parent: Arc<dyn Fn(ToolEvent) + Send + Sync>,
    listeners: Mutex<Vec<Arc<dyn Fn(ToolEvent) + Send + Sync>>>,
    detached: AtomicBool,
}

impl ToolActor {
    fn start(self: &Arc<Self>) {
        if self.state.lock().as_str() != "preparing" {
            return;
        }
        let decision = ToolBeforeDecision::Proceed {
            tool_call: Some(self.context.lock().tool_call.clone()),
        };
        match decision {
            ToolBeforeDecision::Denied { result } => self.finish(result),
            ToolBeforeDecision::Proceed { tool_call } => {
                {
                    let mut context = self.context.lock();
                    if let Some(tool_call) = tool_call {
                        context.tool_call = tool_call;
                    }
                }
                *self.state.lock() = "executing".to_string();
                self.execute();
            }
        }
    }

    fn execute(self: &Arc<Self>) {
        let this = self.clone();
        let (tool_call, signal, wait_for_tasks) = {
            let context = self.context.lock();
            (
                context.tool_call.clone(),
                context.input.signal.clone(),
                context.input.wait_for_tasks.clone(),
            )
        };
        let tool_call_id = tool_call.id.clone();
        let update_actor = self.clone();
        let detach_actor = self.clone();
        let input = ToolExecuteInput {
            tool_call,
            signal: signal.clone(),
            on_update: Some(Arc::new(move |update| {
                update_actor.send(ToolEvent::Update {
                    tool_call_id: tool_call_id.clone(),
                    update,
                });
            })),
            detach: Some(Arc::new(move |ack| {
                if detach_actor.detached.swap(true, Ordering::AcqRel) {
                    return;
                }
                detach_actor.send(ToolEvent::Detached {
                    tool_call_id: detach_actor.context.lock().tool_call.id.clone(),
                    text: ack.text,
                });
            })),
            wait_for_tasks,
        };
        tokio::spawn(async move {
            match this.executor.execute(input).await {
                Ok(result) => this.send(ToolEvent::Done {
                    tool_call_id: this.context.lock().tool_call.id.clone(),
                    result,
                }),
                Err(error) if signal.is_cancelled() => this.send(ToolEvent::Aborted {
                    tool_call_id: this.context.lock().tool_call.id.clone(),
                }),
                Err(error) => this.send(ToolEvent::Failed {
                    tool_call_id: this.context.lock().tool_call.id.clone(),
                    error: error.to_string(),
                }),
            }
        });
    }

    pub fn send(self: &Arc<Self>, event: ToolEvent) {
        let state = self.state.lock().clone();
        match (state.as_str(), event) {
            ("preparing", ToolEvent::Abort) => {
                self.context.lock().outcome = Some(ToolOutcome::Aborted);
                *self.state.lock() = "aborted".to_string();
                let id = self.context.lock().tool_call.id.clone();
                self.emit(ToolEvent::Aborted { tool_call_id: id });
            }
            ("executing", ToolEvent::Abort) => {}
            ("executing", event @ ToolEvent::Update { .. })
            | ("executing", event @ ToolEvent::Detached { .. }) => self.emit(event),
            ("executing", ToolEvent::Done { result, .. }) => self.finish(result),
            ("executing", ToolEvent::Failed { error, .. }) => self.fail(error),
            ("executing", ToolEvent::Aborted { .. }) => self.abort(),
            _ => {}
        }
    }

    fn finish(self: &Arc<Self>, result: ToolResult) {
        *self.state.lock() = "finishing".to_string();
        self.context.lock().result = Some(result.clone());
        let after = ToolAfterInput {
            tool_call: self.context.lock().tool_call.clone(),
            result,
        };
        let finishing = self.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            finishing.succeed(after.result);
        });
    }

    fn succeed(&self, result: ToolResult) {
        {
            let mut context = self.context.lock();
            context.outcome = Some(ToolOutcome::Succeeded);
            context.result = Some(result.clone());
        }
        *self.state.lock() = "succeeded".to_string();
        let id = self.context.lock().tool_call.id.clone();
        self.emit(ToolEvent::Done {
            tool_call_id: id,
            result,
        });
    }

    fn fail(&self, error: String) {
        {
            let mut context = self.context.lock();
            context.outcome = Some(ToolOutcome::Failed);
            context.error = Some(error.clone());
        }
        *self.state.lock() = "failed".to_string();
        let id = self.context.lock().tool_call.id.clone();
        self.emit(ToolEvent::Failed {
            tool_call_id: id,
            error,
        });
    }

    fn abort(&self) {
        self.context.lock().outcome = Some(ToolOutcome::Aborted);
        *self.state.lock() = "aborted".to_string();
        let id = self.context.lock().tool_call.id.clone();
        self.emit(ToolEvent::Aborted { tool_call_id: id });
    }

    fn emit(&self, event: ToolEvent) {
        for listener in self.listeners.lock().iter() {
            listener(event.clone());
        }
        (self.parent)(event);
    }

    pub fn subscribe(&self, listener: Arc<dyn Fn(ToolEvent) + Send + Sync>) {
        self.listeners.lock().push(listener);
    }

    pub fn snapshot(&self) -> (ToolMachineContext, String) {
        (self.context.lock().clone(), self.state.lock().clone())
    }

    pub fn output(&self) -> Option<ToolOutput> {
        let context = self.context.lock();
        match context.outcome {
            Some(ToolOutcome::Succeeded) => context.result.clone().map(|result| {
                ToolOutput::Succeeded { result }
            }),
            Some(ToolOutcome::Failed) => Some(ToolOutput::Failed {
                error: context.error.clone().unwrap_or_default(),
            }),
            Some(ToolOutcome::Aborted) => Some(ToolOutput::Aborted),
            None => None,
        }
    }
}

pub fn create_tool_machine(executor: Arc<dyn ToolExecutor>) -> ToolMachineLogic {
    ToolMachineLogic { executor }
}

pub fn wait_for_tasks_input(
    wait: &WaitForTasks,
    task_id: Option<String>,
    timeout_ms: u64,
) -> futures_util::future::BoxFuture<'static, Result<TaskWaitOutcome, crate::wire::record::WireAsyncError>> {
    wait(TaskWaitInput {
        task_id,
        timeout_ms,
    })
}
