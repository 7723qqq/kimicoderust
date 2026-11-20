use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::agent::actor_service::agent_actor_runtime::AgentActorHost;
use crate::agent::actor_service::agent_actor_service::{
    AgentActorContext, AgentActorDurable, AgentActorOptions, AgentActorService, StateFold,
};
use crate::agent::actor_service::services::{
    AgentLoopService, EventBusService, TelemetryService,
};
use crate::agent::agent_context::AgentContext;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::tool_policy::tool_policy::IAgentToolPolicyService;
use crate::tool::tool_contract::ToolSource;
use crate::callbacks::MAIN_AGENT_ID;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{ContextInjectionContent, ContextInjectionResult};
use crate::features::todo::todoItem::{
    TODO_LIST_TOOL_NAME, TodoItem, read_todo_items, todo_progress,
};
use crate::features::todo::todoListReminder::{
    TODO_LIST_REMINDER_VARIANT, todo_list_stale_reminder,
};
use crate::features::todo::todoOps::ToolsUpdateStore;
use crate::human::xstate2::{
    Action, ActorLogic, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent,
    from_callback,
};

pub type TodoState = Vec<TodoItem>;

#[derive(Clone)]
pub struct TodoActorContext {
    pub todos: TodoState,
    pub runtime: crate::human::xstate2::Context,
}

pub fn todo_actor_logic() -> Machine {
    let states = std::collections::HashMap::from([
        (
            "beforeRestore".to_string(),
            StateNode {
                final_: false,
                on: std::collections::HashMap::from([(
                    "runtime.restore".to_string(),
                    vec![Transition {
                        target: Some("active".to_string()),
                        actions: Vec::new(),
                        ..Default::default()
                    }],
                )]),
                invoke: None,
                ..Default::default()
            },
        ),
        (
            "active".to_string(),
            StateNode {
                final_: false,
                on: std::collections::HashMap::new(),
                invoke: Some(InvokeDef {
                    id: "todoReminder".to_string(),
                    src: "todoReminder".to_string(),
                    input: Arc::new(|args| {
                        let context = args
                            .context
                            .downcast_ref::<Mutex<TodoActorContext>>()
                            .unwrap()
                            .lock();
                        InvokeInput {
                            runtime: context.runtime.clone(),
                            restore: args.event.restore.clone(),
                        }
                    }),
                    on_done: Vec::new(),
                    on_error: Vec::new(),
                }),
                ..Default::default()
            },
        ),
    ]);
    Machine {
        context: Arc::new(|input| {
            Arc::new(Mutex::new(TodoActorContext {
                todos: Vec::new(),
                runtime: input,
            }))
        }),
        initial: "beforeRestore".to_string(),
        states,
        on: std::collections::HashMap::from([(
            "todo.commit".to_string(),
            vec![Transition {
                target: None,
                actions: vec![Action::Custom(Arc::new(|context, args| {
                    let context = context.downcast_ref::<Mutex<TodoActorContext>>().unwrap();
                    context.lock().todos =
                        serde_json::from_value(args.event.payload.clone()).unwrap_or_default();
                }))],
                ..Default::default()
            }],
        )]),
        actors: std::collections::HashMap::from([("todoReminder".to_string(), todo_reminder())]),
        ..Default::default()
    }
}

fn todo_reminder() -> ActorLogic {
    from_callback(|input| {
        let runtime = input
            .runtime
            .downcast_ref::<AgentActorContext<TodoState>>()
            .unwrap()
            .clone();
        if runtime.agent.agent_id != MAIN_AGENT_ID {
            return Box::new(|| {});
        }
        let telemetry = runtime.service::<dyn TelemetryService>("telemetryService");
        let bus = runtime.service::<dyn EventBusService>("eventBus");
        let tool_policy = runtime.service::<dyn IAgentToolPolicyService>("toolPolicyService");
        let memory = runtime.service::<dyn IAgentContextMemoryService>("contextMemoryService");
        let updated_in_turn = Arc::new(Mutex::new(false));
        let registration = runtime
            .service::<dyn ReminderService>("reminderService")
            .register(TODO_LIST_REMINDER_VARIANT, {
                let runtime = runtime.clone();
                let telemetry = telemetry.clone();
                let tool_policy = tool_policy.clone();
                let memory = memory.clone();
                Arc::new(move |_| {
                    let todos = (runtime.get_state)();
                    let active =
                        tool_policy.is_tool_active(TODO_LIST_TOOL_NAME, Some(ToolSource::Builtin));
                    let history = memory.get();
                    let reminder = todo_list_stale_reminder(active, &history, &todos);
                    let turn_id = runtime
                        .service::<dyn AgentLoopService>("loopService")
                        .snapshot()
                        .get("activeTurnId")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let telemetry = telemetry.clone();
                    Box::pin(async move {
                        Ok(reminder.map(|reminder| {
                            let progress = todo_progress(&todos);
                            telemetry.track2(
                                "todo_reminder_shown",
                                json!({
                                    "turn_id": turn_id,
                                    "steps_since_write": reminder.steps_since_last_write,
                                    "item_count": progress.total,
                                    "open_count": progress.open,
                                }),
                            );
                            ContextInjectionResult {
                                content: ContextInjectionContent::Text(reminder.text),
                                disclosure: None,
                            }
                        }))
                    })
                        as crate::rpc::types::BoxFuture<
                            'static,
                            Result<Option<ContextInjectionResult>, String>,
                        >
                })
            });
        let change = runtime.on_did_change.add({
            let updated_in_turn = updated_in_turn.clone();
            move |_| {
                *updated_in_turn.lock() = true;
                Ok(())
            }
        });
        let started = bus.subscribe("turn.started", {
            let updated_in_turn = updated_in_turn.clone();
            Arc::new(move |_| {
                *updated_in_turn.lock() = false;
            })
        });
        let ended = bus.subscribe("turn.ended", {
            let runtime = runtime.clone();
            let telemetry = telemetry.clone();
            let updated_in_turn = updated_in_turn.clone();
            Arc::new(move |event: Value| {
                let progress = todo_progress(&(runtime.get_state)());
                if progress.total == 0 && !*updated_in_turn.lock() {
                    return;
                }
                telemetry.track2(
                    "todo_turn_ended",
                    json!({
                        "turn_id": event.get("turnId"),
                        "reason": event.get("reason"),
                        "item_count": progress.total,
                        "open_count": progress.open,
                        "updated_in_turn": *updated_in_turn.lock(),
                    }),
                );
            })
        });
        Box::new(move || {
            ended();
            started();
            change.dispose();
            registration();
        })
    })
}

