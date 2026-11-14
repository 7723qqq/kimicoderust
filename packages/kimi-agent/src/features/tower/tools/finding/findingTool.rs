use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::features::tower::protocol::store::TowerFindingInput;
use crate::features::tower::tools::support::{caller_name, new_tower_store, run_tower_tool};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::finding::{TowerFindingToolInput, tower_finding_tool_input_schema};

pub const TOWER_FINDING_TOOL_NAME: &str = "TowerFinding";

#[derive(Clone)]
pub struct TowerFindingTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl TowerFindingTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            session_context,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_FINDING_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("finding.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_finding_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerFinding input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerFindingToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerFinding: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerFindingToolInput) -> ToolExecution {
        let description = format!(
            "Filing tower {} finding: {}",
            args.r#type.as_str(),
            args.title
        );
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_FINDING_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerFindingToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let state = store.load().await?;
            let caller = caller_name(&self.scope_context.agent_id(), &store, &state)?;
            let rel = store
                .file_finding(
                    &caller,
                    TowerFindingInput {
                        finding_type: args.r#type,
                        title: args.title,
                        severity: args.severity,
                        summary: args.summary,
                        location: args.location,
                        details: args.details,
                        suggested_fix: args.suggested_fix,
                    },
                )
                .await?;
            Ok(ExecutableToolResult {
                output: format!(
                    "finding filed: {rel}\nThe tower will route it — do not fix out-of-scope issues yourself."
                )
                .into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerFindingTool {
    fn name(&self) -> &str {
        TowerFindingTool::name(self)
    }

    fn description(&self) -> &str {
        TowerFindingTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerFindingTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
