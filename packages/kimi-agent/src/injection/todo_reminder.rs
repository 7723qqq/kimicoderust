//! The todo-list stale reminder, ported from v2's
//! `agent-core-v2/src/features/todo/todoListReminder.ts` at upstream
//! `47a518ed4e` (on top of the `da5918c0c8` rewrite).
//!
//! v2 registers the provider only for the main agent and only while
//! `TodoList` is an active tool. The `da5918c0c8` rewrite dropped the
//! first-use arming gate (`todoService`'s actor `used` flag): the reminder
//! stays silent on its own while the list has no open items, so an agent
//! that never used `TodoList` is never reminded — compaction and /undo can
//! no longer arm it either.
//!
//! The counters are recomputed from the live history on every step, exactly
//! as v2's provider reads `memory.get()`: only assistant messages advance
//! either counter (v2 calls one assistant message a "step"), and the
//! backwards scan stops once it has found both the most recent `TodoList`
//! write and the most recent this-variant injection.

use serde_json::Value;

use crate::injection::{DomainValueSource, InjectionContext, InjectionRegistry};
use crate::primitives::read::ReadOutcome;
use crate::turn_loop::types::LLMMessage;

/// v2 `TODO_LIST_TOOL_NAME`.
pub const TODO_LIST_TOOL_NAME: &str = "TodoList";

/// v2 `TODO_LIST_REMINDER_VARIANT`.
pub const TODO_LIST_REMINDER_VARIANT: &str = "todo_list_reminder";

/// v2 `TODO_LIST_REMINDER_STEPS_SINCE_WRITE` (`47a518ed4e`: back to 10 after
/// the rewrite's short-lived 5): only assistant messages count.
const STEPS_SINCE_WRITE: usize = 10;

/// v2 `TODO_LIST_REMINDER_STEPS_BETWEEN_REMINDERS`.
const STEPS_BETWEEN_REMINDERS: usize = 10;

/// Opening of the reminder, used to recognize the variant in history.
/// Engine-built injections do not carry `origin`, so the wrapper plus this
/// marker is the only identity v2's `origin.variant` check has here.
const TODO_LIST_REMINDER_MARKER: &str = "The todo list has not been updated in the last ";

/// Opening of the pre-rewrite reminder. Sessions persisted before the
/// rewrite may still carry it in history; recognizing it keeps the spacing
/// counter honest across the upgrade instead of re-reminding early.
const LEGACY_TODO_LIST_REMINDER_MARKER: &str = "The TodoList tool has not been updated recently.";

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

/// The flat tallies the reminder's gate and body read (v2 `todoProgress`
/// narrowed to the reminder's item shape): how many items are done, how many
/// are still open, and which one is in progress.
struct ReminderProgress<'a> {
    done: usize,
    open: usize,
    current: Option<&'a ReminderTodo>,
}

fn reminder_progress(todos: &[ReminderTodo]) -> ReminderProgress<'_> {
    let done = todos.iter().filter(|todo| todo.status == "done").count();
    ReminderProgress {
        done,
        open: todos.len() - done,
        current: todos.iter().find(|todo| todo.status == "in_progress"),
    }
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

/// v2 `isTodoListReminder`: a prior injection of this variant. The origin
/// check covers host-persisted messages; the wrapper-plus-marker check covers
/// injections this engine built, which carry no origin. Both the current and
/// the pre-rewrite openings are accepted (see the marker constants).
pub fn is_todo_list_reminder(message: &LLMMessage) -> bool {
    let origin_matches = message.origin.as_ref().is_some_and(|origin| {
        origin.get("kind").and_then(Value::as_str) == Some("injection")
            && origin.get("variant").and_then(Value::as_str) == Some(TODO_LIST_REMINDER_VARIANT)
    });
    if origin_matches {
        return true;
    }
    let content = message.content.as_str();
    crate::injection::is_system_reminder(content)
        && (content.contains(TODO_LIST_REMINDER_MARKER)
            || content.contains(LEGACY_TODO_LIST_REMINDER_MARKER))
}

