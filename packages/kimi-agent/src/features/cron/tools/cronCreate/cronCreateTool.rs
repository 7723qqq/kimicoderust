use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use serde_json::{Map, Value, json};

use super::cronCreate::{
    CronCreateInput, CronCreateOutput, MAX_CRON_JOBS_PER_SESSION, MAX_PROMPT_BYTES,
    cron_create_input_schema,
};
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::agent::tools::main_agent_only::{CRON_MAIN_AGENT_ONLY, main_agent_only_execution};
use crate::features::cron::cron_service::AgentCronService;
use crate::features::cron::cron_task::CronTaskInit;
use crate::features::cron::internal::cron_expr::{
    ParsedCronExpression, compute_next_cron_run, cron_to_human, has_fire_within_years,
    parse_cron_expression,
};
use crate::features::cron::internal::format::format_local_iso_with_offset;
use crate::tool::rule_match::literal_rule_pattern;
use crate::wire::record::WireAsyncError;

pub const CRON_CREATE_DESCRIPTION: &str = include_str!("cron-create.md");
pub const ONE_SHOT_MAX_FUTURE_MS: i64 = 350 * 24 * 60 * 60 * 1000;
pub const CRON_DISABLED_MESSAGE: &str = "Cron scheduling is disabled (KIMI_DISABLE_CRON=1).";

#[derive(Clone)]
pub struct CronCreateTool {
    cron: Arc<AgentCronService>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl CronCreateTool {
    pub fn new(cron: Arc<AgentCronService>, scope_context: Arc<dyn IAgentScopeContext>) -> Self {
        Self {
            cron,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        "CronCreate"
    }

    pub fn description(&self) -> &'static str {
        CRON_CREATE_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        cron_create_input_schema()
            .as_object()
            .cloned()
            .expect("CronCreate input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<CronCreateInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for CronCreate: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &CronCreateInput) -> ToolExecution {
        let agent_id = self.scope_context.agent_id();
        if let Some(denied) = main_agent_only_execution(&agent_id, CRON_MAIN_AGENT_ONLY) {
            return ToolExecution::Error(ExecutableToolResult::error(denied.content));
        }
        if self.cron.is_disabled() {
            return ToolExecution::Error(ExecutableToolResult::error(CRON_DISABLED_MESSAGE));
        }

        let normalized_cron = args.cron.split_whitespace().collect::<Vec<_>>().join(" ");

        let parsed = match parse_cron_expression(&normalized_cron) {
            Ok(parsed) => parsed,
            Err(error) => {
                return ToolExecution::Error(ExecutableToolResult::error(format!(
                    "Invalid cron expression: {error}"
                )));
            }
        };

        let now_at_prepare = self.cron.now();
        if !has_fire_within_years(&parsed, 5.0, now_at_prepare) {
            return ToolExecution::Error(ExecutableToolResult::error(format!(
                "Cron expression {} has no fire within 5 years; refusing to schedule.",
                json_string(&normalized_cron)
            )));
        }

        if self.cron.list().len() >= MAX_CRON_JOBS_PER_SESSION {
            return ToolExecution::Error(ExecutableToolResult::error(format!(
                "Cron job cap reached (max {MAX_CRON_JOBS_PER_SESSION} per session)."
            )));
        }

        let byte_len = args.prompt.len();
        if byte_len > MAX_PROMPT_BYTES {
            return ToolExecution::Error(ExecutableToolResult::error(format!(
                "Prompt exceeds {MAX_PROMPT_BYTES} bytes (got {byte_len})."
            )));
        }

        let recurring = args.recurring;

        if !recurring {
            let first_fire = compute_next_cron_run(&parsed, now_at_prepare);
            if let Some(first_fire) = first_fire
                && first_fire - now_at_prepare > ONE_SHOT_MAX_FUTURE_MS
            {
                return ToolExecution::Error(ExecutableToolResult::error(format!(
                    "One-shot cron {} would not fire until {} (more than a year out). If you meant \"today\" or a near date, the pinned day/month has already passed this year — pick a future date or use wildcards.",
                    json_string(&normalized_cron),
                    format_local_iso_with_offset(first_fire)
                )));
            }
        }

        let approval_subject = json!({
            "cron": normalized_cron,
            "prompt": args.prompt,
            "recurring": recurring,
        })
        .to_string();
        let description = if recurring {
            format!("Scheduling cron {normalized_cron}")
        } else {
            format!("Scheduling one-shot {normalized_cron}")
        };
        let cron = self.cron.clone();
        let agent_id = self.scope_context.agent_id();
        let prompt = args.prompt.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: literal_rule_pattern("CronCreate", &approval_subject),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let cron = cron.clone();
                let agent_id = agent_id.clone();
                let normalized_cron = normalized_cron.clone();
                let prompt = prompt.clone();
                let parsed = parsed.clone();
                Box::pin(async move {
                    let now_ms = cron.now();

                    if cron.list().len() >= MAX_CRON_JOBS_PER_SESSION {
                        return Ok(ExecutableToolResult::error(format!(
                            "Cron job cap reached (max {MAX_CRON_JOBS_PER_SESSION} per session)."
                        )));
                    }

                    let task = cron
                        .add_task(CronTaskInit {
                            cron: normalized_cron.clone(),
                            prompt,
                            recurring: Some(recurring),
                            ..Default::default()
                        })
                        .map_err(|error| {
                            Box::new(Error2::new(INTERNAL_CODE, error)) as WireAsyncError
                        })?;

                    let ideal = compute_next_cron_run(&parsed, now_ms);
                    let next_fire_at = ideal
                        .and_then(|ideal| cron.compute_display_next_fire(&task, &parsed, ideal));
                    let human_schedule = cron_to_human(&parsed);

                    cron.emit_scheduled(&task, Some(&agent_id));

                    let output = CronCreateOutput {
                        id: task.id,
                        cron: normalized_cron,
                        human_schedule,
                        recurring,
                        next_fire_at,
                    };

                    Ok(ExecutableToolResult {
                        output: format_output(&output).into(),
                        is_error: Some(false),
                        ..Default::default()
                    })
                })
            }),
        })
    }
}

fn format_output(output: &CronCreateOutput) -> String {
    let next_fire_at = match output.next_fire_at {
        Some(next_fire_at) => format_local_iso_with_offset(next_fire_at),
        None => "null".to_string(),
    };
    [
        format!("id: {}", output.id),
        format!("cron: {}", output.cron),
        format!("humanSchedule: {}", output.human_schedule),
        format!("recurring: {}", output.recurring),
        format!("nextFireAt: {next_fire_at}"),
    ]
    .join("\n")
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

impl ExecutableTool for CronCreateTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Write)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        CronCreateTool::name(self)
    }

    fn description(&self) -> &str {
        CronCreateTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        CronCreateTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
