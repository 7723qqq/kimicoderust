use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::human::agent::machine::{AgentEvent, AgentInput, AgentMachineLogic};
use crate::human::agent::turn::TurnLlmEvent;
use crate::human::llm::requester::requester::LlmRequestConfig;
use crate::human::tool::executor::ToolUpdate;
use crate::human::tool::machine::ToolEvent;
use crate::human::xstate2::{
    Action, ActionArgs, Actor, Context, Machine, StateNode, Transition, XEvent, assign, emit,
    send_to_ref, stop_child,
};

pub type AgentActorRef = Arc<Actor>;

#[derive(Clone)]
pub struct AgentEntry {
    pub ref_: AgentActorRef,
    pub logic: Arc<AgentMachineLogic>,
    pub input: AgentInput,
    pub pending_restart: bool,
}

#[derive(Clone)]
pub struct SessionInput {
    pub request: LlmRequestConfig,
}

pub enum SessionEvent {
    Llm(TurnLlmEvent),
    Tool(ToolEvent),
    ToolUpdate {
        tool_call_id: String,
        update: ToolUpdate,
    },
    AgentCreate {
        agent_id: Option<String>,
        logic: Arc<AgentMachineLogic>,
        input: AgentInput,
    },
    AgentFork {
        source_id: String,
        agent_id: Option<String>,
        logic: Arc<AgentMachineLogic>,
        input: AgentInput,
    },
    AgentSend {
        agent_id: String,
        event: AgentEvent,
    },
    AgentRestart {
        agent_id: String,
    },
    AgentStop {
        agent_id: String,
    },
    DoneActor {
        actor_id: String,
    },
}

#[derive(Clone)]
pub enum SessionEmitted {
    AgentCreated {
        agent_id: String,
        branch_id: String,
        ref_: AgentActorRef,
    },
    AgentForked {
        source_id: String,
        agent_id: String,
        branch_id: String,
        ref_: AgentActorRef,
    },
    AgentRestarted {
        agent_id: String,
        ref_: AgentActorRef,
    },
    AgentStopped {
        agent_id: String,
    },
    AgentFailed {
        agent_id: String,
        error: String,
    },
}

impl SessionEmitted {
    pub fn type_(&self) -> &'static str {
        match self {
            Self::AgentCreated { .. } => "agent.created",
            Self::AgentForked { .. } => "agent.forked",
            Self::AgentRestarted { .. } => "agent.restarted",
            Self::AgentStopped { .. } => "agent.stopped",
            Self::AgentFailed { .. } => "agent.failed",
        }
    }
}

#[derive(Clone)]
pub struct SessionMachineContext {
    pub input: SessionInput,
    pub agents: HashMap<String, AgentEntry>,
    pub anonymous_count: u64,
}

pub struct SessionMachineLogic {
    machine: Machine,
}

impl SessionMachineLogic {
    pub fn machine(&self) -> &Machine {
        &self.machine
    }
}

fn context_of(context: &Context) -> &SessionMachineContext {
    context
        .downcast_ref::<SessionMachineContext>()
        .expect("session machine context")
}

fn emitted_xevent(emitted: SessionEmitted) -> XEvent {
    XEvent {
        type_: emitted.type_().to_string(),
        payload: Value::Null,
        restore: None,
        data: Some(Arc::new(emitted)),
    }
}

pub fn session_emitted_of(event: &XEvent) -> Option<SessionEmitted> {
    event
        .data
        .as_ref()
        .and_then(|data| data.downcast_ref::<SessionEmitted>())
        .cloned()
}

fn session_event_of(event: &XEvent) -> Option<&SessionEvent> {
    event
        .data
        .as_ref()
        .and_then(|data| data.downcast_ref::<SessionEvent>())
}

fn done_actor_id(event: &XEvent) -> Option<String> {
    event
        .type_
        .strip_prefix("xstate.done.actor.")
        .map(str::to_string)
}

fn next_anonymous_count(context: &SessionMachineContext) -> u64 {
    let mut count = context.anonymous_count + 1;
    while context.agents.contains_key(&format!("agent-{count}")) {
        count += 1;
    }
    count
}

