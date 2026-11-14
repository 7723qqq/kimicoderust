use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TODO_LIST_TOOL_NAME: &str = "TodoList";
pub const TODO_LIST_HEADER: &str = "Current todo list:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoKind {
    Milestone,
    Task,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub id: String,
    pub parent_id: Option<String>,
    pub kind: TodoKind,
    pub title: String,
    pub status: TodoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TodoProgressReport {
    pub overall: u32,
    pub done: usize,
    pub total: usize,
    pub by_id: HashMap<String, u32>,
}

#[derive(Debug, Clone)]
pub struct TodoProgress {
    pub total: usize,
    pub done: usize,
    pub open: usize,
    pub in_progress: Vec<TodoItem>,
}

fn is_raw_todo_item(value: &Value) -> bool {
    let Some(record) = value.as_object() else {
        return false;
    };
    record
        .get("title")
        .and_then(Value::as_str)
        .map(|title| !title.is_empty())
        .unwrap_or(false)
        && record
            .get("status")
            .and_then(Value::as_str)
            .and_then(TodoStatus::from_str)
            .is_some()
}

pub fn read_todo_items(raw: &Value) -> Vec<TodoItem> {
    let Some(entries) = raw.as_array() else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for entry in entries {
        if !is_raw_todo_item(entry) {
            continue;
        }
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or_default()
            .to_string();
        let parent_id = entry
            .get("parentId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let kind = if entry.get("kind").and_then(Value::as_str) == Some("milestone") {
            TodoKind::Milestone
        } else {
            TodoKind::Task
        };
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .and_then(TodoStatus::from_str)
            .unwrap_or(TodoStatus::Pending);
        let progress = entry
            .get("progress")
            .and_then(Value::as_f64)
            .filter(|progress| progress.is_finite())
            .map(|progress| progress.clamp(0.0, 100.0).round());
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        items.push(TodoItem {
            id,
            parent_id,
            kind,
            title,
            status,
            progress,
            description,
        });
    }
    assign_missing_ids(items)
}

fn assign_missing_ids(items: Vec<TodoItem>) -> Vec<TodoItem> {
    let mut used: HashSet<String> = items
        .iter()
        .filter(|item| !item.id.is_empty())
        .map(|item| item.id.clone())
        .collect();
    items
        .into_iter()
        .map(|mut item| {
            if !item.id.is_empty() {
                return item;
            }
            let id = match &item.parent_id {
                None => smallest_free(&mut used, |n| format!("T{n}")),
                Some(parent_id) => smallest_free(&mut used, |n| format!("{parent_id}.{n}")),
            };
            item.id = id;
            item
        })
        .collect()
}

fn smallest_free(used: &mut HashSet<String>, format: impl Fn(u32) -> String) -> String {
    let mut n = 1;
    while used.contains(&format(n)) {
        n += 1;
    }
    let id = format(n);
    used.insert(id.clone());
    id
}

fn leaf_progress(todo: &TodoItem) -> u32 {
    match todo.status {
        TodoStatus::Done => 100,
        TodoStatus::InProgress => todo.progress.unwrap_or(0.0).round() as u32,
        TodoStatus::Pending => 0,
    }
}

