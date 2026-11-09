use serde_json::Value;

use crate::primitives::deadline::Deadline;
use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};
use crate::workflow::code_runtime::{
    CodeRunLimits, CodeRunOutcome, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, execute_code,
};

pub const RUN_CODE_TOOL_DESCRIPTION: &str = include_str!("run_code_tool.md");

pub fn run_code_tool_def() -> ToolInfo {
    ToolInfo {
        name: "run_code".into(),
        description: RUN_CODE_TOOL_DESCRIPTION.into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The TypeScript/JavaScript program to run. The body executes as an async function, so top-level await and return work. console.log/info/warn/error/debug output is captured."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": MIN_TIMEOUT_MS,
                    "maximum": MAX_TIMEOUT_MS,
                    "default": DEFAULT_TIMEOUT_MS,
                    "description": "Execution budget in milliseconds before the worker is terminated (default 30000)."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    }
}

pub async fn execute_run_code(args: &Value) -> ExecutableToolResult {
    let code = match args
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
    {
        Some(code) => code.to_string(),
        None => return error_result("`code` must be a non-empty string."),
    };
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    let limits = CodeRunLimits {
        timeout_ms,
        ..CodeRunLimits::default()
    };

    let (sender, receiver) = tokio::sync::oneshot::channel();
    let worker = std::thread::Builder::new()
        .name("run-code".to_string())
        .spawn(move || {
            let outcome = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(execute_code(&code, limits)),
                Err(error) => CodeRunOutcome::failed("worker-error", error.to_string()),
            };
            let _ = sender.send(outcome);
        });
    if let Err(error) = worker {
        return error_result(format!("could not start the run_code worker: {error}"));
    }

    let deadline = Deadline::new("run_code", std::time::Duration::from_millis(timeout_ms));
    let outcome = match deadline
        .wait("waiting for the code worker to report a result", receiver)
        .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => CodeRunOutcome::failed(
            "worker-error",
            "the run_code worker stopped without reporting a result",
        ),

        Err(_) => CodeRunOutcome::failed(
            "timeout",
            format!(
                "program did not finish within {timeout_ms}ms; the worker thread may still be running"
            ),
        ),
    };

    render(outcome)
}

fn render(outcome: CodeRunOutcome) -> ExecutableToolResult {
    let mut parts: Vec<String> = Vec::new();
    match &outcome.error {
        Some(error) => parts.push(format!("error ({}): {}", error.kind, error.message)),
        None => match &outcome.value {
            Some(value) => {
                let json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
                parts.push(format!("value: {json}"));
            }
            None => parts.push("value: undefined".to_string()),
        },
    }
    if !outcome.logs.is_empty() {
        parts.push(format!("logs:\n{}", outcome.logs.join("\n")));
    }
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        stop_turn: false,
        content: parts.join("\n"),
        is_error: outcome.error.is_some(),
        note: None,
        delivery: None,
        display: None,
    }
}

fn error_result(message: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        stop_turn: false,
        content: message.into(),
        is_error: true,
        note: None,
        delivery: None,
        display: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_def_matches_the_reference_contract() {
        let def = run_code_tool_def();
        assert_eq!(def.name, "run_code");
        assert_eq!(def.input_schema["required"], serde_json::json!(["code"]));
        assert_eq!(def.input_schema["additionalProperties"], false);
        assert_eq!(
            def.input_schema["properties"]["timeout_ms"]["default"],
            30_000
        );
        assert_eq!(
            def.input_schema["properties"]["timeout_ms"]["minimum"],
            1_000
        );
        assert_eq!(
            def.input_schema["properties"]["timeout_ms"]["maximum"],
            120_000
        );
    }

    #[tokio::test]
    async fn a_missing_code_argument_is_an_error_result() {
        let result = execute_run_code(&serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("`code`"), "{}", result.content);
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn renders_the_value_and_logs() {
        let result = execute_run_code(&serde_json::json!({
            "code": "console.log('hi'); return { n: 42 };",
            "timeout_ms": 5_000,
        }))
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "value: {\"n\":42}\nlogs:\nhi");
    }

    #[cfg(feature = "workflow-js")]
    #[tokio::test]
    async fn renders_a_thrown_error_with_its_kind() {
        let result = execute_run_code(&serde_json::json!({
            "code": "throw new Error('boom');",
            "timeout_ms": 5_000,
        }))
        .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("error (exception): "),
            "{}",
            result.content
        );
        assert!(result.content.contains("boom"), "{}", result.content);
    }
}
