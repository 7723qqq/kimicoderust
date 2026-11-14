use std::sync::Arc;

use crate::state::state::{UndoableProtocol, register_undoable_protocol};

use super::context_events::{
    CONTEXT_APPEND_MESSAGE_CLASS, CONTEXT_APPLY_COMPACTION_CLASS, CONTEXT_CLEAR_CLASS,
    CONTEXT_UNDO_CLASS,
};
use super::types::{
    ContextMessage, PluginCommandOrigin, PluginCommandTrigger, PromptOrigin, SkillActivationOrigin,
    SkillActivationTrigger, UserPromptOrigin,
};

pub trait InTurnOrigin {
    fn mark_in_turn(&mut self);
}

impl InTurnOrigin for UserPromptOrigin {
    fn mark_in_turn(&mut self) {
        self.in_turn = Some(true);
    }
}

impl InTurnOrigin for SkillActivationOrigin {
    fn mark_in_turn(&mut self) {
        self.in_turn = Some(true);
    }
}

impl InTurnOrigin for PluginCommandOrigin {
    fn mark_in_turn(&mut self) {
        self.in_turn = Some(true);
    }
}

pub fn mark_in_turn_origin<T: InTurnOrigin>(mut origin: T) -> T {
    origin.mark_in_turn();
    origin
}

pub fn is_undo_anchor_origin(origin: Option<&PromptOrigin>) -> bool {
    match origin {
        Some(PromptOrigin::User(origin)) if origin.in_turn == Some(true) => false,
        Some(PromptOrigin::SkillActivation(origin)) if origin.in_turn == Some(true) => false,
        Some(PromptOrigin::PluginCommand(origin)) if origin.in_turn == Some(true) => false,
        None | Some(PromptOrigin::User(_)) => true,
        Some(PromptOrigin::SkillActivation(origin)) => {
            origin.trigger == SkillActivationTrigger::UserSlash
        }
        Some(PromptOrigin::PluginCommand(origin)) => {
            origin.trigger == PluginCommandTrigger::UserSlash
        }
        _ => false,
    }
}

pub fn is_undo_anchor(message: &ContextMessage) -> bool {
    message.role == crate::human::llm::message::Role::User
        && is_undo_anchor_origin(message.origin.as_ref())
}

pub fn is_prompt_owned_injection(message: &ContextMessage, prompt: &ContextMessage) -> bool {
    let Some(PromptOrigin::Injection(origin)) = &message.origin else {
        return false;
    };
    origin.owner_prompt_id.is_some()
        && origin.owner_prompt_id == prompt.id
}

pub fn is_valid_undo_count(count: f64) -> bool {
    count.is_finite()
        && count > 0.0
        && count.fract() == 0.0
        && count <= 9_007_199_254_740_991.0
}

pub fn register_conversation_time_undoable_protocol(
) -> Result<(), crate::state::errors::StateError> {
    register_undoable_protocol(UndoableProtocol {
        append_message: CONTEXT_APPEND_MESSAGE_CLASS.clone(),
        apply_compaction: CONTEXT_APPLY_COMPACTION_CLASS.clone(),
        clear: CONTEXT_CLEAR_CLASS.clone(),
        undo: CONTEXT_UNDO_CLASS.clone(),
        is_undo_anchor: Arc::new(|message| {
            serde_json::from_value::<ContextMessage>(message.clone())
                .ok()
                .is_some_and(|message| is_undo_anchor(&message))
        }),
        is_valid_undo_count: Arc::new(is_valid_undo_count),
    })
}
