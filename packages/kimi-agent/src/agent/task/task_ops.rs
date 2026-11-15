use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::task::task::AgentTaskNotificationContext;
use crate::agent::task::types::AgentTaskInfo;
use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::state::state::{FoldContext, ReplayableOptions, ReplayableStateKey};

pub type TaskModelState = HashMap<String, AgentTaskInfo>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStartedPayload {
    pub agent_id: String,
    pub info: AgentTaskInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTerminatedPayload {
    pub agent_id: String,
    pub info: AgentTaskInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTerminatedNoticePayload {
    pub agent_id: String,
    pub info: AgentTaskInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutputPayload {
    pub agent_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<String>,
    pub output_size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskWaitDeliveredPayload {
    pub agent_id: String,
    pub keys: Vec<String>,
}

macro_rules! task_event {
    ($name:ident, $payload:ty, $class:ident, $type:literal, $durable:literal, $observable:literal, $schema:expr) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: $durable,
                observable: $observable,
                agent_domain: true,
                schema: $schema,
            })
        });

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;
            pub const DURABLE: bool = $durable;
            pub const OBSERVABLE: bool = $observable;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("task event is serializable"),
                    None,
                );
                Self { event, payload }
            }

            pub fn class() -> Arc<Event2Class> {
                $class.clone()
            }

            pub fn as_event(&self) -> &Event2 {
                &self.event
            }

            pub fn from_event(event: Event2) -> Option<Self> {
                let payload = serde_json::from_value(event.payload.clone()).ok()?;
                Some(Self { event, payload })
            }
        }

        impl Deref for $name {
            type Target = $payload;

            fn deref(&self) -> &Self::Target {
                &self.payload
            }
        }

        impl From<$name> for Event2 {
            fn from(event: $name) -> Self {
                event.event
            }
        }
    };
}

task_event!(
    TaskStarted,
    TaskStartedPayload,
    TASK_STARTED_CLASS,
    "task.started",
    true,
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "info"],
        "properties": {"agentId": {"type": "string"}, "info": {}}
    }))
);
task_event!(
    TaskTerminated,
    TaskTerminatedPayload,
    TASK_TERMINATED_CLASS,
    "task.terminated",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "info"],
        "properties": {
            "agentId": {"type": "string"},
            "info": {},
            "outputTail": {"type": "string"}
        }
    }))
);
task_event!(
    TaskTerminatedNotice,
    TaskTerminatedNoticePayload,
    TASK_TERMINATED_NOTICE_CLASS,
    "task.terminated",
    false,
    true,
    None
);
task_event!(
    TaskNotified,
    AgentTaskNotificationContext,
    TASK_NOTIFIED_CLASS,
    "task.notified",
    false,
    true,
    None
);
task_event!(
    TaskOutput,
    TaskOutputPayload,
    TASK_OUTPUT_CLASS,
    "task.output",
    false,
    true,
    None
);
task_event!(
    TaskWaitDelivered,
    TaskWaitDeliveredPayload,
    TASK_WAIT_DELIVERED_CLASS,
    "task.waitDelivered",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "keys"],
        "properties": {
            "agentId": {"type": "string"},
            "keys": {"type": "array", "items": {"type": "string"}}
        }
    }))
);

pub fn register_task_event_classes() -> Result<(), String> {
    for class in [
        &TASK_STARTED_CLASS,
        &TASK_TERMINATED_CLASS,
        &TASK_TERMINATED_NOTICE_CLASS,
        &TASK_NOTIFIED_CLASS,
        &TASK_OUTPUT_CLASS,
        &TASK_WAIT_DELIVERED_CLASS,
    ] {
        register_event2_class(class)?;
    }
    Ok(())
}

pub fn task_key() -> crate::_base::state::state_registry::StateKey<TaskModelState> {
    crate::_base::state::state_registry::StateKey::new("task", TaskModelState::default)
}

pub fn fold_task_state(
    current: Value,
    event: &Event2,
    context: &mut dyn FoldContext,
) -> Option<Value> {
    let mut state = serde_json::from_value::<TaskModelState>(current).ok()?;
    match event.type_.as_str() {
        "task.started" => {
            let payload = serde_json::from_value::<TaskStartedPayload>(event.payload.clone()).ok()?;
            state.insert(task_id_of(&payload.info), payload.info);
        }
        "task.terminated" => {
            let payload = serde_json::from_value::<TaskTerminatedPayload>(event.payload.clone()).ok()?;
            let task_id = task_id_of(&payload.info);
            state.insert(task_id, payload.info.clone());
            context.emit(
                TaskTerminatedNotice::new(TaskTerminatedNoticePayload {
                    agent_id: payload.agent_id,
                    info: payload.info,
                })
                .into(),
            );
        }
        _ => return None,
    }
    serde_json::to_value(state).ok()
}

fn task_id_of(info: &AgentTaskInfo) -> String {
    serde_json::to_value(info)
        .ok()
        .and_then(|value| value.get("taskId")?.as_str().map(ToString::to_string))
        .unwrap_or_default()
}

pub static TASK_REPLAYABLE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "task".to_string(),
        Arc::new(|| serde_json::to_value(TaskModelState::default).unwrap_or(Value::Null)),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    for class in [&TASK_STARTED_CLASS, &TASK_TERMINATED_CLASS] {
        key.on(class, Arc::new(fold_task_state))
            .unwrap_or_else(|error| panic!("failed to register task state fold: {error}"));
    }
    Arc::new(key)
});

pub fn task_replayable_key() -> Arc<ReplayableStateKey> {
    TASK_REPLAYABLE_KEY.clone()
}
