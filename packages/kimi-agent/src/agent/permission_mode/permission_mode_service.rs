use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::lifecycle::DisposableRef;
use crate::agent::state::errors::StateError;
use crate::agent::state::listeners::Listeners;
use crate::agent::state::agent_state::IAgentStateService;
use crate::callbacks::MAIN_AGENT_ID;
use crate::events::bus::EventBus;
use crate::permission::{PermissionEngine, PermissionMode};
use crate::session::sqlite_store::SqliteSessionStore;

use super::permission_mode_ops::{
    mode_as_str, mode_from_value, permission_mode_configured_key, permission_mode_key,
    permission_set_mode_event, permission_status_event, record_mode,
};
use super::permission_mode_replay::fold_permission_mode;

pub use super::permission_mode::PermissionModeChangedContext;

#[derive(Debug, Default)]
pub struct AgentPermissionModes {
    session: RwLock<Option<PermissionMode>>,
    bound: RwLock<HashMap<String, PermissionMode>>,
}

impl AgentPermissionModes {
    pub fn set_session_mode(&self, mode: PermissionMode) {
        *self.session.write().unwrap_or_else(|error| error.into_inner()) = Some(mode);
    }

    pub fn session_mode(&self) -> Option<PermissionMode> {
        *self.session.read().unwrap_or_else(|error| error.into_inner())
    }

    pub fn bind(&self, agent_id: &str, mode: PermissionMode) {
        self.bound
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(agent_id.to_string(), mode);
    }

    pub fn is_bound(&self, agent_id: &str) -> bool {
        self.bound
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(agent_id)
    }

    pub fn mode_for(&self, agent_id: &str) -> Option<PermissionMode> {
        self.bound
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(agent_id)
            .copied()
            .or_else(|| self.session_mode())
    }
}

pub type SharedAgentPermissionModes = Arc<AgentPermissionModes>;

pub trait PermissionModeHost: Send + Sync {
    fn emit_event(&self, event: Value);

    fn broadcast_permission_mode(&self, mode: PermissionMode);

    fn track_toggle(&self, event: &str, enabled: bool);
}

pub type PermissionModeToggleSink = Arc<dyn Fn(&str, bool) + Send + Sync>;

pub struct EventBusPermissionModeHost {
    bus: Arc<EventBus>,
    engine: Option<Arc<PermissionEngine>>,
    modes: SharedAgentPermissionModes,
    store: Option<(Arc<SqliteSessionStore>, String)>,
    toggle_sink: PermissionModeToggleSink,
}

impl EventBusPermissionModeHost {
    pub fn new(
        bus: Arc<EventBus>,
        engine: Option<Arc<PermissionEngine>>,
        modes: SharedAgentPermissionModes,
        store: Option<(Arc<SqliteSessionStore>, String)>,
        toggle_sink: PermissionModeToggleSink,
    ) -> Self {
        if let Some(engine) = &engine {
            engine.set_agent_permission_modes(modes.clone());
        }
        Self {
            bus,
            engine,
            modes,
            store,
            toggle_sink,
        }
    }
}

impl PermissionModeHost for EventBusPermissionModeHost {
    fn emit_event(&self, event: Value) {
        if event.get("type").and_then(Value::as_str) == Some("permission.set_mode") {
            let agent_id = event
                .get("agentId")
                .and_then(Value::as_str)
                .unwrap_or(MAIN_AGENT_ID);
            if let Some(mode) =
                event
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(|mode| match mode {
                        "auto" => Some(PermissionMode::Auto),
                        "yolo" => Some(PermissionMode::Yolo),
                        "manual" => Some(PermissionMode::Manual),
                        _ => None,
                    })
            {
                if agent_id == MAIN_AGENT_ID
                    && let Some(engine) = &self.engine
                {
                    engine.set_mode(mode);
                }
                self.modes.bind(agent_id, mode);
                if let Some((store, session_id)) = &self.store {
                    let now = chrono::Utc::now().timestamp_millis();
                    if let Err(error) = record_mode(store, session_id, agent_id, mode, now) {
                        tracing::warn!(%session_id, %agent_id, %error, "failed to persist permission.set_mode");
                    }
                }
            }
        }
        self.bus.publish_json(event);
    }

    fn broadcast_permission_mode(&self, mode: PermissionMode) {
        if let Some(engine) = &self.engine {
            engine.set_mode(mode);
        }
        self.modes.set_session_mode(mode);
        self.bus
            .publish_json(permission_status_event(MAIN_AGENT_ID, mode));
    }

