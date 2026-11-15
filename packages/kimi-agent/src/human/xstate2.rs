use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::wire::record::WireAsyncError;

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

pub struct ActionArgs<'a> {
    pub context: &'a Context,
    pub event: &'a XEvent,
    pub self_: &'a Arc<Actor>,
    pub spawn: &'a Spawner,
}

#[derive(Clone)]
pub enum SendTarget {
    Named(String),
    Ref(Arc<dyn Fn(&ActionArgs) -> Option<Arc<Actor>> + Send + Sync>),
    Actor(Arc<Actor>),
}

#[derive(Clone)]
pub enum Action {
    Custom(Arc<dyn Fn(&mut Context, &ActionArgs) + Send + Sync>),
    Assign(Arc<dyn Fn(&ActionArgs) -> Context + Send + Sync>),
    SendTo(
        SendTarget,
        Arc<dyn Fn(&ActionArgs) -> XEvent + Send + Sync>,
    ),
    StopChild(Arc<dyn Fn(&ActionArgs) -> Option<String> + Send + Sync>),
    Emit(Arc<dyn Fn(&ActionArgs) -> XEvent + Send + Sync>),
    Enqueue(Arc<dyn Fn(&ActionArgs, &Enqueuer) + Send + Sync>),
    Raise(Arc<dyn Fn(&ActionArgs) -> XEvent + Send + Sync>),
}

#[derive(Default)]
pub struct Enqueuer {
    queued: Mutex<Vec<Action>>,
}

impl Enqueuer {
    fn push(&self, action: Action) {
        self.queued.lock().push(action);
    }

    pub fn assign(&self, next: Context) {
        self.push(Action::Assign(Arc::new(move |_| next.clone())));
    }

    pub fn emit(&self, event: XEvent) {
        self.push(Action::Emit(Arc::new(move |_| event.clone())));
    }

    pub fn send_to(&self, actor: &str, event: XEvent) {
        self.push(Action::SendTo(
            SendTarget::Named(actor.to_string()),
            Arc::new(move |_| event.clone()),
        ));
    }

    pub fn send_to_child(&self, child: Arc<Actor>, event: XEvent) {
        self.push(Action::SendTo(
            SendTarget::Actor(child),
            Arc::new(move |_| event.clone()),
        ));
    }

    pub fn stop_child(&self, id: String) {
        self.push(Action::StopChild(Arc::new(move |_| Some(id.clone()))));
    }

    fn drain(&self) -> Vec<Action> {
        std::mem::take(&mut *self.queued.lock())
    }
}

pub type Guard = Arc<dyn Fn(&Context, &XEvent) -> bool + Send + Sync>;

#[derive(Clone, Default)]
pub struct Transition {
    pub target: Option<String>,
    pub guard: Option<Guard>,
    pub actions: Vec<Action>,
}

#[derive(Clone)]
pub struct InvokeDef {
    pub id: String,
    pub src: String,
    pub input: Arc<dyn Fn(&ActionArgs) -> InvokeInput + Send + Sync>,
    pub on_done: Vec<Transition>,
    pub on_error: Vec<Transition>,
}

pub struct InvokeInput {
    pub runtime: Context,
    pub restore: Option<AgentActorRestore>,
}

#[derive(Clone, Default)]
pub struct StateNode {
    pub on: HashMap<String, Vec<Transition>>,
    pub always: Vec<Transition>,
    pub after: Vec<(String, Transition)>,
    pub entry: Vec<Action>,
    pub exit: Vec<Action>,
    pub invoke: Option<InvokeDef>,
    pub initial: Option<String>,
    pub states: HashMap<String, StateNode>,
    pub final_: bool,
}

#[derive(Clone)]
pub struct PromiseInput {
    pub input: Context,
    pub signal: CancellationToken,
}

#[derive(Clone)]
pub struct FromCallback {
    pub start: Arc<dyn Fn(CallbackInput) -> Box<dyn FnOnce() + Send> + Send + Sync>,
}

#[derive(Clone)]
pub struct FromPromise {
    pub run: Arc<
        dyn Fn(PromiseInput) -> BoxFuture<'static, Result<Context, WireAsyncError>> + Send + Sync,
    >,
}

#[derive(Clone)]
pub enum ActorLogic {
    Callback(FromCallback),
    Promise(FromPromise),
}

pub struct CallbackInput {
    pub runtime: Context,
    pub restore: Option<AgentActorRestore>,
    pub receive: Arc<dyn Fn(Arc<dyn Fn(XEvent) + Send + Sync>) + Send + Sync>,
    pub send_back: Arc<dyn Fn(XEvent) + Send + Sync>,
}

pub fn from_callback(
    start: impl Fn(CallbackInput) -> Box<dyn FnOnce() + Send> + Send + Sync + 'static,
) -> ActorLogic {
    ActorLogic::Callback(FromCallback {
        start: Arc::new(start),
    })
}

