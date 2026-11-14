use std::sync::{Arc, LazyLock};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use super::cronDelete::{CronDeleteInput, cron_delete_input_schema};
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tools::main_agent_only::{CRON_MAIN_AGENT_ONLY, main_agent_only_execution};
use crate::features::cron::cron_service::AgentCronService;
use crate::wire::record::WireAsyncError;

pub const CRON_DELETE_DESCRIPTION: &str = include_str!("cron-delete.md");

static ID_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(?:[0-9a-f]{8}|[0-9A-HJKMNP-TV-Z]{26})$").expect("valid cron id regex")
});

#[derive(Clone)]
pub struct CronDeleteTool {
    cron: Arc<AgentCronService>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl CronDeleteTool {
    pub fn new(cron: Arc<AgentCronService>, scope_context: Arc<dyn IAgentScopeContext>) -> Self {
        Self {
            cron,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        "CronDelete"
    }

    pub fn side_effect(&self) -> &'static str {
        "write"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        CRON_DELETE_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        cron_delete_input_schema()
            .as_object()
            .cloned()
            .expect("CronDelete input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<CronDeleteInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for CronDelete: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &CronDeleteInput) -> ToolExecution {
        let agent_id = self.scope_context.agent_id();
        if let Some(denied) = main_agent_only_execution(&agent_id, CRON_MAIN_AGENT_ONLY) {
            return ToolExecution::Error(ExecutableToolResult::error(denied.content));
        }
        if !ID_PATTERN.is_match(&args.id) {
            return ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid cron job id {} — must be a ULID.",
                json_string(&args.id)
            )));
        }

        let cron = self.cron.clone();
        let agent_id = self.scope_context.agent_id();
        let id = args.id.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(format!("Deleting cron {id}")),
            stop_batch_after_this: None,
            approval_rule: "CronDelete".to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let cron = cron.clone();
                let agent_id = agent_id.clone();
                let id = id.clone();
                Box::pin(async move {
                    let removed = cron.remove_tasks(std::slice::from_ref(&id));
                    if removed.is_empty() {
                        return Ok(ExecutableToolResult::error(format!(
                            "No cron job with id {id}."
                        )));
                    }

                    cron.emit_deleted(&id, Some(&agent_id));

                    Ok(ExecutableToolResult {
                        output: format!("Deleted cron job {id}.").into(),
                        is_error: Some(false),
                        ..Default::default()
                    })
                })
            }),
        })
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

impl ExecutableTool for CronDeleteTool {
    fn name(&self) -> &str {
        CronDeleteTool::name(self)
    }

    fn description(&self) -> &str {
        CronDeleteTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        CronDeleteTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
