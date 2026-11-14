use std::any::Any;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::agent::actor_service::agent_actor_runtime::{
    ActorAttachment, ActorStateEmitter, AgentActorHost, AgentActorRestore,
    DurableAgentRuntimeParticipant,
};
use crate::agent::agent_context::AgentContext;
use crate::agent::state::agent_state_service::Listeners;
use crate::human::xstate2::{Actor, Context, Machine, XEvent, create_actor};
use crate::rpc::types::BoxFuture;

pub type StateFold<State> = Arc<dyn Fn(State, XEvent) -> State + Send + Sync>;

#[derive(Clone)]
pub struct AgentActorDurable<State: PartialEq + Clone + Send + Sync + 'static> {
    pub events: Vec<String>,
    pub undoable: bool,
    pub transition: StateFold<State>,
    pub read: Arc<dyn Fn(&Context) -> State + Send + Sync>,
    pub commit: Arc<dyn Fn(&Arc<Actor>, State, bool) + Send + Sync>,
}

pub struct AgentActorOptions<State: PartialEq + Clone + Send + Sync + 'static> {
    pub id: String,
    pub input: Option<Context>,
    pub durable: Option<AgentActorDurable<State>>,
}

#[derive(Clone)]
pub struct AgentActorContext<State: PartialEq + Clone + Send + Sync + 'static> {
    pub agent: AgentContext,
    pub get: Arc<dyn Fn(&str) -> Arc<dyn Any + Send + Sync> + Send + Sync>,
    pub get_state: Arc<dyn Fn() -> State + Send + Sync>,
    pub get_logic_state: Arc<dyn Fn() -> Context + Send + Sync>,
    pub dispatch: Arc<dyn Fn(Value) -> BoxFuture<'static, Result<(), String>> + Send + Sync>,
    pub send: Arc<dyn Fn(XEvent) + Send + Sync>,
    pub on_did_change: Arc<Listeners<State>>,
}

pub struct ServiceHolder<T: ?Sized + Send + Sync + 'static>(pub Arc<T>);

impl<State: PartialEq + Clone + Send + Sync + 'static> AgentActorContext<State> {
    pub fn service<T: ?Sized + Send + Sync + 'static>(&self, name: &str) -> Arc<T> {
        match (self.get)(name).downcast::<ServiceHolder<T>>() {
            Ok(service) => service.0.clone(),
            Err(_) => panic!("service not registered: {name}"),
        }
    }
}

struct ActorParticipant<State: PartialEq + Clone + Send + Sync + 'static> {
    id: String,
    events: Vec<String>,
    undoable: bool,
    transition: StateFold<State>,
    read: Arc<dyn Fn(&Context) -> State + Send + Sync>,
    commit: Arc<dyn Fn(&Arc<Actor>, State, bool) + Send + Sync>,
    actor: Arc<Actor>,
}

impl<State: PartialEq + Clone + Send + Sync + 'static> ActorParticipant<State> {
    fn get_state(&self) -> State {
        (self.read)(&self.actor.snapshot().0)
    }

    fn commit_state(&self, state: State, silent: bool) {
        (self.commit)(&self.actor, state, silent);
    }

    fn transition(&self) -> &StateFold<State> {
        &self.transition
    }
}

impl<State: PartialEq + Clone + Send + Sync + 'static> DurableAgentRuntimeParticipant
    for ActorParticipant<State>
{
    fn id(&self) -> &str {
        &self.id
    }

    fn events(&self) -> Vec<String> {
        self.events.clone()
    }

    fn undoable(&self) -> bool {
        self.undoable
    }
}

type ActorSlot = Arc<Mutex<Option<Arc<Actor>>>>;