    fn track_toggle(&self, event: &str, enabled: bool) {
        (self.toggle_sink)(event, enabled);
    }
}

pub struct AgentPermissionModeService {
    state: Arc<dyn IAgentStateService>,
    agent_id: String,
    host: Arc<dyn PermissionModeHost>,
    changed: Arc<Listeners<PermissionModeChangedContext>>,
    _registrations: Mutex<Vec<DisposableRef>>,
}

impl AgentPermissionModeService {
    pub fn new(
        state: Arc<dyn IAgentStateService>,
        agent_id: impl Into<String>,
        host: Arc<dyn PermissionModeHost>,
    ) -> Result<Self, StateError> {
        let mode_registration = state.contribute_state(permission_mode_key().state_key())?;
        let configured_registration =
            state.contribute_state(permission_mode_configured_key().state_key())?;
        Ok(Self {
            state,
            agent_id: agent_id.into(),
            host,
            changed: Arc::new(Listeners::default()),
            _registrations: Mutex::new(vec![mode_registration, configured_registration]),
        })
    }

    pub fn new_restored(
        state: Arc<dyn IAgentStateService>,
        agent_id: impl Into<String>,
        host: Arc<dyn PermissionModeHost>,
        store: &SqliteSessionStore,
        session_id: &str,
    ) -> Result<Self, StateError> {
        let agent_id = agent_id.into();
        let service = Self::new(state, agent_id.clone(), host)?;
        let restored = fold_permission_mode(store, session_id, &agent_id);
        service.state.set(
            &permission_mode_key().state_key(),
            Value::String(mode_as_str(restored.mode).to_string()),
        );
        service.state.set(
            &permission_mode_configured_key().state_key(),
            Value::Bool(restored.configured),
        );
        Ok(service)
    }

