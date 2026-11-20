use std::sync::{Arc, LazyLock};

use serde_json::json;

use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey, define_state};

pub use crate::tools::tool_select_ops::*;

pub static TOOL_SELECT_INITIALIZED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: TOOL_SELECT_INITIALIZED.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "tools"],
            "properties": {
                "agentId": {"type": "string"},
                "tools": {"type": "array", "items": {}},
            },
        })),
    })
});

pub static TOOL_SELECT_TOOLS_CHANGED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: TOOL_SELECT_TOOLS_CHANGED.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "added", "removed"],
            "properties": {
                "agentId": {"type": "string"},
                "added": {"type": "array", "items": {}},
                "removed": {"type": "array", "items": {}},
            },
        })),
    })
});

impl ToolSelectInitialized {
    pub fn class() -> Arc<Event2Class> {
        TOOL_SELECT_INITIALIZED_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &TOOL_SELECT_INITIALIZED_CLASS,
            serde_json::to_value(self).expect("tool select initialization is serializable"),
            None,
        )
    }
}

impl From<ToolSelectInitialized> for Event2 {
    fn from(event: ToolSelectInitialized) -> Self {
        event.to_event2()
    }
}

impl ToolSelectToolsChanged {
    pub fn class() -> Arc<Event2Class> {
        TOOL_SELECT_TOOLS_CHANGED_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &TOOL_SELECT_TOOLS_CHANGED_CLASS,
            serde_json::to_value(self).expect("tool select change is serializable"),
            None,
        )
    }
}

impl From<ToolSelectToolsChanged> for Event2 {
    fn from(event: ToolSelectToolsChanged) -> Self {
        event.to_event2()
    }
}

pub static TOOL_SELECT_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = define_state(
        "toolSelect",
        Arc::new(|| {
            serde_json::to_value(ToolSelectModelState::default())
                .expect("tool select state is serializable")
        }),
    )
    .replayable(ReplayableOptions {
        schema: json!({}),
        durable: None,
        blobs: None,
        mutable_replay: None,
    });
    Arc::get_mut(&mut key).expect("fresh state key").on(&TOOL_SELECT_INITIALIZED_CLASS, Arc::new(|state, event, _ctx| {
        let mut model = serde_json::from_value::<ToolSelectModelState>(state).ok()?;
        if model.initial.is_some() {
            return None;
        }
        let event = serde_json::from_value::<ToolSelectInitialized>(event.payload.clone()).ok()?;
        model.initialize(event.tools);
        serde_json::to_value(model).ok()
    }))
    .expect("tool select initialization fold can be registered");
    Arc::get_mut(&mut key).expect("fresh state key").on(&TOOL_SELECT_TOOLS_CHANGED_CLASS, Arc::new(|state, event, _ctx| {
        let mut model = serde_json::from_value::<ToolSelectModelState>(state).ok()?;
        let event = serde_json::from_value::<ToolSelectToolsChanged>(event.payload.clone()).ok()?;
        model.change(event.added, event.removed);
        serde_json::to_value(model).ok()
    }))
    .expect("tool select change fold can be registered");
    key
});

pub fn tool_select_key() -> Arc<ReplayableStateKey> {
    TOOL_SELECT_KEY.clone()
}

