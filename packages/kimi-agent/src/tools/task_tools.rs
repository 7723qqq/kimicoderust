use serde_json::Value;

pub use crate::agent::tools::task::task_list::task_list_tool::{
    execute_task_list, task_list_tool_def,
};
pub use crate::agent::tools::task::task_output::task_output_tool::{
    execute_task_output, task_output_tool_def,
};
pub use crate::agent::tools::task::task_stop::task_stop_tool::{
    execute_task_stop, task_stop_tool_def,
};
pub use crate::agent::tools::task::task_wait::task_wait_tool::{
    WaitContext, WaitTally, execute_task_wait, task_wait_tool_def, wait_for_enabled, wait_for_flag,
    wait_for_tool_def,
};
use crate::tools::err_result;
use crate::tools::task_format;
use crate::turn_loop::types::ExecutableToolResult;

pub(crate) const STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE: &str = "The connected client does not support the state bridge. Do NOT call this tool again — the host cannot manage background tasks.";

pub(crate) fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "timed_out" | "killed" | "lost"
    )
}

pub(crate) fn render_task_entry(value: &Value) -> String {
    let Some(obj) = value.as_object() else {
        return String::new();
    };
    let mut mapped = serde_json::Map::new();
    for (key, field) in obj {
        match key.as_str() {
            "output" | "preview" => continue,
            "id" => mapped.insert("taskId".into(), field.clone()),
            _ => mapped.insert(key.clone(), field.clone()),
        };
    }
    task_format::format_task_record(&Value::Object(mapped))
}

pub(crate) fn truncated_line(value: &Value) -> Option<String> {
    let obj = value.as_object()?;
    let truncated = obj
        .get("truncated")
        .or_else(|| obj.get("outputTruncated"))
        .and_then(|v| v.as_bool());
    if truncated != Some(true) {
        return None;
    }
    let full_available = obj.get("fullOutputAvailable").and_then(|v| v.as_bool()) == Some(true);
    let output_path = obj.get("outputPath").and_then(|v| v.as_str());
    match (full_available, output_path) {
        (true, Some(path)) => Some(format!("[Truncated. Full output: {path}]")),
        _ => Some("[Truncated. No persisted full log is available for this task.]".into()),
    }
}

pub(crate) fn output_preview(value: &Value) -> String {
    match value.get("preview").or_else(|| value.get("output")) {
        Some(Value::String(preview)) if !preview.is_empty() => preview.clone(),
        _ => "[no output available]".into(),
    }
}

pub(crate) fn has_output_section(value: &Value) -> bool {
    value.get("preview").is_some() || value.get("output").is_some()
}

pub(crate) fn parse_task_id(args: &Value, tool: &str) -> Result<String, String> {
    match args.get("task_id").and_then(|t| t.as_str()) {
        Some(task_id) if !task_id.is_empty() => Ok(task_id.to_string()),
        _ => Err(format!(
            "Invalid {tool} arguments: `task_id` must be a non-empty string."
        )),
    }
}

pub(crate) fn map_state_error(error: String, task_id: Option<&str>) -> ExecutableToolResult {
    if error.contains("does not support state bridge") {
        err_result(STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE.into())
    } else if let Some(task_id) = task_id
        && is_task_not_found(&error)
    {
        err_result(format!("Task not found: {task_id}"))
    } else {
        err_result(error)
    }
}

pub(crate) fn is_task_not_found(error: &str) -> bool {
    error.contains("-32002") || error.to_lowercase().contains("task not found")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::callbacks::HostCallbacks;
    use crate::rpc::types::{
        BoxFuture, PermissionDecision, StateReadRequest, StateReadResponse, StateWriteRequest,
        StateWriteResponse,
    };
    use std::sync::Arc;

    pub(crate) struct ScriptedCallbacks {
        read_response: Result<StateReadResponse, String>,
        write_response: Result<StateWriteResponse, String>,
        read_received: Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        write_received: Arc<std::sync::Mutex<Option<StateWriteRequest>>>,
    }

    impl HostCallbacks for ScriptedCallbacks {
        fn llm_chat(
            &self,
            _: crate::rpc::types::LlmChatRequest,
        ) -> BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _: crate::rpc::types::ToolExecuteRequest,
        ) -> BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _: crate::rpc::types::PermissionCheckRequest,
        ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
            Box::pin(async { Ok(PermissionDecision::allow()) })
        }

        fn state_read(
            &self,
            request: StateReadRequest,
        ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
            *self.read_received.lock().unwrap() = Some(request);
            let response = self.read_response.clone();
            Box::pin(async move { response })
        }

        fn state_write(
            &self,
            request: StateWriteRequest,
        ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
            *self.write_received.lock().unwrap() = Some(request);
            let response = self.write_response.clone();
            Box::pin(async move { response })
        }
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn scripted(
        read_response: Result<StateReadResponse, String>,
        write_response: Result<StateWriteResponse, String>,
    ) -> (
        ScriptedCallbacks,
        Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        Arc<std::sync::Mutex<Option<StateWriteRequest>>>,
    ) {
        let read_received = Arc::new(std::sync::Mutex::new(None));
        let write_received = Arc::new(std::sync::Mutex::new(None));
        (
            ScriptedCallbacks {
                read_response,
                write_response,
                read_received: read_received.clone(),
                write_received: write_received.clone(),
            },
            read_received,
            write_received,
        )
    }

    pub(crate) fn read_ok(value: Value) -> Result<StateReadResponse, String> {
        Ok(StateReadResponse { value })
    }

    pub(crate) fn write_ok(value: Value) -> Result<StateWriteResponse, String> {
        Ok(StateWriteResponse { ok: true, value })
    }

    pub(crate) fn sample_snapshot() -> Value {
        serde_json::json!({
            "taskId": "task-1",
            "description": "Running tests",
            "status": "completed",
            "startedAt": 1700000000000u64,
            "endedAt": 1700000001000u64,
            "outputPath": "C:/logs/task-1.log",
            "outputSizeBytes": 1024,
            "previewBytes": 512,
            "truncated": false,
            "fullOutputAvailable": true,
            "preview": "All tests passed."
        })
    }

    #[test]
    fn test_render_task_entry_maps_contract_id_to_task_id() {
        let rendered = render_task_entry(&serde_json::json!({
            "id": "task-9",
            "description": "Contract shape",
            "status": "running"
        }));
        assert!(rendered.contains("task_id: task-9"));
        assert!(rendered.contains("description: Contract shape"));
        let rendered = render_task_entry(&serde_json::json!({
            "taskId": "task-8",
            "description": "Host shape",
            "status": "running"
        }));
        assert!(rendered.contains("task_id: task-8"));
    }
}
