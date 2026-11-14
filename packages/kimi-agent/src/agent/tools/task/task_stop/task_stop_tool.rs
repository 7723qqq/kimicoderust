use serde_json::Value;

use super::task_stop::task_stop_input_schema;
use crate::callbacks::HostCallbacks;
use crate::rpc::types::StateWriteRequest;
use crate::tool::output_pointer::{OutputUnit, output_size_line};
use crate::tools::task_tools::{map_state_error, parse_task_id, truncated_line};
use crate::tools::{err_result, ok_result};
use crate::turn_loop::types::ExecutableToolResult;
use crate::turn_loop::wall_time;

pub async fn execute_task_stop(
    callbacks: &dyn HostCallbacks,
    args: &Value,
) -> ExecutableToolResult {
    let task_id = match parse_task_id(args, "TaskStop") {
        Ok(task_id) => task_id,
        Err(message) => return err_result(message),
    };

    let reason = match args.get("reason").and_then(|r| r.as_str()) {
        Some(reason) => {
            let trimmed = reason.trim();
            if trimmed.is_empty() {
                "Stopped by TaskStop".to_string()
            } else {
                trimmed.to_string()
            }
        }
        None => "Stopped by TaskStop".to_string(),
    };
    let request = StateWriteRequest {
        domain: "task".into(),
        key: "task".into(),
        value: serde_json::json!({ "action": "stop", "id": task_id, "reason": reason }),
        undoable: false,
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
    match callbacks.state_write(request).await {
        Ok(response) => render_task_stop(&response.value, &task_id, &reason),
        Err(error) => map_state_error(error, Some(&task_id)),
    }
}

const NO_PARTIAL_OUTPUT: &str =
    "[partial output] none — the subagent wrote no text in this run before it ended.";

fn render_task_stop(value: &Value, task_id: &str, reason: &str) -> ExecutableToolResult {
    let Some(obj) = value.as_object() else {
        return err_result("Invalid task state from host: expected the stopped task.".into());
    };
    let id = obj
        .get("taskId")
        .or_else(|| obj.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or(task_id);
    let mut lines = vec![format!("task_id: {id}")];
    if let Some(status) = obj.get("status").and_then(|v| v.as_str()) {
        lines.push(format!("status: {status}"));
    }
    let stop_reason = obj
        .get("stopReason")
        .or_else(|| obj.get("reason"))
        .and_then(|v| v.as_str())
        .unwrap_or(reason);
    lines.push(format!("reason: {stop_reason}"));
    let is_subagent = matches!(
        obj.get("kind").and_then(|value| value.as_str()),
        Some("subagent" | "agent")
    );
    if is_subagent {
        let agent_id = obj.get("agentId").and_then(|value| value.as_str());
        if let Some(agent_id) = agent_id {
            lines.push(format!("agent_id: {agent_id}"));
        }
        let output_path = obj
            .get("outputPath")
            .or_else(|| obj.get("output_path"))
            .and_then(|value| value.as_str())
            .filter(|_| {
                obj.get("fullOutputAvailable").and_then(Value::as_bool) != Some(false)
            });
        if let Some(path) = output_path {
            lines.push(format!("output_path: {path}"));
            if let Some(size) = obj
                .get("outputSizeBytes")
                .or_else(|| obj.get("output_size_bytes"))
                .and_then(Value::as_u64)
            {
                lines.push(output_size_line(OutputUnit::Bytes, size as usize, None));
            }
        }
        lines.push(String::new());
        if let Some(truncated) = truncated_line(value) {
            lines.push(truncated);
        }
        let output = obj
            .get("output")
            .or_else(|| obj.get("preview"))
            .and_then(Value::as_str)
            .filter(|output| !output.is_empty());
        lines.push(output.unwrap_or(NO_PARTIAL_OUTPUT).to_string());
        if let Some(agent_id) = agent_id {
            lines.push(String::new());
            lines.push(format!(
                "resume_hint: To continue this subagent from where it stopped, call Agent(resume=\"{agent_id}\", prompt=\"...\"). Use agent_id (\"{agent_id}\"), not task_id (\"{id}\")."
            ));
        }
    }
    let body = lines.join("\n");

    let content = match task_wall_time_ms(value) {
        Some(duration_ms) => format!("{}\n{body}", wall_time::wall_time_header(duration_ms)),
        None => body,
    };
    ok_result(content)
}

fn task_wall_time_ms(value: &Value) -> Option<u64> {
    let obj = value.as_object()?;
    let started_at = obj.get("startedAt").and_then(|v| v.as_u64())?;
    let ended_at = obj
        .get("endedAt")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(now_ms);
    Some(ended_at.saturating_sub(started_at))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn task_stop_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "TaskStop".into(),
        description: r#"Stop a running background task.

Only use this when a task must genuinely be cancelled — for a task that is
finishing normally, wait for its completion notification or inspect it with
`TaskOutput` instead of stopping it.

Guidelines:
- This is a general-purpose stop capability for any background task. It is not
  a bash-specific kill.
- Stopping a task is destructive: it may leave partial side effects behind.
  Use it with care.
- If the task has already finished, this tool simply returns its current
  status."#
            .into(),
        input_schema: task_stop_input_schema(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::task_tools::STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE;
    use crate::tools::task_tools::tests::{read_ok, scripted, write_ok};

    #[tokio::test]
    async fn test_stop_sends_action_and_renders() {
        let (callbacks, read_received, write_received) = scripted(
            read_ok(Value::Null),
            write_ok(serde_json::json!({
                "taskId": "task-1",
                "status": "killed",
                "stopReason": "Stopped by TaskStop"
            })),
        );
        let result =
            execute_task_stop(&callbacks, &serde_json::json!({ "task_id": "task-1" })).await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "task_id: task-1\nstatus: killed\nreason: Stopped by TaskStop"
        );
        assert!(read_received.lock().unwrap().is_none());
        let request = write_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "task");
        assert_eq!(request.key, "task");
        assert!(!request.undoable);
        assert_eq!(request.value["action"], "stop");
        assert_eq!(request.value["id"], "task-1");
    }

    #[test]
    fn test_stop_render_falls_back_to_requested_id_and_reason() {
        let rendered = render_task_stop(
            &serde_json::json!({ "status": "killed" }),
            "task-1",
            "Stopped by TaskStop",
        );
        assert!(!rendered.is_error);
        assert_eq!(
            rendered.content,
            "task_id: task-1\nstatus: killed\nreason: Stopped by TaskStop"
        );

        let rendered = render_task_stop(
            &serde_json::json!({ "id": "task-2", "status": "completed", "reason": "done" }),
            "task-1",
            "Stopped by TaskStop",
        );
        assert_eq!(
            rendered.content,
            "task_id: task-2\nstatus: completed\nreason: done"
        );
    }

    #[test]
    fn bg_partial_output_task_stop_render_includes_output_and_resume_hint() {
        let rendered = render_task_stop(
            &serde_json::json!({
                "taskId": "task-1",
                "kind": "subagent",
                "agentId": "agent-child",
                "status": "killed",
                "stopReason": "Stopped by TaskStop",
                "output": "[partial output] The subagent did not finish this run.\n\nunverified finding",
                "outputPath": "C:/state/tasks/task-1/output.log"
            }),
            "task-1",
            "Stopped by TaskStop",
        );
        assert!(!rendered.is_error);
        assert!(rendered.content.contains("agent_id: agent-child"));
        assert!(
            rendered
                .content
                .contains("output_path: C:/state/tasks/task-1/output.log")
        );
        assert!(rendered.content.contains("[partial output]"));
        assert!(rendered.content.contains("unverified finding"));
        assert!(
            rendered
                .content
                .contains("Agent(resume=\"agent-child\", prompt=\"...\")")
        );
    }

    #[test]
    fn test_stop_render_prepends_wall_time_when_the_response_is_timed() {
        let rendered = render_task_stop(
            &serde_json::json!({
                "taskId": "task-1",
                "status": "killed",
                "stopReason": "Stopped by TaskStop",
                "startedAt": 1700000000000u64,
                "endedAt": 1700000002500u64
            }),
            "task-1",
            "Stopped by TaskStop",
        );
        assert!(!rendered.is_error);
        assert_eq!(
            rendered.content,
            "Wall time: 2.500 seconds\n\
             task_id: task-1\n\
             status: killed\n\
             reason: Stopped by TaskStop"
        );
    }

    #[tokio::test]
    async fn test_stop_custom_reason_is_trimmed_for_fallback() {
        let (callbacks, _, write_received) = scripted(
            read_ok(Value::Null),
            write_ok(serde_json::json!({ "taskId": "task-1", "status": "killed" })),
        );
        let result = execute_task_stop(
            &callbacks,
            &serde_json::json!({ "task_id": "task-1", "reason": "  user asked  " }),
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("reason: user asked"));

        let request = write_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.value["action"], "stop");
        assert_eq!(request.value["reason"], "user asked");
    }

    #[tokio::test]
    async fn test_stop_not_found_maps_to_task_not_found() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32002] task not found: task-9".into()),
        );
        let result =
            execute_task_stop(&callbacks, &serde_json::json!({ "task_id": "task-9" })).await;
        assert!(result.is_error);
        assert_eq!(result.content, "Task not found: task-9");
    }

    #[tokio::test]
    async fn test_stop_invalid_args_return_error_without_calling_host() {
        let (callbacks, read_received, write_received) =
            scripted(read_ok(Value::Null), write_ok(Value::Null));
        for bad in [serde_json::json!({}), serde_json::json!({ "task_id": 42 })] {
            let result = execute_task_stop(&callbacks, &bad).await;
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid TaskStop arguments"));
        }
        assert!(read_received.lock().unwrap().is_none());
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_stop_unsupported_host_returns_failure_message() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32603] host does not support state bridge".into()),
        );
        let result =
            execute_task_stop(&callbacks, &serde_json::json!({ "task_id": "task-1" })).await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[test]
    fn test_task_stop_tool_def_matches_v2_schema() {
        let def = task_stop_tool_def();
        assert_eq!(def.name, "TaskStop");
        assert_eq!(def.input_schema["required"][0], "task_id");
        assert_eq!(
            def.input_schema["properties"]["reason"]["default"],
            "Stopped by TaskStop"
        );
        assert!(def.description.contains("Stop a running background task"));
    }
}
