use serde_json::json;
use std::sync::Arc;

use super::agent_state::{ReplayableStateMeta, StateInspector, StateKey};
use super::agent_state_service::AgentStateService;

fn key(name: &str, initial: serde_json::Value) -> StateKey {
    StateKey::new(name, Arc::new(move || initial.clone()))
}

#[test]
fn set_and_get_round_trip_and_notify() {
    let service = Arc::new(AgentStateService::new());
    let counter = key("counter", json!(0));
    let _registration = service.contribute_state(counter.clone()).unwrap();

    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let emitter = service.on_did_change(&counter);
    let sink = seen.clone();
    let _listener = emitter.add(move |value| {
        sink.lock().push(value.clone());
        Ok(())
    });

    service.set(&counter, json!(1)).unwrap();

    assert_eq!(service.get(&counter).unwrap(), json!(1));
    assert_eq!(*seen.lock(), vec![json!(1)]);
    assert_eq!(service.snapshot().get("counter"), Some(&json!(1)));
    assert_eq!(service.entries(), vec![("counter".to_string(), json!(1))]);
}

#[test]
fn duplicate_registration_is_rejected_with_v2_message() {
    let service = Arc::new(AgentStateService::new());
    let first = key("dup", json!(null));
    let _registration = service.contribute_state(first.clone()).unwrap();
    let error = service.contribute_state(first).unwrap_err();
    assert_eq!(error.message, "state key 'dup' is already registered");
}

#[test]
fn unregistered_key_reports_v2_message() {
    let service = Arc::new(AgentStateService::new());
    let missing = key("missing", json!(null));
    assert_eq!(
        service.get(&missing).unwrap_err().message,
        "state key 'missing' is not registered"
    );
    assert_eq!(
        service.set(&missing, json!(1)).unwrap_err().message,
        "state key 'missing' is not registered"
    );
}

#[test]
fn snapshot_excluded_keys_stay_out_of_snapshot_but_not_entries() {
    let service = Arc::new(AgentStateService::new());
    let transient = key("transient", json!("x")).snapshot_excluded(true);
    let _registration = service.contribute_state(transient.clone()).unwrap();

    assert!(!service.snapshot().contains_key("transient"));
    assert!(
        service
            .entries()
            .iter()
            .any(|(name, _)| name == "transient")
    );
    let inspection = service.inspect();
    assert_eq!(inspection.scope, "agent");
    assert!(!inspection.state.contains_key("transient"));
}

#[test]
fn replayable_keys_track_contribute_and_withdraw() {
    let service = Arc::new(AgentStateService::new());
    let replayable = key("replay", json!(0)).replayable(ReplayableStateMeta::new(true, false));
    let plain = key("plain", json!(0));

    let contributed = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = contributed.clone();
    let _on_contribute = service.on_did_contribute_replayable().add(move |key| {
        sink.lock().push(key.name.clone());
        Ok(())
    });

    let withdrawn = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = withdrawn.clone();
    let _on_withdraw = service.on_did_withdraw_replayable().add(move |key| {
        sink.lock().push(key.name.clone());
        Ok(())
    });

    let replay_registration = service.contribute_state(replayable.clone()).unwrap();
    let _plain_registration = service.contribute_state(plain).unwrap();

    assert_eq!(
        service
            .replayable_keys()
            .iter()
            .map(|key| key.name.clone())
            .collect::<Vec<_>>(),
        vec!["replay".to_string()]
    );
    assert_eq!(*contributed.lock(), vec!["replay".to_string()]);

    replay_registration.dispose();

    assert!(service.replayable_keys().is_empty());
    assert_eq!(*withdrawn.lock(), vec!["replay".to_string()]);
    assert!(!service.has(&replayable));
}

#[test]
fn failing_contribute_listener_rolls_the_registration_back() {
    let service = Arc::new(AgentStateService::new());
    let replayable = key("replay", json!(0)).replayable(ReplayableStateMeta::new(true, false));

    let withdrawn = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = withdrawn.clone();
    let _on_withdraw = service.on_did_withdraw_replayable().add(move |key| {
        sink.lock().push(key.name.clone());
        Ok(())
    });

    let _failing = service
        .on_did_contribute_replayable()
        .add(|_| Err(super::agent_state::StateError::new("boom")));

    let error = service.contribute_state(replayable.clone()).unwrap_err();
    assert_eq!(error.message, "boom");
    assert!(!service.has(&replayable));
    assert!(service.replayable_keys().is_empty());
    assert_eq!(*withdrawn.lock(), vec!["replay".to_string()]);
}

#[test]
fn inspection_chains_to_the_parent_scope() {
    let parent = Arc::new(AgentStateService::new());
    let parent_key = key("session_value", json!(7));
    let _parent_registration = parent.contribute_state(parent_key).unwrap();

    let child = Arc::new(AgentStateService::with_parent(parent));
    let child_key = key("agent_value", json!(1));
    let _child_registration = child.contribute_state(child_key).unwrap();

    let inspection = child.inspect();
    assert_eq!(inspection.scope, "agent");
    assert_eq!(inspection.state.get("agent_value"), Some(&json!(1)));
    let parent_inspection = inspection.parent.expect("parent scope is inspected");
    assert_eq!(parent_inspection.scope, "agent");
    assert_eq!(
        parent_inspection.state.get("session_value"),
        Some(&json!(7))
    );
}
