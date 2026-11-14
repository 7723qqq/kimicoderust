use std::sync::{Arc, LazyLock};

use serde_json::{Value, json};

use crate::app::event::event2::Event2Class;
use crate::permission::PermissionMode;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

pub const PERMISSION_SET_MODE_EVENT: &str = "permission.set_mode";

pub const AGENT_STATUS_UPDATED_EVENT: &str = "agent.status.updated";

pub const PERMISSION_MODE_KEY_NAME: &str = "permissionMode";

pub const PERMISSION_MODE_CONFIGURED_KEY_NAME: &str = "permissionMode.configured";

pub static PERMISSION_SET_MODE_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: PERMISSION_SET_MODE_EVENT.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "mode"],
            "properties": {
                "agentId": {"type": "string"},
                "mode": {},
            },
        })),
    })
});

pub static PERMISSION_MODE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        PERMISSION_MODE_KEY_NAME.to_string(),
        Arc::new(|| Value::String("manual".to_string())),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    key.on(&PERMISSION_SET_MODE_CLASS, Arc::new(|_state, event, _ctx| {
        event.payload.get("mode").cloned()
    }))
    .expect("permission mode fold can be registered");
    Arc::new(key)
});

pub static PERMISSION_MODE_CONFIGURED_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        let mut key = ReplayableStateKey::new(
            PERMISSION_MODE_CONFIGURED_KEY_NAME.to_string(),
            Arc::new(|| Value::Bool(false)),
            ReplayableOptions {
                schema: json!({}),
                durable: None,
                blobs: None,
                mutable_replay: None,
            },
        );
        key.on(&PERMISSION_SET_MODE_CLASS, Arc::new(|_state, _event, _ctx| {
            Some(Value::Bool(true))
        }))
        .expect("permission mode configured fold can be registered");
        Arc::new(key)
    });

pub fn permission_mode_key() -> Arc<ReplayableStateKey> {
    PERMISSION_MODE_KEY.clone()
}

pub fn permission_mode_configured_key() -> Arc<ReplayableStateKey> {
    PERMISSION_MODE_CONFIGURED_KEY.clone()
}

pub fn mode_as_str(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Manual => "manual",
        PermissionMode::Auto => "auto",
        PermissionMode::Yolo => "yolo",
        PermissionMode::Unknown => "unknown",
    }
}

pub fn mode_from_value(value: Option<&Value>) -> PermissionMode {
    match value.and_then(Value::as_str) {
        Some("auto") => PermissionMode::Auto,
        Some("yolo") => PermissionMode::Yolo,
        _ => PermissionMode::Manual,
    }
}

pub fn parse_mode(mode: &str) -> Option<PermissionMode> {
    match mode {
        "manual" => Some(PermissionMode::Manual),
        "auto" => Some(PermissionMode::Auto),
        "yolo" => Some(PermissionMode::Yolo),
        _ => None,
    }
}

pub fn recorded_mode(
    store: &crate::session::sqlite_store::SqliteSessionStore,
    session_id: &str,
    agent_id: &str,
) -> Option<PermissionMode> {
    store
        .wire_event_payloads(session_id, PERMISSION_SET_MODE_EVENT)
        .ok()?
        .into_iter()
        .rev()
        .find(|payload| payload.get("agentId").and_then(Value::as_str) == Some(agent_id))
        .and_then(|payload| {
            payload
                .get("mode")
                .and_then(Value::as_str)
                .and_then(parse_mode)
        })
}

pub fn record_mode(
    store: &crate::session::sqlite_store::SqliteSessionStore,
    session_id: &str,
    agent_id: &str,
    mode: PermissionMode,
    now_ms: i64,
) -> Result<bool, String> {
    if recorded_mode(store, session_id, agent_id) == Some(mode) {
        return Ok(false);
    }
    let event = permission_set_mode_event(agent_id, mode);
    let payload = serde_json::to_string(&event).map_err(|error| error.to_string())?;
    store
        .append_wire_event_json(
            &format!("wevt-{}", fastrand::u64(..)),
            session_id,
            PERMISSION_SET_MODE_EVENT,
            &payload,
            false,
            false,
            now_ms,
        )
        .map_err(|error| error.to_string())?;
    Ok(true)
}