    pub fn on_did_change_mode(&self) -> Arc<Listeners<PermissionModeChangedContext>> {
        self.changed.clone()
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn mode(&self) -> PermissionMode {
        mode_from_value(Some(&self.state.get(&permission_mode_key().state_key())))
    }

    pub fn configured(&self) -> bool {
        self.state
            .get(&permission_mode_configured_key().state_key())
            .as_bool()
            .unwrap_or(false)
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        let previous_mode = self.mode();
        let changed = mode != previous_mode;
        if !changed && self.configured() {
            return;
        }
        self.state.set(
            &permission_mode_key().state_key(),
            Value::String(mode_as_str(mode).into()),
        );
        self.state.set(
            &permission_mode_configured_key().state_key(),
            Value::Bool(true),
        );
        self.host
            .emit_event(permission_set_mode_event(&self.agent_id, mode));
        self.host
            .emit_event(permission_status_event(&self.agent_id, mode));
        if changed {
            let _ = self.changed.fire(&PermissionModeChangedContext {
                mode,
                previous_mode,
            });
        }
    }

    pub fn set_mode_and_broadcast(&self, mode: PermissionMode) {
        let was_yolo = self.mode() == PermissionMode::Yolo;
        let was_auto = self.mode() == PermissionMode::Auto;
        self.set_mode(mode);
        if self.agent_id == MAIN_AGENT_ID {
            self.host.broadcast_permission_mode(mode);
        }
        let yolo_enabled = self.mode() == PermissionMode::Yolo;
        if yolo_enabled != was_yolo {
            self.host.track_toggle("yolo_toggle", yolo_enabled);
        }
        let afk_enabled = self.mode() == PermissionMode::Auto;
        if afk_enabled != was_auto {
            self.host.track_toggle("afk_toggle", afk_enabled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::agent_state_service::AgentStateService;
    use serde_json::json;

    #[derive(Default)]
    struct HostProbe {
        events: Mutex<Vec<Value>>,
        broadcasts: Mutex<Vec<PermissionMode>>,
        toggles: Mutex<Vec<(String, bool)>>,
    }

    impl PermissionModeHost for HostProbe {
        fn emit_event(&self, event: Value) {
            self.events.lock().push(event);
        }

        fn broadcast_permission_mode(&self, mode: PermissionMode) {
            self.broadcasts.lock().push(mode);
        }

        fn track_toggle(&self, event: &str, enabled: bool) {
            self.toggles.lock().push((event.to_string(), enabled));
        }
    }

    fn service(
        agent_id: &str,
    ) -> (
        Arc<dyn IAgentStateService>,
        Arc<HostProbe>,
        AgentPermissionModeService,
    ) {
        let state: Arc<dyn IAgentStateService> = Arc::new(AgentStateService::new(None));
        let host = Arc::new(HostProbe::default());
        let service = AgentPermissionModeService::new(state.clone(), agent_id, host.clone())
            .expect("fresh state accepts the permission-mode keys");
        (state, host, service)
    }

    #[test]
    fn defaults_are_manual_and_unconfigured() {
        let (state, _host, service) = service(MAIN_AGENT_ID);
        assert_eq!(service.mode(), PermissionMode::Manual);
        assert!(!service.configured());
        assert_eq!(
            state.get(&permission_mode_key().state_key()),
            json!("manual")
        );
        assert_eq!(
            state.get(&permission_mode_configured_key().state_key()),
            json!(false)
        );
    }

    #[test]
    fn set_mode_records_state_and_emits_events() {
        let (state, host, service) = service(MAIN_AGENT_ID);
        let listener = service.on_did_change_mode().add(|_| Ok(()));
        assert_eq!(service.on_did_change_mode().len(), 1);

        service.set_mode(PermissionMode::Auto);
        assert_eq!(service.mode(), PermissionMode::Auto);
        assert!(service.configured());
        assert_eq!(
            state.get(&permission_mode_key().state_key()),
            json!("auto")
        );
        assert_eq!(
            state.get(&permission_mode_configured_key().state_key()),
            json!(true)
        );
        assert_eq!(
            *host.events.lock(),
            vec![
                json!({"type": "permission.set_mode", "agentId": "main", "mode": "auto"}),
                json!({"type": "agent.status.updated", "agentId": "main", "permission": "auto"}),
            ]
        );
        assert_eq!(service.on_did_change_mode().len(), 1);
        drop(listener);
        assert_eq!(service.on_did_change_mode().len(), 0);
    }

    #[test]
    fn set_mode_notifies_listeners_with_the_change_context() {
        let (_state, _host, service) = service(MAIN_AGENT_ID);
        let seen: Arc<Mutex<Vec<(PermissionMode, PermissionMode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            let _registration = service.on_did_change_mode().add(move |context| {
                seen.lock().push((context.mode, context.previous_mode));
                Ok(())
            });
            service.set_mode(PermissionMode::Yolo);
        }
        assert_eq!(
            *seen.lock(),
            vec![(PermissionMode::Yolo, PermissionMode::Manual)]
        );
    }

    #[test]
    fn an_unchanged_configured_mode_is_not_dispatched_again() {
        let (_state, host, service) = service(MAIN_AGENT_ID);
        service.set_mode(PermissionMode::Auto);
        host.events.lock().clear();
        let seen = Arc::new(Mutex::new(0usize));
        let _registration = service.on_did_change_mode().add({
            let seen = seen.clone();
            move |_| {
                *seen.lock() += 1;
                Ok(())
            }
        });
        service.set_mode(PermissionMode::Auto);
        assert!(host.events.lock().is_empty());
        assert_eq!(*seen.lock(), 0);
    }

    #[test]
    fn an_unchanged_unconfigured_mode_still_dispatches() {
        let (_state, host, service) = service(MAIN_AGENT_ID);
        let seen = Arc::new(Mutex::new(0usize));
        let _registration = service.on_did_change_mode().add({
            let seen = seen.clone();
            move |_| {
                *seen.lock() += 1;
                Ok(())
            }
        });
        service.set_mode(PermissionMode::Manual);
        assert!(service.configured());
        assert_eq!(
            host.events.lock().len(),
            2,
            "the explicit choice is recorded even when it equals the default"
        );
        assert_eq!(*seen.lock(), 0, "no change context without a change");
    }

    #[test]
    fn set_mode_and_broadcast_from_main_broadcasts_and_tracks_toggles() {
        let (_state, host, service) = service(MAIN_AGENT_ID);
        service.set_mode_and_broadcast(PermissionMode::Yolo);
        assert_eq!(*host.broadcasts.lock(), vec![PermissionMode::Yolo]);
        assert_eq!(
            *host.toggles.lock(),
            vec![("yolo_toggle".to_string(), true)]
        );

        service.set_mode_and_broadcast(PermissionMode::Auto);
        assert_eq!(
            *host.broadcasts.lock(),
            vec![PermissionMode::Yolo, PermissionMode::Auto]
        );
        assert_eq!(
            *host.toggles.lock(),
            vec![
                ("yolo_toggle".to_string(), true),
                ("yolo_toggle".to_string(), false),
                ("afk_toggle".to_string(), true)
            ]
        );

        service.set_mode_and_broadcast(PermissionMode::Manual);
        assert_eq!(
            *host.toggles.lock(),
            vec![
                ("yolo_toggle".to_string(), true),
                ("yolo_toggle".to_string(), false),
                ("afk_toggle".to_string(), true),
                ("afk_toggle".to_string(), false)
            ]
        );
    }

    #[test]
    fn set_mode_and_broadcast_from_a_subagent_does_not_broadcast() {
        let (_state, host, service) = service("agent-1");
        service.set_mode_and_broadcast(PermissionMode::Yolo);
        assert!(host.broadcasts.lock().is_empty());
        assert_eq!(
            *host.toggles.lock(),
            vec![("yolo_toggle".to_string(), true)]
        );
    }

    #[test]
    fn contributing_the_same_agent_twice_is_rejected() {
        let state: Arc<dyn IAgentStateService> = Arc::new(AgentStateService::new(None));
        let host = Arc::new(HostProbe::default());
        let _first =
            AgentPermissionModeService::new(state.clone(), MAIN_AGENT_ID, host.clone()).unwrap();
        let error = match AgentPermissionModeService::new(state.clone(), MAIN_AGENT_ID, host) {
            Ok(_) => panic!("a second service must not re-register the agent keys"),
            Err(error) => error,
        };
        assert!(error.message.contains("already registered"), "{error}");
        assert!(
            state.has(&permission_mode_key().state_key()),
            "the failed service must not withdraw the live keys"
        );
    }

    #[test]
    fn event_bus_host_applies_modes_publishes_events_and_records_each_agent() {
        let bus = Arc::new(EventBus::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _subscription = bus.subscribe({
            let seen = seen.clone();
            move |event| seen.lock().push(event.event_type().to_string())
        });
        let engine = Arc::new(PermissionEngine::new(Default::default()));
        let modes = SharedAgentPermissionModes::default();
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        store.create_session("sess-event", None).unwrap();
        let host = Arc::new(EventBusPermissionModeHost::new(
            bus,
            Some(engine.clone()),
            modes.clone(),
            Some((store.clone(), "sess-event".to_string())),
            Arc::new(|_, _| {}),
        ));
        let service = AgentPermissionModeService::new(
            Arc::new(AgentStateService::new(None)),
            MAIN_AGENT_ID,
            host.clone(),
        )
        .unwrap();
        service.set_mode_and_broadcast(PermissionMode::Auto);
        assert_eq!(engine.mode(), PermissionMode::Auto);
        assert_eq!(modes.mode_for("agent-7"), Some(PermissionMode::Auto));
        assert_eq!(
            crate::agent::permission_mode::permission_mode_ops::recorded_mode(
                &store,
                "sess-event",
                MAIN_AGENT_ID,
            ),
            Some(PermissionMode::Auto)
        );

        let subagent =
            AgentPermissionModeService::new(Arc::new(AgentStateService::new(None)), "agent-7", host)
                .unwrap();
        subagent.set_mode(PermissionMode::Yolo);
        assert_eq!(
            crate::agent::permission_mode::permission_mode_ops::recorded_mode(
                &store,
                "sess-event",
                "agent-7",
            ),
            Some(PermissionMode::Yolo)
        );
        assert!(seen.lock().contains(&"permission.set_mode".to_string()));
        assert!(seen.lock().contains(&"agent.status.updated".to_string()));
    }

    #[test]
    fn new_restored_replays_mode_and_configured_independently() {
        let store = SqliteSessionStore::in_memory().unwrap();
        store.create_session("sess-restore", None).unwrap();
        record_mode(
            &store,
            "sess-restore",
            MAIN_AGENT_ID,
            PermissionMode::Auto,
            1,
        )
        .unwrap();
        let service = AgentPermissionModeService::new_restored(
            Arc::new(AgentStateService::new(None)),
            MAIN_AGENT_ID,
            Arc::new(HostProbe::default()),
            &store,
            "sess-restore",
        )
        .unwrap();
        assert_eq!(service.mode(), PermissionMode::Auto);
        assert!(service.configured());
    }

#[test]
    fn broadcast_skips_pinned_agent_permission_modes() {
        let modes = std::sync::Arc::new(AgentPermissionModes::default());
        modes.set_session_mode(PermissionMode::Manual);
        modes.bind("agent-7", PermissionMode::Auto);

        assert_eq!(modes.mode_for("agent-7"), Some(PermissionMode::Auto));
        assert_eq!(modes.mode_for("agent-8"), Some(PermissionMode::Manual));

        modes.set_session_mode(PermissionMode::Yolo);
        assert_eq!(modes.mode_for("agent-7"), Some(PermissionMode::Auto));
        assert_eq!(modes.mode_for("agent-8"), Some(PermissionMode::Yolo));
    }
}