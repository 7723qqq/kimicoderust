use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde_json::Value;

pub use crate::agent::actor_service::agent_actor_runtime::AgentActorRestore;

pub type Context = Arc<dyn Any + Send + Sync>;

#[derive(Clone)]
pub struct XEvent {
    pub type_: String,
    pub payload: Value,
    pub restore: Option<AgentActorRestore>,
    pub data: Option<Context>,
}

impl XEvent {
    pub fn new(type_: impl Into<String>, payload: Value) -> Self {
        Self {
            type_: type_.into(),
            payload,
            restore: None,
            data: None,
        }
    }

    pub fn with_data(type_: impl Into<String>, payload: Value, data: Context) -> Self {
        Self {
            type_: type_.into(),
            payload,
            restore: None,
            data: Some(data),
        }
    }

    pub fn restore(handle: AgentActorRestore) -> Self {
        Self {
            type_: "runtime.restore".into(),
            payload: Value::Null,
            restore: Some(handle),
            data: None,
        }
    }
}

pub enum Action {
    Custom(Arc<dyn Fn(&mut Context, &XEvent) + Send + Sync>),
    SendTo(String, Arc<dyn Fn(&XEvent) -> XEvent + Send + Sync>),
}

pub struct Transition {
    pub target: Option<String>,
    pub actions: Vec<Action>,
}

pub struct InvokeDef {
    pub id: String,
    pub src: String,
    pub input: Arc<dyn Fn(&Context, &XEvent) -> InvokeInput + Send + Sync>,
}

pub struct InvokeInput {
    pub runtime: Context,
    pub restore: Option<AgentActorRestore>,
}

pub struct StateNode {
    pub on: HashMap<String, Transition>,
    pub invoke: Option<InvokeDef>,
}

pub struct Machine {
    pub context: Arc<dyn Fn(Context) -> Context + Send + Sync>,
    pub initial: String,
    pub states: HashMap<String, StateNode>,
    pub on: HashMap<String, Transition>,
    pub actors: HashMap<String, FromCallback>,
}

pub struct CallbackInput {
    pub runtime: Context,
    pub restore: Option<AgentActorRestore>,
    pub receive: Arc<dyn Fn(Arc<dyn Fn(XEvent) + Send + Sync>) + Send + Sync>,
    pub send_back: Arc<dyn Fn(XEvent) + Send + Sync>,
}

pub struct FromCallback {
    pub start: Arc<dyn Fn(CallbackInput) -> Box<dyn FnOnce() + Send> + Send + Sync>,
}

pub fn from_callback(
    start: impl Fn(CallbackInput) -> Box<dyn FnOnce() + Send> + Send + Sync + 'static,
) -> FromCallback {
    FromCallback {
        start: Arc::new(start),
    }
}

pub fn custom(apply: impl Fn(&mut Context, &XEvent) + Send + Sync + 'static) -> Action {
    Action::Custom(Arc::new(apply))
}

pub fn send_to(actor: &str, event: impl Fn(&XEvent) -> XEvent + Send + Sync + 'static) -> Action {
    Action::SendTo(actor.to_string(), Arc::new(event))
}

struct InvokedActor {
    cleanup: Option<Box<dyn FnOnce() + Send>>,
    receive: Arc<dyn Fn(XEvent) + Send + Sync>,
}

struct ActorInner {
    context: Context,
    current: String,
    invoked: HashMap<String, InvokedActor>,
}

pub struct Actor {
    machine: Machine,
    inner: Mutex<ActorInner>,
    listeners: Arc<Mutex<Vec<Arc<dyn Fn(Context, &str) + Send + Sync>>>>,
}

