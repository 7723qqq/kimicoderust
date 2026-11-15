use std::sync::Arc;

use serde_json::{Value, json};

use crate::_base::state::state_registry::{IStateRegistry, StateKey, StateRegistry};
use crate::agent::state::agent_state_service::AgentStateService;
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::state::errors::StateError;
use crate::session::state::session_state::ISessionStateService;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

fn key(name: &str, initial: Value) -> StateKey<Value> {
    StateKey::new(name, move || initial.clone())
}

fn replayable_key(name: &str, initial: Value) -> Arc<ReplayableStateKey> {
    Arc::new(ReplayableStateKey::new(
        name.to_string(),
        Arc::new(move || initial.clone()),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    ))
}

fn entries(service: &AgentStateService) -> Vec<(String, Value)> {
    service
        .entries()
        .into_iter()
        .map(|(name, value)| {
            let value = Arc::downcast::<Value>(value)
                .expect("agent state values are JSON")
                .as_ref()
                .clone();
            (name, value)
        })
        .collect()
}

#[test]
fn set_and_get_round_trip_and_notify() {
    let service = AgentStateService::new(None);
    let counter = key("counter", json!(0));
    let _registration = service.contribute_state(counter.clone()).unwrap();

    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _subscription = service.on_did_change(&counter).subscribe(move |value| {
        sink.lock().push(value);
    });

    service.set(&counter, json!(1));

    assert_eq!(service.get(&counter), json!(1));
    assert_eq!(*seen.lock(), vec![json!(1)]);
    assert_eq!(
        IStateRegistry::snapshot(&service).get("counter"),
        Some(&json!(1))
    );
    assert_eq!(
        entries(&service),
        vec![("counter".to_string(), json!(1))]
    );
}

#[test]
fn duplicate_registration_is_rejected_with_v2_message() {
    let service = AgentStateService::new(None);
    let first = key("dup", json!(null));
    let _registration = service.contribute_state(first.clone()).unwrap();
    let error = match service.contribute_state(first) {
        Ok(_) => panic!("a duplicate registration must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.message, "state key 'dup' is already registered");
}

#[test]
#[should_panic(expected = "state key 'missing' is not registered")]
fn unregistered_key_get_panics_with_v2_message() {
    let service = AgentStateService::new(None);
    service.get(&key("missing", json!(null)));
}

#[test]
#[should_panic(expected = "state key 'missing' is not registered")]
fn unregistered_key_set_panics_with_v2_message() {
    let service = AgentStateService::new(None);
    service.set(&key("missing", json!(null)), json!(1));
}

#[test]
fn snapshot_excluded_keys_stay_out_of_snapshot_but_not_entries() {
    let service = AgentStateService::new(None);
    let transient = StateKey {
        name: "transient".to_string(),
        initial: Arc::new(|| json!("x")),
        snapshot_excluded: true,
        replayable: None,
    };
    let _registration = service.contribute_state(transient).unwrap();

    assert!(!IStateRegistry::snapshot(&service).contains_key("transient"));
    assert!(
        entries(&service)
            .iter()
            .any(|(name, _)| name == "transient")
    );
    let inspection = service.inspect();
    assert_eq!(inspection.scope, "agent");
    assert!(!inspection.state.contains_key("transient"));
}

#[test]
fn replayable_keys_track_contribute_and_withdraw() {
    let service = AgentStateService::new(None);
    let replayable = replayable_key("replay", json!(0));
    let plain = key("plain", json!(0));

    let contributed = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = contributed.clone();
    let _on_contribute = service.on_did_contribute_replayable(Arc::new(move |key| {
        sink.lock().push(key.name.clone());
        Ok(())
    }));

    let withdrawn = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = withdrawn.clone();
    let _on_withdraw = service.on_did_withdraw_replayable(Arc::new(move |key| {
        sink.lock().push(key.name.clone());
        Ok(())
    }));

    let replay_registration = service.contribute_state(replayable.state_key()).unwrap();
    let _plain_registration = service.contribute_state(plain).unwrap();

    let tracked = service.replayable_keys();
    assert_eq!(
        tracked
            .iter()
            .map(|key| key.name.clone())
            .collect::<Vec<_>>(),
        vec!["replay".to_string()]
    );
    assert!(
        Arc::ptr_eq(&tracked[0], &replayable),
        "replayableKeys must hand back the contributed Arc itself"
    );
    assert_eq!(*contributed.lock(), vec!["replay".to_string()]);

    replay_registration.dispose();

    assert!(service.replayable_keys().is_empty());
    assert_eq!(*withdrawn.lock(), vec!["replay".to_string()]);
    assert!(!service.has(&replayable.state_key()));
}

#[test]
fn failing_contribute_listener_rolls_the_registration_back() {
    let service = AgentStateService::new(None);
    let replayable = replayable_key("replay", json!(0));

    let withdrawn = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = withdrawn.clone();
    let _on_withdraw = service.on_did_withdraw_replayable(Arc::new(move |key| {
        sink.lock().push(key.name.clone());
        Ok(())
    }));

    let _failing =
        service.on_did_contribute_replayable(Arc::new(|_| Err(StateError::new("boom"))));

    let error = match service.contribute_state(replayable.state_key()) {
        Ok(_) => panic!("a failing contribute listener must roll the registration back"),
        Err(error) => error,
    };
    assert_eq!(error.message, "boom");
    assert!(!service.has(&replayable.state_key()));
    assert!(service.replayable_keys().is_empty());
    assert_eq!(*withdrawn.lock(), vec!["replay".to_string()]);
}

#[test]
fn inspection_chains_to_the_parent_scope() {
    let parent = StateRegistry::with_inspect("session", None);
    let _parent_registration = parent
        .contribute_state(key("session_value", json!(7)))
        .unwrap();
    let parent_service: Arc<dyn ISessionStateService> = parent;
    let child = AgentStateService::new(Some(parent_service));
    let _child_registration = child
        .contribute_state(key("agent_value", json!(1)))
        .unwrap();

    let inspection = child.inspect();
    assert_eq!(inspection.scope, "agent");
    assert_eq!(inspection.state.get("agent_value"), Some(&json!(1)));
    let parent_inspection = inspection.parent.expect("parent scope is inspected");
    assert_eq!(parent_inspection.scope, "session");
    assert_eq!(
        parent_inspection.state.get("session_value"),
        Some(&json!(7))
    );
}
