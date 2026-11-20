use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::exitSpecMode::{
    ExitSpecModeInput, exit_spec_mode_input_schema, validate_options,
};
use super::super::super::spec::{IAgentSpecService, SPEC_REQUIRED_FILES, SpecData};
use crate::agent::actor_service::services::TelemetryService;
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::permission::PermissionMode;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool_input_display::{SpecReviewDocument, ToolInputDisplay};
use crate::wire::record::WireAsyncError;

pub const EXIT_SPEC_MODE_DESCRIPTION: &str = include_str!("exit-spec-mode.md");
pub const SPEC_MODE_NOT_ACTIVE_MESSAGE: &str =
    "ExitSpecMode can only be called while spec mode is active. Use EnterSpecMode (or /spec) first.";

#[derive(Clone)]
pub struct ExitSpecModeTool {
    spec: Arc<dyn IAgentSpecService>,
    permission_mode: Arc<dyn IAgentPermissionModeService>,
    telemetry: Arc<dyn TelemetryService>,
    revision_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl ExitSpecModeTool {
    pub fn new(
        spec: Arc<dyn IAgentSpecService>,
        permission_mode: Arc<dyn IAgentPermissionModeService>,
        telemetry: Arc<dyn TelemetryService>,
    ) -> Self {
        Self {
            spec,
            permission_mode,
            telemetry,
            revision_error: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn name(&self) -> &'static str {
        "ExitSpecMode"
    }

    pub fn description(&self) -> &'static str {
        EXIT_SPEC_MODE_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        to_input_json_schema(exit_spec_mode_input_schema())
            .as_object()
            .cloned()
            .expect("ExitSpecMode input schema is an object")
    }

    pub async fn resolve_execution_args(&self, args: &ExitSpecModeInput) -> ToolExecution {
        let display = self.resolve_spec_review_display_for(args).await;
        let tool = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display,
            description: Some("Presenting spec and exiting spec mode".to_string()),
            stop_batch_after_this: None,
            approval_rule: "ExitSpecMode".to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let tool = tool.clone();
                Box::pin(async move { tool.execution().await })
            }),
        })
    }

    async fn resolve_spec_review_display_for(
        &self,
        args: &ExitSpecModeInput,
    ) -> Option<ToolInputDisplay> {
        let data = match self.spec.status().await {
            Ok(data) => data,
            Err(_) => return None,
        };
        let data = data.filter(|data| data.complete)?;
        match self.spec.record_revision(data.clone()).await {
            Ok(()) => {
                *self
                    .revision_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            Err(error) => {
                *self
                    .revision_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
            }
        }
        resolve_spec_review_display(Some(&data), args)
    }

    fn take_revision_warning(&self) -> String {
        let error = self
            .revision_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        match error {
            Some(error) => format!(
                "\n\nWarning: this revision could not be snapshotted ({error}) — it will not appear in history."
            ),
            None => String::new(),
        }
    }

    async fn execution(&self) -> Result<ExecutableToolResult, WireAsyncError> {
        let status = match self.spec.status().await {
            Ok(status) => status,
            Err(error) => return Err(wire_error(error)),
        };
        let Some(status) = status else {
            return Ok(ExecutableToolResult::error(SPEC_MODE_NOT_ACTIVE_MESSAGE));
        };

        if !status.complete {
            return Ok(ExecutableToolResult::error(incomplete_output(&status)));
        }

        let file_count = SPEC_REQUIRED_FILES
            .iter()
            .filter(|name| {
                status
                    .files
                    .get(**name)
                    .is_some_and(|content| !content.trim().is_empty())
            })
            .count();
        self.telemetry
            .track2("spec_submitted", json!({ "file_count": file_count }));

        if let Err(error) = self.spec.exit().await {
            return Ok(ExecutableToolResult::error(format!(
                "Failed to exit spec mode: {error}"
            )));
        }

        if self.permission_mode.mode() == PermissionMode::Auto {
            self.telemetry
                .track2("spec_resolved", json!({ "outcome": "approved_without_review" }));
            let warning = self.take_revision_warning();
            return Ok(ExecutableToolResult {
                output: auto_approved_output(&status, &warning).into(),
                is_error: Some(false),
                ..Default::default()
            });
        }

        self.telemetry
            .track2("spec_resolved", json!({ "outcome": "approved" }));
        let warning = self.take_revision_warning();
        Ok(ExecutableToolResult {
            output: format!(
                "Exited spec mode. Spec saved to: {dir}\n\nWork through {dir}/tasks.md, recording progress in {dir}/progress.md as you go.{warning}",
                dir = status.dir
            )
            .into(),
            is_error: Some(false),
            ..Default::default()
        })
    }
}

pub fn resolve_spec_review_display(
    data: Option<&SpecData>,
    args: &ExitSpecModeInput,
) -> Option<ToolInputDisplay> {
    let data = data?;
    if !data.complete {
        return None;
    }
    Some(ToolInputDisplay::SpecReview {
        dir: data.dir.clone(),
        documents: SPEC_REQUIRED_FILES
            .iter()
            .map(|name| SpecReviewDocument {
                name: name.to_string(),
                content: data.files.get(*name).cloned().unwrap_or_default(),
            })
            .collect(),
        options: args.options.as_ref().and_then(|options| {
            (options.len() >= 2).then(|| options.clone())
        }),
    })
}

pub fn incomplete_output(data: &SpecData) -> String {
    format!(
        "The spec is incomplete. Write {} in {} before calling ExitSpecMode.",
        data.missing.join(", "),
        data.dir
    )
}

pub fn auto_approved_output(data: &SpecData, revision_warning: &str) -> String {
    format!(
        "Exited spec mode. Spec saved to: {} — work through {}/tasks.md, recording progress in {}/progress.md as you go.\nNote: this spec was auto-approved without user review — the user has NOT explicitly approved it. Follow the user's original instructions on whether to proceed with execution; if they asked you to stop, wait, or only summarize, do not start executing.{}",
        data.dir, data.dir, data.dir, revision_warning
    )
}

fn wire_error(message: String) -> WireAsyncError {
    Box::new(kimi_core::primitives::errors::Error2::new(
        kimi_core::primitives::errors::INTERNAL_CODE,
        message,
    ))
}

impl ExecutableTool for ExitSpecModeTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        ExitSpecModeTool::name(self)
    }

    fn description(&self) -> &str {
        ExitSpecModeTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        ExitSpecModeTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let tool = self.clone();
        Box::pin(async move {
            match serde_json::from_value::<ExitSpecModeInput>(input) {
                Ok(args) => {
                    if let Some(options) = args.options.as_deref()
                        && let Err(error) = validate_options(options)
                    {
                        return Ok(ToolExecution::Error(ExecutableToolResult::error(format!(
                            "Invalid arguments: {error}"
                        ))));
                    }
                    Ok(tool.resolve_execution_args(&args).await)
                }
                Err(error) => Ok(ToolExecution::Error(ExecutableToolResult::error(format!(
                    "Invalid arguments for ExitSpecMode: {error}"
                )))),
            }
        })
    }
}
