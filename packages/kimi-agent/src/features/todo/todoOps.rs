use serde_json::{Value, json};

use crate::features::todo::todoItem::TodoItem;

pub const TODO_UPDATED_EVENT: &str = "todo.updated";
pub const TOOLS_UPDATE_STORE_EVENT: &str = "tools.update_store";

pub struct ToolsUpdateStore;

impl ToolsUpdateStore {
    pub fn event_type() -> &'static str {
        TOOLS_UPDATE_STORE_EVENT
    }
}

pub fn todo_updated_event(agent_id: &str, todos: &[TodoItem], rev: u64) -> Value {
    json!({
        "type": TODO_UPDATED_EVENT,
        "agentId": agent_id,
        "todos": todos,
        "rev": rev,
    })
}

pub fn tools_update_store_event(agent_id: &str, key: &str, value: Value) -> Value {
    json!({
        "type": TOOLS_UPDATE_STORE_EVENT,
        "agentId": agent_id,
        "key": key,
        "value": value,
    })
}