impl Actor {
    fn new(machine: Machine, input: Context) -> Self {
        let context = (machine.context)(input);
        Self {
            machine,
            inner: Mutex::new(ActorInner {
                context,
                current: String::new(),
                invoked: HashMap::new(),
            }),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn start(self: &Arc<Self>) {
        let initial = self.machine.initial.clone();
        let init_event = XEvent::new("xstate.init", Value::Null);
        self.enter_state(&initial, &init_event);
        self.notify();
    }

    fn enter_state(self: &Arc<Self>, state: &str, event: &XEvent) {
        let invoke = self
            .machine
            .states
            .get(state)
            .and_then(|node| node.invoke.as_ref())
            .map(|invoke| InvokeDef {
                id: invoke.id.clone(),
                src: invoke.src.clone(),
                input: invoke.input.clone(),
            });
        {
            let mut inner = self.inner.lock();
            inner.current = state.to_string();
        }
        let Some(invoke) = invoke else {
            return;
        };
        let Some(src) = self.machine.actors.get(&invoke.src) else {
            return;
        };
        let context = self.inner.lock().context.clone();
        let input = (invoke.input)(&context, event);
        let parent = self.clone();
        let receive_slot: Arc<Mutex<Option<Arc<dyn Fn(XEvent) + Send + Sync>>>> =
            Arc::new(Mutex::new(None));
        let receive = {
            let slot = receive_slot.clone();
            Arc::new(move |listener: Arc<dyn Fn(XEvent) + Send + Sync>| {
                *slot.lock() = Some(listener);
            })
        };
        let send_back = Arc::new(move |event: XEvent| {
            parent.send(event);
        });
        let cleanup = (src.start)(CallbackInput {
            runtime: input.runtime,
            restore: input.restore,
            receive,
            send_back,
        });
        let listener = receive_slot.lock().clone();
        let mut inner = self.inner.lock();
        inner.invoked.insert(
            invoke.id.clone(),
            InvokedActor {
                cleanup: Some(cleanup),
                receive: listener.unwrap_or_else(|| Arc::new(|_| {})),
            },
        );
    }

    fn exit_state(&self) {
        let mut inner = self.inner.lock();
        for (_, mut invoked) in inner.invoked.drain() {
            if let Some(cleanup) = invoked.cleanup.take() {
                cleanup();
            }
        }
    }

    pub fn send(self: &Arc<Self>, event: XEvent) {
        let transition = {
            let inner = self.inner.lock();
            self.machine
                .states
                .get(&inner.current)
                .and_then(|node| node.on.get(&event.type_))
                .or_else(|| self.machine.on.get(&event.type_))
                .map(|transition| Transition {
                    target: transition.target.clone(),
                    actions: transition
                        .actions
                        .iter()
                        .map(|action| match action {
                            Action::Custom(f) => Action::Custom(f.clone()),
                            Action::SendTo(name, f) => Action::SendTo(name.clone(), f.clone()),
                        })
                        .collect(),
                })
        };
        let Some(transition) = transition else {
            return;
        };
        for action in &transition.actions {
            match action {
                Action::Custom(apply) => {
                    let mut inner = self.inner.lock();
                    apply(&mut inner.context, &event);
                }
                Action::SendTo(name, build) => {
                    let forwarded = build(&event);
                    let receive = self
                        .inner
                        .lock()
                        .invoked
                        .get(name)
                        .map(|invoked| invoked.receive.clone());
                    if let Some(receive) = receive {
                        receive(forwarded);
                    }
                }
            }
        }
        if let Some(target) = transition.target {
            let changed = {
                let inner = self.inner.lock();
                inner.current != target
            };
            if changed {
                self.exit_state();
                self.enter_state(&target, &event);
            }
        }
        self.notify();
    }

    fn notify(&self) {
        let (context, current) = {
            let inner = self.inner.lock();
            (inner.context.clone(), inner.current.clone())
        };
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            listener(context.clone(), &current);
        }
    }

    pub fn subscribe(
        &self,
        listener: Arc<dyn Fn(Context, &str) + Send + Sync>,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let listeners = self.listeners.clone();
        self.listeners.lock().push(listener.clone());
        Arc::new(move || {
            listeners
                .lock()
                .retain(|current| !Arc::ptr_eq(current, &listener));
        })
    }

    pub fn snapshot(&self) -> (Context, String) {
        let inner = self.inner.lock();
        (inner.context.clone(), inner.current.clone())
    }

    pub fn stop(&self) {
        self.exit_state();
    }
}

pub fn create_actor(machine: Machine, input: Context) -> Arc<Actor> {
    Arc::new(Actor::new(machine, input))
}

static ROOT_ERROR_REPORTER: LazyLock<Mutex<Arc<dyn Fn(Value) + Send + Sync>>> =
    LazyLock::new(|| Mutex::new(Arc::new(|_| {})));

pub fn set_root_actor_error_reporter(reporter: Arc<dyn Fn(Value) + Send + Sync>) {
    *ROOT_ERROR_REPORTER.lock() = reporter;
}

pub fn report_root_actor_error(error: Value) {
    let reporter = ROOT_ERROR_REPORTER.lock().clone();
    reporter(error);
}

pub fn create_actor_with_inspect(
    machine: Machine,
    input: Context,
    inspect: Arc<dyn Fn(XEvent) + Send + Sync>,
) -> Arc<Actor> {
    let actor = create_actor(machine, input);
    actor.subscribe(Arc::new(move |_, _| {
        inspect(XEvent::new("xstate.snapshot", Value::Null));
    }));
    actor
}
