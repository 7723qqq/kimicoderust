use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::callbacks::MAIN_AGENT_ID;
use crate::rpc::types::BoxFuture;

pub const CONTEXT_UNDONE_EVENT: &str = "context.undone";

pub trait UndoParticipant: Send + Sync {
    fn id(&self) -> &str;

    fn phase(&self) -> Option<&'static str>;

    fn reconcile_after_undo(&self) -> BoxFuture<'static, Result<(), String>>;
}

#[derive(Default, Clone)]
pub struct UndoParticipantRegistry {
    participants: Arc<Mutex<indexmap::IndexMap<String, Arc<dyn UndoParticipant>>>>,
}

impl UndoParticipantRegistry {
    pub fn register(
        &self,
        participant: Arc<dyn UndoParticipant>,
    ) -> Result<Box<dyn FnOnce() + Send>, String> {
        let mut participants = self.participants.lock();
        if participants.contains_key(participant.id()) {
            return Err(format!(
                "Conversation undo participant \"{}\" is already registered",
                participant.id()
            ));
        }
        let id = participant.id().to_string();
        participants.insert(id.clone(), participant.clone());
        drop(participants);
        let registry = self.clone();
        Ok(Box::new(move || {
            let mut participants = registry.participants.lock();
            if participants
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &participant))
            {
                participants.shift_remove(&id);
            }
        }))
    }

    pub fn list(&self) -> Vec<Arc<dyn UndoParticipant>> {
        self.participants.lock().values().cloned().collect()
    }
}

pub async fn reconcile_participants(registry: &UndoParticipantRegistry, phase: Option<&str>) {
    let participants: Vec<Arc<dyn UndoParticipant>> = registry
        .list()
        .into_iter()
        .filter(|participant| participant.phase() == phase)
        .collect();
    let results = futures_util::future::join_all(
        participants
            .iter()
            .map(|participant| participant.reconcile_after_undo()),
    )
    .await;
    for (participant, result) in participants.iter().zip(results) {
        if let Err(error) = result {
            tracing::error!(
                participant_id = participant.id(),
                %error,
                "undo participant reconciliation failed"
            );
        }
    }
}

pub trait UndoDispatcher: Send + Sync {
    fn restore_phase(&self) -> String;

    fn restore(&self) -> BoxFuture<'static, Result<(), String>>;

    fn flush(&self) -> BoxFuture<'static, Result<(), String>>;

    fn dispatch(&self, event: Value) -> BoxFuture<'static, Result<(), String>>;
}

pub async fn flush_after_reconcile(dispatcher: &dyn UndoDispatcher) -> Result<(), String> {
    dispatcher.flush().await.map_err(|error| {
        tracing::error!(%error, stage = "state reconciliation", "undo wire flush failed after in-memory commit");
        error
    })
}

pub fn context_undone_event(agent_id: &str, turns: u64, from_turn_id: Option<i64>) -> Value {
    let mut event = json!({
        "type": CONTEXT_UNDONE_EVENT,
        "agentId": agent_id,
        "turns": turns,
    });
    if let Some(from_turn_id) = from_turn_id {
        event["fromTurnId"] = json!(from_turn_id);
    }
    event
}

pub fn session_meta_updated_event(
    agent_id: &str,
    session_id: &str,
    last_prompt: Option<&str>,
) -> Value {
    let mut patch = serde_json::Map::new();
    if let Some(last_prompt) = last_prompt {
        patch.insert("lastPrompt".to_string(), Value::String(last_prompt.to_string()));
    }
    json!({
        "type": "session.meta.updated",
        "payload": {
            "agentId": agent_id,
            "sessionId": session_id,
            "patch": Value::Object(patch),
        },
    })
}

pub struct AgentConversationUndoService {
    participants: Arc<UndoParticipantRegistry>,
    dispatcher: Arc<dyn UndoDispatcher>,
    agent_id: String,
    session_id: String,
    undo_queue: Mutex<()>,
}

impl AgentConversationUndoService {
    pub fn new(
        participants: Arc<UndoParticipantRegistry>,
        dispatcher: Arc<dyn UndoDispatcher>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            participants,
            dispatcher,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            undo_queue: Mutex::new(()),
        }
    }

    pub fn participants(&self) -> &UndoParticipantRegistry {
        &self.participants
    }

    pub async fn reconcile_participants(&self, phase: Option<&str>) {
        reconcile_participants(&self.participants, phase).await;
    }

    pub async fn flush_after_reconcile(&self) -> Result<(), String> {
        flush_after_reconcile(self.dispatcher.as_ref()).await
    }

    pub async fn reconcile_last_prompt_safely(&self, last_prompt: Option<String>) {
        if self.agent_id != MAIN_AGENT_ID {
            return;
        }
        let event =
            session_meta_updated_event(MAIN_AGENT_ID, &self.session_id, last_prompt.as_deref());
        if let Err(error) = self.dispatcher.dispatch(event).await {
            tracing::error!(%error, "undo lastPrompt reconciliation failed");
        }
    }

    pub async fn dispatch_context_undone(
        &self,
        turns: u64,
        from_turn_id: Option<i64>,
    ) -> Result<(), String> {
        self.dispatcher
            .dispatch(context_undone_event(&self.agent_id, turns, from_turn_id))
            .await
    }

    pub fn undo_gate(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.undo_queue.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Participant {
        id: &'static str,
        phase: Option<&'static str>,
        calls: Arc<Mutex<usize>>,
    }

    impl UndoParticipant for Participant {
        fn id(&self) -> &str {
            self.id
        }

        fn phase(&self) -> Option<&'static str> {
            self.phase
        }

        fn reconcile_after_undo(&self) -> BoxFuture<'static, Result<(), String>> {
            let calls = self.calls.clone();
            Box::pin(async move {
                *calls.lock() += 1;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn registry_rejects_duplicates_disposes_and_reconciles_by_phase() {
        let registry = UndoParticipantRegistry::default();
        let calls = Arc::new(Mutex::new(0usize));
        let participant = Arc::new(Participant {
            id: "checkpoint",
            phase: None,
            calls: calls.clone(),
        });
        let registration = registry.register(participant.clone()).unwrap();
        assert!(registry.register(participant).is_err());
        registration();
        let participant = Arc::new(Participant {
            id: "checkpoint",
            phase: None,
            calls: calls.clone(),
        });
        let _registration = registry.register(participant).unwrap();
        let after_flush = Arc::new(Participant {
            id: "after",
            phase: Some("after-flush"),
            calls: calls.clone(),
        });
        let _after_registration = registry.register(after_flush).unwrap();
        reconcile_participants(&registry, None).await;
        assert_eq!(*calls.lock(), 1);
        reconcile_participants(&registry, Some("after-flush")).await;
        assert_eq!(*calls.lock(), 2);
    }
}
