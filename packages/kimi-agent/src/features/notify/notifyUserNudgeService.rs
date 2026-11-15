use std::sync::Arc;

use serde_json::{Value, json};

use crate::agent::actor_service::agent_actor_runtime::AgentActorHost;
use crate::agent::actor_service::agent_actor_service::{
    AgentActorContext, AgentActorOptions, AgentActorService,
};
use crate::agent::actor_service::services::{
    AgentLoopService, BootstrapService, FlagService, TelemetryService,
};
use crate::agent::agent_context::AgentContext;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::tool_executor::tool_executor::IAgentToolRegistryService;
use crate::features::notify::notify_user_availability::notify_user_available;
use crate::features::notify::notify_user_nudge::{
    NOTIFY_USER_NUDGE_VARIANT, notify_streak, render_notify_user_nudge, should_nudge_notify_user,
    tool_call_rounds_since_position,
};
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult,
};
use crate::human::xstate2::{
    ActorLogic, Context, InvokeDef, InvokeInput, Machine, StateNode, Transition, from_callback,
};
use crate::tools::notify_user::NOTIFY_USER_TOOL_NAME;

#[derive(Clone)]
pub struct NotifyUserNudgeActorContext {
    pub runtime: Context,
}

pub fn notify_user_nudge_actor_logic() -> Machine {
    Machine {
        context: Arc::new(|input| Arc::new(NotifyUserNudgeActorContext { runtime: input })),
        initial: "beforeRestore".to_string(),
        states: std::collections::HashMap::from([
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
                },
            ),
            (
                "active".to_string(),
                StateNode {
                    final_: false,
                    on: std::collections::HashMap::new(),
                    invoke: Some(InvokeDef {
                        id: "notifyUserNudgeReminders".to_string(),
                        src: "notifyUserNudgeReminders".to_string(),
                        input: Arc::new(|args| {
                            let context = args
                                .context
                                .downcast_ref::<NotifyUserNudgeActorContext>()
                                .unwrap();
                            InvokeInput {
                                runtime: context.runtime.clone(),
                                restore: args.event.restore.clone(),
                            }
                        }),
                        on_done: Vec::new(),
                        on_error: Vec::new(),
                    }),
                },
            ),
        ]),
        on: std::collections::HashMap::new(),
        actors: std::collections::HashMap::from([(
            "notifyUserNudgeReminders".to_string(),
            notify_user_nudge_reminders(),
        )]),
        ..Default::default()
    }
}

fn notify_user_nudge_reminders() -> ActorLogic {
    from_callback(|input| {
        let runtime = input
            .runtime
            .downcast_ref::<AgentActorContext<()>>()
            .unwrap()
            .clone();
        let flags = runtime.service::<dyn FlagService>("flagService");
        let bootstrap = runtime.service::<dyn BootstrapService>("bootstrapService");
        let available = {
            let flags = flags.clone();
            let bootstrap = bootstrap.clone();
            move || notify_user_available(flags.as_ref(), bootstrap.as_ref())
        };
        if !available() {
            return Box::new(|| {});
        }
        let registration = runtime
            .service::<dyn ReminderService>("reminderService")
            .register(NOTIFY_USER_NUDGE_VARIANT, {
                let runtime = runtime.clone();
                let available = available.clone();
                Arc::new(move |context: ContextInjectionContext| {
                    let runtime = runtime.clone();
                    let available = available.clone();
                    Box::pin(async move {
                        if !available() {
                            return Ok(None);
                        }
                        if runtime
                            .service::<dyn IAgentToolRegistryService>("toolRegistryService")
                            .resolve(NOTIFY_USER_TOOL_NAME)
                            .is_none()
                        {
                            return Ok(None);
                        }
                        let history = runtime
                            .service::<dyn IAgentContextMemoryService>("contextMemoryService")
                            .get();
                        let streak = notify_streak(&history);
                        let rounds_since_last_nudge = context
                            .last_injected_at
                            .map(|position| tool_call_rounds_since_position(&history, position));
                        if !should_nudge_notify_user(streak.rounds, rounds_since_last_nudge) {
                            return Ok(None);
                        }
                        let turn_id = runtime
                            .service::<dyn AgentLoopService>("loopService")
                            .snapshot()
                            .get("activeTurnId")
                            .cloned()
                            .unwrap_or(Value::Null);
                        runtime
                            .service::<dyn TelemetryService>("telemetryService")
                            .track2(
                                "notify_user_nudge_shown",
                                json!({
                                    "turn_id": turn_id,
                                    "rounds_since_notify": streak.rounds,
                                    "nudge_index": streak.nudges + 1,
                                }),
                            );
                        Ok(Some(ContextInjectionResult {
                            content: ContextInjectionContent::Text(render_notify_user_nudge(
                                streak.rounds,
                            )),
                            disclosure: None,
                        }))
                    })
                        as crate::rpc::types::BoxFuture<
                            'static,
                            Result<Option<ContextInjectionResult>, String>,
                        >
                })
            });
        Box::new(move || registration())
    })
}

pub struct AgentNotifyUserNudgeService {
    base: AgentActorService,
    actor: AgentActorContext<()>,
}

impl AgentNotifyUserNudgeService {
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
        let actor = base.attach_actor(
            notify_user_nudge_actor_logic(),
            AgentActorOptions {
                id: "notifyUserNudge".to_string(),
                input: None,
                durable: None,
            },
        );
        Self { base, actor }
    }
}
