use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::super::todoItem::{TodoItem, TodoKind, TodoStatus};

pub const TODO_LIST_TOOL_SERVICE_ID: &str = "todoListTool";

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoListInputItem {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub parent_id: Option<Option<String>>,
    #[serde(default)]
    pub kind: Option<TodoKind>,
    pub title: String,
    pub status: TodoStatus,
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct TodoListInput {
    #[serde(default)]
    pub todos: Option<Vec<TodoListInputItem>>,
}

impl TodoListInputItem {
    pub fn into_item(self, index: usize) -> TodoItem {
        TodoItem {
            id: self.id.unwrap_or_else(|| format!("t{}", index + 1)),
            parent_id: self.parent_id.flatten(),
            kind: self.kind.unwrap_or(TodoKind::Task),
            title: self.title,
            status: self.status,
            progress: self.progress,
            description: self.description,
        }
    }
}

pub fn todo_list_input_schema() -> Value {
    let mut schema = crate::tools::todo_list::todo_list_tool_def().input_schema;
    close_object_nodes(&mut schema);
    schema
}

fn close_object_nodes(value: &mut Value) {
    match value {
        Value::Array(entries) => {
            for entry in entries {
                close_object_nodes(entry);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("object") {
                object
                    .entry("additionalProperties".to_string())
                    .or_insert(Value::Bool(false));
            }
            for child in object.values_mut() {
                close_object_nodes(child);
            }
        }
        _ => {}
    }
}
