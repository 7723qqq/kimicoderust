use std::sync::Arc;

use serde_json::{Value, json};

use crate::human::eventStore::eventStore::{Cause, CombinedState, EventStoreImpl};
use crate::human::eventStore::events::ExternalEvent;
use crate::human::eventStore::journal::StoreJournal;
use crate::human::xstate2::{FromCallback, XEvent, from_callback};

pub enum StoreActorEvent {
    Append(Vec<ExternalEvent>),
    Switch(Arc<dyn StoreJournal>),
}

impl StoreActorEvent {
    pub fn into_xevent(self) -> XEvent {
        let type_ = match self {
            Self::Append(_) => "store.append",
            Self::Switch(_) => "store.switch",
        };
        XEvent::with_data(type_, Value::Null, Arc::new(self))
    }
}

pub struct StoreActorInput {
    pub store: Arc<EventStoreImpl>,
}

#[derive(Clone)]
pub enum StoreActorEmitted {
    Ready {
        state: CombinedState,
        branch: String,
    },
    Changed {
        state: CombinedState,
    },
    Reset {
        state: CombinedState,
        branch: String,
    },
    Error {
        error: Value,
    },
}

impl StoreActorEmitted {
    fn into_xevent(self) -> XEvent {
        match self {
            Self::Ready { state, branch } => XEvent::new(
                "store.ready",
                json!({ "state": state, "branch": branch }),
            ),
            Self::Changed { state } => {
                XEvent::new("store.changed", json!({ "state": state }))
            }
            Self::Reset { state, branch } => XEvent::new(
                "store.reset",
                json!({ "state": state, "branch": branch }),
            ),
            Self::Error { error } => XEvent::new("store.error", json!({ "error": error })),
        }
    }
}

pub fn store_actor() -> FromCallback {
    from_callback(|input| {
        let runtime = input.runtime.downcast::<StoreActorInput>().unwrap();
        let store = runtime.store.clone();
        let ready = StoreActorEmitted::Ready {
            state: store.get_state(),
            branch: store.ref_().branch,
        };
        input.send_back(ready.into_xevent());
        let send_back = input.send_back.clone();
        let subscribed_store = store.clone();
        let unsubscribe = store.subscribe(Arc::new(move |state, cause| {
            let emitted = match cause {
                Cause::Reset { .. } => StoreActorEmitted::Reset {
                    state,
                    branch: subscribed_store.ref_().branch,
                },
                _ => StoreActorEmitted::Changed { state },
            };
            send_back(emitted.into_xevent());
        }));
        let event_store = store.clone();
        let send_back = input.send_back.clone();
        input.receive(Arc::new(move |event| {
            let store = event_store.clone();
            let send_back = send_back.clone();
            match event.type_.as_str() {
                "store.append" => {
                    let data = event
                        .data
                        .as_ref()
                        .and_then(|data| data.clone().downcast::<StoreActorEvent>().ok());
                    let events = match data.as_deref() {
                        Some(StoreActorEvent::Append(events)) => Some(events.clone()),
                        _ => parse_append_payload(&event.payload),
                    };
                    if let Some(events) = events {
                        tokio::spawn(async move {
                            if let Err(error) = store.dispatch(events).await {
                                send_back(
                                    StoreActorEmitted::Error {
                                        error: json!({
                                            "code": error.code,
                                            "message": error.message,
                                        }),
                                    }
                                    .into_xevent(),
                                );
                            }
                        });
                    }
                }
                "store.switch" => {
                    let data = event
                        .data
                        .as_ref()
                        .and_then(|data| data.clone().downcast::<StoreActorEvent>().ok());
                    if let Some(StoreActorEvent::Switch(journal)) = data.as_deref() {
                        let journal = journal.clone();
                        tokio::spawn(async move {
                            if let Err(error) = store.reset(journal).await {
                                send_back(
                                    StoreActorEmitted::Error {
                                        error: json!({
                                            "code": error.code,
                                            "message": error.message,
                                        }),
                                    }
                                    .into_xevent(),
                                );
                            }
                        });
                    }
                }
                _ => {}
            }
        }));
        Box::new(move || unsubscribe())
    })
}

fn parse_append_payload(payload: &Value) -> Option<Vec<ExternalEvent>> {
    let event = payload.get("event").unwrap_or(payload);
    if let Some(events) = event.as_array() {
        return Some(
            events
                .iter()
                .filter_map(|event| event.as_object().cloned())
                .collect(),
        );
    }
    event.as_object().cloned().map(|event| vec![event])
}
