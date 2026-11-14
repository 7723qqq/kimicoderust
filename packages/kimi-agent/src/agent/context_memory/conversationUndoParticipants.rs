use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, to_disposable};
use crate::_base::errors::errors::BugIndicatingError;

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
