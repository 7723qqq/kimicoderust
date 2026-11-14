use serde_json::Value;

use super::task_list::task_list_input_schema;
use crate::callbacks::HostCallbacks;
use crate::rpc::types::StateReadRequest;
use crate::tools::task_tools::{is_terminal_status, map_state_error, render_task_entry};
use crate::tools::{arg_error_text, err_result, ok_result};
use crate::turn_loop::types::ExecutableToolResult;

pub async fn execute_task_list(
    callbacks: &dyn HostCallbacks,
    args: &Value,
) -> ExecutableToolResult {
    let active_only = match args.get("active_only") {
        None | Some(Value::Null) => true,
        Some(value) => match value.as_bool() {
            Some(active_only) => active_only,
            None => {
                return err_result(arg_error_text(
                    "TaskList",
                    "active_only",
                    "engine.tools.argMustBeBoolean",
                ));
            }
        },
    };

    let limit = match args.get("limit") {
        None | Some(Value::Null) => 20usize,
        Some(value) => match value.as_u64() {
            Some(limit) if (1..=100).contains(&limit) => limit as usize,
            _ => {
                return err_result(
                    "Invalid TaskList arguments: `limit` must be an integer between 1 and 100."
                        .into(),
                );
            }
        },
    };
    let request = StateReadRequest {
        domain: "task".into(),
        key: "task".into(),
        turn_id: args
            .get("turn_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        tool_call_id: args
            .get("tool_call_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
    };
    match callbacks.state_read(request).await {
        Ok(response) => render_task_list(&response.value, active_only, limit),
        Err(error) => map_state_error(error, None),
    }
}

fn render_task_list(value: &Value, active_only: bool, limit: usize) -> ExecutableToolResult {
    let Some(tasks) = value.as_array() else {
        return err_result(
            "Invalid task state from host: expected an array of task entries.".into(),
        );
    };
    let filtered: Vec<&Value> = if active_only {
        tasks
            .iter()
            .filter(|task| {
                task.get("status")
                    .and_then(|s| s.as_str())
                    .is_none_or(|status| !is_terminal_status(status))
            })
            .take(limit)
            .collect()
    } else {
        tasks.iter().take(limit).collect()
    };
    let label = if active_only {
        "active_background_tasks"
    } else {
        "background_tasks"
    };
    let header = format!("{label}: {}", filtered.len());
    if filtered.is_empty() {
        return ok_result(format!("{header}\nNo background tasks found."));
    }
    let records: Vec<String> = filtered
        .iter()
        .map(|task| render_task_entry(task))
        .collect();
    ok_result(format!("{header}\n{}", records.join("\n---\n")))
}

pub fn task_list_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "TaskList".into(),
        description: r#"List background tasks and their current status, including task IDs for `TaskOutput`.

After context compaction, or whenever unsure which tasks are running, call TaskList instead of guessing IDs. With `active_only=false`, `lost` tasks from previous processes may appear; treat them as terminated.

This tool is read-only and safe in plan mode; it does not return task output."#
            .into(),
        input_schema: task_list_input_schema(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::task_tools::STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE;
    use crate::tools::task_tools::tests::{read_ok, scripted, write_ok};

    fn sample_entry() -> Value {
        serde_json::json!({
            "taskId": "task-1",
            "description": "Running tests",
            "status": "running",
            "startedAt": 1700000000000u64
        })
    }

    #[tokio::test]
    async fn test_list_renders_header_and_records() {
        let (callbacks, read_received, write_received) = scripted(
            read_ok(serde_json::json!([
                sample_entry(),
                {
                    "taskId": "task-2",
                    "description": "Fetching docs",
                    "status": "completed",
                    "startedAt": 1699990000000u64,
                    "endedAt": 1699990001000u64
                }
            ])),
            write_ok(Value::Null),
        );
        let result =
            execute_task_list(&callbacks, &serde_json::json!({ "active_only": false })).await;
        assert!(!result.is_error);
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(lines[0], "background_tasks: 2");
        assert!(lines[1..].contains(&"task_id: task-1"));
        assert!(result.content.contains("description: Running tests"));
        assert!(result.content.contains("status: running"));

        assert!(!result.content.contains("started_at:"));
        assert!(!result.content.contains("ended_at:"));
        assert!(
            result.content.contains("Wall time: 1.000 seconds"),
            "task-2: {}",
            result.content
        );
        assert!(
            result.content.lines().any(|l| l.starts_with("Wall time: ")),
            "task-1: {}",
            result.content
        );
        assert!(result.content.contains("---"));
        assert!(result.content.contains("task_id: task-2"));
        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "task");
        assert_eq!(request.key, "task");
        assert_eq!(request.turn_id, "");
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_list_active_only_filters_terminal_entries() {
        let (callbacks, _, _) = scripted(
            read_ok(serde_json::json!([
                sample_entry(),
                {
                    "taskId": "task-2",
                    "description": "Fetching docs",
                    "status": "completed",
                    "startedAt": 1699990000000u64
                },
                {
                    "taskId": "task-3",
                    "description": "Lost process",
                    "status": "lost",
                    "startedAt": 1699980000000u64
                }
            ])),
            write_ok(Value::Null),
        );
        let result = execute_task_list(&callbacks, &serde_json::json!({})).await;
        assert!(!result.is_error);
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(lines[0], "active_background_tasks: 1");
        assert!(result.content.contains("task_id: task-1"));
        assert!(!result.content.contains("task-2"));
        assert!(!result.content.contains("task-3"));
    }

    #[tokio::test]
    async fn test_list_empty_renders_empty_message() {
        let (callbacks, _, _) = scripted(read_ok(serde_json::json!([])), write_ok(Value::Null));
        let result = execute_task_list(&callbacks, &serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "active_background_tasks: 0\nNo background tasks found."
        );
    }

    #[tokio::test]
    async fn test_list_active_only_false_uses_all_label() {
        let (callbacks, _, _) = scripted(read_ok(serde_json::json!([])), write_ok(Value::Null));
        let result =
            execute_task_list(&callbacks, &serde_json::json!({ "active_only": false })).await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "background_tasks: 0\nNo background tasks found."
        );
    }

    #[tokio::test]
    async fn test_list_invalid_args_return_error_without_calling_host() {
        let (callbacks, read_received, write_received) =
            scripted(read_ok(Value::Null), write_ok(Value::Null));
        for bad in [
            serde_json::json!({ "active_only": "yes" }),
            serde_json::json!({ "limit": 0 }),
            serde_json::json!({ "limit": 101 }),
            serde_json::json!({ "limit": "many" }),
        ] {
            let result = execute_task_list(&callbacks, &bad).await;
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid TaskList arguments"));
        }
        assert!(read_received.lock().unwrap().is_none());
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_list_invalid_wire_shape_returns_error() {
        let (callbacks, _, _) = scripted(
            read_ok(serde_json::json!({ "tasks": [] })),
            write_ok(Value::Null),
        );
        let result = execute_task_list(&callbacks, &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("Invalid task state from host"));
    }

    #[tokio::test]
    async fn test_list_unsupported_host_returns_failure_message() {
        let (callbacks, _, _) = scripted(
            Err("host does not support state bridge".into()),
            write_ok(Value::Null),
        );
        let result = execute_task_list(&callbacks, &serde_json::json!({})).await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_list_other_host_error_passes_through() {
        let (callbacks, _, _) = scripted(
            Err("State read error: [-32001] unknown domain: task".into()),
            write_ok(Value::Null),
        );
        let result = execute_task_list(&callbacks, &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("-32001"));
        assert!(result.content.contains("unknown domain"));
    }

    #[tokio::test]
    async fn test_list_turn_and_tool_call_ids_are_forwarded() {
        let (callbacks, read_received, _) =
            scripted(read_ok(serde_json::json!([])), write_ok(Value::Null));
        let result = execute_task_list(
            &callbacks,
            &serde_json::json!({ "turn_id": "turn-42", "tool_call_id": "call_abc" }),
        )
        .await;
        assert!(!result.is_error);
        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.turn_id, "turn-42");
        assert_eq!(request.tool_call_id, "call_abc");
    }

    #[test]
    fn test_task_list_tool_def_matches_v2_schema() {
        let def = task_list_tool_def();
        assert_eq!(def.name, "TaskList");
        assert_eq!(def.input_schema["type"], "object");
        assert_eq!(def.input_schema["additionalProperties"], false);
        assert_eq!(
            def.input_schema["properties"]["active_only"]["default"],
            true
        );
        assert_eq!(def.input_schema["properties"]["limit"]["maximum"], 100);
        assert!(def.description.contains("List background tasks"));
        assert!(def.description.contains("`TaskOutput`"));
    }
}
