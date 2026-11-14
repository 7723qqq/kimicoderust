use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use super::cronList::{CronListInput, cron_list_input_schema};
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tools::main_agent_only::{CRON_MAIN_AGENT_ONLY, main_agent_only_execution};
use crate::features::cron::cron_service::AgentCronService;
use crate::features::cron::cron_task::CronTask;
use crate::features::cron::internal::cron_expr::{cron_to_human, parse_cron_expression};
use crate::features::cron::internal::format::format_local_iso_with_offset;
use crate::wire::record::WireAsyncError;

pub const CRON_LIST_DESCRIPTION: &str = include_str!("cron-list.md");
pub const PROMPT_PREVIEW_BYTES: usize = 200;
pub const MS_PER_DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

#[derive(Clone)]
pub struct CronListTool {
    cron: Arc<AgentCronService>,
    scope: Arc<dyn IAgentScopeContext>,
}

impl CronListTool {
    pub fn new(cron: Arc<AgentCronService>, scope: Arc<dyn IAgentScopeContext>) -> Self {
        Self { cron, scope }
    }

    pub fn name(&self) -> &'static str {
        "CronList"
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        CRON_LIST_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        cron_list_input_schema()
            .as_object()
            .cloned()
            .expect("CronList input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<CronListInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for CronList: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, _args: &CronListInput) -> ToolExecution {
        let agent_id = self.scope.agent_id();
        if let Some(denied) = main_agent_only_execution(&agent_id, CRON_MAIN_AGENT_ONLY) {
            return ToolExecution::Error(ExecutableToolResult::error(denied.content));
        }
        let cron = self.cron.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some("Listing scheduled cron jobs".to_string()),
            stop_batch_after_this: None,
            approval_rule: "CronList".to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let cron = cron.clone();
                Box::pin(async move {
                    let tasks = cron.list();
                    let now_ms = cron.now();
                    let records: Vec<String> = tasks
                        .iter()
                        .map(|task| render_record(cron.as_ref(), task, now_ms))
                        .collect();
                    let header = format!("cron_jobs: {}", tasks.len());
                    if records.is_empty() {
                        return Ok(ExecutableToolResult {
                            output: format!("{header}\nNo cron jobs scheduled.").into(),
                            is_error: Some(false),
                            ..Default::default()
                        });
                    }
                    Ok(ExecutableToolResult {
                        output: format!("{header}\n{}", records.join("\n---\n")).into(),
                        is_error: Some(false),
                        ..Default::default()
                    })
                })
            }),
        })
    }
}

fn render_record(cron: &AgentCronService, task: &CronTask, now_ms: i64) -> String {
    let recurring = task.recurring.unwrap_or(true);

    let age_ms = now_ms - task.created_at;
    let age_days = {
        let days = age_ms as f64 / MS_PER_DAY;
        if days.is_finite() { days } else { 0.0 }
    };

    let stale = cron.is_stale(task);

    let mut human_schedule = task.cron.clone();
    let mut next_fire_at_iso = "null".to_string();
    if let Ok(parsed) = parse_cron_expression(&task.cron) {
        human_schedule = cron_to_human(&parsed);
        if let Some(next_fire_ms) = cron.get_next_fire_for_task(&task.id) {
            next_fire_at_iso = format_local_iso_with_offset(next_fire_ms);
        }
    }

    [
        format!("id: {}", task.id),
        format!("cron: {}", task.cron),
        format!("humanSchedule: {human_schedule}"),
        format!("prompt: {}", json_string(&preview_prompt(&task.prompt))),
        format!("nextFireAt: {next_fire_at_iso}"),
        format!("recurring: {recurring}"),
        format!("ageDays: {age_days:.2}"),
        format!("stale: {stale}"),
    ]
    .join("\n")
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

pub fn preview_prompt(prompt: &str) -> String {
    let bytes = prompt.as_bytes();
    if bytes.len() <= PROMPT_PREVIEW_BYTES {
        return prompt.to_string();
    }
    let mut end = PROMPT_PREVIEW_BYTES;
    while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    format!("{}…(truncated)", String::from_utf8_lossy(&bytes[..end]))
}

impl ExecutableTool for CronListTool {
    fn name(&self) -> &str {
        CronListTool::name(self)
    }

    fn description(&self) -> &str {
        CronListTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        CronListTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