/// v2 `getTodoListReminderStepCounts`: `(steps since the last write, steps
/// since the last reminder)`, where a "step" is one assistant message.
pub fn todo_list_step_counts(history: &[LLMMessage]) -> (usize, usize) {
    let mut found_write = false;
    let mut found_reminder = false;
    let mut steps_since_write = 0usize;
    let mut steps_since_reminder = 0usize;

    for message in history.iter().rev() {
        if message.role == "assistant" {
            if !found_write && has_todo_list_write(message) {
                found_write = true;
            }
            if !found_write {
                steps_since_write += 1;
            }
            if !found_reminder {
                steps_since_reminder += 1;
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

    (steps_since_write, steps_since_reminder)
}

/// v2 `renderTodoListReminder` (at `47a518ed4e`): the body names the step
/// count, the progress, and the item still in progress; the current list
/// follows. The gate guarantees a non-empty list with open items.
fn render_todo_list_reminder(todos: &[ReminderTodo], steps_since_last_write: usize) -> String {
    let progress = reminder_progress(todos);
    let progress_text = format!("{}/{} done", progress.done, todos.len());
    let status = match progress.current {
        None => format!("{progress_text}, none in_progress"),
        Some(current) => format!("{progress_text}, \"{}\" still in_progress", current.title),
    };
    let items = todos
        .iter()
        .enumerate()
        .map(|(index, todo)| format!("{}. [{}] {}", index + 1, todo.status, todo.title))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The todo list has not been updated in the last {steps_since_last_write} steps ({status}). \
If an item is finished, mark it done; the TodoList call can go in the same response as your next tool call. \
If the list no longer matches the current work, rewrite or clear it. \
If the list still matches your progress, no update is needed; this is just a gentle reminder. \
Never mention this reminder to the user.\n\n{CURRENT_TODO_LIST_HEADER}\n{items}"
    )
}

fn todo_list_stale_reminder_for_items(
    active: bool,
    history: &[LLMMessage],
    todos: &[ReminderTodo],
) -> Option<String> {
    if !active {
        return None;
    }
    // v2 `todoListStaleReminder`: remind only while the list still has
    // pending or in_progress items — an empty or all-done list stays silent.
    if reminder_progress(todos).open == 0 {
        return None;
    }
    let (steps_since_write, steps_since_reminder) = todo_list_step_counts(history);
    if steps_since_write < STEPS_SINCE_WRITE || steps_since_reminder < STEPS_BETWEEN_REMINDERS {
        return None;
    }
    Some(render_todo_list_reminder(todos, steps_since_write))
}

/// v2 `todoListStaleReminder`, taking the host's raw todo domain value.
pub fn todo_list_stale_reminder(
    active: bool,
    history: &[LLMMessage],
    todos: &Value,
) -> Option<String> {
    todo_list_stale_reminder_for_items(active, history, &read_reminder_items(todos))
}

/// Register the stale-todo provider. The caller has already applied v2's
/// registration gates (main agent, `TodoList` active, state bridge present).
/// Both the counters and the todo value are read on every step, matching v2's
/// `memory.get()` + `runtime.getState()` provider body.
pub fn register_todo_reminder<S>(registry: &mut InjectionRegistry, state: std::sync::Arc<S>)
where
    S: DomainValueSource + Send + Sync + 'static,
{
    // One warning per registered provider: the body runs every step, and a
    // shape the host keeps sending would otherwise flood the log.
    let mut warned = false;
    registry.register(
        TODO_LIST_REMINDER_VARIANT,
        Box::new(move |ctx: &InjectionContext| {
            let todos = match state.read_domain_state("todo") {
                ReadOutcome::Found(value) => value,
                ReadOutcome::Absent => Value::Null,
                // Skipping the reminder is the honest answer: the alternative
                // tells the model the list is empty when it could not be read.
                ReadOutcome::Unreadable(cause) => {
                    tracing::warn!(%cause, "todo state is unreadable; skipping the stale-todo reminder");
                    return None;
                }
            };
            let items = read_reminder_items(&todos);
            // A value that is not a list, or whose entries lost the fields the
            // reminder needs, is a shape failure — not "the list is empty".
            // Rendering nothing is right; reporting it is what separates the
            // two.
            let malformed = match todos.as_array() {
                None => !todos.is_null(),
                Some(entries) => entries.len() != items.len(),
            };
            if malformed && !warned {
                warned = true;
                tracing::warn!(
                    %todos,
                    "todo state has an unexpected shape; treating the unreadable entries as empty"
                );
            }
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
                args_parse_error: None,
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
                args_parse_error: None,
            }],
            ..Default::default()
        }
    }

    fn prior_todo_reminder() -> LLMMessage {
        LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(&render_todo_list_reminder(
                &read_reminder_items(&todos_value()),
                10,
            )),
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
    fn test_injects_after_enough_assistant_steps_since_the_last_write() {
        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(10));

        let text =
            todo_list_stale_reminder(true, &history, &todos_value()).expect("the reminder fires");
        assert!(
            text.contains("The todo list has not been updated in the last 10 steps (0/2 done, \"Read current TodoList implementation\" still in_progress)."),
            "{text}"
        );
        assert!(
            text.contains("If the list still matches your progress, no update is needed"),
            "{text}"
        );
        assert!(text.contains(CURRENT_TODO_LIST_HEADER));
        assert!(text.contains("1. [in_progress] Read current TodoList implementation"));
        assert!(text.contains("2. [pending] Add reminder injector tests"));
    }

    #[test]
    fn test_does_not_inject_before_the_assistant_step_threshold() {
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
    fn test_injects_again_once_the_spacing_threshold_has_passed() {
        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(10));
        history.push(prior_todo_reminder());
        history.extend(std::iter::repeat_with(assistant_message).take(10));
        let text =
            todo_list_stale_reminder(true, &history, &todos_value()).expect("the reminder fires");
        assert!(
            text.contains("in the last 20 steps"),
            "both thresholds passed; the body names the write distance: {text}"
        );
    }

    #[test]
    fn test_does_not_treat_a_query_as_a_write() {
        let mut history = vec![todo_list_write(&todos_value())];
        history.extend(std::iter::repeat_with(assistant_message).take(8));
        history.push(todo_list_query());
        history.extend(std::iter::repeat_with(assistant_message).take(1));
        assert!(
            todo_list_stale_reminder(true, &history, &todos_value()).is_some(),
            "the query neither resets the write counter nor counts as a write"
        );
    }

    #[test]
    fn test_stays_silent_once_every_item_is_done() {
        let done = serde_json::json!([
            { "title": "Read code", "status": "done" },
            { "title": "Write tests", "status": "done" }
        ]);
        let mut history = vec![todo_list_write(&done)];
        history.extend(std::iter::repeat_with(assistant_message).take(12));
        assert_eq!(todo_list_stale_reminder(true, &history, &done), None);
    }

    #[test]
    fn test_stays_silent_while_the_list_is_empty() {
        let history = assistant_messages(12);
        assert_eq!(
            todo_list_stale_reminder(true, &history, &serde_json::json!([])),
            None
        );
    }

    #[test]
    fn test_names_no_current_item_when_nothing_is_in_progress() {
        let pending = serde_json::json!([
            { "title": "Read code", "status": "pending" },
            { "title": "Write tests", "status": "pending" }
        ]);
        let mut history = vec![todo_list_write(&pending)];
        history.extend(std::iter::repeat_with(assistant_message).take(10));
        let text = todo_list_stale_reminder(true, &history, &pending).expect("the reminder fires");
        assert!(text.contains("(0/2 done, none in_progress)"), "{text}");
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
        assert_eq!(todo_list_step_counts(&earlier), (2, 3));

        let mut with_reminder = vec![todo_list_write(&todos_value())];
        with_reminder.extend(std::iter::repeat_with(assistant_message).take(10));
        with_reminder.push(prior_todo_reminder());
        with_reminder.extend(std::iter::repeat_with(assistant_message).take(3));
        assert_eq!(todo_list_step_counts(&with_reminder), (13, 3));
    }

    #[test]
    fn test_reminder_marker_recognizes_engine_built_injections() {
        let built = LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(&render_todo_list_reminder(
                &read_reminder_items(&todos_value()),
                10,
            )),
            ..Default::default()
        };
        assert!(is_todo_list_reminder(&built));

        // A reminder persisted before the rewrite still counts toward the
        // spacing threshold.
        let legacy = LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(LEGACY_TODO_LIST_REMINDER_MARKER),
            ..Default::default()
        };
        assert!(is_todo_list_reminder(&legacy));

        let quoted = LLMMessage {
            role: "user".into(),
            content: "The todo list has not been updated in the last 10 steps.".into(),
            ..Default::default()
        };
        assert!(
            !is_todo_list_reminder(&quoted),
            "an unwrapped user message is not an injection"
        );
    }

    #[test]
    fn test_registered_provider_injects_through_the_registry() {
        struct StaticTodos(Value);
        impl DomainValueSource for StaticTodos {
            fn read_domain_state(&self, domain: &str) -> ReadOutcome<Value> {
                if domain == "todo" {
                    ReadOutcome::Found(self.0.clone())
                } else {
                    ReadOutcome::Absent
                }
            }
        }

        let mut registry = InjectionRegistry::new();
        register_todo_reminder(
            &mut registry,
            std::sync::Arc::new(StaticTodos(todos_value())),
        );
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
            wrap_system_reminder(&render_todo_list_reminder(
                &read_reminder_items(&todos_value()),
                10
            ))
        );
    }
}