pub struct AgentTodoService {
    base: AgentActorService,
    actor: AgentActorContext<TodoState>,
    rev: Arc<Mutex<(u64, bool)>>,
}

impl AgentTodoService {
    pub fn new(
        dispatcher: Arc<dyn AgentActorHost>,
        agent: AgentContext,
        get: Arc<dyn Fn(&str) -> Arc<dyn std::any::Any + Send + Sync> + Send + Sync>,
        dispatch: Arc<
            dyn Fn(Value) -> crate::rpc::types::BoxFuture<'static, Result<(), String>>
                + Send
                + Sync,
        >,
    ) -> Self {
        let base = AgentActorService::new(dispatcher, agent, get, dispatch);
        let rev = Arc::new(Mutex::new((0u64, false)));
        let actor = base.attach_actor(
            todo_actor_logic(),
            AgentActorOptions {
                id: "todo".to_string(),
                input: None,
                durable: Some(AgentActorDurable {
                    events: vec![ToolsUpdateStore::event_type().to_string()],
                    undoable: true,
                    transition: todo_transition(),
                    read: Arc::new(|context| {
                        context
                            .downcast_ref::<Mutex<TodoActorContext>>()
                            .unwrap()
                            .lock()
                            .todos
                            .clone()
                    }),
                    commit: todo_commit(
                        rev.clone(),
                        base.agent.agent_id.clone(),
                        base.dispatch.clone(),
                    ),
                }),
            },
        );
        Self { base, actor, rev }
    }

    pub fn on_did_change(&self) -> Arc<crate::agent::state::listeners::Listeners<TodoState>> {
        self.actor.on_did_change.clone()
    }

    pub fn get(&self) -> TodoState {
        (self.actor.get_state)()
    }

    pub async fn replace(&self, todos: TodoState) -> Result<(), String> {
        self.commit(todos).await
    }

    pub async fn clear(&self) -> Result<(), String> {
        self.commit(Vec::new()).await
    }

    async fn commit(&self, todos: TodoState) -> Result<(), String> {
        (self.actor.dispatch)(json!({
            "type": ToolsUpdateStore::event_type(),
            "agentId": self.actor.agent.agent_id,
            "key": "todo",
            "value": todos,
        }))
        .await
    }

    fn publish(&self, todos: TodoState) {
        let mut rev = self.rev.lock();
        if !rev.1 && todos.is_empty() {
            return;
        }
        rev.1 = true;
        rev.0 += 1;
        let rev = rev.0;
        let dispatch = self.base.dispatch.clone();
        let agent_id = self.actor.agent.agent_id.clone();
        tokio::spawn(async move {
            let _ = dispatch(json!({
                "type": "todo.updated",
                "agentId": agent_id,
                "todos": todos,
                "rev": rev,
            }))
            .await;
        });
    }
}

fn todo_transition() -> StateFold<TodoState> {
    Arc::new(|state, event| {
        if event.type_ != ToolsUpdateStore::event_type() {
            return state;
        }
        if event.payload.get("key").and_then(Value::as_str) != Some("todo") {
            return state;
        }
        read_todo_items(event.payload.get("value").unwrap_or(&Value::Null))
    })
}

fn todo_commit(
    rev: Arc<Mutex<(u64, bool)>>,
    agent_id: String,
    dispatch: Arc<
        dyn Fn(Value) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> + Send + Sync,
    >,
) -> Arc<dyn Fn(&Arc<crate::human::xstate2::Actor>, TodoState, bool) + Send + Sync> {
    Arc::new(move |actor, todos, silent| {
        actor.send(XEvent::new(
            "todo.commit",
            serde_json::to_value(&todos).unwrap_or_default(),
        ));
        if silent {
            return;
        }
        let mut rev = rev.lock();
        if !rev.1 && todos.is_empty() {
            return;
        }
        rev.1 = true;
        rev.0 += 1;
        let rev = rev.0;
        let dispatch = dispatch.clone();
        let agent_id = agent_id.clone();
        tokio::spawn(async move {
            let _ = dispatch(json!({
                "type": "todo.updated",
                "agentId": agent_id,
                "todos": todos,
                "rev": rev,
            }))
            .await;
        });
    })
}
