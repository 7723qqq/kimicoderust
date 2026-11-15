use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::human::llm::message::ToolCall;
use crate::human::tool::executor::{
    OnToolDetach, OnToolUpdate, TaskWaitInput, TaskWaitOutcome, ToolExecuteInput, ToolExecutor,
    ToolResult, ToolUpdate, WaitForTasks,
};
use crate::human::xstate2::{
    Action, ActorLogic, Context, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent,
    assign, custom, emit, from_callback, from_promise,
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

#[derive(Clone)]
struct ToolExecuteActorInput {
    tool_call: ToolCall,
    signal: CancellationToken,
    wait_for_tasks: Option<WaitForTasks>,
}

pub struct ToolMachineLogic {
    machine: Machine,
}

impl ToolMachineLogic {
    pub fn machine(&self) -> &Machine {
        &self.machine
    }
}

fn context_of(context: &Context) -> &ToolMachineContext {
    context
        .downcast_ref::<ToolMachineContext>()
        .expect("tool machine context")
}

fn tool_event_name(event: &ToolEvent) -> &'static str {
    match event {
        ToolEvent::Update { .. } => "tool.update",
        ToolEvent::Detached { .. } => "tool.detached",
        ToolEvent::Done { .. } => "tool.done",
        ToolEvent::Failed { .. } => "tool.failed",
        ToolEvent::Aborted { .. } => "tool.aborted",
        ToolEvent::Abort => "tool.abort",
    }
}

pub fn tool_event_xevent(event: &ToolEvent) -> XEvent {
    XEvent {
        type_: tool_event_name(event).to_string(),
        payload: serde_json::to_value(event).expect("tool event serializes"),
        restore: None,
        data: Some(Arc::new(event.clone())),
    }
}

fn tool_event_of(event: &XEvent) -> ToolEvent {
    serde_json::from_value(event.payload.clone()).expect("tool event payload")
}

