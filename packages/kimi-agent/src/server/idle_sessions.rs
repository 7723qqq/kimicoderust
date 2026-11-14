use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::cron::scheduler::CronScheduler;
use crate::events::EngineEvent;
use crate::server::engine::ServerEngine;
use crate::server::hub::EventHub;
use crate::server::interaction::InteractionManager;
use crate::session::idle_reclaim::{IdleSessionControl, SessionIdleReaper};
use crate::session::sqlite_store::SqliteSessionStore;

pub struct ServerIdleControl {
    pub(super) store: Arc<SqliteSessionStore>,
    pub(super) hub: Arc<EventHub>,
    pub(super) engine: Option<Arc<ServerEngine>>,
    pub(super) cron: Arc<tokio::sync::Mutex<CronScheduler>>,
    pub(super) interaction: Arc<InteractionManager>,

    pub(super) reclaimed: Arc<Mutex<HashSet<String>>>,
}

impl IdleSessionControl for ServerIdleControl {
    fn list_sessions_by_activity(&self) -> Vec<(String, i64)> {
        let reclaimed = self
            .reclaimed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.store
            .list_sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|session| {
                !session.archived
                    && session.archived_at.is_none()
                    && !reclaimed.contains(&session.session_id)
            })
            .map(|session| (session.session_id, session.updated_at))
            .collect()
    }

    fn close_if_idle(&self, session_id: &str, _idle_timeout_ms: i64) -> bool {
        {
            let reclaimed = self
                .reclaimed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if reclaimed.contains(session_id) {
                return true;
            }
        }

        if self.is_retained(session_id) {
            return false;
        }
        if let Some(engine) = &self.engine {
            engine.forget_journal_head(session_id);
        }
        self.hub.evict_lane(session_id);
        self.reclaimed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string());

        self.hub
            .bus_for("global")
            .publish(&EngineEvent::Custom(json!({
                "type": "session.reclaimed",
                "sessionId": session_id,
            })));
        true
    }
}

impl ServerIdleControl {
    pub fn is_retained(&self, session_id: &str) -> bool {
        if self
            .engine
            .as_ref()
            .is_some_and(|engine| engine.is_busy(session_id))
        {
            return true;
        }
        if !self.interaction.list_approvals(session_id).is_empty()
            || !self.interaction.list_questions(session_id).is_empty()
        {
            return true;
        }
        match self.cron.try_lock() {
            Ok(scheduler) => scheduler
                .list_entries()
                .iter()
                .any(|entry| entry.session_id.as_deref() == Some(session_id)),

            Err(_) => true,
        }
    }

    pub fn take_reclaimed(&self, session_id: &str) -> bool {
        self.reclaimed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id)
    }

    pub fn clear_reclaimed(&self, session_id: &str) {
        self.reclaimed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }
}

pub(crate) fn remove_session_state_dir(
    store: &SqliteSessionStore,
    session_id: &str,
) -> Result<(), String> {
    let Some(session) = store.get_session(session_id).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    let root = session.cwd.clone().or_else(|| {
        session
            .workspace_id
            .as_deref()
            .and_then(|wid| store.get_workspace(wid).ok().flatten())
            .map(|workspace| workspace.root)
    });
    let Some(root) = root else { return Ok(()) };
    let dir = crate::storage::paths::engine_session_dir(std::path::Path::new(&root), session_id)
        .map_err(|e| e.to_string())?;
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "could not remove the session state directory {}: {e}",
            dir.display()
        )),
    }
}

impl crate::server::HttpServer {
    pub fn idle_reaper(&self) -> Arc<SessionIdleReaper<ServerIdleControl>> {
        self.idle_reaper
            .get_or_init(|| {
                let idle_timeout_ms =
                    crate::session::idle_reclaim::resolve_session_idle_reclaim_ms_from_env()
                        .unwrap_or_else(|error| {
                            tracing::warn!(
                                %error,
                                "session idle reclaim disabled: invalid configuration"
                            );
                            0
                        });
                let control = Arc::new(ServerIdleControl {
                    store: Arc::clone(&self.store),
                    hub: Arc::clone(&self.hub),
                    engine: self.engine.clone(),
                    cron: Arc::clone(&self.cron_scheduler),
                    interaction: Arc::clone(&self.interaction_manager),
                    reclaimed: Arc::clone(&self.reclaimed),
                });
                Arc::new(SessionIdleReaper::new(control, idle_timeout_ms))
            })
            .clone()
    }

    pub fn sweep_idle_sessions(&self) -> usize {
        self.idle_reaper().sweep()
    }

    pub fn take_reclaimed(&self, session_id: &str) -> bool {
        self.reclaimed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id)
    }

    pub fn clear_reclaimed(&self, session_id: &str) {
        self.reclaimed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    pub(crate) fn resume_slot(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut slots = self
            .resume_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            slots
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    pub fn register_session_cleanup_hook(&self, hook: crate::server::SessionCleanupHook) {
        self.session_cleanup_hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(hook);
    }

    pub(crate) fn run_session_cleanup_hooks(&self, session_id: &str) -> Result<(), String> {
        let hooks = self
            .session_cleanup_hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for hook in hooks.iter() {
            hook(session_id)?;
        }
        Ok(())
    }
}
