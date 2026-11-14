use std::sync::Arc;

use once_cell::sync::Lazy;
use serde_json::{Map, Value, json};

use crate::human::llm::message::ContentPart;
use crate::human::tool::executor::{TaskWaitInput, TaskWaitOutcome, ToolResult};
use crate::human::tool::tool::{ToolDefinition, define_tool};

pub const WAIT_FOR_MAX_TIMEOUT_S: u64 = 90;
const DESCRIPTION: &str = include_str!("wait-for.md");

struct WaitForArguments {
    task_id: Option<String>,
    timeout_ms: u64,
}

fn parse_wait_for_arguments(raw: Option<&str>) -> Result<WaitForArguments, String> {
    let display = raw.unwrap_or("null");
    let parsed = match raw {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str::<Value>(raw).map_err(|_| format!("invalid WaitFor arguments: {display}"))?
        }
        _ => json!({}),
    };
    let Some(object) = parsed.as_object() else {
        return Err(format!("invalid WaitFor arguments: {display}"));
    };
    let timeout = object
        .get("timeout")
        .and_then(Value::as_f64)
        .filter(|timeout| {
            timeout.fract() == 0.0
                && *timeout >= 1.0
                && *timeout <= WAIT_FOR_MAX_TIMEOUT_S as f64
        })
        .map(|timeout| timeout as u64)
        .ok_or_else(|| format!("invalid WaitFor arguments: {display}"))?;
    Ok(WaitForArguments {
        task_id: object
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        timeout_ms: timeout * 1000,
    })
}

fn format_wait_for_outcome(outcome: &TaskWaitOutcome, timeout_ms: u64) -> String {
    let mut lines = Vec::new();
    if outcome.completed.is_empty() && outcome.running.is_empty() {
        lines.push("no async tool calls running".to_string());
    }
    if !outcome.completed.is_empty() {
        lines.push(format!("completed: {}", outcome.completed.join(", ")));
    }
    if !outcome.running.is_empty() {
        lines.push(format!("running: {}", outcome.running.join(", ")));
    }
    if outcome.timed_out {
        lines.push(format!("timedOut after {timeout_ms} ms"));
    }
    lines.join("\n")
}

fn text(text: String, is_error: Option<bool>) -> ToolResult {
    ToolResult {
        content: vec![ContentPart::Text { text, meta: None }],
        is_error,
    }
}

pub static WAIT_FOR_TOOL: Lazy<ToolDefinition> = Lazy::new(|| {
    let parameters = json!({
        "type": "object",
        "properties": {
            "timeout": {
                "type": "integer",
                "minimum": 1,
                "maximum": WAIT_FOR_MAX_TIMEOUT_S,
                "description": format!("Maximum time to wait, in seconds (1-{WAIT_FOR_MAX_TIMEOUT_S}). A timeout is not an error: the tool returns the tasks that are still running, and you can call it again to keep waiting.")
            },
            "task_id": {
                "type": "string",
                "description": "The background task ID to wait for. When omitted, the wait ends as soon as any background task that was running at call time finishes."
            }
        },
        "required": ["timeout"]
    });
    define_tool(ToolDefinition {
        name: "WaitFor".to_string(),
        description: DESCRIPTION.to_string(),
        parameters: parameters.as_object().cloned().unwrap_or_else(Map::new),
        deferred: None,
        execute: Arc::new(|input| {
            Box::pin(async move {
                let parsed = match parse_wait_for_arguments(input.tool_call.arguments.as_deref()) {
                    Ok(parsed) => parsed,
                    Err(error) => return Ok(text(error, Some(true))),
                };
                let Some(wait_for_tasks) = input.wait_for_tasks else {
                    return Ok(text(
                        "WaitFor requires background task support from the agent".to_string(),
                        Some(true),
                    ));
                };
                let outcome = wait_for_tasks(TaskWaitInput {
                    task_id: parsed.task_id,
                    timeout_ms: parsed.timeout_ms,
                })
                .await?;
                if !outcome.unknown.is_empty() {
                    return Ok(text(
                        format!("Task not found: {}", outcome.unknown.join(", ")),
                        Some(true),
                    ));
                }
                Ok(text(
                    format_wait_for_outcome(&outcome, parsed.timeout_ms),
                    None,
                ))
            })
        }),
    })
    .expect("WaitFor has a valid name")
});