pub fn from_promise(
    run: impl Fn(PromiseInput) -> BoxFuture<'static, Result<Context, WireAsyncError>>
    + Send
    + Sync
    + 'static,
) -> ActorLogic {
    ActorLogic::Promise(FromPromise { run: Arc::new(run) })
}

pub fn custom(apply: impl Fn(&mut Context, &ActionArgs) + Send + Sync + 'static) -> Action {
    Action::Custom(Arc::new(apply))
}

pub fn assign(apply: impl Fn(&ActionArgs) -> Context + Send + Sync + 'static) -> Action {
    Action::Assign(Arc::new(apply))
}

pub fn send_to(actor: &str, event: impl Fn(&ActionArgs) -> XEvent + Send + Sync + 'static) -> Action {
    Action::SendTo(SendTarget::Named(actor.to_string()), Arc::new(event))
}

pub fn send_to_ref(
    resolve: impl Fn(&ActionArgs) -> Option<Arc<Actor>> + Send + Sync + 'static,
    event: impl Fn(&ActionArgs) -> XEvent + Send + Sync + 'static,
) -> Action {
    Action::SendTo(SendTarget::Ref(Arc::new(resolve)), Arc::new(event))
}

pub fn stop_child(
    resolve: impl Fn(&ActionArgs) -> Option<String> + Send + Sync + 'static,
) -> Action {
    Action::StopChild(Arc::new(resolve))
}

pub fn emit(build: impl Fn(&ActionArgs) -> XEvent + Send + Sync + 'static) -> Action {
    Action::Emit(Arc::new(build))
}

pub fn enqueue_actions(
    build: impl Fn(&ActionArgs, &Enqueuer) + Send + Sync + 'static,
) -> Action {
    Action::Enqueue(Arc::new(build))
}

pub fn raise(build: impl Fn(&ActionArgs) -> XEvent + Send + Sync + 'static) -> Action {
    Action::Raise(Arc::new(build))
}

#[derive(Clone)]
pub struct Machine {
    pub id: String,
    pub context: Arc<dyn Fn(Context) -> Context + Send + Sync>,
    pub initial: String,
    pub states: HashMap<String, StateNode>,
    pub on: HashMap<String, Vec<Transition>>,
    pub invoke: Option<InvokeDef>,
    pub actors: HashMap<String, ActorLogic>,
    pub delays: HashMap<String, Arc<dyn Fn(&Context) -> Duration + Send + Sync>>,
    pub output: Option<Arc<dyn Fn(&Context) -> Option<Context> + Send + Sync>>,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            id: String::new(),
            context: Arc::new(|input| input),
            initial: String::new(),
            states: HashMap::new(),
            on: HashMap::new(),
            invoke: None,
            actors: HashMap::new(),
            delays: HashMap::new(),
            output: None,
        }
    }
}

pub struct Spawner {
    parent: Arc<Actor>,
}

impl Spawner {
    pub fn spawn(&self, logic: &Machine, id: &str, input: Context) -> Arc<Actor> {
        self.parent.spawn_child(logic, id, input)
    }

    pub fn spawn_logic(&self, logic: &ActorLogic, id: &str, input: Context) -> Arc<Actor> {
        self.parent.spawn_logic_child(logic, id, input)
    }
}

struct InvokedActor {
    cleanup: Option<Box<dyn FnOnce() + Send>>,
    receive: Arc<dyn Fn(XEvent) + Send + Sync>,
    cancel: Option<CancellationToken>,
}

struct ActorInner {
    context: Context,
    path: Vec<String>,
    status: String,
    error: Option<Value>,
    output: Option<Context>,
    started: bool,
    invoked: HashMap<String, InvokedActor>,
    timers: HashMap<String, tokio::task::AbortHandle>,
    logic_receive: Option<Arc<dyn Fn(XEvent) + Send + Sync>>,
    logic_cleanup: Option<Box<dyn FnOnce() + Send>>,
    logic_cancel: Option<CancellationToken>,
}

#[derive(Default)]
struct Mailbox {
    queued: VecDeque<XEvent>,
    internal: VecDeque<XEvent>,
    running: bool,
}

#[derive(Clone)]
pub struct Snapshot {
    pub value: String,
    pub context: Context,
    pub status: String,
    pub error: Option<Value>,
    pub output: Option<Context>,
}

