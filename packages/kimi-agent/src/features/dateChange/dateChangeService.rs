use std::sync::Arc;

use serde_json::{Value, json};

use crate::agent::actor_service::agent_actor_runtime::AgentActorHost;
use crate::agent::actor_service::agent_actor_service::{
    AgentActorContext, AgentActorOptions, AgentActorService,
};
use crate::agent::actor_service::services::{EnvironmentBindingService, SessionContextService};
use crate::agent::agent_context::AgentContext;
use crate::agent::profile::AgentProfileService;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::ContextInjectionContent;
use crate::features::reminder::types::{ContextInjectionContext, ContextInjectionResult};
use crate::human::xstate2::{
    Action, Context, FromCallback, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent,
    from_callback,
};
use crate::os::HostClock;
use parking_lot::Mutex;

pub const DATE_CHANGE_INJECTION_VARIANT: &str = "date_change";
pub const AGENT_DATE_CHANGE_SERVICE_ID: &str = "agentDateChangeService";

pub trait IAgentDateChangeService: Send + Sync {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateDisclosure {
    pub local_date: String,
    pub time_zone: String,
    pub render_generation: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateInjectionDisclosure {
    pub kind: String,
    pub render_generation: u32,
    pub local_date: String,
    pub time_zone: String,
}

#[derive(Clone)]
pub struct DateChangeActorContext {
    pub seed: Option<DateDisclosure>,
    pub runtime: Context,
}

pub fn actor_context(runtime: &AgentActorContext<()>) -> DateChangeActorContext {
    (runtime.get_logic_state)()
        .downcast::<Mutex<DateChangeActorContext>>()
        .unwrap()
        .lock()
        .clone()
}

pub fn date_change_actor_logic() -> Machine {
    Machine {
        context: Arc::new(|input| {
            Arc::new(Mutex::new(DateChangeActorContext {
                seed: None,
                runtime: input,
            }))
        }),
        initial: "beforeRestore".to_string(),
        states: std::collections::HashMap::from([
            (
                "beforeRestore".to_string(),
                StateNode {
                    on: std::collections::HashMap::from([(
                        "runtime.restore".to_string(),
                        Transition {
                            target: Some("active".to_string()),
                            actions: Vec::new(),
                        },
                    )]),
                    invoke: None,
                },
            ),
            (
                "active".to_string(),
                StateNode {
                    on: std::collections::HashMap::new(),
                    invoke: Some(InvokeDef {
                        id: "dateChangeInjection".to_string(),
                        src: "dateChangeInjection".to_string(),
                        input: Arc::new(|context, event| {
                            let context = context
                                .downcast_ref::<Mutex<DateChangeActorContext>>()
                                .unwrap()
                                .lock();
                            InvokeInput {
                                runtime: context.runtime.clone(),
                                restore: event.restore.clone(),
                            }
                        }),
                    }),
                },
            ),
        ]),
        on: std::collections::HashMap::from([(
            "dateChange.disclose".to_string(),
            Transition {
                target: None,
                actions: vec![Action::Custom(Arc::new(|context, event| {
                    let context = context
                        .downcast_ref::<Mutex<DateChangeActorContext>>()
                        .unwrap();
                    context.lock().seed = event
                        .payload
                        .get("seed")
                        .cloned()
                        .and_then(|seed| serde_json::from_value(seed).ok());
                }))],
            },
        )]),
        actors: std::collections::HashMap::from([(
            "dateChangeInjection".to_string(),
            date_change_injection(),
        )]),
    }
}

pub fn pick_disclosure_baseline(
    candidates: &[Option<DateInjectionDisclosure>],
) -> Option<DateInjectionDisclosure> {
    let mut winner: Option<DateInjectionDisclosure> = None;
    for candidate in candidates.iter().flatten() {
        let replace = match &winner {
            None => true,
            Some(current) => candidate.render_generation > current.render_generation,
        };
        if replace {
            winner = Some(candidate.clone());
        }
    }
    winner
}

fn current_date_disclosure(clock: &dyn crate::os::HostClock) -> (String, String) {
    let now = clock.now();
    let time_zone = clock.time_zone();
    (now.format("%Y-%m-%d").to_string(), time_zone)
}

fn date_injection_result(
    content: String,
    disclosure: DateInjectionDisclosure,
) -> ContextInjectionResult {
    ContextInjectionResult {
        content: ContextInjectionContent::Text(content),
        disclosure: serde_json::to_value(disclosure).ok(),
    }
}

fn date_change_injection() -> FromCallback {
    from_callback(|input| {
        let runtime = input
            .runtime
            .downcast_ref::<AgentActorContext<()>>()
            .unwrap()
            .clone();
        let reminder = runtime.service::<dyn ReminderService>("reminderService");
        let profile = runtime.service::<dyn AgentProfileService>("profileService");
        let clock = runtime.service::<dyn HostClock>("hostClock");
        let session_context = runtime.service::<dyn SessionContextService>("sessionContext");
        let binding_cwd = {
            let runtime = runtime.clone();
            move || {
                runtime
                    .service::<dyn EnvironmentBindingService>("environmentBindingService")
                    .current_cwd()
            }
        };
        let belongs_to_current_cwd = {
            let profile = profile.clone();
            let binding_cwd = binding_cwd.clone();
            let session_context = session_context.clone();
            move || {
                let environment = profile.data().environment_disclosure;
                let binding = binding_cwd().unwrap_or_else(|| session_context.cwd());
                environment.as_ref().is_none_or(|environment| {
                    environment.cwd.is_empty() || environment.cwd == binding
                })
            }
        };
        let registration = reminder.register(DATE_CHANGE_INJECTION_VARIANT, {
            let runtime = runtime.clone();
            let profile = profile.clone();
            let clock = clock.clone();
            let belongs_to_current_cwd = belongs_to_current_cwd.clone();
            Arc::new(move |context: ContextInjectionContext| {
                let runtime = runtime.clone();
                let profile = profile.clone();
                let clock = clock.clone();
                let belongs_to_current_cwd = belongs_to_current_cwd.clone();
                Box::pin(async move {
                    let last_disclosure = context.last_disclosure.and_then(|value| {
                        serde_json::from_value::<DateInjectionDisclosure>(value).ok()
                    });
                    let profile_data = profile.data();
                    if !belongs_to_current_cwd() {
                        return Ok(None);
                    }
                    let render_generation = profile_data.render_generation.unwrap_or(0);
                    let current = current_date_disclosure(clock.as_ref());
                    let seed = actor_context(&runtime).seed;
                    let baseline = pick_disclosure_baseline(&[
                        last_disclosure.clone(),
                        seed.as_ref().map(|seed| DateInjectionDisclosure {
                            kind: "date".to_string(),
                            render_generation: seed.render_generation,
                            local_date: seed.local_date.clone(),
                            time_zone: seed.time_zone.clone(),
                        }),
                    ]);
                    if let Some(baseline) = baseline
                        && baseline.local_date != current.0
                    {
                        return Ok(Some(date_injection_result(
                            format!(
                                "The date has changed. Today's date is now {}, in the local client machine's timezone ({}). Rely on this reminder over any earlier date statement for the current date. DO NOT mention this to the user explicitly.",
                                current.0, current.1
                            ),
                            DateInjectionDisclosure {
                                kind: "date".to_string(),
                                render_generation,
                                local_date: current.0,
                                time_zone: current.1,
                            },
                        )));
                    }
                    if last_disclosure.is_some() {
                        return Ok(None);
                    }
                    if seed.is_none() {
                        (runtime.send)(XEvent::new(
                            "dateChange.disclose",
                            json!({
                                "seed": {
                                    "localDate": current.0,
                                    "timeZone": current.1,
                                    "renderGeneration": render_generation,
                                }
                            }),
                        ));
                    }
                    Ok(Some(date_injection_result(
                        format!(
                            "Today's date is {}, in the local client machine's timezone ({}). The current date is restated in a reminder whenever it changes; rely on the latest such reminder for the current date. DO NOT mention this to the user explicitly.",
                            current.0, current.1
                        ),
                        DateInjectionDisclosure {
                            kind: "date".to_string(),
                            render_generation,
                            local_date: current.0,
                            time_zone: current.1,
                        },
                    )))
                }) as crate::rpc::types::BoxFuture<
                    'static,
                    Result<Option<ContextInjectionResult>, String>,
                >
            })
        });
        Box::new(move || registration())
    })
}

pub struct AgentDateChangeService {
    base: AgentActorService,
    actor: AgentActorContext<()>,
}

impl AgentDateChangeService {
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
            date_change_actor_logic(),
            AgentActorOptions {
                id: "dateChange".to_string(),
                input: None,
                durable: None,
            },
        );
        Self { base, actor }
    }
}

impl IAgentDateChangeService for AgentDateChangeService {}
