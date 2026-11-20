use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::enterSpecMode::{EnterSpecModeInput, enter_spec_mode_input_schema};
use super::super::super::spec::{IAgentSpecService, SPEC_REQUIRED_FILES};
use crate::agent::actor_service::services::TelemetryService;
use crate::agent::mode_mutex::mode_mutex::{AgentModeMutexService, ExclusiveReviewMode};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::tool::input_schema::to_input_json_schema;
use crate::wire::record::WireAsyncError;

pub const ENTER_SPEC_MODE_DESCRIPTION: &str = include_str!("enter-spec-mode.md");
pub const SPEC_MODE_ALREADY_ACTIVE_MESSAGE: &str =
    "Spec mode is already active. Use ExitSpecMode when the spec is ready.";

#[derive(Clone)]
pub struct EnterSpecModeTool {
    spec: Arc<dyn IAgentSpecService>,
    mutex: Arc<dyn AgentModeMutexService>,
    telemetry: Arc<dyn TelemetryService>,
}

impl EnterSpecModeTool {
    pub fn new(
        spec: Arc<dyn IAgentSpecService>,
        mutex: Arc<dyn AgentModeMutexService>,
        telemetry: Arc<dyn TelemetryService>,
    ) -> Self {
        Self {
            spec,
            mutex,
            telemetry,
        }
    }

    pub fn name(&self) -> &'static str {
        "EnterSpecMode"
    }

    pub fn description(&self) -> &'static str {
        ENTER_SPEC_MODE_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        to_input_json_schema(enter_spec_mode_input_schema())
            .as_object()
            .cloned()
            .expect("EnterSpecMode input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<EnterSpecModeInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for EnterSpecMode: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, _args: &EnterSpecModeInput) -> ToolExecution {
        let tool = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some("Requesting to enter spec mode".to_string()),
            stop_batch_after_this: None,
            approval_rule: "EnterSpecMode".to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let tool = tool.clone();
                Box::pin(async move {
                    let before = tool.spec.status().await.map_err(wire_error)?;
                    if before.is_some() {
                        return Ok(ExecutableToolResult::error(
                            SPEC_MODE_ALREADY_ACTIVE_MESSAGE,
                        ));
                    }

                    let previous = tool.mutex.active_mode();
                    if let Err(error) = tool.mutex.switch_to(ExclusiveReviewMode::Spec).await {
                        return Ok(ExecutableToolResult::error(format!(
                            "Failed to enter spec mode: {error}"
                        )));
                    }

                    tool.telemetry
                        .track2("spec_enter_resolved", json!({ "outcome": "accepted" }));
                    let after = tool.spec.status().await.map_err(wire_error)?;
                    Ok(ExecutableToolResult {
                        output: entered_spec_mode_message(
                            after.as_ref().map(|data| data.dir.as_str()),
                            previous.map(ExclusiveReviewMode::as_str),
                        )
                        .into(),
                        is_error: Some(false),
                        ..Default::default()
                    })
                })
            }),
        })
    }
}

pub fn entered_spec_mode_message(spec_dir: Option<&str>, evicted: Option<&str>) -> String {
    let mut lines = Vec::new();
    if let Some(evicted) = evicted {
        lines.push(format!("Left {evicted} mode to enter spec mode."));
        lines.push(String::new());
    }
    let Some(spec_dir) = spec_dir else {
        lines.extend([
            "Spec mode is now active.".to_string(),
            String::new(),
            "Write the requirements, design and tasks documents, then call ExitSpecMode for approval.".to_string(),
            "No spec directory path is available in this host, so writes cannot be authorised.".to_string(),
        ]);
        return lines.join("\n");
    };
    lines.extend([
        "Spec mode is now active. Your workflow:".to_string(),
        String::new(),
        format!("Spec directory: {spec_dir}"),
        String::new(),
        "1. Investigate with read-only tools (Read, Grep, Glob). Use Bash only when needed.".to_string(),
        "2. Write the requirements document — what to build, and what is out of scope.".to_string(),
        "3. Write the design document — how to build it, and which alternatives you rejected.".to_string(),
        "4. Write the tasks document — each task with acceptance criteria that can be checked.".to_string(),
        "5. Call ExitSpecMode for approval.".to_string(),
        String::new(),
        format!("Only these files may be written: {}.", SPEC_REQUIRED_FILES.join(", ")),
        "Every other write is rejected until the user approves the spec.".to_string(),
    ]);
    lines.join("\n")
}

fn wire_error(message: String) -> WireAsyncError {
    Box::new(kimi_core::primitives::errors::Error2::new(
        kimi_core::primitives::errors::INTERNAL_CODE,
        message,
    ))
}

impl ExecutableTool for EnterSpecModeTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        EnterSpecModeTool::name(self)
    }

    fn description(&self) -> &str {
        EnterSpecModeTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        EnterSpecModeTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
