use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent::usage::usage_events::{AgentStatusUpdated, AgentStatusUpdatedPayload};
use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey, UndoableOptions, define_state};

pub const PLAN_STATE_KEY: &str = "plan";
pub const PLAN_MODE_ENTER_TYPE: &str = "plan_mode.enter";
pub const PLAN_MODE_CANCEL_TYPE: &str = "plan_mode.cancel";
pub const PLAN_MODE_EXIT_TYPE: &str = "plan_mode.exit";
pub const PLAN_REVISION_TYPE: &str = "plan.revision";

pub static PLAN_MODE_ENTER_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: PLAN_MODE_ENTER_TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "id"],
            "properties": {
                "agentId": {"type": "string"},
                "id": {"type": "string"},
            },
        })),
    })
});

pub static PLAN_MODE_CANCEL_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: PLAN_MODE_CANCEL_TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId"],
            "properties": {
                "agentId": {"type": "string"},
                "id": {"type": "string"},
            },
        })),
    })
});

pub static PLAN_MODE_EXIT_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: PLAN_MODE_EXIT_TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId"],
            "properties": {
                "agentId": {"type": "string"},
                "id": {"type": "string"},
            },
        })),
    })
});

pub static PLAN_REVISION_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: PLAN_REVISION_TYPE.to_string(),
        durable: true,
        observable: true,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "id", "version", "key", "sha256", "bytes"],
            "properties": {
                "agentId": {"type": "string"},
                "id": {"type": "string"},
                "version": {"type": "number"},
                "key": {"type": "string"},
                "sha256": {"type": "string"},
                "bytes": {"type": "number"},
            },
        })),
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanTransition {
    Cancel,
    Exit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanState {
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_count: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition: Option<PlanTransition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeEnter {
    pub agent_id: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeCancel {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanModeExit {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRevision {
    pub agent_id: String,
    pub id: String,
    pub version: u64,
    pub key: String,
    pub sha256: String,
    pub bytes: usize,
}

impl PlanModeEnter {
    pub const TYPE: &'static str = PLAN_MODE_ENTER_TYPE;
    pub const DURABLE: bool = true;

    pub fn class() -> Arc<Event2Class> {
        PLAN_MODE_ENTER_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &PLAN_MODE_ENTER_CLASS,
            serde_json::to_value(self).expect("plan mode enter event is serializable"),
            None,
        )
    }
}

impl From<PlanModeEnter> for Event2 {
    fn from(event: PlanModeEnter) -> Self {
        event.to_event2()
    }
}

impl PlanModeCancel {
    pub const TYPE: &'static str = PLAN_MODE_CANCEL_TYPE;
    pub const DURABLE: bool = true;

    pub fn class() -> Arc<Event2Class> {
        PLAN_MODE_CANCEL_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &PLAN_MODE_CANCEL_CLASS,
            serde_json::to_value(self).expect("plan mode cancel event is serializable"),
            None,
        )
    }
}

impl From<PlanModeCancel> for Event2 {
    fn from(event: PlanModeCancel) -> Self {
        event.to_event2()
    }
}

impl PlanModeExit {
    pub const TYPE: &'static str = PLAN_MODE_EXIT_TYPE;
    pub const DURABLE: bool = true;

    pub fn class() -> Arc<Event2Class> {
        PLAN_MODE_EXIT_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &PLAN_MODE_EXIT_CLASS,
            serde_json::to_value(self).expect("plan mode exit event is serializable"),
            None,
        )
    }
}

impl From<PlanModeExit> for Event2 {
    fn from(event: PlanModeExit) -> Self {
        event.to_event2()
    }
}

impl PlanRevision {
    pub const TYPE: &'static str = PLAN_REVISION_TYPE;
    pub const DURABLE: bool = true;
    pub const OBSERVABLE: bool = true;

    pub fn class() -> Arc<Event2Class> {
        PLAN_REVISION_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &PLAN_REVISION_CLASS,
            serde_json::to_value(self).expect("plan revision event is serializable"),
            None,
        )
    }
}

impl From<PlanRevision> for Event2 {
    fn from(event: PlanRevision) -> Self {
        event.to_event2()
    }
}

pub fn apply_enter(state: &mut PlanState, event: &PlanModeEnter) {
    if event.agent_id != crate::callbacks::MAIN_AGENT_ID {
        return;
    }
    if !(state.active && state.id.as_deref() == Some(event.id.as_str())) {
        state.active = true;
        state.id = Some(event.id.clone());
    }
}

pub fn apply_transition(state: &mut PlanState, transition: PlanTransition) {
    if state.active {
        state.active = false;
        state.id = None;
        state.last_transition = Some(transition);
    }
}

pub fn apply_revision(state: &mut PlanState, event: &PlanRevision) {
    state
        .revision_count
        .get_or_insert_with(BTreeMap::new)
        .insert(event.id.clone(), event.version);
}

pub static PLAN_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = define_state(
        PLAN_STATE_KEY,
        Arc::new(|| {
            serde_json::to_value(PlanState::default()).expect("plan state is serializable")
        }),
    )
    .replayable(ReplayableOptions {
        schema: json!({}),
        durable: None,
        blobs: None,
        mutable_replay: None,
    });
    key.undoable(Some(UndoableOptions { on_undo: None }))
        .expect("plan state can be undoable");
    key.on(&PLAN_MODE_ENTER_CLASS, Arc::new(|state, event, context| {
        let event = serde_json::from_value::<PlanModeEnter>(event.payload.clone()).ok()?;
        if event.agent_id != crate::callbacks::MAIN_AGENT_ID {
            return None;
        }
        let mut plan = serde_json::from_value::<PlanState>(state).ok()?;
        apply_enter(&mut plan, &event);
        context.emit(plan_status_updated(&event.agent_id, true).into());
        serde_json::to_value(plan).ok()
    }))
    .expect("plan mode enter fold can be registered");
    key.on(&PLAN_MODE_CANCEL_CLASS, Arc::new(|state, event, context| {
        let event = serde_json::from_value::<PlanModeCancel>(event.payload.clone()).ok()?;
        let mut plan = serde_json::from_value::<PlanState>(state).ok()?;
        apply_transition(&mut plan, PlanTransition::Cancel);
        context.emit(plan_status_updated(&event.agent_id, false).into());
        serde_json::to_value(plan).ok()
    }))
    .expect("plan mode cancel fold can be registered");
    key.on(&PLAN_MODE_EXIT_CLASS, Arc::new(|state, event, context| {
        let event = serde_json::from_value::<PlanModeExit>(event.payload.clone()).ok()?;
        let mut plan = serde_json::from_value::<PlanState>(state).ok()?;
        apply_transition(&mut plan, PlanTransition::Exit);
        context.emit(plan_status_updated(&event.agent_id, false).into());
        serde_json::to_value(plan).ok()
    }))
    .expect("plan mode exit fold can be registered");
    key.on(&PLAN_REVISION_CLASS, Arc::new(|state, event, _context| {
        let event = serde_json::from_value::<PlanRevision>(event.payload.clone()).ok()?;
        let mut plan = serde_json::from_value::<PlanState>(state).ok()?;
        apply_revision(&mut plan, &event);
        serde_json::to_value(plan).ok()
    }))
    .expect("plan revision fold can be registered");
    key
});

pub fn plan_key() -> Arc<ReplayableStateKey> {
    PLAN_KEY.clone()
}

fn plan_status_updated(agent_id: &str, plan_mode: bool) -> AgentStatusUpdated {
    AgentStatusUpdated::new(AgentStatusUpdatedPayload {
        agent_id: agent_id.to_string(),
        usage: None,
        swarm_mode: None,
        hello_world_mode: None,
        hello_world_stage: None,
        tower_mode: None,
        tower_exit_reason: None,
        plan_mode: Some(plan_mode),
        spec_mode: None,
        model: None,
        thinking_effort: None,
        max_context_tokens: None,
        context_tokens: None,
        permission: None,
    })
}