fn branch_id(input: &AgentInput) -> String {
    input
        .store
        .as_ref()
        .map(|store| store.ref_().branch)
        .unwrap_or_else(|| "main".to_string())
}

fn agent_created(agent_id: String, branch_id: String, ref_: AgentActorRef) -> SessionEmitted {
    SessionEmitted::AgentCreated {
        agent_id,
        branch_id,
        ref_,
    }
}

fn emit_agent_fork_failed() -> Action {
    emit(|args| {
        let context = context_of(args.context);
        let Some(SessionEvent::AgentFork {
            source_id,
            agent_id,
            ..
        }) = session_event_of(args.event)
        else {
            return XEvent::new("agent.failed", Value::Null);
        };
        let (agent_id, error) = if !context.agents.contains_key(source_id) {
            (
                agent_id.clone().unwrap_or_else(|| source_id.clone()),
                format!("unknown agent: '{source_id}'"),
            )
        } else {
            let agent_id = agent_id.clone().unwrap_or_default();
            (
                agent_id.clone(),
                format!("duplicate agent id: '{agent_id}'"),
            )
        };
        emitted_xevent(SessionEmitted::AgentFailed { agent_id, error })
    })
}

fn emit_agent_restart_failed() -> Action {
    emit(|args| {
        let context = context_of(args.context);
        let Some(SessionEvent::AgentRestart { agent_id }) = session_event_of(args.event) else {
            return XEvent::new("agent.failed", Value::Null);
        };
        let error = match context.agents.get(agent_id) {
            None => format!("unknown agent: '{agent_id}'"),
            Some(_) => format!("agent '{agent_id}' restart already pending"),
        };
        emitted_xevent(SessionEmitted::AgentFailed {
            agent_id: agent_id.clone(),
            error,
        })
    })
}

impl From<SessionEvent> for XEvent {
    fn from(event: SessionEvent) -> Self {
        match event {
            SessionEvent::Llm(event) => AgentEvent::Llm(event).into(),
            SessionEvent::Tool(event) => AgentEvent::Tool(event).into(),
            SessionEvent::ToolUpdate {
                tool_call_id,
                update,
            } => XEvent {
                type_: "tool.update".to_string(),
                payload: json!({ "toolCallId": tool_call_id, "update": update }),
                restore: None,
                data: None,
            },
            SessionEvent::AgentCreate { ref agent_id, .. } => XEvent {
                type_: "agent.create".to_string(),
                payload: json!({ "agentId": agent_id }),
                restore: None,
                data: Some(Arc::new(event)),
            },
            SessionEvent::AgentFork {
                ref source_id, ..
            } => XEvent {
                type_: "agent.fork".to_string(),
                payload: json!({ "sourceId": source_id }),
                restore: None,
                data: Some(Arc::new(event)),
            },
            SessionEvent::AgentSend { ref agent_id, .. } => XEvent {
                type_: "agent.send".to_string(),
                payload: json!({ "agentId": agent_id }),
                restore: None,
                data: Some(Arc::new(event)),
            },
            SessionEvent::AgentRestart { ref agent_id } => XEvent {
                type_: "agent.restart".to_string(),
                payload: json!({ "agentId": agent_id }),
                restore: None,
                data: Some(Arc::new(event)),
            },
            SessionEvent::AgentStop { ref agent_id } => XEvent {
                type_: "agent.stop".to_string(),
                payload: json!({ "agentId": agent_id }),
                restore: None,
                data: Some(Arc::new(event)),
            },
            SessionEvent::DoneActor { ref actor_id } => XEvent {
                type_: format!("xstate.done.actor.{actor_id}"),
                payload: json!({ "actorId": actor_id }),
                restore: None,
                data: Some(Arc::new(event)),
            },
        }
    }
}

fn spawn_agent(
    agent_id: String,
    logic: Arc<AgentMachineLogic>,
    input: AgentInput,
    args: &ActionArgs,
) -> AgentActorRef {
    args.spawn
        .spawn(logic.machine(), &agent_id, Arc::new(input))
}

