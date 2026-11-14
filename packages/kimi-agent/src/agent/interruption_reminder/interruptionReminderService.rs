use std::sync::Arc;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::conversation_time::is_undo_anchor;
use crate::agent::context_memory::types::{ContextMessage, PromptOrigin};
use crate::agent::context_memory::vacuous_content::is_vacuous_content_part;
use crate::agent::r#loop::turn_events::{TurnEndReason, TurnInterruptReason};
use crate::agent::r#loop::turn_ops::{TurnEnded, TurnEndedPayload};
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::app::event::event_bus::IEventBus;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::ReminderNotification;
use crate::human::llm::message::Role;

use super::interruption_reminder_ops::{INTERRUPTION_REMINDER_VARIANT, interruption_reminder_key};

pub const INTERRUPTION_REMINDER: &str = "The previous turn was interrupted by the user before completion; any partial output shown above is incomplete. The user's next message continues the conversation.";

pub struct AgentInterruptionReminderService {
    _state: DisposableRef,
    _subscription: DisposableRef,
}

impl AgentInterruptionReminderService {
    pub fn new(
        event_bus: Arc<dyn IEventBus>,
        context: Arc<dyn IAgentContextMemoryService>,
        reminder: Arc<dyn ReminderService>,
        agent_state: Arc<dyn IAgentStateService>,
    ) -> Self {
        let state = agent_state
            .contribute_state(interruption_reminder_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        let subscription = event_bus.subscribe_class(
            TurnEnded::class(),
            Arc::new(move |event| {
                let Ok(event) = serde_json::from_value::<TurnEndedPayload>(event.payload) else {
                    return;
                };
                if event.reason != TurnEndReason::Cancelled
                    || event.interrupt_reason != Some(TurnInterruptReason::UserCancelled)
                {
                    return;
                }
                let history = context.get();
                if last_comparable_message(&history).is_some_and(|message| {
                    matches!(
                        &message.origin,
                        Some(PromptOrigin::Injection(origin))
                            if origin.kind == "injection"
                                && origin.variant == INTERRUPTION_REMINDER_VARIANT
                    )
                }) {
                    return;
                }
                let owner_prompt_id = history
                    .iter()
                    .rev()
                    .find(|message| is_undo_anchor(message))
                    .and_then(|message| message.id.clone());
                reminder.notify(
                    INTERRUPTION_REMINDER,
                    &ReminderNotification {
                        variant: INTERRUPTION_REMINDER_VARIANT.to_string(),
                        owner_prompt_id,
                    },
                );
            }),
        );
        Self {
            _state: state,
            _subscription: to_disposable(move || subscription.dispose()),
        }
    }
}

impl Drop for AgentInterruptionReminderService {
    fn drop(&mut self) {
        self._state.dispose();
        self._subscription.dispose();
    }
}

fn last_comparable_message(messages: &[ContextMessage]) -> Option<&ContextMessage> {
    for message in messages.iter().rev() {
        if message.role == Role::Assistant
            && message.partial == Some(true)
            && message.tool_calls.is_empty()
            && message.content.iter().all(is_vacuous_content_part)
        {
            continue;
        }
        return Some(message);
    }
    None
}
