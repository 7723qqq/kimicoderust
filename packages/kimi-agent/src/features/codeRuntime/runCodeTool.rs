use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::app::config::config::IConfigService;
use crate::features::codeRuntime::codeExecutor::{CodeRunOptions, CodeRunOutcome, run_code_in_subprocess};
use crate::features::codeRuntime::codeRuntime::{RunCodeInput, run_code_input_schema};
use crate::i18n::{LocalizedText, i18n_params};
use crate::tool::tool_contract::ToolSideEffect;
use crate::wire::record::WireAsyncError;
use crate::workspace::sandbox::sandbox::{SandboxMode, resolve_sandbox_policy};

pub const RUN_CODE_TOOL_NAME: &str = "run_code";
pub const RUN_CODE_EXECUTION_DESCRIPTION: &str = "Running code in an isolated subprocess";
pub const RUN_CODE_DESCRIPTION: &str = include_str!("tools/run_code/run_code.md");

#[derive(Clone)]
pub struct RunCodeTool {
    config: Arc<dyn IConfigService>,
}

impl RunCodeTool {
    pub fn new(config: Arc<dyn IConfigService>) -> Self {
        Self { config }
    }

    pub fn name(&self) -> &'static str {
        RUN_CODE_TOOL_NAME
    }

    pub fn description(&self) -> &'static str {
        RUN_CODE_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        run_code_input_schema()
            .as_object()
            .cloned()
            .expect("RunCode input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<RunCodeInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for run_code: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &RunCodeInput) -> ToolExecution {
        let code = args.code.clone();
        let timeout_ms = args.timeout_ms;
        let config = self.config.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(RUN_CODE_EXECUTION_DESCRIPTION.to_string()),
            stop_batch_after_this: None,
            approval_rule: RUN_CODE_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let code = code.clone();
                let config = config.clone();
                Box::pin(async move {
                    let workspace_root = std::env::current_dir()
                        .map(|dir| dir.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let policy = resolve_sandbox_policy(config.as_ref(), &workspace_root);
                    if policy.mode != SandboxMode::Off {
                        return Ok(ExecutableToolResult::error(
                            LocalizedText::with_params(
                                "toolsV2.sandbox.codeExecutionBlocked",
                                i18n_params!["mode" => policy.mode.as_str()],
                            )
                            .render(),
                        ));
                    }

                    let outcome = run_code_in_subprocess(
                        &code,
                        CodeRunOptions {
                            timeout_ms: Some(timeout_ms),
                            max_output_chars: None,
                        },
                    )
                    .await;
                    Ok(render(outcome))
                })
            }),
        })
    }
}

fn render(outcome: CodeRunOutcome) -> ExecutableToolResult {
    let mut parts: Vec<String> = Vec::new();
    match &outcome.error {
        Some(error) => parts.push(format!("error ({}): {}", error.kind, error.message)),
        None => match &outcome.value {
            Some(value) => {
                let json =
                    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
                parts.push(format!("value: {json}"));
            }
            None => parts.push("value: undefined".to_string()),
        },
    }
    if !outcome.logs.is_empty() {
        parts.push(format!("logs:\n{}", outcome.logs.join("\n")));
    }
    ExecutableToolResult {
        output: parts.join("\n").into(),
        is_error: Some(outcome.error.is_some()),
        ..Default::default()
    }
}

impl ExecutableTool for RunCodeTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Execute)
    }

    fn name(&self) -> &str {
        RunCodeTool::name(self)
    }

    fn description(&self) -> &str {
        RunCodeTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        RunCodeTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
