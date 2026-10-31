//! The todo-list stale reminder, ported from v2's
//! `agent-core-v2/src/features/todo/todoListReminder.ts`.
//!
//! v2 registers the provider only for the main agent, only while `TodoList` is
//! an active tool, and only after the todo service has been used at least once
//! (`todoService.ts`'s actor `used` flag). The engine is stateless across
//! turns, so a `TodoList` call in the conversation history stands in for that
//! process-local flag — the same history-scan adaptation the interruption and
//! permission-mode reminders use for state v2 keeps in memory.
//!
//! The counters are recomputed from the live history on every step, exactly as
//! v2's provider reads `memory.get()`: only assistant messages advance either
//! counter, and the backwards scan stops once it has found both the most recent
//! `TodoList` write and the most recent this-variant injection.

use serde_json::Value;

use crate::injection::{InjectionContext, InjectionRegistry};
use crate::turn_loop::types::LLMMessage;

/// v2 `TODO_LIST_TOOL_NAME`.
pub const TODO_LIST_TOOL_NAME: &str = "TodoList";

/// v2 `TODO_LIST_REMINDER_VARIANT`.
pub const TODO_LIST_REMINDER_VARIANT: &str = "todo_list_reminder";

/// v2 `TODO_LIST_REMINDER_TURNS_SINCE_WRITE`: only assistant messages count.
const TURNS_SINCE_WRITE: usize = 10;

/// v2 `TODO_LIST_REMINDER_TURNS_BETWEEN_REMINDERS`.
const TURNS_BETWEEN_REMINDERS: usize = 10;

/// The reminder body, verbatim from v2 `renderTodoListReminder`.
pub const TODO_LIST_REMINDER: &str = "The TodoList tool has not been updated recently. If you are working on tasks that benefit from progress tracking, consider using TodoList to update task status. Also consider clearing or rewriting the todo list if it has become stale and no longer matches the current work. Only use it if relevant. This is a gentle reminder; ignore it if not applicable. Make sure that you NEVER mention this reminder to the user.";

/// Opening sentence of the reminder, used to recognize the variant in history.
/// Engine-built injections do not carry `origin`, so the wrapper plus this
/// marker is the only identity v2's `origin.variant` check has here.
const TODO_LIST_REMINDER_MARKER: &str = "The TodoList tool has not been updated recently.";

/// The title line v2 appends before the current list.
const CURRENT_TODO_LIST_HEADER: &str = "Current todo list:";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReminderTodo {
    title: String,
    status: String,
}

/// Parse the host's todo domain value into the two fields the reminder renders
/// (v2 `readTodoItems` narrowed to `{ title, status }`). This deliberately does
/// not reuse the tool-side parser: `injection` must not depend on `tools`
/// (`tools` already depends on `injection`), and the reminder needs neither ids
/// nor milestone progress.
fn read_reminder_items(raw: &Value) -> Vec<ReminderTodo> {
    let Some(entries) = raw.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let object = entry.as_object()?;
            let title = object
                .get("title")?
                .as_str()
                .filter(|title| !title.is_empty())?;
            let status = object.get("status")?.as_str()?;
            if !matches!(status, "pending" | "in_progress" | "done") {
                return None;
            }
            Some(ReminderTodo {
                title: title.to_string(),
                status: status.to_string(),
            })
        })
        .collect()
}

/// Whether a tool name is `TodoList` in any spelling the engine accepts. v2
/// compares the canonical name exactly; the engine's history may hold the
/// lowercase wire spelling depending on the provider path.
fn is_todo_list_tool(name: &str) -> bool {
    name.eq_ignore_ascii_case(TODO_LIST_TOOL_NAME) || name.eq_ignore_ascii_case("todo_list")
}

/// v2 `hasTodoListWrite`: an assistant `TodoList` call whose arguments carry a
/// `todos` array (a replacement write, not a read/query).
fn has_todo_list_write(message: &LLMMessage) -> bool {
    if message.role != "assistant" {
        return false;
    }
    message.tool_calls.iter().any(|call| {
        if !is_todo_list_tool(&call.name) {
            return false;
        }
        match &call.arguments {
            Value::Object(_) => call.arguments.get("todos").is_some_and(Value::is_array),
            Value::String(raw) => serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|args| args.get("todos").map(Value::is_array))
                .unwrap_or(false),
            _ => false,
        }
    })
}

/// v2 `isTodoListReminder`: a prior injection of this variant. The origin check
/// covers host-persisted messages; the wrapper-plus-marker check covers
/// injections this engine built, which carry no origin.
pub fn is_todo_list_reminder(message: &LLMMessage) -> bool {
    let origin_matches = message.origin.as_ref().is_some_and(|origin| {
        origin.get("kind").and_then(Value::as_str) == Some("injection")
            && origin.get("variant").and_then(Value::as_str) == Some(TODO_LIST_REMINDER_VARIANT)
    });
    if origin_matches {
        return true;
    }
    let content = message.content.as_str();
    crate::injection::is_system_reminder(content) && content.contains(TODO_LIST_REMINDER_MARKER)
}