pub struct AgentActorService {
    dispatcher: Arc<dyn AgentActorHost>,
    pub(crate) agent: AgentContext,
    get: Arc<dyn Fn(&str) -> Arc<dyn Any + Send + Sync> + Send + Sync>,
    pub(crate) dispatch: Arc<dyn Fn(Value) -> BoxFuture<'static, Result<(), String>> + Send + Sync>,
    disposables: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl AgentActorService {
    pub fn new(
        dispatcher: Arc<dyn AgentActorHost>,
        agent: AgentContext,
        get: Arc<dyn Fn(&str) -> Arc<dyn Any + Send + Sync> + Send + Sync>,
        dispatch: Arc<dyn Fn(Value) -> BoxFuture<'static, Result<(), String>> + Send + Sync>,
    ) -> Self {
        Self {
            dispatcher,
            agent,
            get,
            dispatch,
            disposables: Mutex::new(Vec::new()),
        }
    }

    pub fn register_disposable(&self, dispose: impl FnOnce() + Send + 'static) {
        self.disposables.lock().push(Box::new(dispose));
    }

    pub fn dispose(&self) {
        let disposables = std::mem::take(&mut *self.disposables.lock());
        for dispose in disposables.into_iter().rev() {
            dispose();
        }
    }

    fn build_context<State: PartialEq + Clone + Send + Sync + 'static>(
        &self,
        slot: &ActorSlot,
        emitter: &ActorStateEmitter<State>,
        durable: Option<&AgentActorDurable<State>>,
        id: &str,
    ) -> AgentActorContext<State> {
        let get_state = {
            let slot = slot.clone();
            let read = durable.map(|durable| durable.read.clone());
            let id = id.to_string();
            Arc::new(move || {
                let read = read
                    .as_ref()
                    .unwrap_or_else(|| panic!("Agent actor '{id}' has no durable state"));
                let actor = slot.lock().as_ref().expect("actor not started").clone();
                read(&actor.snapshot().0)
            })
        };
        let get_logic_state = {
            let slot = slot.clone();
            Arc::new(move || {
                slot.lock()
                    .as_ref()
                    .expect("actor not started")
                    .snapshot()
                    .0
            })
        };
        let send = {
            let slot = slot.clone();
            Arc::new(move |event: XEvent| {
                let actor = slot.lock().as_ref().expect("actor not started").clone();
                actor.send(event);
            })
        };
        AgentActorContext {
            agent: self.agent.clone(),
            get: self.get.clone(),
            get_state,
            get_logic_state,
            dispatch: self.dispatch.clone(),
            send,
            on_did_change: emitter.on_did_change(),
        }
    }

    pub fn attach_actor<State: PartialEq + Clone + Send + Sync + 'static>(
        &self,
        logic: Machine,
        options: AgentActorOptions<State>,
    ) -> AgentActorContext<State> {
        let durable = options.durable;
        let emitter = Arc::new(ActorStateEmitter::<State>::new());
        let slot: ActorSlot = Arc::new(Mutex::new(None));
        let context = self.build_context(&slot, &emitter, durable.as_ref(), &options.id);
        let input = options
            .input
            .unwrap_or_else(|| Arc::new(context.clone()) as Context);
        let actor = create_actor(logic, input);
        *slot.lock() = Some(actor.clone());

        let unsubscribe = durable.as_ref().map(|durable| {
            let read = durable.read.clone();
            let emitter = emitter.clone();
            actor.subscribe(Arc::new(move |context, _| {
                let next = read(&context);
                let _ = emitter.observe(next);
            }))
        });

        actor.start();

        let participant = durable.map(|durable| {
            Arc::new(ActorParticipant {
                id: options.id.clone(),
                events: durable.events.clone(),
                undoable: durable.undoable,
                transition: durable.transition.clone(),
                read: durable.read.clone(),
                commit: durable.commit.clone(),
                actor: actor.clone(),
            }) as Arc<dyn DurableAgentRuntimeParticipant>
        });

        let restore_actor = actor.clone();
        let restore_hook = Arc::new(move |event: AgentActorRestore| {
            restore_actor.send(XEvent::restore(event));
            Box::pin(async move {}) as BoxFuture<'static, ()>
        });

        let attachment = ActorAttachment::attach(
            &options.id,
            participant,
            self.dispatcher.clone(),
            restore_hook,
            Arc::new(|error| tracing::warn!(%error, "agent actor restore failed")),
        );

        self.register_disposable(move || {
            attachment.dispose();
            if let Some(unsubscribe) = unsubscribe {
                unsubscribe();
            }
            actor.stop();
        });

        context
    }
}