fn restart_done_agent() -> Action {
    assign(|args| {
        let mut next = context_of(args.context).clone();
        let Some(actor_id) = done_actor_id(args.event) else {
            return Arc::new(next);
        };
        let Some(entry) = next.agents.get(&actor_id).cloned() else {
            return Arc::new(next);
        };
        if !entry.pending_restart {
            return Arc::new(next);
        }
        let ref_ = spawn_agent(actor_id.clone(), entry.logic.clone(), entry.input.clone(), args);
        next.agents.insert(
            actor_id,
            AgentEntry {
                ref_,
                logic: entry.logic,
                input: entry.input,
                pending_restart: false,
            },
        );
        Arc::new(next)
    })
}

fn emit_agent_restarted() -> Action {
    emit(|args| {
        let actor_id = done_actor_id(args.event).expect("done actor id");
        let entry = context_of(args.context)
            .agents
            .get(&actor_id)
            .expect("restarted agent");
        emitted_xevent(SessionEmitted::AgentRestarted {
            agent_id: actor_id,
            ref_: entry.ref_.clone(),
        })
    })
}

fn drop_done_agent() -> Action {
    assign(|args| {
        let mut next = context_of(args.context).clone();
        if let Some(actor_id) = done_actor_id(args.event) {
            next.agents.remove(&actor_id);
        }
        Arc::new(next)
    })
}

fn spawn_created_agent() -> Action {
    assign(|args| {
        let Some(SessionEvent::AgentCreate {
            agent_id,
            logic,
            input,
        }) = session_event_of(args.event)
        else {
            return args.context.clone();
        };
        let mut next = context_of(args.context).clone();
        let anonymous_count = match agent_id {
            Some(_) => next.anonymous_count,
            None => next_anonymous_count(&next),
        };
        let agent_id = agent_id
            .clone()
            .unwrap_or_else(|| format!("agent-{anonymous_count}"));
        let input = input.clone();
        let ref_ = spawn_agent(agent_id.clone(), logic.clone(), input.clone(), args);
        next.agents.insert(
            agent_id,
            AgentEntry {
                ref_,
                logic: logic.clone(),
                input,
                pending_restart: false,
            },
        );
        next.anonymous_count = anonymous_count;
        Arc::new(next)
    })
}

fn emit_agent_created() -> Action {
    emit(|args| {
        let context = context_of(args.context);
        let Some(SessionEvent::AgentCreate {
            agent_id, input, ..
        }) = session_event_of(args.event)
        else {
            return XEvent::new("agent.created", Value::Null);
        };
        let agent_id = agent_id
            .clone()
            .unwrap_or_else(|| format!("agent-{}", context.anonymous_count));
        let entry = context.agents.get(&agent_id).expect("created agent");
        emitted_xevent(agent_created(
            agent_id,
            branch_id(input),
            entry.ref_.clone(),
        ))
    })
}

fn spawn_forked_agent() -> Action {
    assign(|args| {
        let Some(SessionEvent::AgentFork {
            agent_id,
            logic,
            input,
            ..
        }) = session_event_of(args.event)
        else {
            return args.context.clone();
        };
        let mut next = context_of(args.context).clone();
        let anonymous_count = match agent_id {
            Some(_) => next.anonymous_count,
            None => next_anonymous_count(&next),
        };
        let agent_id = agent_id
            .clone()
            .unwrap_or_else(|| format!("agent-{anonymous_count}"));
        let input = input.clone();
        let ref_ = spawn_agent(agent_id.clone(), logic.clone(), input.clone(), args);
        next.agents.insert(
            agent_id,
            AgentEntry {
                ref_,
                logic: logic.clone(),
                input,
                pending_restart: false,
            },
        );
        next.anonymous_count = anonymous_count;
        Arc::new(next)
    })
}

fn emit_agent_forked() -> Action {
    emit(|args| {
        let context = context_of(args.context);
        let Some(SessionEvent::AgentFork {
            source_id,
            agent_id,
            input,
            ..
        }) = session_event_of(args.event)
        else {
            return XEvent::new("agent.forked", Value::Null);
        };
        let agent_id = agent_id
            .clone()
            .unwrap_or_else(|| format!("agent-{}", context.anonymous_count));
        let entry = context.agents.get(&agent_id).expect("forked agent");
        emitted_xevent(SessionEmitted::AgentForked {
            source_id: source_id.clone(),
            agent_id,
            branch_id: branch_id(input),
            ref_: entry.ref_.clone(),
        })
    })
}

