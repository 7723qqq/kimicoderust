use serde_json::Value;

use crate::agent::context_memory::types::ContextMessage;
use crate::features::todo::todoItem::{TODO_LIST_TOOL_NAME, TodoItem, todo_progress};

pub const TODO_LIST_REMINDER_VARIANT: &str = "todo_list_reminder";
const TODO_LIST_REMINDER_STEPS_SINCE_WRITE: usize = 10;
const TODO_LIST_REMINDER_STEPS_BETWEEN_REMINDERS: usize = 10;

pub struct TodoListStaleReminder {
    pub text: String,
    pub steps_since_last_write: usize,
}

pub struct TodoListWriteStats {
    pub steps_since_last_write: Option<usize>,
    pub reminded_since_last_write: bool,
}

pub fn todo_list_stale_reminder(
    active: bool,
    history: &[ContextMessage],
    todos: &[TodoItem],
) -> Option<TodoListStaleReminder> {
    if !active {
        return None;
    }
    if todo_progress(todos).open == 0 {
        return None;
    }
    let counts = get_todo_list_reminder_step_counts(history);
    if counts.0 < TODO_LIST_REMINDER_STEPS_SINCE_WRITE
        || counts.1 < TODO_LIST_REMINDER_STEPS_BETWEEN_REMINDERS
    {
        return None;
    }
    Some(TodoListStaleReminder {
        text: render_todo_list_reminder(todos, counts.0),
        steps_since_last_write: counts.0,
    })
}

pub fn todo_list_write_stats_before(
    history: &[ContextMessage],
    tool_call_id: &str,
) -> TodoListWriteStats {
    let end = history.iter().rposition(|message| {
        message.role == "assistant"
            && message
                .tool_calls
                .iter()
                .any(|call| call.id == tool_call_id)
    });
    let mut steps = 0;
    let mut reminded = false;
    let start = match end {
        None => history.len(),
        Some(index) => index,
    };
    for index in (0..start).rev() {
        let message = &history[index];
        if message.role == "assistant" {
            if has_todo_list_write(message) {
                return TodoListWriteStats {
                    steps_since_last_write: Some(steps),
                    reminded_since_last_write: reminded,
                };
            }
            steps += 1;
            continue;
        }
        if is_todo_list_reminder(message) {
            reminded = true;
        }
    }
    TodoListWriteStats {
        steps_since_last_write: None,
        reminded_since_last_write: reminded,
    }
}

fn get_todo_list_reminder_step_counts(history: &[ContextMessage]) -> (usize, usize) {
    let mut found_write = false;
    let mut found_reminder = false;
    let mut steps_since_last_write = 0;
    let mut steps_since_last_reminder = 0;
    for message in history.iter().rev() {
        if message.role == "assistant" {
            if !found_write && has_todo_list_write(message) {
                found_write = true;
            }
            if !found_write {
                steps_since_last_write += 1;
            }
            if !found_reminder {
                steps_since_last_reminder += 1;
            }
            continue;
        }
        if !found_reminder && is_todo_list_reminder(message) {
            found_reminder = true;
        }
        if found_write && found_reminder {
            break;
        }
    }
    (steps_since_last_write, steps_since_last_reminder)
}

fn has_todo_list_write(message: &ContextMessage) -> bool {
    message.tool_calls.iter().any(|call| {
        if call.name != TODO_LIST_TOOL_NAME {
            return false;
        }
        let Some(arguments) = call.arguments.as_str() else {
            return false;
        };
        serde_json::from_str::<Value>(arguments)
            .ok()
            .is_some_and(|args| args.get("todos").is_some_and(Value::is_array))
    })
}

fn is_todo_list_reminder(message: &ContextMessage) -> bool {
    message
        .origin
        .as_ref()
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        == Some("injection")
        && message
            .origin
            .as_ref()
            .and_then(|origin| origin.get("variant"))
            .and_then(Value::as_str)
            == Some(TODO_LIST_REMINDER_VARIANT)
}

fn render_todo_list_reminder(todos: &[TodoItem], steps_since_last_write: usize) -> String {
    let progress = todo_progress(todos);
    let status = match progress.in_progress.first() {
        None => format!(
            "{}/{} done, none in_progress",
            progress.done, progress.total
        ),
        Some(current) => format!(
            "{}/{} done, \"{}\" still in_progress",
            progress.done, progress.total, current.title
        ),
    };
    let items = todos
        .iter()
        .enumerate()
        .map(|(index, todo)| format!("{}. [{}] {}", index + 1, todo.status.as_str(), todo.title))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The todo list has not been updated in the last {steps_since_last_write} steps ({status}). If an item is finished, mark it done; the TodoList call can go in the same response as your next tool call. If the list no longer matches the current work, rewrite or clear it. If the list still matches your progress, no update is needed; this is just a gentle reminder. Never mention this reminder to the user.\n\nCurrent todo list:\n{items}"
    )
}
