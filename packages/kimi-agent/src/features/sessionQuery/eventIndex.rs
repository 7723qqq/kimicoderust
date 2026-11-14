use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::events::SessionEventSearchDocument;

#[derive(Clone)]
struct CachedSession {
    revision: Option<u64>,
    events: Arc<Vec<SessionEventSearchDocument>>,
}

#[derive(Default)]
pub struct SessionEventIndex {
    cache: Mutex<BTreeMap<String, CachedSession>>,
}

impl SessionEventIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn wire_scope_of(scope: &str, workspace_id: &str, session_id: &str) -> String {
        format!("{scope}/{workspace_id}/{session_id}/agents/main")
    }

    pub fn cached_events(
        &self,
        session_id: &str,
        revision: Option<u64>,
    ) -> Option<Arc<Vec<SessionEventSearchDocument>>> {
        self.cache
            .lock()
            .unwrap()
            .get(session_id)
            .filter(|cached| cached.revision == revision)
            .map(|cached| cached.events.clone())
    }

    pub fn replace_events(
        &self,
        session_id: &str,
        revision: Option<u64>,
        events: Vec<SessionEventSearchDocument>,
    ) -> Arc<Vec<SessionEventSearchDocument>> {
        let events = Arc::new(events);
        self.cache.lock().unwrap().insert(
            session_id.to_string(),
            CachedSession {
                revision,
                events: events.clone(),
            },
        );
        events
    }

    pub fn invalidate(&self, session_id: &str) {
        self.cache.lock().unwrap().remove(session_id);
    }
}