fn send_to_agent() -> Action {
    send_to_ref(
        |args| {
            let SessionEvent::AgentSend { agent_id, .. } = session_event_of(args.event)? else {
                return None;
            };
            context_of(args.context)
                .agents
                .get(agent_id)
                .map(|entry| entry.ref_.clone())
        },
        |args| {
            let SessionEvent::AgentSend { event, .. } =
                session_event_of(args.event).expect("agent.send event")
            else {
                panic!("agent.send event");
            };
            event.clone().into()
        },
    )
}

fn send_close_to_agent() -> Action {
    send_to_ref(
        |args| {
            let agent_id = match session_event_of(args.event)? {
                SessionEvent::AgentRestart { agent_id } | SessionEvent::AgentStop { agent_id } => {
                    agent_id
                }
                _ => return None,
            };
            context_of(args.context)
                .agents
                .get(agent_id)
                .map(|entry| entry.ref_.clone())
        },
        |_| XEvent::new("input.close", Value::Null),
    )
}

fn mark_pending_restart(pending: bool) -> Action {
    assign(move |args| {
        let mut next = context_of(args.context).clone();
        let agent_id = match session_event_of(args.event) {
            Some(SessionEvent::AgentRestart { agent_id })
            | Some(SessionEvent::AgentStop { agent_id }) => agent_id.clone(),
            _ => return Arc::new(next),
        };
        if let Some(entry) = next.agents.get_mut(&agent_id) {
            entry.pending_restart = pending;
        }
        Arc::new(next)
    })
}

fn emit_agent_failed(
    message: impl Fn(&SessionEvent) -> (String, String) + Send + Sync + 'static,
) -> Action {
    emit(move |args| {
        let Some(event) = session_event_of(args.event) else {
            return XEvent::new("agent.failed", Value::Null);
        };
        let (agent_id, error) = message(event);
        emitted_xevent(SessionEmitted::AgentFailed { agent_id, error })
    })
}

fn event_agent_id(event: &SessionEvent) -> Option<&str> {
    match event {
        SessionEvent::AgentCreate { agent_id, .. } => agent_id.as_deref(),
        SessionEvent::AgentFork { agent_id, .. } => agent_id.as_deref(),
        SessionEvent::AgentSend { agent_id, .. }
        | SessionEvent::AgentRestart { agent_id }
        | SessionEvent::AgentStop { agent_id } => Some(agent_id),
        _ => None,
    }
}