fn error_text(event: &XEvent) -> String {
    match &event.payload {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn failed_event(context: &ToolMachineContext, error: String) -> ToolEvent {
    ToolEvent::Failed {
        tool_call_id: context.tool_call.id.clone(),
        error,
    }
}

fn aborted_event(context: &ToolMachineContext) -> ToolEvent {
    ToolEvent::Aborted {
        tool_call_id: context.tool_call.id.clone(),
    }
}

fn done_event(context: &ToolMachineContext, result: ToolResult) -> ToolEvent {
    ToolEvent::Done {
        tool_call_id: context.tool_call.id.clone(),
        result,
    }
}

fn forward_to_parent() -> Action {
    custom(|_, args| args.self_.forward_to_parent(args.event.clone()))
}

fn prepare_actor() -> ActorLogic {
    from_promise(|input| {
        let before = input
            .input
            .downcast::<ToolBeforeInput>()
            .expect("prepare input");
        async move {
            Ok(Arc::new(ToolBeforeDecision::Proceed {
                tool_call: Some(before.tool_call.clone()),
            }) as Context)
        }
        .boxed()
    })
}

fn finish_actor() -> ActorLogic {
    from_promise(|input| {
        let after = input
            .input
            .downcast::<ToolAfterInput>()
            .expect("finish input");
        async move { Ok(Arc::new(after.result.clone()) as Context) }.boxed()
    })
}

fn execute_actor(executor: Arc<dyn ToolExecutor>) -> ActorLogic {
    from_callback(move |input| {
        let executor = executor.clone();
        let runtime = input
            .runtime
            .downcast::<ToolExecuteActorInput>()
            .expect("execute input");
        let tool_call = runtime.tool_call.clone();
        let tool_call_id = tool_call.id.clone();
        let signal = runtime.signal.clone();
        let wait_for_tasks = runtime.wait_for_tasks.clone();
        let send_back = input.send_back.clone();
        let detached = Arc::new(AtomicBool::new(false));
        let update_send = send_back.clone();
        let update_id = tool_call_id.clone();
        let on_update: OnToolUpdate = Arc::new(move |update| {
            update_send(tool_event_xevent(&ToolEvent::Update {
                tool_call_id: update_id.clone(),
                update,
            }));
        });
        let detach_send = send_back.clone();
        let detach_id = tool_call_id.clone();
        let detach_flag = detached.clone();
        let detach: OnToolDetach = Arc::new(move |ack| {
            if detach_flag.swap(true, Ordering::AcqRel) {
                return;
            }
            detach_send(tool_event_xevent(&ToolEvent::Detached {
                tool_call_id: detach_id.clone(),
                text: ack.text,
            }));
        });
        let done_send = send_back.clone();
        let done_id = tool_call_id.clone();
        let aborted_send = send_back.clone();
        let aborted_id = tool_call_id.clone();
        let failed_send = send_back;
        let failed_id = tool_call_id.clone();
        let cancelled = signal.clone();
        tokio::spawn(async move {
            let result = executor
                .execute(ToolExecuteInput {
                    tool_call,
                    signal,
                    on_update: Some(on_update),
                    detach: Some(detach),
                    wait_for_tasks,
                })
                .await;
            match result {
                Ok(result) => done_send(tool_event_xevent(&ToolEvent::Done {
                    tool_call_id: done_id,
                    result,
                })),
                Err(_) if cancelled.is_cancelled() => {
                    aborted_send(tool_event_xevent(&ToolEvent::Aborted {
                        tool_call_id: aborted_id,
                    }));
                }
                Err(error) => failed_send(tool_event_xevent(&ToolEvent::Failed {
                    tool_call_id: failed_id,
                    error: error.to_string(),
                })),
            }
        });
        Box::new(|| ())
    })
}

pub fn create_tool_machine(executor: Arc<dyn ToolExecutor>) -> ToolMachineLogic {
    let machine = Machine {
        id: "tool".to_string(),
        initial: "preparing".to_string(),
        context: Arc::new(|input| {
            let input = input.downcast::<ToolInput>().expect("tool input");
            Arc::new(ToolMachineContext {
                tool_call: input.tool_call.clone(),
                input: input.as_ref().clone(),
                outcome: None,
                result: None,
                error: None,
            }) as Context
        }),
        states: HashMap::from([
            (
                "preparing".to_string(),
                StateNode {
                    invoke: Some(InvokeDef {
                        id: "preparingActor".to_string(),
                        src: "preparingActor".to_string(),
                        input: Arc::new(|args| InvokeInput {
                            runtime: Arc::new(ToolBeforeInput {
                                tool_call: context_of(args.context).tool_call.clone(),
                            }),
                            restore: None,
                        }),
                        on_done: vec![
                            Transition {
                                target: Some("finishing".to_string()),
                                guard: Some(Arc::new(|_, event| {
                                    matches!(
                                        event
                                            .data
                                            .as_ref()
                                            .and_then(|data| data.downcast_ref::<ToolBeforeDecision>()),
                                        Some(ToolBeforeDecision::Denied { .. })
                                    )
                                })),
                                actions: vec![assign(|args| {
                                    let mut next = context_of(args.context).clone();
                                    if let Some(ToolBeforeDecision::Denied { result }) = args
                                        .event
                                        .data
                                        .as_ref()
                                        .and_then(|data| data.downcast_ref::<ToolBeforeDecision>())
                                    {
                                        next.result = Some(result.clone());
                                    }
                                    Arc::new(next)
                                })],
                            },
                            Transition {
                                target: Some("executing".to_string()),
                                actions: vec![assign(|args| {
                                    let mut next = context_of(args.context).clone();
                                    if let Some(ToolBeforeDecision::Proceed { tool_call }) = args
                                        .event
                                        .data
                                        .as_ref()
                                        .and_then(|data| data.downcast_ref::<ToolBeforeDecision>())
                                        && let Some(tool_call) = tool_call
                                    {
                                        next.tool_call = tool_call.clone();
                                    }
                                    Arc::new(next)
                                })],
                                ..Default::default()
                            },
                        ],
                        on_error: vec![Transition {
                            target: Some("failed".to_string()),
                            actions: vec![
                                assign(|args| {
                                    let mut next = context_of(args.context).clone();
                                    next.outcome = Some(ToolOutcome::Failed);
                                    next.error = Some(error_text(args.event));
                                    Arc::new(next)
                                }),
                                emit(|args| {
                                    tool_event_xevent(&failed_event(
                                        context_of(args.context),
                                        error_text(args.event),
                                    ))
                                }),
                                custom(|_, args| {
                                    args.self_.forward_to_parent(tool_event_xevent(&failed_event(
                                        context_of(args.context),
                                        error_text(args.event),
                                    )));
                                }),
                            ],
                            ..Default::default()
                        }],
                    }),
                    on: HashMap::from([(
                        "tool.abort".to_string(),
                        vec![Transition {
                            target: Some("aborted".to_string()),
                            actions: vec![
                                assign(|args| {
                                    let mut next = context_of(args.context).clone();
                                    next.outcome = Some(ToolOutcome::Aborted);
                                    Arc::new(next)
                                }),
                                emit(|args| {
                                    tool_event_xevent(&aborted_event(context_of(args.context)))
                                }),
                                custom(|_, args| {
                                    args.self_.forward_to_parent(tool_event_xevent(
                                        &aborted_event(context_of(args.context)),
                                    ));
                                }),
                            ],
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                },
            ),
            (
                "executing".to_string(),
                StateNode {
                    invoke: Some(InvokeDef {
                        id: "executeActor".to_string(),
                        src: "executeActor".to_string(),
                        input: Arc::new(|args| {
                            let context = context_of(args.context);
                            InvokeInput {
                                runtime: Arc::new(ToolExecuteActorInput {
                                    tool_call: context.tool_call.clone(),
                                    signal: context.input.signal.clone(),
                                    wait_for_tasks: context.input.wait_for_tasks.clone(),
                                }),
                                restore: None,
                            }
                        }),
                        on_done: Vec::new(),
                        on_error: Vec::new(),
                    }),
                    on: HashMap::from([
                        ("tool.abort".to_string(), vec![Transition::default()]),
                        (
                            "tool.update".to_string(),
                            vec![Transition {
                                actions: vec![emit(|args| args.event.clone()), forward_to_parent()],
                                ..Default::default()
                            }],
                        ),
                        (
                            "tool.detached".to_string(),
                            vec![Transition {
                                actions: vec![emit(|args| args.event.clone()), forward_to_parent()],
                                ..Default::default()
                            }],
                        ),
                        (
                            "tool.done".to_string(),
                            vec![Transition {
                                target: Some("finishing".to_string()),
                                actions: vec![assign(|args| {
                                    let mut next = context_of(args.context).clone();
                                    if let ToolEvent::Done { result, .. } = tool_event_of(args.event) {
                                        next.result = Some(result);
                                    }
                                    Arc::new(next)
                                })],
                                ..Default::default()
                            }],
                        ),
                        (
                            "tool.failed".to_string(),
                            vec![Transition {
                                target: Some("failed".to_string()),
                                actions: vec![
                                    assign(|args| {
                                        let mut next = context_of(args.context).clone();
                                        next.outcome = Some(ToolOutcome::Failed);
                                        if let ToolEvent::Failed { error, .. } = tool_event_of(args.event)
                                        {
                                            next.error = Some(error);
                                        }
                                        Arc::new(next)
                                    }),
                                    emit(|args| args.event.clone()),
                                    forward_to_parent(),
                                ],
                                ..Default::default()
                            }],
                        ),
                        (
                            "tool.aborted".to_string(),
                            vec![Transition {
                                target: Some("aborted".to_string()),
                                actions: vec![
                                    assign(|args| {
                                        let mut next = context_of(args.context).clone();
                                        next.outcome = Some(ToolOutcome::Aborted);
                                        Arc::new(next)
                                    }),
                                    emit(|args| args.event.clone()),
                                    forward_to_parent(),
                                ],
                                ..Default::default()
                            }],
                        ),
                    ]),
                    ..Default::default()
                },
            ),
            (
                "finishing".to_string(),
                StateNode {
                    invoke: Some(InvokeDef {
                        id: "finishingActor".to_string(),
                        src: "finishingActor".to_string(),
                        input: Arc::new(|args| {
                            let context = context_of(args.context);
                            InvokeInput {
                                runtime: Arc::new(ToolAfterInput {
                                    tool_call: context.tool_call.clone(),
                                    result: context.result.clone().expect("tool result"),
                                }),
                                restore: None,
                            }
                        }),
                        on_done: vec![Transition {
                            target: Some("succeeded".to_string()),
                            actions: vec![
                                assign(|args| {
                                    let mut next = context_of(args.context).clone();
                                    next.outcome = Some(ToolOutcome::Succeeded);
                                    if let Some(result) = args
                                        .event
                                        .data
                                        .as_ref()
                                        .and_then(|data| data.downcast_ref::<ToolResult>())
                                    {
                                        next.result = Some(result.clone());
                                    }
                                    Arc::new(next)
                                }),
                                emit(|args| {
                                    let result = args
                                        .event
                                        .data
                                        .as_ref()
                                        .and_then(|data| data.downcast_ref::<ToolResult>())
                                        .cloned()
                                        .expect("tool result");
                                    tool_event_xevent(&done_event(context_of(args.context), result))
                                }),
                                custom(|_, args| {
                                    let result = args
                                        .event
                                        .data
                                        .as_ref()
                                        .and_then(|data| data.downcast_ref::<ToolResult>())
                                        .cloned()
                                        .expect("tool result");
                                    args.self_.forward_to_parent(tool_event_xevent(&done_event(
                                        context_of(args.context),
                                        result,
                                    )));
                                }),
                            ],
                            ..Default::default()
                        }],
                        on_error: vec![Transition {
                            target: Some("failed".to_string()),
                            actions: vec![
                                assign(|args| {
                                    let mut next = context_of(args.context).clone();
                                    next.outcome = Some(ToolOutcome::Failed);
                                    next.error = Some(error_text(args.event));
                                    Arc::new(next)
                                }),
                                emit(|args| {
                                    tool_event_xevent(&failed_event(
                                        context_of(args.context),
                                        error_text(args.event),
                                    ))
                                }),
                                custom(|_, args| {
                                    args.self_.forward_to_parent(tool_event_xevent(&failed_event(
                                        context_of(args.context),
                                        error_text(args.event),
                                    )));
                                }),
                            ],
                            ..Default::default()
                        }],
                    }),
                    ..Default::default()
                },
            ),
            ("succeeded".to_string(), StateNode { final_: true, ..Default::default() }),
            ("failed".to_string(), StateNode { final_: true, ..Default::default() }),
            ("aborted".to_string(), StateNode { final_: true, ..Default::default() }),
        ]),
        actors: HashMap::from([
            ("preparingActor".to_string(), prepare_actor()),
            ("executeActor".to_string(), execute_actor(executor)),
            ("finishingActor".to_string(), finish_actor()),
        ]),
        output: Some(Arc::new(|context| {
            let context = context_of(context);
            Some(Arc::new(match context.outcome {
                Some(ToolOutcome::Failed) => ToolOutput::Failed {
                    error: context.error.clone().unwrap_or_default(),
                },
                Some(ToolOutcome::Aborted) => ToolOutput::Aborted,
                _ => ToolOutput::Succeeded {
                    result: context.result.clone().expect("tool result"),
                },
            }) as Context)
        })),
        ..Default::default()
    };
    ToolMachineLogic { machine }
}

pub fn wait_for_tasks_input(
    wait: &WaitForTasks,
    task_id: Option<String>,
    timeout_ms: u64,
) -> futures_util::future::BoxFuture<'static, Result<TaskWaitOutcome, crate::wire::record::WireAsyncError>>
{
    wait(TaskWaitInput {
        task_id,
        timeout_ms,
    })
}