fn mean(values: &[u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    (values.iter().sum::<u32>() as f64 / values.len() as f64).round() as u32
}

pub fn compute_todo_progress(todos: &[TodoItem]) -> TodoProgressReport {
    if todos.is_empty() {
        return TodoProgressReport::default();
    }
    let mut children_of: HashMap<String, Vec<&TodoItem>> = HashMap::new();
    for todo in todos {
        if let Some(parent_id) = &todo.parent_id {
            children_of.entry(parent_id.clone()).or_default().push(todo);
        }
    }
    let mut by_id = HashMap::new();
    for todo in todos {
        if todo.kind == TodoKind::Milestone {
            continue;
        }
        by_id.insert(todo.id.clone(), leaf_progress(todo));
    }
    let milestones: Vec<&TodoItem> = todos
        .iter()
        .filter(|todo| todo.kind == TodoKind::Milestone)
        .collect();
    if milestones.is_empty() {
        let values: Vec<u32> = todos
            .iter()
            .map(|todo| by_id.get(&todo.id).copied().unwrap_or(0))
            .collect();
        return TodoProgressReport {
            overall: mean(&values),
            done: todos
                .iter()
                .filter(|todo| todo.status == TodoStatus::Done)
                .count(),
            total: todos.len(),
            by_id,
        };
    }
    let mut milestone_values = Vec::new();
    for milestone in &milestones {
        let children = children_of.get(&milestone.id).cloned().unwrap_or_default();
        let value = if children.is_empty() {
            leaf_progress(milestone)
        } else {
            let child_values: Vec<u32> = children
                .iter()
                .map(|child| by_id.get(&child.id).copied().unwrap_or(0))
                .collect();
            mean(&child_values)
        };
        by_id.insert(milestone.id.clone(), value);
        milestone_values.push(value);
    }
    let done = todos
        .iter()
        .filter(|todo| match todo.kind {
            TodoKind::Milestone => by_id.get(&todo.id).copied().unwrap_or(0) >= 100,
            TodoKind::Task => todo.status == TodoStatus::Done,
        })
        .count();
    TodoProgressReport {
        overall: mean(&milestone_values),
        done,
        total: todos.len(),
        by_id,
    }
}

pub fn todo_progress(todos: &[TodoItem]) -> TodoProgress {
    let done = todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::Done)
        .count();
    TodoProgress {
        total: todos.len(),
        done,
        open: todos.len() - done,
        in_progress: todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .cloned()
            .collect(),
    }
}

pub fn render_todo_list(todos: &[TodoItem], title: &str) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".to_string();
    }
    let report = compute_todo_progress(todos);
    let known: HashSet<&str> = todos.iter().map(|todo| todo.id.as_str()).collect();
    let mut children_of: HashMap<String, Vec<&TodoItem>> = HashMap::new();
    for todo in todos {
        if let Some(parent_id) = &todo.parent_id {
            if known.contains(parent_id.as_str()) {
                children_of.entry(parent_id.clone()).or_default().push(todo);
            }
        }
    }
    let mut lines = vec![format!(
        "{title} (overall {}/{} · {}%)",
        report.done, report.total, report.overall
    )];
    let roots: Vec<&TodoItem> = todos
        .iter()
        .filter(|todo| {
            todo.parent_id.is_none()
                || !todo
                    .parent_id
                    .as_ref()
                    .map(|id| known.contains(id.as_str()))
                    .unwrap_or(false)
        })
        .collect();
    for root in roots {
        render_tree_line(root, &children_of, &report.by_id, 0, &mut lines);
    }
    lines.join("\n")
}

fn render_tree_line(
    item: &TodoItem,
    children_of: &HashMap<String, Vec<&TodoItem>>,
    by_id: &HashMap<String, u32>,
    depth: usize,
    lines: &mut Vec<String>,
) {
    let indent = "  ".repeat(depth + 1);
    let progress = by_id.get(&item.id).copied().unwrap_or(0);
    let status = effective_status(item, progress);
    let suffix = progress_suffix(item, children_of, progress);
    lines.push(format!(
        "{indent}{} {}: {}{suffix}",
        status_marker(status),
        item.id,
        item.title
    ));
    for child in children_of.get(&item.id).cloned().unwrap_or_default() {
        render_tree_line(child, children_of, by_id, depth + 1, lines);
    }
}

fn effective_status(item: &TodoItem, progress: u32) -> TodoStatus {
    if item.kind != TodoKind::Milestone {
        return item.status;
    }
    if progress >= 100 {
        return TodoStatus::Done;
    }
    if progress > 0 {
        return TodoStatus::InProgress;
    }
    TodoStatus::Pending
}

fn progress_suffix(
    item: &TodoItem,
    children_of: &HashMap<String, Vec<&TodoItem>>,
    progress: u32,
) -> String {
    if item.kind == TodoKind::Milestone {
        let children = children_of.get(&item.id).cloned().unwrap_or_default();
        let done = children
            .iter()
            .filter(|child| child.status == TodoStatus::Done)
            .count();
        return format!(" ({done}/{} · {progress}%)", children.len());
    }
    if item.status == TodoStatus::Done {
        return String::new();
    }
    if progress > 0 {
        return format!(" ({progress}%)");
    }
    String::new()
}

fn status_marker(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "[pending]",
        TodoStatus::InProgress => "[in_progress]",
        TodoStatus::Done => "[done]",
    }
}
