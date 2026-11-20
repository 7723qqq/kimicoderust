use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Once};

use serde_json::{Map, Value, json};

use crate::human::eventStore::eventStore::SliceMap;
use crate::human::eventStore::events::{EventDefinition, EventSchema, define_event};
use crate::human::eventStore::events::InternalEvent;
use crate::human::eventStore::slice::{FoldContext, Reducer, Slice};
use crate::human::session::events::{
    AGENT_CLOSED_TYPE, AGENT_OPENED_TYPE, AGENT_SWITCHED_TYPE, COMPACTION_CANCELLED_TYPE,
    COMPACTION_COMPLETED_TYPE, COMPACTION_STARTED_TYPE, SESSION_META_UPDATED_TYPE,
};

static REGISTER_EVENTS: Once = Once::new();

fn schema(check: impl Fn(&Map<String, Value>) -> Result<(), String> + Send + Sync + 'static) -> EventSchema {
    Arc::new(check)
}

fn required_string(event: &Map<String, Value>, key: &str) -> Result<(), String> {
    if event.get(key).is_some_and(Value::is_string) {
        Ok(())
    } else {
        Err(format!("{key} must be a string"))
    }
}

fn optional_string(event: &Map<String, Value>, key: &str) -> Result<(), String> {
    if event.get(key).is_none_or(Value::is_string) {
        Ok(())
    } else {
        Err(format!("{key} must be a string"))
    }
}

fn ensure_event_schemas() {
    REGISTER_EVENTS.call_once(|| {
        define_event(EventDefinition {
            type_: AGENT_OPENED_TYPE.to_string(),
            schema: schema(|event| {
                required_string(event, "agentId")?;
                required_string(event, "branch")
            }),
        });
        define_event(EventDefinition {
            type_: AGENT_CLOSED_TYPE.to_string(),
            schema: schema(|event| required_string(event, "agentId")),
        });
        define_event(EventDefinition {
            type_: AGENT_SWITCHED_TYPE.to_string(),
            schema: schema(|event| {
                required_string(event, "agentId")?;
                required_string(event, "branch")?;
                optional_string(event, "reason")?;
                if let Some(stats) = event.get("stats") {
                    let valid = stats.as_object().is_some_and(|stats| {
                        stats.values().all(Value::is_number)
                    });
                    if !valid {
                        return Err("stats must contain numeric values".to_string());
                    }
                }
                Ok(())
            }),
        });
        define_event(EventDefinition {
            type_: SESSION_META_UPDATED_TYPE.to_string(),
            schema: schema(|_| Ok(())),
        });
        define_event(EventDefinition {
            type_: COMPACTION_STARTED_TYPE.to_string(),
            schema: schema(|event| {
                required_string(event, "agentId")?;
                required_string(event, "reason")?;
                optional_string(event, "instruction")
            }),
        });
        define_event(EventDefinition {
            type_: COMPACTION_COMPLETED_TYPE.to_string(),
            schema: schema(|event| {
                required_string(event, "agentId")?;
                required_string(event, "branch")
            }),
        });
        define_event(EventDefinition {
            type_: COMPACTION_CANCELLED_TYPE.to_string(),
            schema: schema(|event| {
                required_string(event, "agentId")?;
                required_string(event, "cause")?;
                optional_string(event, "errorMessage")
            }),
        });
    });
}

pub fn roster_slice() -> Slice<Value> {
    let mut reducers: HashMap<String, Reducer<Value>> = HashMap::new();
    reducers.insert(
        AGENT_OPENED_TYPE.to_string(),
        Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
            let agents = draft
                .as_object_mut()
                .unwrap()
                .entry("agents".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap();
            agents.insert(
                event.get("agentId").and_then(Value::as_str).unwrap().to_string(),
                event.get("branch").cloned().unwrap(),
            );
            None
        }) as Reducer<Value>,
    );
    reducers.insert(
        AGENT_CLOSED_TYPE.to_string(),
        Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
            if let Some(agents) = draft
                .as_object_mut()
                .and_then(|draft| draft.get_mut("agents"))
                .and_then(Value::as_object_mut)
            {
                agents.remove(event.get("agentId").and_then(Value::as_str).unwrap());
            }
            None
        }) as Reducer<Value>,
    );
    reducers.insert(
        AGENT_SWITCHED_TYPE.to_string(),
        Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
            let agents = draft
                .as_object_mut()
                .unwrap()
                .entry("agents".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap();
            agents.insert(
                event.get("agentId").and_then(Value::as_str).unwrap().to_string(),
                event.get("branch").cloned().unwrap(),
            );
            None
        }) as Reducer<Value>,
    );
    Slice {
        name: "roster".to_string(),
        initial_state: Arc::new(|| json!({ "agents": {} })),
        reducers,
    }
}

pub fn session_meta_slice() -> Slice<Value> {
    let mut reducers: HashMap<String, Reducer<Value>> = HashMap::new();
    reducers.insert(
        SESSION_META_UPDATED_TYPE.to_string(),
        Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
            draft.as_object_mut().unwrap().insert(
                "value".to_string(),
                event.get("meta").cloned().unwrap_or(Value::Null),
            );
            None
        }) as Reducer<Value>,
    );
    Slice {
        name: "sessionMeta".to_string(),
        initial_state: Arc::new(|| json!({ "value": null })),
        reducers,
    }
}

pub static SESSION_SLICES: LazyLock<SliceMap> = LazyLock::new(|| {
    ensure_event_schemas();
    let mut slices = SliceMap::new();
    let roster = roster_slice();
    let session_meta = session_meta_slice();
    slices.insert(roster.name.clone(), roster);
    slices.insert(session_meta.name.clone(), session_meta);
    slices
});

pub fn session_slices() -> SliceMap {
    SESSION_SLICES.clone()
}
