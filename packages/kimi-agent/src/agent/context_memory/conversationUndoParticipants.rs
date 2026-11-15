use std::sync::Arc;

use once_cell::sync::Lazy;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::{DisposableRef, to_disposable};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::errors::errors::BugIndicatingError;
use crate::app::scopes::LifecycleScope;

pub trait AgentConversationUndoParticipant: Send + Sync {
    fn id(&self) -> &str;

    fn phase(&self) -> Option<&'static str>;

    fn reconcile_after_undo(&self) -> BoxFuture<'static, Result<(), String>>;
}

pub trait IAgentConversationUndoParticipantRegistry: Send + Sync {
    fn register(
        &self,
        participant: Arc<dyn AgentConversationUndoParticipant>,
    ) -> Result<DisposableRef, BugIndicatingError>;

    fn list(&self) -> Vec<Arc<dyn AgentConversationUndoParticipant>>;
}

pub static I_AGENT_CONVERSATION_UNDO_PARTICIPANT_REGISTRY: Lazy<
    ServiceIdentifier<Arc<dyn IAgentConversationUndoParticipantRegistry>>,
> = Lazy::new(|| create_decorator("agentConversationUndoParticipantRegistry"));

#[derive(Default, Clone)]
pub struct AgentConversationUndoParticipantRegistry {
    participants: Arc<Mutex<IndexMap<String, Arc<dyn AgentConversationUndoParticipant>>>>,
}

impl AgentConversationUndoParticipantRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IAgentConversationUndoParticipantRegistry for AgentConversationUndoParticipantRegistry {
    fn register(
        &self,
        participant: Arc<dyn AgentConversationUndoParticipant>,
    ) -> Result<DisposableRef, BugIndicatingError> {
        let id = participant.id().to_string();
        let mut participants = self.participants.lock();
        if participants.contains_key(&id) {
            return Err(BugIndicatingError::new(format!(
                "Conversation undo participant \"{id}\" is already registered"
            )));
        }
        participants.insert(id.clone(), participant.clone());
        drop(participants);
        let registry = self.participants.clone();
        Ok(to_disposable(move || {
            let mut participants = registry.lock();
            if participants
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &participant))
            {
                participants.shift_remove(&id);
            }
        }))
    }

    fn list(&self) -> Vec<Arc<dyn AgentConversationUndoParticipant>> {
        self.participants.lock().values().cloned().collect()
    }
}

static UNDO_PARTICIPANT_REGISTRY_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|_| {
        let service: Arc<dyn IAgentConversationUndoParticipantRegistry> =
            Arc::new(AgentConversationUndoParticipantRegistry::new());
        Arc::new(service) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        create_decorator("agentConversationUndoParticipantRegistry"),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "conversationUndoParticipants",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_agent_conversation_undo_participant_registry_registered() -> Result<(), String> {
    Lazy::force(&UNDO_PARTICIPANT_REGISTRY_REGISTRATION).clone()
}
