use std::sync::Arc;

use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use serde_json::Value;

use super::exitPlanMode::{ExitPlanModeInput, ExitPlanModePlanSource};
use super::super::super::plan::{IAgentPlanService, PlanData};
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::callbacks::HostCallbacks;
use crate::permission::PermissionMode;
use crate::tool_input_display::ToolInputDisplay;
use crate::turn_loop::types::ExecutableToolResult as LegacyExecutableToolResult;
use crate::wire::record::WireAsyncError;

pub const EXIT_PLAN_MODE_DESCRIPTION: &str = include_str!("exit-plan-mode.md");

#[derive(Clone)]
pub struct ExitPlanModeTool {
    plan_mode: Arc<dyn IAgentPlanService>,
}

enum ResolvePlanResult {
    Ok { plan: String, path: Option<String> },
    Error(ExecutableToolResult),
}

impl ExitPlanModeTool {
    pub fn new(plan_mode: Arc<dyn IAgentPlanService>) -> Self {
        Self { plan_mode }
    }

    pub async fn execute(
        &self,
        callbacks: &dyn HostCallbacks,
        args: &Value,
    ) -> LegacyExecutableToolResult {
        crate::tools::exit_plan_mode::execute_exit_plan_mode(callbacks, args).await
    }

    pub async fn execute_with_mode(
        &self,
        callbacks: &dyn HostCallbacks,
        args: &Value,
        mode: PermissionMode,
    ) -> LegacyExecutableToolResult {
        crate::tools::exit_plan_mode::execute_exit_plan_mode_with_mode(callbacks, args, mode).await
    }

    pub async fn resolve_execution(&self, args: ExitPlanModeInput) -> ToolExecution {
        let display = self.resolve_plan_review_display(&args).await;
        let tool = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display,
            description: Some("Presenting plan and exiting plan mode".to_string()),
            stop_batch_after_this: None,
            approval_rule: "ExitPlanMode".to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let tool = tool.clone();
                Box::pin(async move {
                    tool.execution().await.map_err(|error| {
                        Box::new(Error2::new(INTERNAL_CODE, error)) as WireAsyncError
                    })
                })
            }),
        })
    }

    async fn resolve_plan_review_display(
        &self,
        args: &ExitPlanModeInput,
    ) -> Option<ToolInputDisplay> {
        let data = match self.plan_mode.status().await {
            Ok(data) => data,
            Err(_) => return None,
        };
        if data
            .as_ref()
            .is_none_or(|data| trim_js(&data.content).is_empty())
        {
            return None;
        }
        let _ = self.plan_mode.record_revision().await;
        resolve_plan_review_display(data.as_ref(), args)
    }

    async fn execution(&self) -> Result<ExecutableToolResult, String> {
        let status = self.plan_mode.status().await?;
        if status.is_none() {
            return Ok(tool_error(PLAN_MODE_NOT_ACTIVE_MESSAGE.to_string()));
        }
        if let ResolvePlanResult::Error(error) = self.resolve_plan().await? {
            return Ok(error);
        }
        Ok(tool_error(PLAN_REVIEW_UNAVAILABLE_MESSAGE.to_string()))
    }

    async fn resolve_plan(&self) -> Result<ResolvePlanResult, String> {
        let source = match self.plan_mode.status().await {
            Ok(data) => data.map(|data| ExitPlanModePlanSource {
                plan: data.content,
                path: data.path,
            }),
            Err(error) => {
                return Ok(ResolvePlanResult::Error(tool_error(format!(
                    "Failed to read plan file: {error}"
                ))));
            }
        };
        if let Some(source) = source
            .as_ref()
            .filter(|source| !trim_js(&source.plan).is_empty())
        {
            return Ok(ResolvePlanResult::Ok {
                plan: source.plan.clone(),
                path: source.path.clone(),
            });
        }
        let status = self.plan_mode.status().await?;
        let path = source
            .as_ref()
            .and_then(|source| source.path.clone())
            .or_else(|| status.as_ref().and_then(|status| status.path.clone()));
        Ok(ResolvePlanResult::Error(tool_error(no_plan_output(
            path.as_deref(),
        ))))
    }
}

fn tool_error(output: String) -> ExecutableToolResult {
    ExecutableToolResult {
        output: output.into(),
        is_error: Some(true),
        ..Default::default()
    }
}

pub fn resolve_plan_review_display(
    data: Option<&PlanData>,
    args: &ExitPlanModeInput,
) -> Option<ToolInputDisplay> {
    let data = data?;
    if trim_js(&data.content).is_empty() {
        return None;
    }
    Some(ToolInputDisplay::PlanReview {
        plan: data.content.clone(),
        path: data.path.clone(),
        options: args.options.as_ref().and_then(|options| {
            (options.len() >= 2).then(|| options.clone())
        }),
    })
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

pub fn no_plan_output(path: Option<&str>) -> String {
    match path {
        Some(path) => {
            format!("No plan file found. Write your plan to {path} first, then call ExitPlanMode.")
        }
        None => "No plan file found. Write the plan to the current plan file first, then call ExitPlanMode.".to_string(),
    }
}

pub const PLAN_MODE_NOT_ACTIVE_MESSAGE: &str =
    "ExitPlanMode can only be called while plan mode is active. Only the user can turn plan mode on.";
pub const PLAN_REVIEW_UNAVAILABLE_MESSAGE: &str =
    "The plan could not be shown to the user for review, so plan mode is still active. Call ExitPlanMode again.";
