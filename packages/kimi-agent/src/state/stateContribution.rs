use std::collections::HashMap;
use std::sync::Arc;

use crate::app::event::errors::{EVENT_DUPLICATE_EVENT, EventError};
use crate::app::event::event2::{EVENT2_REGISTRY, Event2Class};
use crate::_base::errors::unexpected_error::on_unexpected_error;

use super::state::{ReplayableStateKey, StateFold, expanded_state_folds};

#[derive(Clone)]
pub struct EventStateContributionRecord {
    pub events: Option<Vec<Arc<Event2Class>>>,
}

#[derive(Clone)]
pub struct StateFoldRegistration {
    pub key: Arc<ReplayableStateKey>,
    pub fold: StateFold,
}

pub struct FoldedEventStateRegistry {
    pub events: HashMap<String, Arc<Event2Class>>,
    pub folds: HashMap<String, Vec<StateFoldRegistration>>,
    pub states: Vec<Arc<ReplayableStateKey>>,
}

pub fn fold_event_state_contributions(
    records: &[EventStateContributionRecord],
    replayable_keys: &[Arc<ReplayableStateKey>],
) -> FoldedEventStateRegistry {
    let mut events = HashMap::new();
    let mut classes_by_address = HashMap::new();
    let mut folds = HashMap::<String, Vec<StateFoldRegistration>>::new();
    let mut states = Vec::new();
    for cls in EVENT2_REGISTRY.lock().values() {
        events.insert(cls.type_.clone(), cls.clone());
        classes_by_address.insert(Arc::as_ptr(cls) as usize, cls.clone());
    }
    for key in replayable_keys {
        states.push(key.clone());
        for (address, fold) in expanded_state_folds(key).unwrap() {
            let Some(cls) = classes_by_address.get(&address) else {
                continue;
            };
            let cls = cls.clone();
            folds.entry(cls.type_.clone()).or_default().push(StateFoldRegistration {
                key: key.clone(),
                fold,
            });
            if cls.durable {
                events.entry(cls.type_.clone()).or_insert(cls);
            }
        }
    }
    for record in records {
        for cls in record.events.clone().unwrap_or_default() {
            if events.contains_key(&cls.type_) {
                on_unexpected_error(Box::new(EventError::with_options(
                    EVENT_DUPLICATE_EVENT,
                    format!(
                        "Duplicate event type contributed: '{}'; keeping the already-folded registration",
                        cls.type_
                    ),
                    kimi_core::primitives::errors::Error2Options {
                        details: Some(serde_json::Map::from_iter([(
                            "type".to_string(),
                            cls.type_.clone().into(),
                        )])),
                        ..Default::default()
                    },
                )));
                continue;
            }
            events.insert(cls.type_.clone(), cls);
        }
    }
    FoldedEventStateRegistry {
        events,
        folds,
        states,
    }
}
