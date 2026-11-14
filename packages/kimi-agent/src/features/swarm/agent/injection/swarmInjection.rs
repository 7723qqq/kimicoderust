use std::sync::Arc;

use serde_json::{Value, json};

use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::PromptOrigin;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult,
};
use crate::swarm::mode::SwarmModeTrigger;

pub const SWARM_MODE_INJECTION_VARIANT: &str = "swarm_mode";
pub const LEGACY_SWARM_MODE_EXIT_VARIANT: &str = "swarm_mode_exit";
pub const SWARM_MODE_ENTER_REMINDER: &str = include_str!("../enter-reminder.md");
pub const SWARM_MODE_EXIT_REMINDER: &str = include_str!("../exit-reminder.md");

pub type SwarmModeTriggerProvider = Arc<dyn Fn() -> Option<SwarmModeTrigger> + Send + Sync>;

#[derive(Clone)]
pub struct SwarmInjectionOptions {
    pub get_trigger: SwarmModeTriggerProvider,
}

pub struct SwarmInjection {
    registration: Arc<dyn Fn() + Send + Sync>,
}

impl Drop for SwarmInjection {
    fn drop(&mut self) {
        (self.registration)();
    }
}

impl SwarmInjection {
    pub fn new(
        options: SwarmInjectionOptions,
        injector: Arc<dyn ReminderService>,
        context: Arc<dyn IAgentContextMemoryService>,
    ) -> Self {
        let registration = injector.register(
            SWARM_MODE_INJECTION_VARIANT,
            Arc::new(move |injection_context: ContextInjectionContext| {
                let get_trigger = options.get_trigger.clone();
                let context = context.clone();
                Box::pin(async move { reminder(&get_trigger, &context, &injection_context) })
                    as crate::rpc::types::BoxFuture<
                        'static,
                        Result<Option<ContextInjectionResult>, String>,
                    >
            }),
        );
        Self { registration }
    }
}

fn reminder(
    get_trigger: &SwarmModeTriggerProvider,
    context: &Arc<dyn IAgentContextMemoryService>,
    injection_context: &ContextInjectionContext,
) -> Result<Option<ContextInjectionResult>, String> {
    let trigger = get_trigger();
    let active = trigger.is_some_and(|trigger| trigger != SwarmModeTrigger::Tool);
    let rendered = rendered_state(injection_context, context);
    if active {
        if rendered == Some(true) {
            return Ok(None);
        }
        return Ok(Some(ContextInjectionResult {
            content: ContextInjectionContent::Text(SWARM_MODE_ENTER_REMINDER.to_string()),
            disclosure: Some(disclosure("active")),
        }));
    }
    if rendered == Some(true) {
        return Ok(Some(ContextInjectionResult {
            content: ContextInjectionContent::Text(SWARM_MODE_EXIT_REMINDER.to_string()),
            disclosure: Some(disclosure("inactive")),
        }));
    }
    Ok(None)
}

fn rendered_state(
    injection_context: &ContextInjectionContext,
    context: &Arc<dyn IAgentContextMemoryService>,
) -> Option<bool> {
    if let Some(disclosure) = &injection_context.last_disclosure {
        return disclosure
            .get("state")
            .and_then(Value::as_str)
            .map(|state| state == "active");
    }
    let history = context.get();
    for message in history.iter().rev() {
        let Some(PromptOrigin::Injection(origin)) = message.origin.as_ref() else {
            continue;
        };
        if origin.variant == LEGACY_SWARM_MODE_EXIT_VARIANT {
            return Some(false);
        }
        if origin.variant == SWARM_MODE_INJECTION_VARIANT {
            return Some(true);
        }
    }
    None
}

fn disclosure(state: &str) -> Value {
    json!({ "kind": "swarm_mode", "state": state })
}
