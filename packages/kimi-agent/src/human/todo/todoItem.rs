use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TODO_LIST_TOOL_NAME: &str = "TodoList";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TodoStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "done")]
    Done,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TodoItem {
    pub title: String,
    pub status: TodoStatus,
}

pub fn read_todo_items(raw: &Value) -> Vec<TodoItem> {
    raw.as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| is_todo_item(item))
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

pub fn is_todo_item(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|record| record.get("title").is_some_and(Value::is_string) && record.get("status").and_then(Value::as_str).is_some_and(|status| matches!(status, "pending" | "in_progress" | "done")))
}

pub fn render_todo_list(todos: &[TodoItem]) -> String {
    render_todo_list_with_title(todos, "Current todo list:")
}

pub fn render_todo_list_with_title(todos: &[TodoItem], title: &str) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".to_string();
    }
    std::iter::once(title.to_string())
        .chain(todos.iter().map(|todo| {
            let marker = match todo.status {
                TodoStatus::Pending => "[pending]",
                TodoStatus::InProgress => "[in_progress]",
                TodoStatus::Done => "[done]",
            };
            format!("  {marker} {}", todo.title)
        }))
        .collect::<Vec<_>>()
        .join("\n")
}