pub fn create_session_machine() -> SessionMachineLogic {
    let mut root_on: HashMap<String, Vec<Transition>> = HashMap::new();
    for event_type in [
        "llm.sent",
        "llm.streaming.*",
        "llm.done",
        "llm.failed.syntax",
        "llm.failed.remote",
        "llm.retrying",
        "tool.detached",
        "tool.update",
        "tool.done",
        "tool.failed",
        "tool.aborted",
        "context.reset",
        "store.reset",
        "store.error",
    ] {
        root_on.insert(event_type.to_string(), vec![Transition::default()]);
    }
    root_on.insert(
        "xstate.done.actor.*".to_string(),
        vec![
            Transition {
                guard: Some(Arc::new(|context, event| {
                    let Some(actor_id) = done_actor_id(event) else {
                        return false;
                    };
                    context_of(context)
                        .agents
                        .get(&actor_id)
                        .is_some_and(|entry| entry.pending_restart)
                })),
                actions: vec![restart_done_agent(), emit_agent_restarted()],
                ..Default::default()
            },
            Transition {
                guard: Some(Arc::new(|context, event| {
                    done_actor_id(event)
                        .is_some_and(|actor_id| context_of(context).agents.contains_key(&actor_id))
                })),
                actions: vec![
                    drop_done_agent(),
                    stop_child(|args| done_actor_id(args.event)),
                    emit(|args| {
                        emitted_xevent(SessionEmitted::AgentStopped {
                            agent_id: done_actor_id(args.event).expect("done actor id"),
                        })
                    }),
                ],
                ..Default::default()
            },
        ],
    );

    let mut active_on: HashMap<String, Vec<Transition>> = HashMap::new();
    active_on.insert(
        "agent.create".to_string(),
        vec![
            Transition {
                guard: Some(Arc::new(|context, event| {
                    let Some(SessionEvent::AgentCreate {
                        agent_id: Some(agent_id),
                        ..
                    }) = session_event_of(event)
                    else {
                        return false;
                    };
                    context_of(context).agents.contains_key(agent_id)
                })),
                actions: vec![emit_agent_failed(|event| {
                    let agent_id = event_agent_id(event).unwrap_or_default().to_string();
                    (
                        agent_id.clone(),
                        format!("duplicate agent id: '{agent_id}'"),
                    )
                })],
                ..Default::default()
            },
            Transition {
                actions: vec![spawn_created_agent(), emit_agent_created()],
                ..Default::default()
            },
        ],
    );
    active_on.insert(
        "agent.fork".to_string(),
        vec![
            Transition {
                guard: Some(Arc::new(|context, event| {
                    let Some(SessionEvent::AgentFork {
                        source_id,
                        agent_id,
                        ..
                    }) = session_event_of(event)
                    else {
                        return false;
                    };
                    let context = context_of(context);
                    !context.agents.contains_key(source_id)
                        || agent_id
                            .as_ref()
                            .is_some_and(|agent_id| context.agents.contains_key(agent_id))
                })),
                actions: vec![emit_agent_fork_failed()],
                ..Default::default()
            },
            Transition {
                actions: vec![spawn_forked_agent(), emit_agent_forked()],
                ..Default::default()
            },
        ],
    );
    active_on.insert(
        "agent.send".to_string(),
        vec![
            Transition {
                guard: Some(Arc::new(|context, event| {
                    let Some(SessionEvent::AgentSend { agent_id, .. }) = session_event_of(event)
                    else {
                        return false;
                    };
                    !context_of(context).agents.contains_key(agent_id)
                })),
                actions: vec![emit_agent_failed(|event| {
                    let agent_id = event_agent_id(event).unwrap_or_default().to_string();
                    (
                        agent_id.clone(),
                        format!("unknown agent: '{agent_id}'"),
                    )
                })],
                ..Default::default()
            },
            Transition {
                actions: vec![send_to_agent()],
                ..Default::default()
            },
        ],
    );
    active_on.insert(
        "agent.restart".to_string(),
        vec![
            Transition {
                guard: Some(Arc::new(|context, event| {
                    let Some(SessionEvent::AgentRestart { agent_id }) = session_event_of(event)
                    else {
                        return false;
                    };
                    let context = context_of(context);
                    match context.agents.get(agent_id) {
                        None => true,
                        Some(entry) => entry.pending_restart,
                    }
                })),
                actions: vec![emit_agent_restart_failed()],
                ..Default::default()
            },
            Transition {
                actions: vec![mark_pending_restart(true), send_close_to_agent()],
                ..Default::default()
            },
        ],
    );
    active_on.insert(
        "agent.stop".to_string(),
        vec![
            Transition {
                guard: Some(Arc::new(|context, event| {
                    let Some(SessionEvent::AgentStop { agent_id }) = session_event_of(event) else {
                        return false;
                    };
                    !context_of(context).agents.contains_key(agent_id)
                })),
                actions: vec![emit_agent_failed(|event| {
                    let agent_id = event_agent_id(event).unwrap_or_default().to_string();
                    (
                        agent_id.clone(),
                        format!("unknown agent: '{agent_id}'"),
                    )
                })],
                ..Default::default()
            },
            Transition {
                actions: vec![mark_pending_restart(false), send_close_to_agent()],
                ..Default::default()
            },
        ],
    );

    let active = StateNode {
        on: active_on,
        ..Default::default()
    };

    let machine = Machine {
        id: "session".to_string(),
        initial: "active".to_string(),
        context: Arc::new(|input| {
            let input = input.downcast::<SessionInput>().expect("session input");
            Arc::new(SessionMachineContext {
                input: input.as_ref().clone(),
                agents: HashMap::new(),
                anonymous_count: 0,
            }) as Context
        }),
        states: HashMap::from([("active".to_string(), active)]),
        on: root_on,
        invoke: None,
        actors: HashMap::new(),
        delays: HashMap::new(),
        output: None,
    };
    SessionMachineLogic { machine }
}
