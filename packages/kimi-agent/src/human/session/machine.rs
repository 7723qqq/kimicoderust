use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::human::agent::machine::{AgentEvent, AgentInput};
use crate::human::llm::requester::requester::LlmRequestConfig;
use crate::human::tool::executor::ToolUpdate;
use crate::human::xstate2::{Actor, XEvent};

pub type AgentActorRef = Arc<Actor>;

pub trait AgentLogic: Send + Sync {
    fn spawn(&self, id: &str, input: AgentInput) -> AgentActorRef;
}

impl<F> AgentLogic for F
where
    F: Fn(&str, AgentInput) -> AgentActorRef + Send + Sync,
{
    fn spawn(&self, id: &str, input: AgentInput) -> AgentActorRef {
        self(id, input)
    }
}

#[derive(Clone)]
pub struct AgentEntry {
    pub ref_: AgentActorRef,
    pub logic: Arc<dyn AgentLogic>,
    pub input: AgentInput,
    pub pending_restart: bool,
}

#[derive(Clone)]
pub struct SessionInput {
    pub request: LlmRequestConfig,
}

pub enum SessionEvent {
    Agent(AgentEvent),
    ToolUpdate {
        tool_call_id: String,
        update: ToolUpdate,
    },
    AgentCreate {
        agent_id: Option<String>,
        logic: Arc<dyn AgentLogic>,
        input: AgentInput,
    },
    AgentFork {
        source_id: String,
        agent_id: Option<String>,
        logic: Arc<dyn AgentLogic>,
        input: AgentInput,
    },
    AgentRestart {
        agent_id: String,
    },
    AgentSend {
        agent_id: String,
        event: AgentEvent,
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

#[derive(Clone)]
pub struct SessionMachineContext {
    pub input: SessionInput,
    pub agents: HashMap<String, AgentEntry>,
    pub anonymous_count: u64,
}

pub struct SessionMachineLogic;

pub struct SessionActor {
    context: Mutex<SessionMachineContext>,
    listeners: Mutex<Vec<Arc<dyn Fn(SessionEmitted) + Send + Sync>>>,
}

impl SessionMachineLogic {
    pub fn start(&self, input: SessionInput) -> Arc<SessionActor> {
        Arc::new(SessionActor {
            context: Mutex::new(SessionMachineContext {
                input,
                agents: HashMap::new(),
                anonymous_count: 0,
            }),
            listeners: Mutex::new(Vec::new()),
        })
    }
}

impl SessionActor {
    pub fn send(&self, event: SessionEvent) {
        match event {
            SessionEvent::AgentCreate {
                agent_id,
                logic,
                input,
            } => self.create(agent_id, logic, input),
            SessionEvent::AgentFork {
                source_id,
                agent_id,
                logic,
                input,
            } => self.fork(source_id, agent_id, logic, input),
            SessionEvent::AgentRestart { agent_id } => self.restart(agent_id),
            SessionEvent::AgentSend { agent_id, event } => self.send_to_agent(agent_id, event),
            SessionEvent::AgentStop { agent_id } => self.stop_agent(agent_id),
            SessionEvent::DoneActor { actor_id } => self.actor_done(actor_id),
            SessionEvent::Agent(_) | SessionEvent::ToolUpdate { .. } => {}
        }
    }

    fn create(&self, agent_id: Option<String>, logic: Arc<dyn AgentLogic>, input: AgentInput) {
        if let Some(agent_id) = agent_id.as_ref() {
            if self.context.lock().agents.contains_key(agent_id) {
                self.emit(SessionEmitted::AgentFailed {
                    agent_id: agent_id.clone(),
                    error: format!("duplicate agent id: '{agent_id}'"),
                });
                return;
            }
        }
        let (agent_id, branch_id, ref_) = {
            let mut context = self.context.lock();
            let anonymous_count = if agent_id.is_none() {
                next_anonymous_count(&context)
            } else {
                context.anonymous_count
            };
            let agent_id = agent_id.unwrap_or_else(|| format!("agent-{anonymous_count}"));
            let ref_ = logic.spawn(&agent_id, input.clone());
            context.agents.insert(
                agent_id.clone(),
                AgentEntry {
                    ref_: ref_.clone(),
                    logic,
                    input: input.clone(),
                    pending_restart: false,
                },
            );
            context.anonymous_count = anonymous_count;
            (agent_id.clone(), branch_id(&input), ref_)
        };
        self.emit(SessionEmitted::AgentCreated {
            agent_id,
            branch_id,
            ref_,
        });
    }

    fn fork(
        &self,
        source_id: String,
        agent_id: Option<String>,
        logic: Arc<dyn AgentLogic>,
        input: AgentInput,
    ) {
        {
            let context = self.context.lock();
            if !context.agents.contains_key(&source_id) {
                self.emit(SessionEmitted::AgentFailed {
                    agent_id: agent_id.unwrap_or(source_id),
                    error: format!("unknown agent: '{source_id}'"),
                });
                return;
            }
            if let Some(agent_id) = agent_id.as_ref() {
                if context.agents.contains_key(agent_id) {
                    self.emit(SessionEmitted::AgentFailed {
                        agent_id: agent_id.clone(),
                        error: format!("duplicate agent id: '{agent_id}'"),
                    });
                    return;
                }
            }
        }
        let (agent_id, branch_id, ref_) = {
            let mut context = self.context.lock();
            let anonymous_count = if agent_id.is_none() {
                next_anonymous_count(&context)
            } else {
                context.anonymous_count
            };
            let agent_id = agent_id.unwrap_or_else(|| format!("agent-{anonymous_count}"));
            let ref_ = logic.spawn(&agent_id, input.clone());
            context.agents.insert(
                agent_id.clone(),
                AgentEntry {
                    ref_: ref_.clone(),
                    logic,
                    input: input.clone(),
                    pending_restart: false,
                },
            );
            context.anonymous_count = anonymous_count;
            (agent_id.clone(), branch_id(&input), ref_)
        };
        self.emit(SessionEmitted::AgentForked {
            source_id,
            agent_id,
            branch_id,
            ref_,
        });
    }

    fn restart(&self, agent_id: String) {
        let ref_ = {
            let mut context = self.context.lock();
            let Some(entry) = context.agents.get_mut(&agent_id) else {
                drop(context);
                self.emit(SessionEmitted::AgentFailed {
                    agent_id: agent_id.clone(),
                    error: format!("unknown agent: '{agent_id}'"),
                });
                return;
            };
            if entry.pending_restart {
                drop(context);
                self.emit(SessionEmitted::AgentFailed {
                    agent_id: agent_id.clone(),
                    error: format!("agent '{agent_id}' restart already pending"),
                });
                return;
            }
            entry.pending_restart = true;
            entry.ref_.clone()
        };
        ref_.send(XEvent::new("input.close", Value::Null));
    }

    fn send_to_agent(&self, agent_id: String, event: AgentEvent) {
        let ref_ = self
            .context
            .lock()
            .agents
            .get(&agent_id)
            .map(|entry| entry.ref_.clone());
        let Some(ref_) = ref_ else {
            self.emit(SessionEmitted::AgentFailed {
                agent_id: agent_id.clone(),
                error: format!("unknown agent: '{agent_id}'"),
            });
            return;
        };
        ref_.send(event.into());
    }

    fn stop_agent(&self, agent_id: String) {
        let ref_ = {
            let mut context = self.context.lock();
            let Some(entry) = context.agents.get_mut(&agent_id) else {
                drop(context);
                self.emit(SessionEmitted::AgentFailed {
                    agent_id: agent_id.clone(),
                    error: format!("unknown agent: '{agent_id}'"),
                });
                return;
            };
            entry.pending_restart = false;
            entry.ref_.clone()
        };
        ref_.send(XEvent::new("input.close", Value::Null));
    }

    fn actor_done(&self, actor_id: String) {
        let action = {
            let mut context = self.context.lock();
            match context.agents.get(&actor_id) {
                Some(entry) if entry.pending_restart => {
                    let ref_ = entry.logic.spawn(&actor_id, entry.input.clone());
                    context.agents.insert(
                        actor_id.clone(),
                        AgentEntry {
                            ref_: ref_.clone(),
                            logic: entry.logic.clone(),
                            input: entry.input.clone(),
                            pending_restart: false,
                        },
                    );
                    Some(Ok(ref_))
                }
                Some(_) => {
                    let entry = context.agents.remove(&actor_id).unwrap();
                    Some(Err(entry.ref_))
                }
                None => None,
            }
        };
        match action {
            Some(Ok(ref_)) => self.emit(SessionEmitted::AgentRestarted {
                agent_id: actor_id,
                ref_,
            }),
            Some(Err(ref_)) => {
                ref_.stop();
                self.emit(SessionEmitted::AgentStopped { agent_id: actor_id });
            }
            None => {}
        }
    }

    pub fn subscribe(&self, listener: Arc<dyn Fn(SessionEmitted) + Send + Sync>) {
        self.listeners.lock().push(listener);
    }

    pub fn snapshot(&self) -> SessionMachineContext {
        self.context.lock().clone()
    }

    pub fn stop(&self) {
        let agents = std::mem::take(&mut self.context.lock().agents);
        for (_, entry) in agents {
            entry.ref_.stop();
        }
    }

    fn emit(&self, event: SessionEmitted) {
        for listener in self.listeners.lock().iter() {
            listener(event.clone());
        }
    }
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

pub fn create_session_machine() -> SessionMachineLogic {
    SessionMachineLogic
}