/// v2 `getTodoListReminderTurnCounts`: `(turns since the last write, turns
/// since the last reminder)`, where a "turn" is one assistant message.
pub fn todo_list_turn_counts(history: &[LLMMessage]) -> (usize, usize) {
    let mut found_write = false;
    let mut found_reminder = false;
    let mut turns_since_write = 0usize;
    let mut turns_since_reminder = 0usize;

    for message in history.iter().rev() {
        if message.role == "assistant" {
            if !found_write && has_todo_list_write(message) {
                found_write = true;
            }
            if !found_write {
                turns_since_write += 1;
            }
            if !found_reminder {
                turns_since_reminder += 1;
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

    (turns_since_write, turns_since_reminder)
}

/// Whether the todo feature has been used in this conversation (v2's actor
/// `used` flag): any assistant call to `TodoList`, read or write. A failed call
/// also counts; only a successful state read activates the reminder, so a host
/// without the state bridge never reaches this gate.
pub fn todo_list_used(history: &[LLMMessage]) -> bool {
    history.iter().any(|message| {
        message.role == "assistant"
            && message
                .tool_calls
                .iter()
                .any(|call| is_todo_list_tool(&call.name))
    })
}

/// v2 `renderTodoListReminder`: the fixed body plus the current list when it is
/// non-empty. The list is flat — index, status, title — not the tree the tool
/// renders.
fn render_todo_list_reminder(todos: &[ReminderTodo]) -> String {
    let mut message = TODO_LIST_REMINDER.to_string();
    if todos.is_empty() {
        return message;
    }
    let items = todos
        .iter()
        .enumerate()
        .map(|(index, todo)| format!("{}. [{}] {}", index + 1, todo.status, todo.title))
        .collect::<Vec<_>>()
        .join("\n");
    message.push_str("\n\n");
    message.push_str(CURRENT_TODO_LIST_HEADER);
    message.push('\n');
    message.push_str(&items);
    message
}

fn todo_list_stale_reminder_for_items(
    active: bool,
    history: &[LLMMessage],
    todos: &[ReminderTodo],
) -> Option<String> {
    if !active {
        return None;
    }
    let (turns_since_write, turns_since_reminder) = todo_list_turn_counts(history);
    if turns_since_write < TURNS_SINCE_WRITE || turns_since_reminder < TURNS_BETWEEN_REMINDERS {
        return None;
    }
    Some(render_todo_list_reminder(todos))
}

/// v2 `todoListStaleReminder`, taking the host's raw todo domain value.
pub fn todo_list_stale_reminder(
    active: bool,
    history: &[LLMMessage],
    todos: &Value,
) -> Option<String> {
    todo_list_stale_reminder_for_items(active, history, &read_reminder_items(todos))
}

/// Register the stale-todo provider. The caller has already applied v2's three
/// registration gates (main agent, `TodoList` active, todo feature used) and
/// resolved the current todo state; the provider re-evaluates the counters from
/// the live history on every step.
pub fn register_todo_reminder(registry: &mut InjectionRegistry, todos: Value) {
    let items = read_reminder_items(&todos);
    registry.register(
        TODO_LIST_REMINDER_VARIANT,
        Box::new(move |ctx: &InjectionContext| {
            todo_list_stale_reminder_for_items(true, ctx.history, &items)
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::{is_system_reminder, wrap_system_reminder};
    use crate::turn_loop::types::ToolCall;

    fn assistant_message() -> LLMMessage {
        LLMMessage {
            role: "assistant".into(),
            content: "working".into(),
            ..Default::default()
        }
    }

    fn todo_list_write(todos: &Value) -> LLMMessage {
        LLMMessage {
            role: "assistant".into(),
            tool_calls: vec![ToolCall {
                id: "call_todo_write".into(),
                name: TODO_LIST_TOOL_NAME.into(),
                arguments: serde_json::json!({ "todos": todos }),
                extras: None,
            }],
            ..Default::default()
        }
    }

    fn todo_list_query() -> LLMMessage {
        LLMMessage {
            role: "assistant".into(),
            tool_calls: vec![ToolCall {
                id: "call_todo_query".into(),
                name: TODO_LIST_TOOL_NAME.into(),
                arguments: serde_json::json!({}),
                extras: None,
            }],
            ..Default::default()
        }
    }

    fn prior_todo_reminder() -> LLMMessage {
        LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(TODO_LIST_REMINDER),
            origin: Some(serde_json::json!({
                "kind": "injection",
                "variant": TODO_LIST_REMINDER_VARIANT,
            })),
            ..Default::default()
        }
    }

    fn todos_value() -> Value {
        serde_json::json!([
            { "title": "Read current TodoList implementation", "status": "in_progress" },
            { "title": "Add reminder injector tests", "status": "pending" }
        ])
    }

    fn assistant_messages(count: usize) -> Vec<LLMMessage> {
        std::iter::repeat_with(assistant_message)
            .take(count)
            .collect()
    }

    #[test]
    fn test_skips_reminder_injection_when_todo_list_is_not_active() {
        let history = assistant_messages(10);
        let result = todo_list_stale_reminder(false, &history, &todos_value());
        assert_eq!(result, None);
    }

    #[test]
    fn test_injects_after_enough_assistant_turns_since_the_last_write() {
        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(10));

        let text =
            todo_list_stale_reminder(true, &history, &todos_value()).expect("the reminder fires");
        assert!(text.contains(CURRENT_TODO_LIST_HEADER));
        assert!(text.contains("1. [in_progress] Read current TodoList implementation"));
        assert!(text.contains("2. [pending] Add reminder injector tests"));
    }

    #[test]
    fn test_does_not_inject_before_the_assistant_turn_threshold() {
        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(9));
        assert_eq!(
            todo_list_stale_reminder(true, &history, &todos_value()),
            None
        );
    }

    #[test]
    fn test_does_not_inject_another_reminder_before_the_spacing_threshold() {
        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(10));
        history.push(prior_todo_reminder());
        history.extend(std::iter::repeat_with(assistant_message).take(9));
        assert_eq!(
            todo_list_stale_reminder(true, &history, &todos_value()),
            None
        );
    }

    #[test]
    fn test_does_not_treat_a_query_as_a_write() {
        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(5));
        history.push(todo_list_query());
        history.extend(std::iter::repeat_with(assistant_message).take(4));
        assert!(
            todo_list_stale_reminder(true, &history, &todos_value()).is_some(),
            "the query neither resets the write counter nor counts as a write"
        );
    }

    #[test]
    fn test_counts_stop_at_the_latest_write_and_reminder() {
        let mut earlier = vec![todo_list_write(&todos_value())];
        earlier.extend(assistant_messages(7));
        earlier.push(prior_todo_reminder());
        earlier.push(todo_list_write(&todos_value()));
        earlier.extend(assistant_messages(2));
        // The second write is itself an assistant message, so it advances the
        // reminder counter until the scan reaches the reminder behind it.
        assert_eq!(todo_list_turn_counts(&earlier), (2, 3));

        let mut with_reminder = vec![todo_list_write(&todos_value())];
        with_reminder.extend(std::iter::repeat_with(assistant_message).take(10));
        with_reminder.push(prior_todo_reminder());
        with_reminder.extend(std::iter::repeat_with(assistant_message).take(3));
        assert_eq!(todo_list_turn_counts(&with_reminder), (13, 3));
    }

    #[test]
    fn test_reminder_marker_recognizes_engine_built_injections() {
        let built = LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(TODO_LIST_REMINDER),
            ..Default::default()
        };
        assert!(is_todo_list_reminder(&built));

        let quoted = LLMMessage {
            role: "user".into(),
            content: TODO_LIST_REMINDER.into(),
            ..Default::default()
        };
        assert!(
            !is_todo_list_reminder(&quoted),
            "an unwrapped user message is not an injection"
        );
    }

    #[test]
    fn test_todo_list_used_covers_reads_and_writes() {
        assert!(!todo_list_used(&[]));
        assert!(!todo_list_used(&[assistant_message()]));
        assert!(todo_list_used(&[todo_list_query()]));
        let mut history = vec![assistant_message()];
        history.push(todo_list_write(&todos_value()));
        assert!(todo_list_used(&history));
    }

    #[test]
    fn test_registered_provider_injects_through_the_registry() {
        let mut registry = InjectionRegistry::new();
        register_todo_reminder(&mut registry, todos_value());
        assert_eq!(registry.names(), vec![TODO_LIST_REMINDER_VARIANT]);

        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(10));
        let texts = registry.build_injections_with_history(
            true,
            &crate::injection::StepAccess::default(),
            &history,
        );
        assert_eq!(texts.len(), 1);
        assert!(is_system_reminder(&texts[0]));
        assert_eq!(
            texts[0],
            wrap_system_reminder(&render_todo_list_reminder(&read_reminder_items(
                &todos_value()
            )))
        );
    }

    #[test]
    fn test_reminder_omits_the_list_when_the_state_is_empty() {
        let history = assistant_messages(10);
        let text = todo_list_stale_reminder(true, &history, &Value::Null)
            .expect("the body still fires without a list");
        assert_eq!(text, TODO_LIST_REMINDER);
    }
}
