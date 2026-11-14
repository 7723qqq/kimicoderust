use serde_json::Value;

use super::task_output::task_output_input_schema;
use crate::callbacks::HostCallbacks;
use crate::rpc::types::StateReadRequest;
use crate::tools::task_tools::{
    map_state_error, output_preview, parse_task_id, render_task_entry, truncated_line,
};
use crate::tools::{err_result, ok_result};
use crate::turn_loop::types::ExecutableToolResult;

pub async fn execute_task_output(
    callbacks: &dyn HostCallbacks,
    args: &Value,
) -> ExecutableToolResult {
    let task_id = match parse_task_id(args, "TaskOutput") {
        Ok(task_id) => task_id,
        Err(message) => return err_result(message),
    };
    let request = StateReadRequest {
        domain: "task".into(),
        key: task_id.clone(),
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
        Ok(response) => render_task_output(&response.value),
        Err(error) => map_state_error(error, Some(&task_id)),
    }
}

fn render_task_output(value: &Value) -> ExecutableToolResult {
    if !value.is_object() {
        return err_result("Invalid task state from host: expected a task output snapshot.".into());
    }
    let mut lines = vec![render_task_entry(value), String::new()];
    if let Some(truncated) = truncated_line(value) {
        lines.push(truncated);
    }
    lines.push("[output]".into());
    lines.push(output_preview(value));
    ok_result(lines.join("\n"))
}

pub fn task_output_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "TaskOutput".into(),
        description: r#"Retrieve a non-blocking snapshot of a running or completed background task.

Use after `Bash(run_in_background=true)`, `Agent(run_in_background=true)`, or `AskUserQuestion(background=true)` to check progress or read completed output. Prefer automatic completion notifications over polling. If your next step depends on task completion, run the task in the foreground instead."#
            .into(),
        input_schema: task_output_input_schema(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::task_tools::STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE;
    use crate::tools::task_tools::tests::{read_ok, sample_snapshot, scripted, write_ok};

    #[tokio::test]
    async fn test_output_reads_task_key_and_renders() {
        let (callbacks, read_received, write_received) =
            scripted(read_ok(sample_snapshot()), write_ok(Value::Null));
        let result =
            execute_task_output(&callbacks, &serde_json::json!({ "task_id": "task-1" })).await;
        assert!(!result.is_error);
        assert!(result.content.contains("task_id: task-1"));
        assert!(result.content.contains("status: completed"));
        assert!(result.content.contains("output_path: C:/logs/task-1.log"));
        assert!(result.content.contains("full_output_available: true"));
        assert!(result.content.contains("[output]"));
        assert!(result.content.contains("All tests passed."));

        let metadata = result.content.split("[output]").next().unwrap();
        assert!(!metadata.contains("output:"));
        assert!(!metadata.contains("preview:"));
        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "task");
        assert_eq!(request.key, "task-1");
        assert!(write_received.lock().unwrap().is_none());
    }

    #[test]
    fn test_output_truncated_note_variants() {
        let mut snapshot = sample_snapshot();
        snapshot["truncated"] = serde_json::json!(true);
        let rendered = render_task_output(&snapshot);
        assert!(!rendered.is_error);
        assert!(
            rendered
                .content
                .contains("[Truncated. Full output: C:/logs/task-1.log]")
        );

        snapshot["fullOutputAvailable"] = serde_json::json!(false);
        let rendered = render_task_output(&snapshot);
        assert!(
            rendered
                .content
                .contains("[Truncated. No persisted full log is available for this task.]")
        );

        snapshot["outputPath"] = Value::Null;
        let rendered = render_task_output(&snapshot);
        assert!(
            rendered
                .content
                .contains("[Truncated. No persisted full log is available for this task.]")
        );
    }

    #[test]
    fn test_output_truncated_contract_field_fallback() {
        let mut snapshot = sample_snapshot();
        snapshot.as_object_mut().unwrap().remove("truncated");
        snapshot.as_object_mut().unwrap().remove("preview");
        snapshot["outputTruncated"] = serde_json::json!(true);
        snapshot["output"] = serde_json::json!("partial output");
        let rendered = render_task_output(&snapshot);
        assert!(
            rendered
                .content
                .contains("[Truncated. Full output: C:/logs/task-1.log]")
        );
        assert!(rendered.content.contains("[output]\npartial output"));
    }

    #[test]
    fn test_output_no_preview_renders_placeholder() {
        let mut snapshot = sample_snapshot();
        snapshot["preview"] = Value::Null;
        let rendered = render_task_output(&snapshot);
        assert!(rendered.content.contains("[output]\n[no output available]"));
    }

    #[tokio::test]
    async fn test_output_not_found_maps_to_task_not_found() {
        let (callbacks, _, _) = scripted(
            Err("State read error: [-32002] unknown key: task-9".into()),
            write_ok(Value::Null),
        );
        let result =
            execute_task_output(&callbacks, &serde_json::json!({ "task_id": "task-9" })).await;
        assert!(result.is_error);
        assert_eq!(result.content, "Task not found: task-9");
    }

    #[tokio::test]
    async fn test_output_invalid_args_return_error_without_calling_host() {
        let (callbacks, read_received, write_received) =
            scripted(read_ok(Value::Null), write_ok(Value::Null));
        for bad in [serde_json::json!({}), serde_json::json!({ "task_id": "" })] {
            let result = execute_task_output(&callbacks, &bad).await;
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid TaskOutput arguments"));
        }
        assert!(read_received.lock().unwrap().is_none());
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_output_invalid_wire_shape_returns_error() {
        let (callbacks, _, _) = scripted(read_ok(serde_json::json!("nope")), write_ok(Value::Null));
        let result =
            execute_task_output(&callbacks, &serde_json::json!({ "task_id": "task-1" })).await;
        assert!(result.is_error);
        assert!(result.content.contains("Invalid task state from host"));
    }

    #[tokio::test]
    async fn test_output_unsupported_host_returns_failure_message() {
        let (callbacks, _, _) = scripted(
            Err("host does not support state bridge".into()),
            write_ok(Value::Null),
        );
        let result =
            execute_task_output(&callbacks, &serde_json::json!({ "task_id": "task-1" })).await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[test]
    fn test_task_output_tool_def_matches_v2_schema() {
        let def = task_output_tool_def();
        assert_eq!(def.name, "TaskOutput");
        assert_eq!(def.input_schema["required"][0], "task_id");
        assert!(def.description.contains("Retrieve a non-blocking snapshot"));
    }
}
