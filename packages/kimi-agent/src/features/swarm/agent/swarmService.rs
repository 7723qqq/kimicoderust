use std::sync::Arc;

use kimi_core::primitives::errors::domains::core::VALIDATION_FAILED;
use kimi_core::primitives::errors::Error2;
use serde_json::Value;

use super::injection::swarmInjection::{SwarmInjection, SwarmInjectionOptions};
use super::swarm::AGENT_SWARM_SERVICE_ID;
use crate::agent::actor_service::services::TelemetryService;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::tool_approval::tool_approval::IAgentToolApprovalService;
use crate::app::event::event_bus::IEventBus;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::swarm::swarmOps::{
    swarm_key, swarm_mode_enter_event, swarm_mode_exit_event,
};
use crate::i18n::LocalizedText;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::swarm::mode::SwarmModeTrigger;

pub const SWARM_MODE_OFF_MESSAGE: &str = "Swarm mode is off, so AgentSwarm did not run and no subagents were started. Only the user can turn swarm mode on; do not call AgentSwarm again until they do. If the user asked for a swarm, tell them swarm mode is off and that they can turn it on with /swarm, or run /swarm <task> to start the task as a swarm. Otherwise, continue without AgentSwarm and do not mention swarm mode.";

pub struct AgentSwarmService {
    dispatcher: Arc<dyn IEventDispatcher>,
    reminder: Arc<dyn ReminderService>,
    event_bus: Arc<dyn IEventBus>,
    context: Arc<dyn IAgentContextMemoryService>,
    tool_approval: Arc<dyn IAgentToolApprovalService>,
    agent_ctx: Arc<dyn IAgentScopeContext>,
    agent_state: Arc<dyn IAgentStateService>,
    telemetry: Arc<dyn TelemetryService>,
    injection: SwarmInjection,
}

impl AgentSwarmService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dispatcher: Arc<dyn IEventDispatcher>,
        reminder: Arc<dyn ReminderService>,
        event_bus: Arc<dyn IEventBus>,
        context: Arc<dyn IAgentContextMemoryService>,
        tool_approval: Arc<dyn IAgentToolApprovalService>,
        agent_ctx: Arc<dyn IAgentScopeContext>,
        agent_state: Arc<dyn IAgentStateService>,
        telemetry: Arc<dyn TelemetryService>,
    ) -> Self {
        let state_registration = agent_state
            .contribute_state(swarm_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        let _ = state_registration;
        let agent_state_for_trigger = agent_state.clone();
        let injection = SwarmInjection::new(
            SwarmInjectionOptions {
                get_trigger: Arc::new(move || {
                    trigger_of(&agent_state_for_trigger.get(&swarm_key().state_key()))
                }),
            },
            reminder.clone(),
            context.clone(),
        );
        Self {
            dispatcher,
            reminder,
            event_bus,
            context,
            tool_approval,
            agent_ctx,
            agent_state,
            telemetry,
            injection,
        }
    }

    pub fn service_id(&self) -> &'static str {
        AGENT_SWARM_SERVICE_ID
    }

    pub fn enter(&self, trigger: SwarmModeTrigger) -> Result<(), Error2> {
        if trigger == SwarmModeTrigger::Tool {
            return Err(Error2::new(
                VALIDATION_FAILED,
                "Only the user can turn on swarm mode.",
            ));
        }
        if self.trigger().is_some() {
            return Ok(());
        }
        self.agent_state
            .set(&swarm_key().state_key(), Value::String(trigger.as_str().to_string()));
        let event = swarm_mode_enter_event(&self.agent_ctx.agent_id(), trigger);
        self.dispatch(event);
        self.telemetry
            .track2("swarm_mode_entered", serde_json::json!({ "trigger": trigger.as_str() }));
        Ok(())
    }

    pub fn exit(&self) {
        let Some(trigger) = self.trigger() else {
            return;
        };
        self.agent_state.set(&swarm_key().state_key(), Value::Null);
        let event = swarm_mode_exit_event(&self.agent_ctx.agent_id());
        self.dispatch(event);
        self.telemetry.track2(
            "swarm_mode_exited",
            serde_json::json!({ "trigger": trigger.as_str() }),
        );
    }

    pub fn is_active(&self) -> bool {
        self.trigger().is_some()
    }

    pub fn should_auto_exit(&self) -> bool {
        self.trigger()
            .is_some_and(|trigger| trigger == SwarmModeTrigger::Task || trigger == SwarmModeTrigger::Tool)
    }

    pub fn on_turn_ended(&self) {
        if self.should_auto_exit() {
            self.exit();
        }
    }

    pub fn veto_agent(&self, tool_name: &str) -> Option<String> {
        if !self.is_active() {
            return None;
        }
        if !tool_name.eq_ignore_ascii_case("agent") {
            return None;
        }
        Some(self.tool_approval.format_deny_message(
            &LocalizedText::new("toolsV2.swarm.agentDeniedInSwarmMode").render(),
        ))
    }

    pub fn veto_tool_batch(&self, tool_names: &[&str]) -> Option<String> {
        if !self.is_active() {
            return None;
        }
        let swarm_count = tool_names
            .iter()
            .filter(|name| name.eq_ignore_ascii_case("agentswarm"))
            .count();
        if swarm_count == 0 {
            return None;
        }
        let others = tool_names.len() - swarm_count;
        let message = if swarm_count > 1 {
            let suffix = if others > 0 {
                " AgentSwarm also must not be combined with other tools in the same response."
            } else {
                ""
            };
            format!(
                "AgentSwarm must be called one swarm at a time. Multiple AgentSwarm calls are not forbidden, but issue them sequentially: call one AgentSwarm, wait for its result, then call the next; or merge the work into a single AgentSwarm when one swarm can cover it.{suffix}"
            )
        } else if others > 0 {
            "AgentSwarm must be the only tool call in a model response. Retry with a single AgentSwarm call by itself, then call any other tools after it returns.".to_string()
        } else {
            return None;
        };
        Some(self.tool_approval.format_deny_message(&message))
    }

    fn trigger(&self) -> Option<SwarmModeTrigger> {
        trigger_of(&self.agent_state.get(&swarm_key().state_key()))
    }

    fn dispatch(&self, event: crate::app::event::event2::Event2) {
        let dispatcher = self.dispatcher.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = dispatcher.dispatch(event).await;
            });
        }
    }
}

fn trigger_of(value: &Value) -> Option<SwarmModeTrigger> {
    match value.as_str() {
        Some("manual") => Some(SwarmModeTrigger::Manual),
        Some("task") => Some(SwarmModeTrigger::Task),
        Some("tool") => Some(SwarmModeTrigger::Tool),
        _ => None,
    }
}