pub struct Subscription {
    unsubscribe: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Subscription {
    fn new(unsubscribe: impl FnOnce() + Send + 'static) -> Self {
        Self {
            unsubscribe: Mutex::new(Some(Box::new(unsubscribe))),
        }
    }

    pub fn unsubscribe(&self) {
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
    }
}

type SnapshotListener = Arc<dyn Fn(Snapshot) + Send + Sync>;
type EventListener = (u64, String, Arc<dyn Fn(&XEvent) + Send + Sync>);

#[derive(Clone)]
enum ActorSource {
    Machine(Machine),
    Logic(ActorLogic),
}

pub struct Actor {
    source: ActorSource,
    inner: Mutex<ActorInner>,
    parent: Mutex<Weak<Actor>>,
    children: Arc<Mutex<HashMap<String, Arc<Actor>>>>,
    mailbox: Arc<Mutex<Mailbox>>,
    listeners: Arc<Mutex<Vec<(u64, SnapshotListener)>>>,
    event_listeners: Arc<Mutex<Vec<EventListener>>>,
    next_listener_id: AtomicU64,
}

impl Actor {
    fn new(machine: Machine, input: Context) -> Self {
        let context = (machine.context)(input);
        let path = descend(&machine, vec![machine.initial.clone()]);
        Self {
            source: ActorSource::Machine(machine),
            inner: Mutex::new(ActorInner {
                context,
                path,
                status: "active".to_string(),
                error: None,
                output: None,
                started: false,
                invoked: HashMap::new(),
                timers: HashMap::new(),
                logic_receive: None,
                logic_cleanup: None,
                logic_cancel: None,
            }),
            parent: Mutex::new(Weak::new()),
            children: Arc::new(Mutex::new(HashMap::new())),
            mailbox: Arc::new(Mutex::new(Mailbox::default())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            event_listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(0),
        }
    }

    fn new_logic(logic: ActorLogic, input: Context) -> Self {
        Self {
            source: ActorSource::Logic(logic),
            inner: Mutex::new(ActorInner {
                context: input,
                path: Vec::new(),
                status: "active".to_string(),
                error: None,
                output: None,
                started: false,
                invoked: HashMap::new(),
                timers: HashMap::new(),
                logic_receive: None,
                logic_cleanup: None,
                logic_cancel: None,
            }),
            parent: Mutex::new(Weak::new()),
            children: Arc::new(Mutex::new(HashMap::new())),
            mailbox: Arc::new(Mutex::new(Mailbox::default())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            event_listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(0),
        }
    }

    fn machine(&self) -> &Machine {
        match &self.source {
            ActorSource::Machine(machine) => machine,
            ActorSource::Logic(_) => panic!("actor has no machine"),
        }
    }

    pub fn parent(&self) -> Option<Arc<Actor>> {
        self.parent.lock().upgrade()
    }

    pub fn forward_to_parent(&self, event: XEvent) {
        if let Some(parent) = self.parent() {
            parent.send(event);
        }
    }

    pub fn start(self: &Arc<Self>) {
        {
            let mut inner = self.inner.lock();
            if inner.started || inner.status != "active" {
                return;
            }
            inner.started = true;
        }
        if let ActorSource::Logic(logic) = self.source.clone() {
            self.start_logic(logic);
            return;
        }
        {
            let mut mailbox = self.mailbox.lock();
            if mailbox.running {
                return;
            }
            mailbox.running = true;
        }
        let path = self.inner.lock().path.clone();
        let init_event = XEvent::new("xstate.init", Value::Null);
        let enter_event = init_event.clone();
        let this = self.clone();
        let entered = match catch_unwind(AssertUnwindSafe(move || {
            this.start_root_invoke(&enter_event);
            this.enter_path(&path, 0, &enter_event);
        })) {
            Ok(()) => {
                self.run_always(&init_event);
                self.settle();
                true
            }
            Err(payload) => {
                self.fail(payload);
                false
            }
        };
        if entered {
            self.drain();
        } else {
            let mut mailbox = self.mailbox.lock();
            mailbox.queued.clear();
            mailbox.internal.clear();
            mailbox.running = false;
        }
    }

    pub fn send(self: &Arc<Self>, event: XEvent) {
        if let ActorSource::Logic(_) = self.source {
            let receive = {
                let inner = self.inner.lock();
                if inner.status != "active" {
                    return;
                }
                inner.logic_receive.clone()
            };
            if let Some(receive) = receive {
                receive(event);
            }
            return;
        }
        {
            let mut mailbox = self.mailbox.lock();
            mailbox.queued.push_back(event);
            if mailbox.running {
                return;
            }
            mailbox.running = true;
        }
        self.drain();
    }

    fn drain(self: &Arc<Self>) {
        loop {
            let next = {
                let mut mailbox = self.mailbox.lock();
                match mailbox.internal.pop_front().or_else(|| mailbox.queued.pop_front()) {
                    Some(event) => Some(event),
                    None => {
                        mailbox.running = false;
                        None
                    }
                }
            };
            let Some(event) = next else {
                return;
            };
            if self.get_snapshot().status != "active" {
                let mut mailbox = self.mailbox.lock();
                mailbox.queued.clear();
                mailbox.internal.clear();
                mailbox.running = false;
                return;
            }
            let this = self.clone();
            match catch_unwind(AssertUnwindSafe(move || this.apply_event(event))) {
                Ok(()) => self.settle(),
                Err(payload) => {
                    self.fail(payload);
                    let mut mailbox = self.mailbox.lock();
                    mailbox.queued.clear();
                    mailbox.internal.clear();
                    mailbox.running = false;
                    return;
                }
            }
        }
    }

    fn apply_event(self: &Arc<Self>, event: XEvent) {
        let (path, context) = {
            let inner = self.inner.lock();
            (inner.path.clone(), inner.context.clone())
        };
        let Some((transition, defined_at)) = self.select_transition(&path, &context, &event) else {
            return;
        };
        self.apply_transition(&transition, &defined_at, &event);
        self.run_always(&event);
    }

    fn select_transition(
        &self,
        path: &[String],
        context: &Context,
        event: &XEvent,
    ) -> Option<(Transition, Vec<String>)> {
        let candidates = event_type_candidates(&event.type_);
        for depth in (0..=path.len()).rev() {
            let node_path = &path[..depth];
            let node = match depth {
                0 => None,
                _ => self.node(node_path),
            };
            if let Some(invoke) = root_or_node_invoke(self.machine(), node)
                && let Some(transition) = invoke_transition(invoke, context, event)
            {
                return Some((transition, node_path.to_vec()));
            }
            if let Some(node) = node
                && let Some(transition) = after_transition(node, node_path, context, event)
            {
                return Some((transition, node_path.to_vec()));
            }
            let on = match depth {
                0 => Some(&self.machine().on),
                _ => node.map(|node| &node.on),
            };
            if let Some(on) = on
                && let Some(transition) = first_matching(on, &candidates, context, event)
            {
                return Some((transition, node_path.to_vec()));
            }
        }
        None
    }

    fn run_always(self: &Arc<Self>, event: &XEvent) {
        loop {
            let (path, context) = {
                let inner = self.inner.lock();
                (inner.path.clone(), inner.context.clone())
            };
            let Some((transition, defined_at)) = self.select_always(&path, &context, event) else {
                return;
            };
            self.apply_transition(&transition, &defined_at, event);
        }
    }

    fn select_always(
        &self,
        path: &[String],
        context: &Context,
        event: &XEvent,
    ) -> Option<(Transition, Vec<String>)> {
        for depth in (1..=path.len()).rev() {
            let node_path = &path[..depth];
            let Some(node) = self.node(node_path) else {
                continue;
            };
            if let Some(transition) = first_passing(&node.always, context, event) {
                return Some((transition, node_path.to_vec()));
            }
        }
        None
    }

    fn apply_transition(
        self: &Arc<Self>,
        transition: &Transition,
        defined_at: &[String],
        event: &XEvent,
    ) {
        let current = self.inner.lock().path.clone();
        let Some(target) = transition.target.as_deref() else {
            self.execute_actions(&transition.actions, event);
            return;
        };
        let next = descend(self.machine(), self.resolve_target(target, defined_at));
        let same = next == current;
        let lca = if same {
            0
        } else {
            common_prefix(&current, &next)
        };
        self.exit_path(lca, event);
        self.execute_actions(&transition.actions, event);
        if same {
            self.enter_path(&next, 0, event);
            return;
        }
        self.inner.lock().path = next[..lca].to_vec();
        self.enter_path(&next, lca, event);
    }

    fn resolve_target(&self, target: &str, defined_at: &[String]) -> Vec<String> {
        if let Some(rest) = target.strip_prefix('#') {
            let mut parts = rest.split('.').map(str::to_string).collect::<Vec<_>>();
            if parts
                .first()
                .is_some_and(|id| id == &self.machine().id)
            {
                parts.remove(0);
            }
            return parts;
        }
        if let Some(name) = target.strip_prefix('.') {
            let mut path = defined_at.to_vec();
            path.push(name.to_string());
            return path;
        }
        let mut parent = defined_at.to_vec();
        if !parent.is_empty() {
            parent.pop();
        }
        loop {
            let mut candidate = parent.clone();
            candidate.push(target.to_string());
            if self.node(&candidate).is_some() {
                return candidate;
            }
            if parent.is_empty() {
                return vec![target.to_string()];
            }
            parent.pop();
        }
    }

    fn enter_path(self: &Arc<Self>, path: &[String], from: usize, event: &XEvent) {
        for depth in from..path.len() {
            let prefix = &path[..=depth];
            self.inner.lock().path = prefix.to_vec();
            let Some(node) = self.node(prefix).cloned() else {
                continue;
            };
            self.execute_actions(&node.entry, event);
            if let Some(invoke) = &node.invoke {
                self.start_invoke(invoke, event);
            }
            self.schedule_after(prefix, &node);
            if node.final_ {
                self.mark_done();
            }
        }
    }

    fn exit_path(self: &Arc<Self>, down_to: usize, event: &XEvent) {
        let path = self.inner.lock().path.clone();
        for depth in (down_to..path.len()).rev() {
            let prefix = &path[..=depth];
            let Some(node) = self.node(prefix).cloned() else {
                continue;
            };
            self.execute_actions(&node.exit, event);
            if let Some(invoke) = &node.invoke {
                self.stop_invoke(&invoke.id);
            }
            self.cancel_after(prefix);
        }
    }

    fn execute_actions(self: &Arc<Self>, actions: &[Action], event: &XEvent) {
        for action in actions {
            let context = self.inner.lock().context.clone();
            let spawner = Spawner {
                parent: self.clone(),
            };
            let args = ActionArgs {
                context: &context,
                event,
                self_: self,
                spawn: &spawner,
            };
            match action {
                Action::Custom(apply) => {
                    let mut inner = self.inner.lock();
                    apply(&mut inner.context, &args);
                }
                Action::Assign(apply) => {
                    let next = apply(&args);
                    self.inner.lock().context = next;
                }
                Action::SendTo(target, build) => {
                    let forwarded = build(&args);
                    match target {
                        SendTarget::Named(name) => {
                            let child = self.children.lock().get(name).cloned();
                            if let Some(child) = child {
                                child.send(forwarded);
                                continue;
                            }
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
                        SendTarget::Ref(resolve) => {
                            if let Some(child) = resolve(&args) {
                                child.send(forwarded);
                            }
                        }
                        SendTarget::Actor(child) => {
                            child.send(forwarded);
                        }
                    }
                }
                Action::StopChild(resolve) => {
                    if let Some(id) = resolve(&args) {
                        self.remove_child(&id);
                    }
                }
                Action::Emit(build) => {
                    let emitted = build(&args);
                    self.emit_event(&emitted);
                }
                Action::Enqueue(build) => {
                    let enqueuer = Enqueuer::default();
                    build(&args, &enqueuer);
                    self.execute_actions(&enqueuer.drain(), event);
                }
                Action::Raise(build) => {
                    let raised = build(&args);
                    self.mailbox.lock().internal.push_back(raised);
                }
            }
        }
    }

    fn start_root_invoke(self: &Arc<Self>, event: &XEvent) {
        let Some(invoke) = self.machine().invoke.clone() else {
            return;
        };
        self.start_invoke(&invoke, event);
    }

    fn start_invoke(self: &Arc<Self>, invoke: &InvokeDef, event: &XEvent) {
        let Some(logic) = self.machine().actors.get(&invoke.src).cloned() else {
            return;
        };
        let context = self.inner.lock().context.clone();
        let spawner = Spawner {
            parent: self.clone(),
        };
        let args = ActionArgs {
            context: &context,
            event,
            self_: self,
            spawn: &spawner,
        };
        let input = (invoke.input)(&args);
        match logic {
            ActorLogic::Callback(src) => {
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
                self.inner.lock().invoked.insert(
                    invoke.id.clone(),
                    InvokedActor {
                        cleanup: Some(cleanup),
                        receive: listener.unwrap_or_else(|| Arc::new(|_| {})),
                        cancel: None,
                    },
                );
            }
            ActorLogic::Promise(src) => {
                let token = CancellationToken::new();
                let parent = self.clone();
                let id = invoke.id.clone();
                let run = src.run.clone();
                let runtime = input.runtime;
                let signal = token.clone();
                tokio::spawn(async move {
                    let result = run(PromiseInput {
                        input: runtime,
                        signal,
                    })
                    .await;
                    let event = match result {
                        Ok(output) => XEvent {
                            type_: format!("xstate.done.actor.{id}"),
                            payload: Value::Null,
                            restore: None,
                            data: Some(output),
                        },
                        Err(error) => XEvent::new(
                            format!("xstate.error.actor.{id}"),
                            Value::String(error.to_string()),
                        ),
                    };
                    parent.send(event);
                });
                self.inner.lock().invoked.insert(
                    invoke.id.clone(),
                    InvokedActor {
                        cleanup: None,
                        receive: Arc::new(|_| {}),
                        cancel: Some(token),
                    },
                );
            }
        }
    }

    fn stop_invoke(&self, id: &str) {
        let invoked = self.inner.lock().invoked.remove(id);
        if let Some(mut invoked) = invoked {
            if let Some(cancel) = invoked.cancel.take() {
                cancel.cancel();
            }
            if let Some(cleanup) = invoked.cleanup.take() {
                cleanup();
            }
        }
    }

    fn shutdown_invoked(&self) {
        let invoked = std::mem::take(&mut self.inner.lock().invoked);
        for (_, mut invoked) in invoked {
            if let Some(cancel) = invoked.cancel.take() {
                cancel.cancel();
            }
            if let Some(cleanup) = invoked.cleanup.take() {
                cleanup();
            }
        }
        let timers = std::mem::take(&mut self.inner.lock().timers);
        for (_, handle) in timers {
            handle.abort();
        }
    }

    fn schedule_after(self: &Arc<Self>, node_path: &[String], node: &StateNode) {
        for (delay, _) in &node.after {
            let Some(resolve) = self.machine().delays.get(delay).cloned() else {
                continue;
            };
            let duration = {
                let context = self.inner.lock().context.clone();
                resolve(&context)
            };
            let key = format!("{delay}#{}", node_path.join("."));
            let actor = self.clone();
            let event = XEvent::new(format!("xstate.after.{key}"), Value::Null);
            let handle = tokio::spawn(async move {
                tokio::time::sleep(duration).await;
                actor.send(event);
            })
            .abort_handle();
            self.inner.lock().timers.insert(key, handle);
        }
    }

    fn cancel_after(&self, node_path: &[String]) {
        let suffix = format!("#{}", node_path.join("."));
        let mut inner = self.inner.lock();
        inner.timers.retain(|key, handle| {
            if key.ends_with(&suffix) {
                handle.abort();
                false
            } else {
                true
            }
        });
    }

    fn node(&self, path: &[String]) -> Option<&StateNode> {
        node_in(&self.machine().states, path)
    }

    fn mark_done(&self) {
        let context = self.inner.lock().context.clone();
        let output = self.machine().output.as_ref().and_then(|output| output(&context));
        let mut inner = self.inner.lock();
        if inner.status != "active" {
            return;
        }
        inner.status = "done".to_string();
        inner.output = output;
    }

    fn spawn_child(self: &Arc<Self>, logic: &Machine, id: &str, input: Context) -> Arc<Actor> {
        self.register_child(id, Arc::new(Actor::new(logic.clone(), input)))
    }

    fn spawn_logic_child(
        self: &Arc<Self>,
        logic: &ActorLogic,
        id: &str,
        input: Context,
    ) -> Arc<Actor> {
        self.register_child(id, Arc::new(Actor::new_logic(logic.clone(), input)))
    }

    fn register_child(self: &Arc<Self>, id: &str, child: Arc<Actor>) -> Arc<Actor> {
        *child.parent.lock() = Arc::downgrade(self);
        self.children.lock().insert(id.to_string(), child.clone());
        let parent = Arc::downgrade(self);
        let child_id = id.to_string();
        let _subscription = child.subscribe(Arc::new(move |snapshot| {
            let type_ = match snapshot.status.as_str() {
                "done" => format!("xstate.done.actor.{child_id}"),
                "error" => format!("xstate.error.actor.{child_id}"),
                _ => return,
            };
            let Some(parent) = parent.upgrade() else {
                return;
            };
            let data = snapshot
                .output
                .clone()
                .or_else(|| snapshot.error.clone().map(|error| Arc::new(error) as Context));
            parent.send(XEvent {
                type_,
                payload: snapshot.error.clone().unwrap_or(Value::Null),
                restore: None,
                data,
            });
        }));
        child.start();
        child
    }

    fn start_logic(self: &Arc<Self>, logic: ActorLogic) {
        match logic {
            ActorLogic::Callback(src) => {
                let actor = self.clone();
                let receive_slot: Arc<Mutex<Option<Arc<dyn Fn(XEvent) + Send + Sync>>>> =
                    Arc::new(Mutex::new(None));
                let receive = {
                    let slot = receive_slot.clone();
                    Arc::new(move |listener: Arc<dyn Fn(XEvent) + Send + Sync>| {
                        *slot.lock() = Some(listener);
                    })
                };
                let send_back = Arc::new(move |event: XEvent| {
                    actor.forward_to_parent(event);
                });
                let runtime = self.inner.lock().context.clone();
                let cleanup = (src.start)(CallbackInput {
                    runtime,
                    restore: None,
                    receive,
                    send_back,
                });
                let listener = receive_slot.lock().clone();
                let mut inner = self.inner.lock();
                inner.logic_receive = listener;
                inner.logic_cleanup = Some(cleanup);
            }
            ActorLogic::Promise(src) => {
                let token = CancellationToken::new();
                let actor = self.clone();
                let run = src.run.clone();
                let runtime = self.inner.lock().context.clone();
                let signal = token.clone();
                tokio::spawn(async move {
                    let result = run(PromiseInput {
                        input: runtime,
                        signal,
                    })
                    .await;
                    {
                        let mut inner = actor.inner.lock();
                        if inner.status != "active" {
                            return;
                        }
                        match result {
                            Ok(output) => {
                                inner.status = "done".to_string();
                                inner.output = Some(output);
                            }
                            Err(error) => {
                                inner.status = "error".to_string();
                                inner.error = Some(Value::String(error.to_string()));
                            }
                        }
                    }
                    actor.notify();
                });
                self.inner.lock().logic_cancel = Some(token);
            }
        }
    }

    fn remove_child(&self, id: &str) {
        let child = self.children.lock().remove(id);
        let cleanup = {
            let mut inner = self.inner.lock();
            inner
                .invoked
                .remove(id)
                .and_then(|mut invoked| invoked.cleanup.take())
        };
        if let Some(cleanup) = cleanup {
            cleanup();
        }
        if let Some(child) = child {
            child.stop();
        }
    }

    fn stop_children(&self) {
        let children = std::mem::take(&mut *self.children.lock());
        for (_, child) in children {
            child.stop();
        }
    }

    fn settle(&self) {
        self.notify();
        if self.get_snapshot().status == "done" {
            self.shutdown_invoked();
            self.stop_children();
            self.clear_listeners();
        }
    }

    fn fail(&self, payload: Box<dyn Any + Send>) {
        let error = match payload.downcast::<String>() {
            Ok(message) => Value::String(*message),
            Err(payload) => match payload.downcast::<&'static str>() {
                Ok(message) => Value::String((*message).to_string()),
                Err(_) => Value::String("actor transition panicked".to_string()),
            },
        };
        {
            let mut inner = self.inner.lock();
            inner.status = "error".to_string();
            inner.error = Some(error.clone());
        }
        self.notify();
        self.clear_listeners();
        report_root_actor_error(error);
    }

    fn notify(&self) {
        let snapshot = self.get_snapshot();
        let listeners = self.listeners.lock().clone();
        for (_, listener) in listeners {
            listener(snapshot.clone());
        }
    }

    fn emit_event(&self, event: &XEvent) {
        let listeners = self.event_listeners.lock().clone();
        for (_, type_, handler) in listeners.iter() {
            if type_ == &event.type_ {
                handler(event);
            }
        }
        for (_, type_, handler) in listeners.iter() {
            if type_ == "*" {
                handler(event);
            }
        }
    }

    fn clear_listeners(&self) {
        self.listeners.lock().clear();
        self.event_listeners.lock().clear();
    }

    pub fn subscribe(&self, listener: SnapshotListener) -> Subscription {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let listeners = self.listeners.clone();
        Subscription::new(move || {
            listeners.lock().retain(|(candidate, _)| *candidate != id);
        })
    }

    pub fn on(&self, type_: &str, handler: Arc<dyn Fn(&XEvent) + Send + Sync>) -> Subscription {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.event_listeners
            .lock()
            .push((id, type_.to_string(), handler));
        let listeners = self.event_listeners.clone();
        Subscription::new(move || {
            listeners
                .lock()
                .retain(|(candidate, _, _)| *candidate != id);
        })
    }

    pub fn get_snapshot(&self) -> Snapshot {
        let inner = self.inner.lock();
        Snapshot {
            value: inner.path.join("."),
            context: inner.context.clone(),
            status: inner.status.clone(),
            error: inner.error.clone(),
            output: inner.output.clone(),
        }
    }

    pub fn stop(&self) {
        {
            let mut inner = self.inner.lock();
            if inner.status != "active" {
                return;
            }
            inner.status = "stopped".to_string();
        }
        let (cleanup, cancel) = {
            let mut inner = self.inner.lock();
            (inner.logic_cleanup.take(), inner.logic_cancel.take())
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        if let Some(cleanup) = cleanup {
            cleanup();
        }
        self.shutdown_invoked();
        self.stop_children();
        self.notify();
        self.clear_listeners();
    }
}

fn node_in<'a>(states: &'a HashMap<String, StateNode>, path: &[String]) -> Option<&'a StateNode> {
    let (first, rest) = path.split_first()?;
    let node = states.get(first)?;
    if rest.is_empty() {
        return Some(node);
    }
    node_in(&node.states, rest)
}

fn descend(machine: &Machine, mut path: Vec<String>) -> Vec<String> {
    loop {
        let Some(node) = node_in(&machine.states, &path) else {
            return path;
        };
        if node.states.is_empty() {
            return path;
        }
        let Some(initial) = node.initial.clone() else {
            return path;
        };
        path.push(initial);
    }
}

fn common_prefix(left: &[String], right: &[String]) -> usize {
    let mut length = 0;
    while length < left.len() && length < right.len() && left[length] == right[length] {
        length += 1;
    }
    length
}

fn root_or_node_invoke<'a>(machine: &'a Machine, node: Option<&'a StateNode>) -> Option<&'a InvokeDef> {
    match node {
        Some(node) => node.invoke.as_ref(),
        None => machine.invoke.as_ref(),
    }
}

fn invoke_transition(invoke: &InvokeDef, context: &Context, event: &XEvent) -> Option<Transition> {
    if event.type_ == format!("xstate.done.actor.{}", invoke.id) {
        return first_passing(&invoke.on_done, context, event);
    }
    if event.type_ == format!("xstate.error.actor.{}", invoke.id) {
        return first_passing(&invoke.on_error, context, event);
    }
    None
}

fn after_transition(
    node: &StateNode,
    node_path: &[String],
    context: &Context,
    event: &XEvent,
) -> Option<Transition> {
    let rest = event.type_.strip_prefix("xstate.after.")?;
    for (delay, transition) in &node.after {
        if rest != format!("{delay}#{}", node_path.join(".")) {
            continue;
        }
        let passes = transition
            .guard
            .as_ref()
            .is_none_or(|guard| guard(context, event));
        if passes {
            return Some(transition.clone());
        }
    }
    None
}

fn event_type_candidates(type_: &str) -> Vec<String> {
    let mut candidates = vec![type_.to_string()];
    let mut current = type_;
    while let Some(index) = current.rfind('.') {
        current = &current[..index];
        candidates.push(format!("{current}.*"));
    }
    candidates.push("*".to_string());
    candidates
}

fn first_matching(
    on: &HashMap<String, Vec<Transition>>,
    candidates: &[String],
    context: &Context,
    event: &XEvent,
) -> Option<Transition> {
    for candidate in candidates {
        let Some(transitions) = on.get(candidate) else {
            continue;
        };
        if let Some(transition) = first_passing(transitions, context, event) {
            return Some(transition);
        }
    }
    None
}

fn first_passing(
    transitions: &[Transition],
    context: &Context,
    event: &XEvent,
) -> Option<Transition> {
    for transition in transitions {
        let passes = transition
            .guard
            .as_ref()
            .is_none_or(|guard| guard(context, event));
        if passes {
            return Some(transition.clone());
        }
    }
    None
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
    actor.subscribe(Arc::new(move |_| {
        inspect(XEvent::new("xstate.snapshot", Value::Null));
    }));
    actor
}

#[derive(Default)]
pub struct WaitForOptions {
    pub timeout: Option<Duration>,
}

pub async fn wait_for(
    actor: &Actor,
    predicate: impl Fn(&Snapshot) -> bool + Send + Sync,
) -> Result<Snapshot, WireAsyncError> {
    wait_for_with_options(actor, predicate, WaitForOptions::default()).await
}

pub async fn wait_for_with_options(
    actor: &Actor,
    predicate: impl Fn(&Snapshot) -> bool + Send + Sync,
    options: WaitForOptions,
) -> Result<Snapshot, WireAsyncError> {
    let current = actor.get_snapshot();
    if predicate(&current) {
        return Ok(current);
    }
    if let Some(result) = terminal_snapshot(&current) {
        return result;
    }
    let notify = Arc::new(Notify::new());
    let latest = Arc::new(Mutex::new(None::<Snapshot>));
    let listener = {
        let notify = notify.clone();
        let latest = latest.clone();
        Arc::new(move |snapshot: Snapshot| {
            *latest.lock() = Some(snapshot);
            notify.notify_one();
        })
    };
    let subscription = actor.subscribe(listener);
    let outcome = wait_for_snapshots(actor, &predicate, &notify, &latest, options.timeout).await;
    subscription.unsubscribe();
    outcome
}

async fn wait_for_snapshots(
    actor: &Actor,
    predicate: &(impl Fn(&Snapshot) -> bool + Send + Sync),
    notify: &Notify,
    latest: &Mutex<Option<Snapshot>>,
    timeout: Option<Duration>,
) -> Result<Snapshot, WireAsyncError> {
    let deadline = timeout.map(|limit| (tokio::time::Instant::now() + limit, limit));
    let mut snapshot = actor.get_snapshot();
    loop {
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if let Some(result) = terminal_snapshot(&snapshot) {
            return result;
        }
        let notified = notify.notified();
        if let Some(next) = latest.lock().take() {
            snapshot = next;
            continue;
        }
        match deadline {
            Some((at, limit)) => {
                if tokio::time::timeout_at(at, notified).await.is_err() {
                    return Err(format!("Timeout of {} ms exceeded", limit.as_millis()).into());
                }
            }
            None => notified.await,
        }
        snapshot = latest.lock().take().unwrap_or_else(|| actor.get_snapshot());
    }
}

fn terminal_snapshot(snapshot: &Snapshot) -> Option<Result<Snapshot, WireAsyncError>> {
    match snapshot.status.as_str() {
        "done" | "stopped" => Some(Err("Actor terminated without satisfying predicate".into())),
        "error" => Some(Err(snapshot_error(snapshot))),
        _ => None,
    }
}

fn snapshot_error(snapshot: &Snapshot) -> WireAsyncError {
    match snapshot.error.as_ref() {
        Some(Value::String(message)) => message.clone().into(),
        Some(error) => error.to_string().into(),
        None => "Actor errored".into(),
    }
}