pub fn permission_set_mode_event(agent_id: &str, mode: PermissionMode) -> Value {
    json!({
        "type": PERMISSION_SET_MODE_EVENT,
        "agentId": agent_id,
        "mode": mode_as_str(mode),
    })
}

pub fn permission_status_event(agent_id: &str, mode: PermissionMode) -> Value {
    json!({
        "type": AGENT_STATUS_UPDATED_EVENT,
        "agentId": agent_id,
        "permission": mode_as_str(mode),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_replayable_with_v2_defaults() {
        let mode = permission_mode_key();
        assert_eq!(mode.name, PERMISSION_MODE_KEY_NAME);
        assert_eq!((mode.initial)(), json!("manual"));
        assert!(mode.replayable.durable);
        assert!(!mode.replayable.mutable_replay);

        let configured = permission_mode_configured_key();
        assert_eq!(configured.name, PERMISSION_MODE_CONFIGURED_KEY_NAME);
        assert_eq!((configured.initial)(), json!(false));
        assert!(configured.replayable.durable);
        assert!(!configured.replayable.mutable_replay);
    }

    #[test]
    fn mode_strings_round_trip() {
        for mode in [
            PermissionMode::Manual,
            PermissionMode::Auto,
            PermissionMode::Yolo,
        ] {
            assert_eq!(
                mode_from_value(Some(&json!(mode_as_str(mode)))),
                mode,
                "{mode:?}"
            );
        }
        assert_eq!(mode_as_str(PermissionMode::Unknown), "unknown");
        assert_eq!(mode_from_value(None), PermissionMode::Manual);
        assert_eq!(
            mode_from_value(Some(&json!("plan"))),
            PermissionMode::Manual
        );
        assert_eq!(mode_from_value(Some(&Value::Null)), PermissionMode::Manual);
    }

    #[test]
    fn parse_mode_accepts_only_the_three_recorded_modes() {
        assert_eq!(parse_mode("manual"), Some(PermissionMode::Manual));
        assert_eq!(parse_mode("auto"), Some(PermissionMode::Auto));
        assert_eq!(parse_mode("yolo"), Some(PermissionMode::Yolo));
        assert_eq!(parse_mode("plan"), None);
        assert_eq!(parse_mode(""), None);
    }

    #[test]
    fn recorded_mode_keeps_the_last_record_per_agent() {
        let store = crate::session::sqlite_store::SqliteSessionStore::in_memory().unwrap();
        store.create_session("sess-mode", None).unwrap();
        assert_eq!(recorded_mode(&store, "sess-mode", "main"), None);

        assert!(record_mode(&store, "sess-mode", "main", PermissionMode::Auto, 1).unwrap());
        assert_eq!(
            recorded_mode(&store, "sess-mode", "main"),
            Some(PermissionMode::Auto)
        );
        assert!(
            !record_mode(&store, "sess-mode", "main", PermissionMode::Auto, 2).unwrap(),
            "an unchanged mode is not recorded twice"
        );
        assert!(record_mode(&store, "sess-mode", "agent-7", PermissionMode::Yolo, 3).unwrap());
        assert!(record_mode(&store, "sess-mode", "main", PermissionMode::Manual, 4).unwrap());
        assert_eq!(
            recorded_mode(&store, "sess-mode", "main"),
            Some(PermissionMode::Manual),
            "the newest record for the agent wins"
        );
        assert_eq!(
            recorded_mode(&store, "sess-mode", "agent-7"),
            Some(PermissionMode::Yolo)
        );
        assert_eq!(recorded_mode(&store, "sess-mode", "agent-other"), None);
    }

    #[test]
    fn events_carry_the_v2_payload_shape() {
        assert_eq!(
            permission_set_mode_event("main", PermissionMode::Auto),
            json!({"type": "permission.set_mode", "agentId": "main", "mode": "auto"})
        );
        assert_eq!(
            permission_status_event("main", PermissionMode::Yolo),
            json!({"type": "agent.status.updated", "agentId": "main", "permission": "yolo"})
        );
    }
}
